use std::{
    borrow::Cow,
    error::Error,
    fmt, io,
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
use futures::Stream;
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ClientJsonRpcMessage, CompleteRequestParams, CompleteResult, ContentBlock,
        ErrorCode, GetPromptRequestParams, GetPromptResponse, Implementation, JsonObject, ListPromptsResult, ListResourceTemplatesResult,
        ListResourcesResult, ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse, ServerCapabilities,
        ServerConfig, ServerJsonRpcMessage, Tool, ToolAnnotations,
    },
    service::RequestContext,
    transport::{
        common::server_side_http::ServerSseMessage,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService,
            session::{
                SessionId, SessionManager,
                local::{LocalSessionManager, LocalSessionManagerError},
            },
        },
    },
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
    MAX_CONCURRENT_REQUESTS, MAX_JSON_RESPONSE_BYTES, MAX_REQUEST_BYTES, MAX_SESSIONS, REQUEST_TIMEOUT, SERVER_INSTRUCTIONS,
    SESSION_IDLE_TIMEOUT, prompts, resources,
    tools::{TOOLS, ToolCallContext, ToolContent, find_tool, screenshot, wait_for},
};
use crate::inspection::ViewerInspectionHandle;

#[derive(Clone, Copy, Debug)]
pub struct McpServerConfig {
    pub port: u16,
    pub max_concurrent_requests: usize,
    pub max_sessions: usize,
    pub session_idle_timeout: Duration,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub request_timeout: Duration,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            port: 0,
            max_concurrent_requests: MAX_CONCURRENT_REQUESTS,
            max_sessions: MAX_SESSIONS,
            session_idle_timeout: SESSION_IDLE_TIMEOUT,
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
        http_config.legacy_session_mode = true;
        http_config.json_response = false;
        http_config.max_request_body_bytes = config.max_request_bytes;
        http_config = http_config.with_allowed_hosts([format!("127.0.0.1:{}", addr.port()), format!("localhost:{}", addr.port())]);
        let cancellation_token = http_config.cancellation_token.clone();

        let session_manager = Arc::new(BoundedSessionManager::new(config.max_sessions, config.session_idle_timeout));
        let service = StreamableHttpService::new(move || Ok(handler.clone()), session_manager, http_config);
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

#[derive(Debug)]
enum BoundedSessionError {
    LimitReached { max_sessions: usize },
    Inner(LocalSessionManagerError),
}

impl fmt::Display for BoundedSessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitReached { max_sessions } => write!(f, "session limit reached ({max_sessions})"),
            Self::Inner(error) => error.fmt(f),
        }
    }
}

impl Error for BoundedSessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::LimitReached { .. } => None,
            Self::Inner(error) => Some(error),
        }
    }
}

impl From<LocalSessionManagerError> for BoundedSessionError {
    fn from(error: LocalSessionManagerError) -> Self {
        Self::Inner(error)
    }
}

struct BoundedSessionManager {
    inner: LocalSessionManager,
    max_sessions: usize,
}

impl BoundedSessionManager {
    fn new(max_sessions: usize, idle_timeout: Duration) -> Self {
        let mut inner = LocalSessionManager::default();
        inner.session_config.keep_alive = Some(idle_timeout);
        Self { inner, max_sessions: max_sessions.max(1) }
    }
}

impl SessionManager for BoundedSessionManager {
    type Error = BoundedSessionError;
    type Transport = <LocalSessionManager as SessionManager>::Transport;

    async fn create_session(&self) -> Result<(SessionId, Self::Transport), Self::Error> {
        let live_count = self.inner.sessions.read().await.len();
        if live_count >= self.max_sessions {
            return Err(BoundedSessionError::LimitReached { max_sessions: self.max_sessions });
        }
        Ok(self.inner.create_session().await?)
    }

    async fn initialize_session(&self, id: &SessionId, message: ClientJsonRpcMessage) -> Result<ServerJsonRpcMessage, Self::Error> {
        Ok(self.inner.initialize_session(id, message).await?)
    }

    async fn has_session(&self, id: &SessionId) -> Result<bool, Self::Error> {
        Ok(self.inner.has_session(id).await?)
    }

    async fn close_session(&self, id: &SessionId) -> Result<(), Self::Error> {
        Ok(self.inner.close_session(id).await?)
    }

    async fn create_stream(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + 'static, Self::Error> {
        Ok(self.inner.create_stream(id, message).await?)
    }

