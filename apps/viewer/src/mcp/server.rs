use std::{
    borrow::Cow,
    io,
    net::{Ipv4Addr, SocketAddr, TcpListener as StdTcpListener},
    sync::Arc,
    time::Duration,
};

use axum::{
    Router,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorCode, Implementation, JsonObject, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    },
    service::RequestContext,
    transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager},
};
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    runtime::Handle,
    sync::{Semaphore, watch},
    task::JoinHandle,
    time,
};

use super::{
    MAX_CONCURRENT_REQUESTS, MAX_JSON_RESPONSE_BYTES, MAX_REQUEST_BYTES, REQUEST_TIMEOUT,
    tools::{TOOLS, ToolContent, find_tool},
};
use crate::inspection::ViewerInspectionHandle;

#[derive(Clone, Copy, Debug)]
pub struct McpServerConfig {
    pub port: u16,
    pub max_concurrent_requests: usize,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub request_timeout: Duration,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            port: 0,
            max_concurrent_requests: MAX_CONCURRENT_REQUESTS,
            max_request_bytes: MAX_REQUEST_BYTES,
            max_response_bytes: MAX_JSON_RESPONSE_BYTES,
            request_timeout: REQUEST_TIMEOUT,
        }
    }
}

pub struct McpServer {
    addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl McpServer {
    pub fn start(handle: ViewerInspectionHandle, config: McpServerConfig, runtime: &Handle) -> io::Result<Self> {
        let bind_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, config.port));
        let std_listener = StdTcpListener::bind(bind_addr)?;
        std_listener.set_nonblocking(true)?;
        let addr = std_listener.local_addr()?;
        let _entered = runtime.enter();
        let listener = TcpListener::from_std(std_listener)?;
        let (shutdown, mut shutdown_rx) = watch::channel(false);

        let handler = RadiaMcpHandler { handle, config };
        let mut http_config = StreamableHttpServerConfig::default();
        http_config.legacy_session_mode = false;
        http_config.json_response = true;
        http_config.sse_keep_alive = None;
        http_config.max_request_body_bytes = config.max_request_bytes;
        http_config = http_config.with_allowed_hosts([format!("127.0.0.1:{}", addr.port()), format!("localhost:{}", addr.port())]);
        let cancellation_token = http_config.cancellation_token.clone();

        let service = StreamableHttpService::new(move || Ok(handler.clone()), Arc::new(NeverSessionManager::default()), http_config);
        let guard_state = GuardState { port: addr.port(), semaphore: Arc::new(Semaphore::new(config.max_concurrent_requests.max(1))) };
        let router = Router::new().nest_service("/mcp", service).layer(middleware::from_fn_with_state(guard_state, guard_middleware));

        let task = runtime.spawn(async move {
            let server = axum::serve(listener, router).with_graceful_shutdown(async move {
                while !*shutdown_rx.borrow_and_update() {
                    if shutdown_rx.changed().await.is_err() {
                        break;
                    }
                }
                cancellation_token.cancel();
            });
            if let Err(error) = server.await {
                tracing::warn!(%error, "local MCP server stopped with error");
            }
        });
        tracing::info!(%addr, "local MCP server listening");
        Ok(Self { addr, shutdown, task })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn shutdown(self) {
        self.shutdown.send_replace(true);
        self.task.abort();
    }
}

#[derive(Clone)]
struct GuardState {
    port: u16,
    semaphore: Arc<Semaphore>,
}

