use std::{io, str, time::Duration};

use llsd::{Value, xml};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task::{JoinHandle, JoinSet},
    time::timeout,
};
use url::Url;

pub(super) const TEST_TIMEOUT: Duration = Duration::from_secs(45);

const MAX_ACTIVE_REQUESTS: usize = 16;
const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;
const MAX_REQUEST_HEADER_BYTES: usize = 16 * 1024;

pub(super) struct PeerRequest {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) headers: String,
    pub(super) body: Vec<u8>,
    stream: TcpStream,
}

impl PeerRequest {
    pub(super) async fn respond(self, status: u16, body: impl AsRef<[u8]>) {
        self.try_respond(status, body).await.expect("write inventory peer response");
    }

    pub(super) async fn try_respond(self, status: u16, body: impl AsRef<[u8]>) -> io::Result<()> {
        self.try_respond_with_headers(status, body, &[]).await
    }

    pub(super) async fn respond_with_headers(self, status: u16, body: impl AsRef<[u8]>, headers: &[(&str, &str)]) {
        self.try_respond_with_headers(status, body, headers).await.expect("write inventory peer response");
    }

    pub(super) fn value(&self) -> Value {
        let body = str::from_utf8(&self.body).expect("request LLSD is UTF-8");
        xml::from_str(body).expect("request body is LLSD XML")
    }

    pub(super) async fn wait_for_close(mut self) {
        let mut byte = [0_u8; 1];
        let result = timeout(TEST_TIMEOUT, self.stream.read(&mut byte)).await.expect("peer close timeout");
        match result {
            Ok(0) => {}
            Err(error) if matches!(error.kind(), io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe) => {}
            result => panic!("expected client to close the in-flight request, received {result:?}"),
        }
    }

    async fn try_respond_with_headers(self, status: u16, body: impl AsRef<[u8]>, headers: &[(&str, &str)]) -> io::Result<()> {
        let body = body.as_ref();
        let reason = match status {
            200 => "OK",
            403 => "Forbidden",
            404 => "Not Found",
            410 => "Gone",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "Response",
        };
        let mut response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/llsd+xml\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (name, value) in headers {
            response.push_str(name);
            response.push_str(": ");
            response.push_str(value);
            response.push_str("\r\n");
        }
        response.push_str("\r\n");
        let mut stream = self.stream;
        stream.write_all(response.as_bytes()).await?;
        stream.write_all(body).await?;
        Ok(())
    }
}

pub(super) struct LoopbackPeer {
    base: Url,
    pub(super) requests: mpsc::Receiver<PeerRequest>,
    task: JoinHandle<()>,
}

impl LoopbackPeer {
    pub(super) async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind inventory test peer");
        let address = listener.local_addr().expect("read inventory peer address");
        let base = Url::parse(&format!("http://{address}")).expect("build inventory peer URL");
        let (sender, requests) = mpsc::channel(MAX_ACTIVE_REQUESTS);
        let task = tokio::spawn(async move {
            let mut accepted = JoinSet::new();
            loop {
                tokio::select! {
                    result = listener.accept(), if accepted.len() < MAX_ACTIVE_REQUESTS => {
                        let Ok((stream, _)) = result else {
                            break;
                        };
                        let sender = sender.clone();
                        accepted.spawn(async move {
                            if let Ok(Ok(request)) = timeout(TEST_TIMEOUT, read_request(stream)).await
                                && let Err(error) = sender.send(request).await
                            {
                                drop(error.0);
                            }
                        });
                    }
                    Some(_) = accepted.join_next(), if !accepted.is_empty() => {}
                }
            }
        });
        Self { base, requests, task }
    }

    pub(super) fn url(&self, path: &str) -> Url {
        self.base.join(path).expect("join inventory peer path")
    }

    pub(super) async fn next(&mut self) -> PeerRequest {
        timeout(TEST_TIMEOUT, self.requests.recv()).await.expect("inventory peer request timeout").expect("inventory peer listener closed")
    }

    pub(super) async fn grant(&mut self, names: &[(&str, &str)]) {
        let request = self.next().await;
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/seed");
        let value = Value::Map(names.iter().map(|(name, path)| ((*name).to_owned(), Value::String(self.url(path).to_string()))).collect());
        let body = xml::to_string(&value).expect("encode capability grants");
        request.respond(200, body).await;
    }
}

impl Drop for LoopbackPeer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(mut stream: TcpStream) -> io::Result<PeerRequest> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    let (header_end, content_length) = loop {
        if bytes.len() == MAX_REQUEST_HEADER_BYTES {
            return Err(io::Error::other("inventory peer request headers exceeded their bound"));
        }
        let limit = (MAX_REQUEST_HEADER_BYTES - bytes.len()).min(chunk.len());
        let read = stream.read(&mut chunk[..limit]).await?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        bytes.extend_from_slice(&chunk[..read]);
        let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let header_end = end + 4;
        let headers = str::from_utf8(&bytes[..end]).map_err(io::Error::other)?;
        let content_length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.trim().parse::<usize>())
            .transpose()
            .map_err(io::Error::other)?
            .unwrap_or_default();
        if content_length > MAX_REQUEST_BODY_BYTES {
            return Err(io::Error::other("inventory peer request body exceeded its bound"));
        }
        break (header_end, content_length);
    };
    let request_end = header_end.checked_add(content_length).ok_or_else(|| io::Error::other("inventory peer request size overflow"))?;
    while bytes.len() < request_end {
        let limit = (request_end - bytes.len()).min(chunk.len());
        let read = stream.read(&mut chunk[..limit]).await?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    let headers = str::from_utf8(&bytes[..header_end - 4]).map_err(io::Error::other)?.to_owned();
    let mut line = headers.lines().next().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?.split_whitespace();
    let method = line.next().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?.to_owned();
    let path = line.next().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?.to_owned();
    Ok(PeerRequest { method, path, headers, body: bytes[header_end..request_end].to_vec(), stream })
}
