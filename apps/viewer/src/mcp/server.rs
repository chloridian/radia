use std::{
    error::Error,
    fmt, io,
    net::{Ipv4Addr, SocketAddr, TcpListener as StdTcpListener},
    sync::Arc,
    time::Duration,
};

use axum::Router;
use futures::Stream;
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::{
        router::{prompt::PromptRouter, tool::ToolRouter},
        tool::ToolCallContext,
    },
    model::{
        CallToolRequestParams, CallToolResponse, ClientJsonRpcMessage, CompleteRequestParams, CompleteResult, ContentBlock, ErrorCode,
        Implementation, ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams, ReadResourceRequestParams,
        ReadResourceResponse, ServerCapabilities, ServerConfig, ServerJsonRpcMessage,
    },
    prompt_handler,
    service::RequestContext,
    tool_handler,
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
use tokio::{
    net::TcpListener,
    runtime::Handle,
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, watch},
    task::JoinHandle,
    time,
};

use super::{
    MAX_CONCURRENT_REQUESTS, MAX_JSON_RESPONSE_BYTES, MAX_PNG_RESPONSE_BYTES, MAX_REQUEST_BYTES, MAX_SESSIONS, REQUEST_TIMEOUT,
    SERVER_INSTRUCTIONS, SESSION_IDLE_TIMEOUT, prompts, resources, tools,
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
    #[cfg(test)]
    semaphore: Arc<Semaphore>,
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

        let semaphore = Arc::new(Semaphore::new(config.max_concurrent_requests.max(1)));
        let server_handler = RadiaServer::new(handle, config, Arc::clone(&semaphore));
        let mut http_config = StreamableHttpServerConfig::default();
        http_config.legacy_session_mode = true;
        http_config.json_response = false;
        http_config.max_request_body_bytes = config.max_request_bytes;
        http_config = http_config
            .with_allowed_hosts([format!("127.0.0.1:{}", addr.port()), format!("localhost:{}", addr.port())])
            .with_allowed_origins(["http://127.0.0.1:*", "https://127.0.0.1:*", "http://localhost:*", "https://localhost:*"]);
        let cancellation_token = http_config.cancellation_token.clone();

        let session_manager = Arc::new(BoundedSessionManager::new(config.max_sessions, config.session_idle_timeout));
        let service = StreamableHttpService::new(move || Ok(server_handler.clone()), session_manager, http_config);
        let router = Router::new().nest_service("/mcp", service);

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
        Ok(Self {
            addr,
            shutdown,
            task,
            #[cfg(test)]
            semaphore,
        })
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
pub(super) struct RadiaServer {
    pub(super) handle: ViewerInspectionHandle,
    pub(super) config: McpServerConfig,
    semaphore: Arc<Semaphore>,
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
}

impl RadiaServer {
    fn new(handle: ViewerInspectionHandle, config: McpServerConfig, semaphore: Arc<Semaphore>) -> Self {
        Self { handle, config, semaphore, tool_router: tools::build_tool_router(), prompt_router: prompts::build_prompt_router() }
    }

    fn acquire_permit(&self) -> Result<OwnedSemaphorePermit, ErrorData> {
        Arc::clone(&self.semaphore)
            .try_acquire_owned()
            .map_err(|_| ErrorData::new(ErrorCode(-32000), "server busy: concurrency limit reached", None))
    }
}

pub(super) fn response_too_large_error() -> ErrorData {
    ErrorData::new(ErrorCode(-32002), "response exceeded size limit", None)
}

#[tool_handler(router = self.tool_router)]
#[prompt_handler(router = self.prompt_router)]
impl ServerHandler for RadiaServer {
    fn get_info(&self) -> ServerConfig {
        let capabilities = ServerCapabilities::builder().enable_tools().enable_resources().enable_prompts().enable_completions().build();
        ServerConfig::new(capabilities)
            .with_server_info(Implementation::new("radia", env!("CARGO_PKG_VERSION")))
            .with_instructions(SERVER_INSTRUCTIONS)
    }

    async fn call_tool(&self, request: CallToolRequestParams, context: RequestContext<RoleServer>) -> Result<CallToolResponse, ErrorData> {
        let _permit = self.acquire_permit()?;
        let tool_name = request.name.clone();
        let ct = context.ct.clone();
        let tcc = ToolCallContext::new(self, request, context);
        let outcome = tokio::select! {
            biased;
            () = ct.cancelled() => {
                return Err(ErrorData::new(ErrorCode(-32000), "request cancelled", None));
            }
            outcome = time::timeout(self.config.request_timeout, self.tool_router.call(tcc)) => outcome,
        };
        let mut response = match outcome {
            Ok(result) => result?,
            Err(_) => return Err(ErrorData::new(ErrorCode(-32001), "tool call timed out", None)),
        };
        if let CallToolResponse::Complete(ref mut result) = response {
            if result.is_error != Some(true)
                && let Some(link) = resources::resource_for_tool(tool_name.as_ref())
            {
                result.content.push(ContentBlock::resource_link(link));
            }
            let has_image = result.content.iter().any(|block| matches!(block, ContentBlock::Image(_)));
            let max_bytes = if has_image { MAX_PNG_RESPONSE_BYTES } else { self.config.max_response_bytes };
            let encoded_len = serde_json::to_vec(result).map(|bytes| bytes.len()).unwrap_or(usize::MAX);
            if encoded_len > max_bytes {
                return Err(response_too_large_error());
            }
        }
        Ok(response)
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
        let _permit = self.acquire_permit()?;
        let ct = context.ct.clone();
        let outcome = tokio::select! {
            biased;
            () = ct.cancelled() => {
                return Err(ErrorData::new(ErrorCode(-32000), "request cancelled", None));
            }
            outcome = time::timeout(self.config.request_timeout, resources::read_resource(self, &request.uri, context)) => outcome,
        };
        let result = match outcome {
            Ok(result) => result?,
            Err(_) => return Err(ErrorData::new(ErrorCode(-32001), "resource read timed out", None)),
        };
        Ok(ReadResourceResponse::Complete(result))
    }

    async fn complete(&self, request: CompleteRequestParams, _context: RequestContext<RoleServer>) -> Result<CompleteResult, ErrorData> {
        Ok(prompts::complete(&request, &self.handle).await)
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
    create_lock: Mutex<()>,
}

impl BoundedSessionManager {
    fn new(max_sessions: usize, idle_timeout: Duration) -> Self {
        let mut inner = LocalSessionManager::default();
        inner.session_config.keep_alive = Some(idle_timeout);
        Self { inner, max_sessions: max_sessions.max(1), create_lock: Mutex::new(()) }
    }
}

impl SessionManager for BoundedSessionManager {
    type Error = BoundedSessionError;
    type Transport = <LocalSessionManager as SessionManager>::Transport;

    async fn create_session(&self) -> Result<(SessionId, Self::Transport), Self::Error> {
        let _guard = self.create_lock.lock().await;
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

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        future,
        net::SocketAddr,
        sync::{Arc, atomic::AtomicU64},
    };

    use futures::FutureExt;
    use network::{Grid, LoginCredentials, Session, StartLocation};
    use reqwest::{
        Client,
        header::{ACCEPT, CONTENT_TYPE, HOST, ORIGIN},
    };
    use serde_json::{Value, json};
    use tokio::{
        runtime::Handle,
        sync::{mpsc, watch},
        task,
    };
    use uuid::Uuid;
    use world::{World, WorldLimits};

    use super::{McpServer, McpServerConfig};
    use crate::{
        asset_coordinator::AssetCoordinator,
        inspection::{
            InventoryFetchStats, ObjectCacheStats, RenderStatsSnapshot, SessionDiagnostics, SessionInspection, SessionQueuesSnapshot,
            VIEWPORT_QUERY_CAPACITY, inspection_channel,
        },
        inventory::{InventoryEditController, InventoryFetchController},
        logging::{LogLevel, RecentLogEntry, RecentLogs},
        world_reader::WorldReadHost,
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
        assert!(init_parsed["result"]["instructions"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(init_parsed["result"]["capabilities"]["tools"].is_object());
        assert!(init_parsed["result"]["capabilities"]["resources"].is_object());
        assert!(init_parsed["result"]["capabilities"]["prompts"].is_object());
        assert!(init_parsed["result"]["capabilities"]["completions"].is_object());

        let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let (status, _, body) = post_mcp(&client, addr, Some(&session_id), &list).await;
        assert_eq!(status, 200, "list body: {body}");
        let parsed = parse_mcp_payload(&body);
        let tools = parsed["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 21);

        let mut non_read_only = Vec::new();
        for tool in tools {
            let name = tool["name"].as_str().expect("tool name");
            assert!(tool["title"].as_str().is_some_and(|t| !t.trim().is_empty()), "tool {name} missing title");
            assert!(tool["inputSchema"].is_object(), "tool {name} missing inputSchema");
            if name != "screenshot" {
                assert!(tool["outputSchema"].is_object(), "tool {name} missing outputSchema");
            }
            let ann = tool["annotations"].as_object().unwrap_or_else(|| panic!("tool {name} missing annotations"));
            let read_only = ann.get("readOnlyHint").and_then(Value::as_bool).unwrap_or(false);
            if !read_only {
                non_read_only.push(name.to_owned());
                assert_eq!(ann.get("destructiveHint").and_then(Value::as_bool), Some(false));
                assert_eq!(ann.get("idempotentHint").and_then(Value::as_bool), Some(true));
                assert_eq!(ann.get("openWorldHint").and_then(Value::as_bool), Some(name == "set_camera"));
            } else {
                assert_eq!(ann.get("openWorldHint").and_then(Value::as_bool), Some(false));
            }
        }
        non_read_only.sort_unstable();
        assert_eq!(non_read_only, vec!["set_camera", "set_log_level", "set_setting"]);

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
        assert!(parsed["result"]["structuredContent"].is_object());
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

        let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "logs", "arguments": {} }});
        let (status, _, body) = post_mcp(&client, addr, Some(&session_id), &call).await;
        assert_eq!(status, 200);
        let parsed = parse_mcp_payload(&body);
        assert_eq!(parsed["error"]["code"], -32002);

        server.shutdown();
    }

    #[tokio::test]
    async fn session_cap_rejects_excess_and_concurrent_sessions() {
        let (_pub, handle) = inspection_channel(RecentLogs::default());
        let config = McpServerConfig { max_sessions: 2, ..McpServerConfig::default() };
        let server = McpServer::start(handle, config, &Handle::current()).expect("start server");
        let addr = server.addr();
        let client = Client::new();

        let mut tasks = Vec::new();
        for _ in 0..5 {
            let client = client.clone();
            tasks.push(tokio::spawn(async move { post_mcp(&client, addr, None, &init_payload()).await.0 }));
        }
        let mut ok_count = 0usize;
        for task in tasks {
            if task.await.expect("init task finished") == 200 {
                ok_count += 1;
            }
        }
        assert_eq!(ok_count, 2, "concurrent initializes must not exceed max_sessions");

        let (status, _, _) = post_mcp(&client, addr, None, &init_payload()).await;
        assert_ne!(status, 200, "extra session beyond cap must be rejected");

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
        for expected in ["viewer://world", "viewer://stats", "viewer://region", "viewer://settings", "viewer://logs", "viewer://screenshot"]
        {
            assert!(uris.contains(&expected), "missing resource {expected} in {uris:?}");
        }

        let tpl_req = json!({"jsonrpc": "2.0", "id": 11, "method": "resources/templates/list"});
        let (status, _, body) = post_mcp(&client, addr, Some(&session_id), &tpl_req).await;
        assert_eq!(status, 200, "resources/templates/list body: {body}");
        let parsed = parse_mcp_payload(&body);
        let templates = parsed["result"]["resourceTemplates"].as_array().expect("templates array");
        assert!(templates.iter().any(|entry| entry["uriTemplate"] == "viewer://object/{object_id}"));

        let tool_req = json!({"jsonrpc": "2.0", "id": 12, "method": "tools/call", "params": { "name": "logs", "arguments": {} }});
        let (_, _, tool_body) = post_mcp(&client, addr, Some(&session_id), &tool_req).await;
        let tool_text = parse_mcp_payload(&tool_body)["result"]["content"][0]["text"].as_str().expect("tool text").to_owned();

        let read_req = json!({"jsonrpc": "2.0", "id": 13, "method": "resources/read", "params": { "uri": "viewer://logs" }});
        let (status, _, read_body) = post_mcp(&client, addr, Some(&session_id), &read_req).await;
        assert_eq!(status, 200, "resources/read body: {read_body}");
        let res_text = parse_mcp_payload(&read_body)["result"]["contents"][0]["text"].as_str().expect("resource text").to_owned();
        assert_eq!(res_text, tool_text);

        let unknown_uuid = Uuid::from_u128(0x1234_5678);
        let missing_req =
            json!({"jsonrpc": "2.0", "id": 14, "method": "resources/read", "params": { "uri": format!("viewer://object/{unknown_uuid}") }});
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
        assert!(parse_mcp_payload(&missing_body).get("error").is_some(), "expected MCP error for missing object_id, got: {missing_body}");

        let target_uuid = Uuid::from_u128(42).to_string();
        let ok_req = json!({
            "jsonrpc": "2.0",
            "id": 22,
            "method": "prompts/get",
            "params": { "name": "diagnose_object", "arguments": { "object_id": target_uuid } }
        });
        let (status, _, ok_body) = post_mcp(&client, addr, Some(&session_id), &ok_req).await;
        assert_eq!(status, 200, "prompts/get body: {ok_body}");
        let messages = parse_mcp_payload(&ok_body)["result"]["messages"].as_array().expect("messages array").clone();
        assert!(!messages.is_empty());
        assert!(messages[0]["content"]["text"].as_str().expect("prompt text").contains(&target_uuid));

        server.shutdown();
    }

    #[tokio::test]
    async fn concurrency_is_bounded() {
        let (publisher, handle) = inspection_channel(RecentLogs::default());
        let runtime = Handle::current();
        let world = World::new(WorldLimits {
            max_regions: 2,
            max_entities: 8,
            max_entities_per_region: 8,
            max_tombstones_per_region: 8,
            max_tombstones: 8,
            max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(8),
            max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(8),
            max_journal_entries: 8,
            max_journal_changes: 32,
            max_region_name_bytes: 64,
            max_recovery_targets: 4,
        });
        let reads = WorldReadHost::new(&world, runtime.clone());
        let cancellation = future::pending::<()>().boxed().shared();
        let assets = AssetCoordinator::new(runtime.clone(), cancellation, None);
        let inventory_edits = InventoryEditController::new();
        let inventory_fetch = InventoryFetchController::new(inventory_edits.handle());
        let network_session = Session::start(LoginCredentials {
            first_name: "Test".to_owned(),
            last_name: "User".to_owned(),
            password: String::new(),
            grid: Grid::Agni,
            start_location: StartLocation::Last,
            mfa_hash: String::new(),
        })
        .expect("start test network session");
        let network_handle = network_session.handle();
        let (_render_tx, render_stats) = watch::channel(RenderStatsSnapshot::default());
        let (viewport_queries, _viewport_rx) = mpsc::channel(VIEWPORT_QUERY_CAPACITY);
        let (_motion_tx, agent_motion) = watch::channel(None);
        let (_oc_tx, object_cache) = watch::channel(ObjectCacheStats::default());
        let (_inv_tx, inventory) = watch::channel(InventoryFetchStats::default());
        let (_net_tx, network) = watch::channel(Vec::new());
        let (_sim_tx, sim_stats) = watch::channel(HashMap::new());
        let (_q_tx, session_queues) = watch::channel(SessionQueuesSnapshot::default());
        publisher.set_session(Some(SessionInspection {
            world: reads.reader(),
            assets: assets.handle(),
            render_stats,
            viewport_queries,
            viewport_query_full: Arc::new(AtomicU64::new(0)),
            agent_motion,
            diagnostics: SessionDiagnostics {
                object_cache,
                inventory,
                network,
                sim_stats,
                session_queues,
                network_session: network_handle,
                inventory_fetch: inventory_fetch.handle(),
            },
        }));

        let config = McpServerConfig { max_concurrent_requests: 1, ..McpServerConfig::default() };
        let server = McpServer::start(handle, config, &runtime).expect("start server");
        let addr = server.addr();
        let client = Client::new();
        let (session_id, _) = open_session(&client, addr).await;

        let wait_call = json!({
            "jsonrpc": "2.0",
            "id": 100,
            "method": "tools/call",
            "params": { "name": "wait_for", "arguments": { "condition": "world_revision", "revision": 999_999, "timeout_ms": 10_000 } }
        });
        let first = tokio::spawn({
            let client = client.clone();
            let session_id = session_id.clone();
            async move { post_mcp(&client, addr, Some(&session_id), &wait_call).await }
        });

        while server.semaphore.available_permits() > 0 {
            task::yield_now().await;
        }

        let second_call = json!({
            "jsonrpc": "2.0",
            "id": 101,
            "method": "tools/call",
            "params": { "name": "logs", "arguments": {} }
        });
        let (second_status, _, second_body) = post_mcp(&client, addr, Some(&session_id), &second_call).await;
        assert_eq!(second_status, 200);
        let second_parsed = parse_mcp_payload(&second_body);
        assert!(
            second_parsed["error"]["message"].as_str().is_some_and(|msg| msg.contains("server busy")),
            "expected server busy error, got: {second_body}"
        );

        let cancel_notif = json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": { "requestId": 100 }
        });
        let _ = post_mcp(&client, addr, Some(&session_id), &cancel_notif).await;
        let _ = first.await.expect("first task finished");

        let third_call = json!({
            "jsonrpc": "2.0",
            "id": 102,
            "method": "tools/call",
            "params": { "name": "logs", "arguments": {} }
        });
        let (third_status, _, third_body) = post_mcp(&client, addr, Some(&session_id), &third_call).await;
        assert_eq!(third_status, 200);
        let third_parsed = parse_mcp_payload(&third_body);
        assert_eq!(third_parsed["result"]["isError"], false, "expected call to succeed after permit release: {third_body}");

        server.shutdown();
    }
}