async fn guard_middleware(State(state): State<GuardState>, request: Request, next: Next) -> Response {
    let mut hosts = request.headers().get_all(header::HOST).iter();
    let host = match (hosts.next(), hosts.next()) {
        (Some(value), None) => value.to_str().ok(),
        _ => None,
    };
    if !is_allowed_host(host, state.port) {
        return (StatusCode::FORBIDDEN, "{\"error\":\"forbidden host or origin\"}").into_response();
    }

    let mut origins = request.headers().get_all(header::ORIGIN).iter();
    let origin = match (origins.next(), origins.next()) {
        (None, _) => Some(None),
        (Some(value), None) => value.to_str().ok().map(Some),
        _ => None,
    };
    let Some(origin) = origin else {
        return (StatusCode::FORBIDDEN, "{\"error\":\"forbidden host or origin\"}").into_response();
    };
    if !is_allowed_origin(origin) {
        return (StatusCode::FORBIDDEN, "{\"error\":\"forbidden host or origin\"}").into_response();
    }

    let Ok(_permit) = state.semaphore.try_acquire_owned() else {
        return (StatusCode::TOO_MANY_REQUESTS, "{\"error\":\"server busy: concurrency limit reached\"}").into_response();
    };

    next.run(request).await
}

#[derive(Clone)]
struct RadiaMcpHandler {
    handle: ViewerInspectionHandle,
    config: McpServerConfig,
}

impl ServerHandler for RadiaMcpHandler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("radia", env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = TOOLS
            .iter()
            .map(|tool| {
                let schema = match (tool.input_schema)() {
                    Value::Object(map) => map,
                    _ => JsonObject::new(),
                };
                Tool::new_with_raw(tool.name, Some(Cow::Borrowed(tool.description)), Arc::new(schema))
            })
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(&self, request: CallToolRequestParams, _context: RequestContext<RoleServer>) -> Result<CallToolResponse, ErrorData> {
        let Some(tool) = find_tool(request.name.as_ref()) else {
            return Err(ErrorData::invalid_params("unknown tool", None));
        };
        let arguments = request.arguments.map(Value::Object).unwrap_or_else(|| json!({}));
        let outcome = time::timeout(self.config.request_timeout, (tool.handler)(self.handle.clone(), arguments)).await;
        let (result, max_bytes) = match outcome {
            Ok(Ok(ToolContent::Json(value))) => {
                let limit = tool.max_response_bytes.min(self.config.max_response_bytes);
                (CallToolResult::success(vec![ContentBlock::text(value.to_string())]), limit)
            }
            Ok(Ok(ToolContent::ImagePng { data_base64, width, height })) => (
                CallToolResult::success(vec![
                    ContentBlock::image(data_base64, "image/png"),
                    ContentBlock::text(json!({ "width": width, "height": height }).to_string()),
                ]),
                tool.max_response_bytes,
            ),
            Ok(Err(message)) => (CallToolResult::error(vec![ContentBlock::text(message)]), self.config.max_response_bytes),
            Err(_) => return Err(ErrorData::new(ErrorCode(-32001), "tool call timed out", None)),
        };
        let encoded_len = serde_json::to_vec(&result).map(|bytes| bytes.len()).unwrap_or(usize::MAX);
        if encoded_len > max_bytes {
            return Err(ErrorData::new(ErrorCode(-32002), "response exceeded size limit", None));
        }
        Ok(CallToolResponse::Complete(result))
    }
}

fn is_allowed_host(host: Option<&str>, port: u16) -> bool {
    let Some(host) = host else {
        return false;
    };
    let Some((name, host_port)) = host.split_once(':') else {
        return false;
    };
    host_port.bytes().all(|byte| byte.is_ascii_digit()) && host_port.parse::<u16>().ok() == Some(port) && is_loopback_host(name)
}

fn is_allowed_origin(origin: Option<&str>) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    if authority.is_empty() || authority.contains(['/', '?', '#', '@']) {
        return false;
    }
    let host = match authority.split_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) && port.parse::<u16>().is_ok() => host,
        Some(_) => return false,
        None => authority,
    };
    is_loopback_host(host)
}

fn is_loopback_host(host: &str) -> bool {
    host == "127.0.0.1" || host.eq_ignore_ascii_case("localhost")
}

#[cfg(test)]
mod tests {
    use std::{
        net::{Ipv4Addr, SocketAddr, TcpListener as StdTcpListener},
        sync::Arc,
    };