    async fn create_standalone_stream(&self, id: &SessionId) -> Result<impl Stream<Item = ServerSseMessage> + Send + 'static, Self::Error> {
        Ok(self.inner.create_standalone_stream(id).await?)
    }

    async fn resume(
        &self,
        id: &SessionId,
        last_event_id: String,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + 'static, Self::Error> {
        Ok(self.inner.resume(id, last_event_id).await?)
    }

    async fn accept_message(&self, id: &SessionId, message: ClientJsonRpcMessage) -> Result<(), Self::Error> {
        Ok(self.inner.accept_message(id, message).await?)
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
        let capabilities = ServerCapabilities::builder().enable_tools().enable_resources().enable_prompts().enable_completions().build();
        ServerConfig::new(capabilities)
            .with_server_info(Implementation::new("radia", env!("CARGO_PKG_VERSION")))
            .with_instructions(SERVER_INSTRUCTIONS)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(resources::static_resources()))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(ListResourceTemplatesResult::with_all_items(resources::resource_templates()))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let ct = context.ct.clone();
        let tool_ctx = ToolCallContext::new(context);
        let result = tokio::select! {
            biased;
            () = ct.cancelled() => {
                return Err(ErrorData::new(ErrorCode(-32000), "request cancelled", None));
            }
            result = resources::read_resource(&request.uri, &self.handle, self.config, tool_ctx) => result?,
        };
        Ok(ReadResourceResponse::Complete(result))
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult::with_all_items(prompts::list_prompts()))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        Ok(GetPromptResponse::Complete(prompts::get_prompt(&request)?))
    }

    async fn complete(&self, request: CompleteRequestParams, _context: RequestContext<RoleServer>) -> Result<CompleteResult, ErrorData> {
        Ok(prompts::complete(&request, &self.handle).await)
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
                let mut annotations = ToolAnnotations::new().read_only(tool.annotations.read_only).open_world(tool.annotations.open_world);
                if let Some(destructive) = tool.annotations.destructive {
                    annotations = annotations.destructive(destructive);
                }
                if let Some(idempotent) = tool.annotations.idempotent {
                    annotations = annotations.idempotent(idempotent);
                }
                Tool::new_with_raw(tool.name, Some(Cow::Borrowed(tool.description)), Arc::new(schema))
                    .with_title(tool.title)
                    .with_annotations(annotations)
            })
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(&self, request: CallToolRequestParams, context: RequestContext<RoleServer>) -> Result<CallToolResponse, ErrorData> {
        let Some(tool) = find_tool(request.name.as_ref()) else {
            return Err(ErrorData::invalid_params("unknown tool", None));
        };
        let arguments = request.arguments.map(Value::Object).unwrap_or_else(|| json!({}));
        let ct = context.ct.clone();
        let tool_ctx = ToolCallContext::new(context);
        let call_future = match tool.name {
            "wait_for" => wait_for::call_with_context(self.handle.clone(), arguments, tool_ctx),
            "screenshot" => screenshot::call_with_context(self.handle.clone(), arguments, tool_ctx),
            _ => (tool.handler)(self.handle.clone(), arguments),
        };
        let outcome = tokio::select! {
            biased;
            () = ct.cancelled() => {
                return Err(ErrorData::new(ErrorCode(-32000), "request cancelled", None));
            }
            outcome = time::timeout(self.config.request_timeout, call_future) => outcome,
        };
        let (mut result, max_bytes) = match outcome {
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
        if result.is_error == Some(false)
            && matches!(tool.name, "screenshot" | "world" | "logs")
            && let Some(link) = resources::resource_for_tool(tool.name)
        {
            result.content.push(ContentBlock::resource_link(link));
        }
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

    const SESSION_HEADER: &str = "mcp-session-id";

    fn mcp_url(addr: SocketAddr) -> String {
        format!("http://127.0.0.1:{}/mcp", addr.port())
    }

    fn init_payload() -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "test-client", "version": "0.1.0" }
            }
        })
    }

    fn parse_mcp_payload(body: &str) -> Value {
        if let Ok(value) = serde_json::from_str::<Value>(body) {
            return value;
        }
        for line in body.lines() {
            if let Some(data) = line.strip_prefix("data:") {
                let trimmed = data.trim();
                if !trimmed.is_empty()
                    && let Ok(value) = serde_json::from_str::<Value>(trimmed)
                {
                    return value;
                }
            }
        }
        panic!("expected JSON or SSE data payload, got: {body}");
    }

    async fn post_mcp(client: &Client, addr: SocketAddr, session_id: Option<&str>, body: &Value) -> (u16, Option<String>, String) {
        let mut request = client
            .post(mcp_url(addr))
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .body(body.to_string());
        if let Some(id) = session_id {
            request = request.header(SESSION_HEADER, id);
        }
        let response = request.send().await.expect("send MCP request");
        let status = response.status().as_u16();
        let next_session = response.headers().get(SESSION_HEADER).and_then(|value| value.to_str().ok()).map(str::to_owned);
        let text = response.text().await.expect("read response body");
        (status, next_session, text)
    }

    async fn open_session(client: &Client, addr: SocketAddr) -> (String, Value) {
        let (status, session_id, body) = post_mcp(client, addr, None, &init_payload()).await;
        assert_eq!(status, 200, "initialize body: {body}");
        let session_id = session_id.expect("mcp-session-id header");
        let parsed = parse_mcp_payload(&body);
        let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let (notif_status, _, notif_body) = post_mcp(client, addr, Some(&session_id), &initialized).await;
        assert_eq!(notif_status, 202, "initialized body: {notif_body}");
        (session_id, parsed)
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

        let (session_id, init_parsed) = open_session(&client, addr).await;
        assert_eq!(init_parsed["result"]["serverInfo"]["name"], "radia");

        let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let (status, _, body) = post_mcp(&client, addr, Some(&session_id), &list).await;
        assert_eq!(status, 200, "list body: {body}");
        let parsed = parse_mcp_payload(&body);
        let tools = parsed["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), TOOLS.len());

        let call = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "logs", "arguments": { "limit": 10 } }
        });
        let (status, _, body) = post_mcp(&client, addr, Some(&session_id), &call).await;
        assert_eq!(status, 200, "call body: {body}");
        let parsed = parse_mcp_payload(&body);
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
        let (session_id, _) = open_session(&client, addr).await;

        let call = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "does_not_exist", "arguments": {} }
        });
        let (status, _, body) = post_mcp(&client, addr, Some(&session_id), &call).await;
        assert_eq!(status, 200);
        let parsed = parse_mcp_payload(&body);
        assert!(parsed.get("error").is_some(), "expected JSON-RPC error, got: {body}");

        server.shutdown();
    }

    #[tokio::test]
    async fn rejects_bad_host_and_origin() {
        let (_pub, handle) = inspection_channel(RecentLogs::default());
        let server = McpServer::start(handle, McpServerConfig::default(), &Handle::current()).expect("start server");
        let addr = server.addr();
        let client = Client::new();
        let body = init_payload().to_string();

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
        let config = McpServerConfig { max_request_bytes: 256, max_response_bytes: 128, ..McpServerConfig::default() };
        let server = McpServer::start(handle, config, &Handle::current()).expect("start server");
        let addr = server.addr();
        let client = Client::new();
        let (session_id, _) = open_session(&client, addr).await;

        let response = client
            .post(mcp_url(addr))
            .header(SESSION_HEADER, &session_id)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .body("a".repeat(300))
            .send()
            .await
            .expect("send oversized request");
        assert_eq!(response.status().as_u16(), 413);

        let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "logs" }});
        let (status, _, body) = post_mcp(&client, addr, Some(&session_id), &call).await;
        assert_eq!(status, 200);
        let parsed = parse_mcp_payload(&body);
        assert_eq!(parsed["error"]["code"], -32002);

        server.shutdown();
    }

    #[tokio::test]
    async fn session_cap_rejects_excess_sessions() {
        let (_pub, handle) = inspection_channel(RecentLogs::default());
        let config = McpServerConfig { max_sessions: 2, ..McpServerConfig::default() };
        let server = McpServer::start(handle, config, &Handle::current()).expect("start server");
        let addr = server.addr();
        let client = Client::new();

        let _first = open_session(&client, addr).await;
        let _second = open_session(&client, addr).await;

        let (status, _, _) = post_mcp(&client, addr, None, &init_payload()).await;
        assert_ne!(status, 200, "third session beyond cap must be rejected");

        server.shutdown();
    }

    #[tokio::test]
    async fn resources_list_templates_read_and_unknown_object() {
        let logs = RecentLogs::with_capacity(8);
        logs.push(RecentLogEntry {
            timestamp_ms: 200,
            level: LogLevel::Warn,
            target: "viewer::res".to_owned(),
            message: "resource check".to_owned(),
        });
        let (_pub, handle) = inspection_channel(logs);
        let server = McpServer::start(handle, McpServerConfig::default(), &Handle::current()).expect("start server");
        let addr = server.addr();
        let client = Client::new();
        let (session_id, _) = open_session(&client, addr).await;

        let list_req = json!({"jsonrpc": "2.0", "id": 10, "method": "resources/list"});
        let (status, _, body) = post_mcp(&client, addr, Some(&session_id), &list_req).await;
        assert_eq!(status, 200, "resources/list body: {body}");
        let parsed = parse_mcp_payload(&body);
        let uris: Vec<&str> =
            parsed["result"]["resources"].as_array().expect("resources array").iter().filter_map(|entry| entry["uri"].as_str()).collect();
        for expected in ["radia://world", "radia://stats", "radia://region", "radia://settings", "radia://logs", "radia://screenshot"] {
            assert!(uris.contains(&expected), "missing resource {expected} in {uris:?}");
        }

        let tpl_req = json!({"jsonrpc": "2.0", "id": 11, "method": "resources/templates/list"});
        let (status, _, body) = post_mcp(&client, addr, Some(&session_id), &tpl_req).await;
        assert_eq!(status, 200, "resources/templates/list body: {body}");
        let parsed = parse_mcp_payload(&body);
        let templates = parsed["result"]["resourceTemplates"].as_array().expect("templates array");
        assert!(templates.iter().any(|entry| entry["uriTemplate"] == "radia://object/{local_id}"));

        let tool_req = json!({"jsonrpc": "2.0", "id": 12, "method": "tools/call", "params": { "name": "logs", "arguments": {} }});
        let (_, _, tool_body) = post_mcp(&client, addr, Some(&session_id), &tool_req).await;
        let tool_text = parse_mcp_payload(&tool_body)["result"]["content"][0]["text"].as_str().expect("tool text").to_owned();

        let read_req = json!({"jsonrpc": "2.0", "id": 13, "method": "resources/read", "params": { "uri": "radia://logs" }});
        let (status, _, read_body) = post_mcp(&client, addr, Some(&session_id), &read_req).await;
        assert_eq!(status, 200, "resources/read body: {read_body}");
        let res_text = parse_mcp_payload(&read_body)["result"]["contents"][0]["text"].as_str().expect("resource text").to_owned();
        assert_eq!(res_text, tool_text);

        let missing_req = json!({"jsonrpc": "2.0", "id": 14, "method": "resources/read", "params": { "uri": "radia://object/999999" }});
        let (status, _, missing_body) = post_mcp(&client, addr, Some(&session_id), &missing_req).await;
        assert_eq!(status, 200);
        assert!(parse_mcp_payload(&missing_body).get("error").is_some(), "expected MCP error for unknown object, got: {missing_body}");

        server.shutdown();
    }

    #[tokio::test]
    async fn prompts_list_get_and_missing_arg() {
        let (_pub, handle) = inspection_channel(RecentLogs::default());
        let server = McpServer::start(handle, McpServerConfig::default(), &Handle::current()).expect("start server");
        let addr = server.addr();
        let client = Client::new();
        let (session_id, _) = open_session(&client, addr).await;

        let list_req = json!({"jsonrpc": "2.0", "id": 20, "method": "prompts/list"});
        let (status, _, body) = post_mcp(&client, addr, Some(&session_id), &list_req).await;
        assert_eq!(status, 200, "prompts/list body: {body}");
        let names: Vec<String> = parse_mcp_payload(&body)["result"]["prompts"]
            .as_array()
            .expect("prompts array")
            .iter()
            .filter_map(|entry| entry["name"].as_str().map(str::to_owned))
            .collect();
        for expected in ["diagnose_scene", "diagnose_object", "diagnose_performance"] {
            assert!(names.iter().any(|name| name == expected), "missing prompt {expected} in {names:?}");
        }

        let missing_req = json!({"jsonrpc": "2.0", "id": 21, "method": "prompts/get", "params": { "name": "diagnose_object" }});
        let (status, _, missing_body) = post_mcp(&client, addr, Some(&session_id), &missing_req).await;
        assert_eq!(status, 200);
        assert!(parse_mcp_payload(&missing_body).get("error").is_some(), "expected MCP error for missing local_id, got: {missing_body}");

        let ok_req = json!({
            "jsonrpc": "2.0",
            "id": 22,
            "method": "prompts/get",
            "params": { "name": "diagnose_object", "arguments": { "local_id": "42" } }
        });
        let (status, _, ok_body) = post_mcp(&client, addr, Some(&session_id), &ok_req).await;
        assert_eq!(status, 200, "prompts/get body: {ok_body}");
        let messages = parse_mcp_payload(&ok_body)["result"]["messages"].as_array().expect("messages array").clone();
        assert!(!messages.is_empty());
        assert!(messages[0]["content"]["text"].as_str().expect("prompt text").contains("42"));

        server.shutdown();
    }

    #[test]
    fn tool_registry_titles_and_annotations() {
        let mut non_read_only = Vec::new();
        for tool in TOOLS {
            assert!(!tool.title.trim().is_empty(), "tool {} missing title", tool.name);
            if !tool.annotations.read_only {
                non_read_only.push(tool.name);
                assert_eq!(tool.annotations.destructive, Some(false), "tool {} must not be destructive", tool.name);
                assert_eq!(tool.annotations.idempotent, Some(true), "tool {} must be idempotent", tool.name);
                assert_eq!(tool.annotations.open_world, tool.name == "set_camera", "unexpected open_world for {}", tool.name);
            } else {
                assert!(!tool.annotations.open_world, "read tool {} must have open_world=false", tool.name);
            }
        }
        non_read_only.sort_unstable();
        assert_eq!(non_read_only, vec!["set_camera", "set_log_level", "set_setting"]);
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
