use std::{
    io,
    net::{Ipv4Addr, SocketAddr, TcpListener as StdTcpListener},
    str,
    sync::Arc,
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
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

const HEADER_END: &[u8] = b"\r\n\r\n";
const MAX_HEADER_BYTES: usize = 8 * 1024;
const PROTOCOL_VERSION: &str = "2025-03-26";

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
        let (shutdown, shutdown_rx) = watch::channel(false);
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent_requests.max(1)));
        let task = runtime.spawn(accept_loop(listener, handle, config, semaphore, shutdown_rx));
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

async fn accept_loop(
    listener: TcpListener,
    handle: ViewerInspectionHandle,
    config: McpServerConfig,
    semaphore: Arc<Semaphore>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _peer)) => {
                        let handle = handle.clone();
                        let semaphore = Arc::clone(&semaphore);
                        tokio::spawn(async move {
                            if let Err(error) = handle_connection(stream, handle, config, semaphore).await {
                                tracing::debug!(%error, "MCP client connection closed");
                            }
                        });
                    }
                    Err(error) => {
                        tracing::warn!(%error, "MCP accept failed");
                    }
                }
            }
        }
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    handle: ViewerInspectionHandle,
    config: McpServerConfig,
    semaphore: Arc<Semaphore>,
) -> io::Result<()> {
    let request = match read_http_request(&mut stream, config.max_request_bytes).await? {
        HttpRead::Request(req) => req,
        HttpRead::Closed => return Ok(()),
        HttpRead::PayloadTooLarge => {
            write_http_response(&mut stream, 413, "Payload Too Large", b"{\"error\":\"request too large\"}").await?;
            return Ok(());
        }
        HttpRead::BadRequest(msg) => {
            let body = json!({ "error": msg }).to_string();
            write_http_response(&mut stream, 400, "Bad Request", body.as_bytes()).await?;
            return Ok(());
        }
    };

    if request.method != "POST" {
        write_http_response(&mut stream, 405, "Method Not Allowed", b"{\"error\":\"method not allowed\"}").await?;
        return Ok(());
    }

    let Ok(permit) = semaphore.try_acquire_owned() else {
        let body = json_rpc_error(Value::Null, -32000, "server busy: concurrency limit reached").to_string();
        write_http_response(&mut stream, 429, "Too Many Requests", body.as_bytes()).await?;
        return Ok(());
    };

    let response = dispatch_json_rpc(&request.body, &handle, config).await;
    drop(permit);

    match response {
        DispatchOutcome::Accepted => write_http_response(&mut stream, 202, "Accepted", b"").await?,
        DispatchOutcome::Json(bytes) => write_http_response(&mut stream, 200, "OK", &bytes).await?,
    }
    Ok(())
}

struct HttpRequest {
    method: String,
    body: Vec<u8>,
}

enum HttpRead {
    Request(HttpRequest),
    Closed,
    PayloadTooLarge,
    BadRequest(&'static str),
}

async fn read_http_request(stream: &mut TcpStream, max_body: usize) -> io::Result<HttpRead> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        if let Some(pos) = find_subsequence(&buf, HEADER_END) {
            break pos;
        }
        if buf.len() >= MAX_HEADER_BYTES {
            return Ok(HttpRead::BadRequest("headers too large"));
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(if buf.is_empty() { HttpRead::Closed } else { HttpRead::BadRequest("incomplete headers") });
        }
        buf.extend_from_slice(&chunk[..read]);
    };

    let header_text = match str::from_utf8(&buf[..header_end]) {
        Ok(text) => text,
        Err(_) => return Ok(HttpRead::BadRequest("invalid header utf-8")),
    };
    let mut lines = header_text.split("\r\n");
    let Some(request_line) = lines.next() else {
        return Ok(HttpRead::BadRequest("missing request line"));
    };
    let Some(method) = request_line.split_whitespace().next() else {
        return Ok(HttpRead::BadRequest("missing method"));
    };

    let mut content_length: usize = 0;
    for line in lines {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            let Ok(len) = value.trim().parse::<usize>() else {
                return Ok(HttpRead::BadRequest("invalid content-length"));
            };
            content_length = len;
        }
    }

    if content_length > max_body {
        return Ok(HttpRead::PayloadTooLarge);
    }

    let body_start = header_end + HEADER_END.len();
    let mut body = Vec::with_capacity(content_length);
    let already = buf.len().saturating_sub(body_start);
    if already > 0 {
        let take = already.min(content_length);
        body.extend_from_slice(&buf[body_start..body_start + take]);
    }
    while body.len() < content_length {
        let remaining = content_length - body.len();
        let to_read = remaining.min(chunk.len());
        let read = stream.read(&mut chunk[..to_read]).await?;
        if read == 0 {
            return Ok(HttpRead::BadRequest("unexpected eof in body"));
        }
        body.extend_from_slice(&chunk[..read]);
    }

    Ok(HttpRead::Request(HttpRequest { method: method.to_owned(), body }))
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

async fn write_http_response(stream: &mut TcpStream, status: u16, reason: &str, body: &[u8]) -> io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    if !body.is_empty() {
        stream.write_all(body).await?;
    }
    stream.flush().await
}

enum DispatchOutcome {
    Accepted,
    Json(Vec<u8>),
}