    use axum::{Router, middleware, routing::post};
    use reqwest::{
        Client,
        header::{ACCEPT, CONTENT_TYPE, HOST, ORIGIN},
    };
    use serde_json::{Value, json};
    use tokio::{
        net::TcpListener,
        runtime::Handle,
        sync::{Mutex, Semaphore, oneshot},
    };

    use super::{GuardState, McpServer, McpServerConfig, TOOLS, guard_middleware};
    use crate::{
        inspection::inspection_channel,
        logging::{LogLevel, RecentLogEntry, RecentLogs},
    };

    fn mcp_url(addr: SocketAddr) -> String {
        format!("http://127.0.0.1:{}/mcp", addr.port())
    }

    async fn post_mcp(client: &Client, addr: SocketAddr, body: &Value) -> (u16, String) {
        let response = client
            .post(mcp_url(addr))
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .body(body.to_string())
            .send()
            .await
            .expect("send MCP request");
        let status = response.status().as_u16();
        let text = response.text().await.expect("read response body");
        (status, text)
    }

    #[tokio::test]
    async fn initialize_list_and_call_work() {
        let logs = RecentLogs::with_capacity(8);
        logs.push(RecentLogEntry {
            timestamp_ms: 100,
            level: LogLevel::Warn,
            target: "viewer::test".to_owned(),
            message: "sample warning".to_owned(),
        });
        let (_pub, handle) = inspection_channel(logs);
        let server = McpServer::start(handle, McpServerConfig::default(), &Handle::current()).expect("start server");
        let addr = server.addr();
        let client = Client::new();

        let init = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "test-client", "version": "0.1.0" }
            }
        });
        let (status, body) = post_mcp(&client, addr, &init).await;
        assert_eq!(status, 200, "initialize body: {body}");
        let parsed: Value = serde_json::from_str(&body).expect("json response");
        assert_eq!(parsed["result"]["serverInfo"]["name"], "radia");

        let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let (status, body) = post_mcp(&client, addr, &list).await;
        assert_eq!(status, 200, "list body: {body}");
        let parsed: Value = serde_json::from_str(&body).expect("json response");
        let tools = parsed["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), TOOLS.len());

        let call = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "logs", "arguments": { "limit": 10 } }
        });
        let (status, body) = post_mcp(&client, addr, &call).await;
        assert_eq!(status, 200, "call body: {body}");
        let parsed: Value = serde_json::from_str(&body).expect("json response");
        assert_eq!(parsed["result"]["isError"], false);
        let text = parsed["result"]["content"][0]["text"].as_str().expect("text content");
        assert!(text.contains("sample warning"));

        server.shutdown();
    }

    #[tokio::test]
    async fn unknown_tool_returns_error() {
        let (_pub, handle) = inspection_channel(RecentLogs::default());
        let server = McpServer::start(handle, McpServerConfig::default(), &Handle::current()).expect("start server");
        let addr = server.addr();
        let client = Client::new();

        let call = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "does_not_exist", "arguments": {} }
        });
        let (status, body) = post_mcp(&client, addr, &call).await;
        assert_eq!(status, 200);
        let parsed: Value = serde_json::from_str(&body).expect("json response");
        assert!(parsed.get("error").is_some(), "expected JSON-RPC error, got: {body}");

        server.shutdown();
    }

    #[tokio::test]
    async fn rejects_bad_host_and_origin() {
        let (_pub, handle) = inspection_channel(RecentLogs::default());
        let server = McpServer::start(handle, McpServerConfig::default(), &Handle::current()).expect("start server");
        let addr = server.addr();
        let client = Client::new();
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string();

        let evil_host = format!("evil.example:{}", addr.port());
        for host in ["127.0.0.1", evil_host.as_str()] {
            let response = client
                .post(mcp_url(addr))
                .header(HOST, host)
                .header(CONTENT_TYPE, "application/json")
                .header(ACCEPT, "application/json, text/event-stream")
                .body(body.clone())
                .send()
                .await
                .expect("send bad host request");
            assert_eq!(response.status().as_u16(), 403, "expected 403 for Host={host}");
        }

        for origin in ["http://evil.example", "http://localhost.evil.example:3000", "null"] {
            let response = client
                .post(mcp_url(addr))
                .header(ORIGIN, origin)
                .header(CONTENT_TYPE, "application/json")
                .header(ACCEPT, "application/json, text/event-stream")
                .body(body.clone())
                .send()
                .await
                .expect("send bad origin request");
            assert_eq!(response.status().as_u16(), 403, "expected 403 for Origin={origin}");
        }

        let localhost_origin = format!("http://localhost:{}", addr.port());
        let ok = client
            .post(mcp_url(addr))
            .header(ORIGIN, localhost_origin)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .body(body)
            .send()
            .await
            .expect("send loopback origin request");
        assert_eq!(ok.status().as_u16(), 200);

        server.shutdown();
    }

    #[tokio::test]
    async fn rejects_oversized_request_and_response() {
        let logs = RecentLogs::with_capacity(8);
        logs.push(RecentLogEntry {
            timestamp_ms: 100,
            level: LogLevel::Error,
            target: "viewer::test".to_owned(),
            message: "x".repeat(200),
        });
        let (_pub, handle) = inspection_channel(logs);
        let config = McpServerConfig { max_request_bytes: 100, max_response_bytes: 128, ..McpServerConfig::default() };
        let server = McpServer::start(handle, config, &Handle::current()).expect("start server");
        let addr = server.addr();
        let client = Client::new();

        let response = client
            .post(mcp_url(addr))
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .body("a".repeat(128))
            .send()
            .await
            .expect("send oversized request");
        assert_eq!(response.status().as_u16(), 413);

        let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "logs" }});
        let (status, body) = post_mcp(&client, addr, &call).await;
        assert_eq!(status, 200);
        let parsed: Value = serde_json::from_str(&body).expect("json response");
        assert_eq!(parsed["error"]["code"], -32002);

        server.shutdown();
    }

    #[tokio::test]
    async fn concurrency_is_bounded() {
        let std_listener = StdTcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).expect("bind listener");
        std_listener.set_nonblocking(true).expect("set nonblocking");
        let addr = std_listener.local_addr().expect("local addr");
        let listener = TcpListener::from_std(std_listener).expect("tokio listener");

        let (entered_tx, entered_rx) = oneshot::channel::<()>();
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let entered_tx = Arc::new(Mutex::new(Some(entered_tx)));
        let release_rx = Arc::new(Mutex::new(Some(release_rx)));

        let router = Router::new()
            .route(
                "/mcp",
                post({
                    let entered_tx = Arc::clone(&entered_tx);
                    let release_rx = Arc::clone(&release_rx);
                    move || {
                        let entered_tx = Arc::clone(&entered_tx);
                        let release_rx = Arc::clone(&release_rx);
                        async move {
                            if let Some(tx) = entered_tx.lock().await.take() {
                                let _ = tx.send(());
                            }
                            if let Some(rx) = release_rx.lock().await.take() {
                                let _ = rx.await;
                            }
                            "ok"
                        }
                    }
                }),
            )
            .layer(middleware::from_fn_with_state(
                GuardState { port: addr.port(), semaphore: Arc::new(Semaphore::new(1)) },
                guard_middleware,
            ));

        let server_task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let client = Client::new();
        let first = tokio::spawn({
            let client = client.clone();
            async move { client.post(mcp_url(addr)).send().await.expect("first request").status().as_u16() }
        });

        entered_rx.await.expect("first request entered handler");
        let second_status = client.post(mcp_url(addr)).send().await.expect("second request").status().as_u16();
        let _ = release_tx.send(());
        let first_status = first.await.expect("first task finished");

        assert_eq!(first_status, 200);
        assert_eq!(second_status, 429);
        server_task.abort();
    }
}