async fn dispatch_json_rpc(body: &[u8], handle: &ViewerInspectionHandle, config: McpServerConfig) -> DispatchOutcome {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => return DispatchOutcome::Json(json_rpc_error(Value::Null, -32700, "parse error").to_string().into_bytes()),
    };

    let id = parsed.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = parsed.get("method").and_then(Value::as_str) else {
        return DispatchOutcome::Json(json_rpc_error(id, -32600, "invalid request: missing method").to_string().into_bytes());
    };
    let params = parsed.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            let result = json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "radia-viewer",
                    "version": env!("CARGO_PKG_VERSION")
                }
            });
            DispatchOutcome::Json(json_rpc_ok(id, result).to_string().into_bytes())
        }
        "notifications/initialized" => DispatchOutcome::Accepted,
        "tools/list" => {
            let tools: Vec<Value> = TOOLS
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "inputSchema": (tool.input_schema)()
                    })
                })
                .collect();
            DispatchOutcome::Json(json_rpc_ok(id, json!({ "tools": tools })).to_string().into_bytes())
        }
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return DispatchOutcome::Json(json_rpc_error(id, -32602, "missing tool name").to_string().into_bytes());
            };
            let Some(tool) = find_tool(name) else {
                return DispatchOutcome::Json(json_rpc_error(id, -32602, "unknown tool").to_string().into_bytes());
            };
            let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let outcome = time::timeout(config.request_timeout, (tool.handler)(handle.clone(), arguments)).await;
            let (result_value, max_bytes) = match outcome {
                Ok(Ok(ToolContent::Json(value))) => {
                    let text = value.to_string();
                    let limit = tool.max_response_bytes.min(config.max_response_bytes);
                    (
                        json!({
                            "content": [{ "type": "text", "text": text }],
                            "isError": false
                        }),
                        limit,
                    )
                }
                Ok(Ok(ToolContent::ImagePng { data_base64, width, height })) => (
                    json!({
                        "content": [
                            { "type": "image", "data": data_base64, "mimeType": "image/png" },
                            { "type": "text", "text": json!({ "width": width, "height": height }).to_string() }
                        ],
                        "isError": false
                    }),
                    tool.max_response_bytes,
                ),
                Ok(Err(message)) => (
                    json!({
                        "content": [{ "type": "text", "text": message }],
                        "isError": true
                    }),
                    config.max_response_bytes,
                ),
                Err(_) => {
                    return DispatchOutcome::Json(json_rpc_error(id, -32001, "tool call timed out").to_string().into_bytes());
                }
            };
            let encoded = json_rpc_ok(id.clone(), result_value).to_string().into_bytes();
            if encoded.len() > max_bytes {
                return DispatchOutcome::Json(json_rpc_error(id, -32002, "response exceeded size limit").to_string().into_bytes());
            }
            DispatchOutcome::Json(encoded)
        }
        _ => DispatchOutcome::Json(json_rpc_error(id, -32601, "method not found").to_string().into_bytes()),
    }
}

fn json_rpc_ok(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn json_rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use serde_json::{Value, json};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
        runtime::Handle,
    };

    use super::{McpServer, McpServerConfig};
    use crate::{
        inspection::inspection_channel,
        logging::{LogLevel, RecentLogEntry, RecentLogs},
    };

    async fn post_json(addr: SocketAddr, body: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(addr).await.expect("connect to local MCP server");
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(req.as_bytes()).await.expect("write request");
        stream.flush().await.expect("flush request");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await.expect("read response");
        let text = String::from_utf8(raw).expect("valid utf-8 response");
        let (head, resp_body) = text.split_once("\r\n\r\n").expect("valid http response");
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .expect("http status code");
        (status, resp_body.to_owned())
    }

    #[tokio::test]
    async fn mcp_handshake_and_tools_list() {
        let (_pub, handle) = inspection_channel(RecentLogs::default());
        let server = McpServer::start(handle, McpServerConfig::default(), &Handle::current()).expect("start server");
        let addr = server.addr();

        let (status, body) = post_json(addr, &json!({"jsonrpc":"2.0","id":1,"method":"initialize"}).to_string()).await;
        assert_eq!(status, 200);
        let parsed: Value = serde_json::from_str(&body).expect("json response");
        assert_eq!(parsed["result"]["serverInfo"]["name"], "radia-viewer");

        let (status, body) = post_json(addr, &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}).to_string()).await;
        assert_eq!(status, 200);
        let parsed: Value = serde_json::from_str(&body).expect("json response");
        let tools = parsed["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 7);

        let (status, _) = post_json(addr, &json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string()).await;
        assert_eq!(status, 202);

        server.shutdown();
    }

    #[tokio::test]
    async fn mcp_logs_and_disconnect_resilience() {
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

        let abrupt = TcpStream::connect(addr).await.expect("connect abrupt client");
        drop(abrupt);

        let call = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "viewer_logs", "arguments": { "limit": 10 } }
        });
        let (status, body) = post_json(addr, &call.to_string()).await;
        assert_eq!(status, 200);
        let parsed: Value = serde_json::from_str(&body).expect("json response");
        assert_eq!(parsed["result"]["isError"], false);
        let text = parsed["result"]["content"][0]["text"].as_str().expect("text content");
        assert!(text.contains("sample warning"));

        server.shutdown();
    }

    #[tokio::test]
    async fn mcp_rejects_oversized_request_and_response() {
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

        let big_body = "a".repeat(128);
        let (status, _) = post_json(addr, &big_body).await;
        assert_eq!(status, 413);

        let call = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"viewer_logs"}});
        let (status, body) = post_json(addr, &call.to_string()).await;
        assert_eq!(status, 200);
        let parsed: Value = serde_json::from_str(&body).expect("json response");
        assert_eq!(parsed["error"]["code"], -32002);

        server.shutdown();
    }
}
