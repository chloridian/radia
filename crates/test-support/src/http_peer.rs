use std::{
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use url::Url;

const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
const MAX_PARALLEL_RESPONSES: usize = 48;
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_READ_DEADLINE: Duration = Duration::from_secs(30);
const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct LoopbackPeer {
    url: Url,
    thread: Option<thread::JoinHandle<Vec<Vec<u8>>>>,
    stop_address: Option<SocketAddr>,
    response_bytes: Option<Arc<AtomicUsize>>,
}

impl LoopbackPeer {
    pub fn new(count: usize, mut respond: impl FnMut(usize, &[u8], &Url) -> (u16, Vec<u8>) + Send + 'static) -> Self {
        Self::new_with_headers(count, move |index, request, base| {
            let (status, body) = respond(index, request, base);
            (status, Vec::new(), body)
        })
    }

    pub fn new_with_headers(
        count: usize,
        mut respond: impl FnMut(usize, &[u8], &Url) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        listener.set_nonblocking(true).expect("set loopback listener nonblocking");
        let url = Url::parse(&format!("http://{}/", listener.local_addr().expect("read loopback address"))).expect("build loopback URL");
        let base = url.clone();
        let thread = thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..count {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                            thread::sleep(Duration::from_millis(1))
                        }
                        Err(error) => panic!("expected loopback request: {error}"),
                    }
                };
                let request = read_request(&mut stream).expect("read loopback request");
                let (status, headers, body) = respond(index, &request, &base);
                write_response(&mut stream, status, headers, &body);
                requests.push(request);
            }
            requests
        });
        Self { url, thread: Some(thread), stop_address: None, response_bytes: None }
    }

    pub fn new_parallel(
        delay: Duration,
        respond: impl Fn(&[u8], &Url) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let address = listener.local_addr().expect("read loopback address");
        let url = Url::parse(&format!("http://{address}/")).expect("build loopback URL");
        let base = url.clone();
        let requests = Arc::new(Mutex::new(Vec::<(usize, Vec<u8>)>::new()));
        let peer_requests = Arc::clone(&requests);
        let response_bytes = Arc::new(AtomicUsize::new(0));
        let peer_response_bytes = Arc::clone(&response_bytes);
        let respond = Arc::new(respond);
        let thread = thread::spawn(move || {
            let mut workers = Vec::with_capacity(MAX_PARALLEL_RESPONSES);
            let mut index = 0;
            loop {
                reap_finished_workers(&mut workers);
                if workers.len() == MAX_PARALLEL_RESPONSES {
                    workers.remove(0).join().expect("loopback response thread completes");
                }
                let (mut stream, _) = listener.accept().expect("accept loopback request");
                let request = match read_request_inner(&mut stream) {
                    Ok(Some(request)) => request,
                    Ok(None) | Err(_) => continue,
                };
                if String::from_utf8_lossy(&request).to_ascii_lowercase().starts_with("get /__stop ") {
                    break;
                }
                peer_requests.lock().expect("lock loopback request list").push((index, request.clone()));
                index += 1;
                let (status, headers, body) = respond(&request, &base);
                let peer_response_bytes = Arc::clone(&peer_response_bytes);
                workers.push(thread::spawn(move || {
                    thread::sleep(delay);
                    if write_response(&mut stream, status, headers, &body) {
                        peer_response_bytes.fetch_add(body.len(), Ordering::Relaxed);
                    }
                }));
            }
            for worker in workers {
                worker.join().expect("loopback response thread completes");
            }
            let mut requests = requests.lock().expect("lock loopback request list");
            requests.sort_unstable_by_key(|(index, _)| *index);
            requests.iter().map(|(_, request)| request.clone()).collect()
        });
        Self { url, thread: Some(thread), stop_address: Some(address), response_bytes: Some(response_bytes) }
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn finish(self) -> Vec<Vec<u8>> {
        self.finish_stats().0
    }

    pub fn finish_stats(mut self) -> (Vec<Vec<u8>>, u64) {
        if self.stop_address.is_some() {
            self.signal_stop().expect("stop loopback peer");
        }
        let requests = self.thread.take().expect("loopback peer thread exists").join().expect("loopback peer completes");
        let response_bytes = self.response_bytes.as_ref().map_or(0, |bytes| bytes.load(Ordering::Relaxed));
        (requests, u64::try_from(response_bytes).expect("loopback byte count fits u64"))
    }

    fn signal_stop(&self) -> io::Result<()> {
        if let Some(address) = self.stop_address {
            let deadline = Instant::now() + RESPONSE_WRITE_TIMEOUT;
            let mut stream = TcpStream::connect_timeout(&address, RESPONSE_WRITE_TIMEOUT)?;
            write_until(&mut stream, b"GET /__stop HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n", deadline)?;
        }
        Ok(())
    }
}

impl Drop for LoopbackPeer {
    fn drop(&mut self) {
        if self.stop_address.is_none() || self.thread.is_none() {
            return;
        }
        let thread_finished = self.thread.as_ref().is_some_and(|thread| thread.is_finished());
        if !thread_finished && self.signal_stop().is_err() {
            return;
        }
        if let Some(thread) = self.thread.take() {
            drop(thread.join());
        }
    }
}

fn reap_finished_workers(workers: &mut Vec<thread::JoinHandle<()>>) {
    let mut index = 0;
    while index < workers.len() {
        if workers[index].is_finished() {
            workers.swap_remove(index).join().expect("loopback response thread completes");
        } else {
            index += 1;
        }
    }
}

fn read_request(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    read_request_inner(stream)?.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "request ended before framing completed"))
}

fn read_request_inner(stream: &mut TcpStream) -> io::Result<Option<Vec<u8>>> {
    stream.set_nonblocking(false)?;
    let deadline = Instant::now() + REQUEST_READ_DEADLINE;
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    let mut header_scan_from = 0;
    let mut request_end = None;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "request read deadline elapsed"));
        }
        stream.set_read_timeout(Some(REQUEST_READ_TIMEOUT.min(remaining)))?;
        let count = stream.read(&mut chunk)?;
        if count == 0 {
            if bytes.is_empty() {
                return Ok(None);
            }
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "request ended before framing completed"));
        }
        let next_len =
            bytes.len().checked_add(count).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "request length overflow"))?;
        if next_len > MAX_REQUEST_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "request exceeds byte limit"));
        }
        bytes.extend_from_slice(&chunk[..count]);
        if request_end.is_none() {
            if let Some(header_offset) = bytes[header_scan_from..].windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                let header = header_scan_from
                    .checked_add(header_offset)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "request framing overflow"))?;
                let body_start =
                    header.checked_add(4).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "request framing overflow"))?;
                let body_length = request_body_length(&bytes[..header])?;
                let expected_end = body_start
                    .checked_add(body_length)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "request framing overflow"))?;
                if expected_end > MAX_REQUEST_BYTES {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "request exceeds byte limit"));
                }
                request_end = Some(expected_end);
            } else {
                header_scan_from = bytes.len().saturating_sub(3);
            }
        }
        if let Some(request_end) = request_end
            && bytes.len() >= request_end
        {
            bytes.truncate(request_end);
            return Ok(Some(bytes));
        }
    }
}

fn request_body_length(header: &[u8]) -> io::Result<usize> {
    let header = String::from_utf8_lossy(header);
    let mut content_length = None;
    for line in header.lines().skip(1) {
        let (name, value) = line.split_once(':').ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed request header"))?;
        if name.is_empty() || !name.bytes().all(is_header_name_token) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "malformed request header name"));
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "transfer encoding is unsupported"));
        }
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "duplicate Content-Length"));
            }
            let value = value.trim_matches(|character| character == ' ' || character == '\t');
            content_length = Some(parse_content_length(value)?);
        }
    }
    Ok(content_length.unwrap_or(0))
}

fn is_header_name_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn parse_content_length(value: &str) -> io::Result<usize> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "malformed Content-Length"));
    }
    value
        .bytes()
        .try_fold(0usize, |length, digit| length.checked_mul(10).and_then(|length| length.checked_add(usize::from(digit - b'0'))))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Content-Length overflow"))
}

fn write_response(stream: &mut TcpStream, status: u16, headers: Vec<(String, String)>, body: &[u8]) -> bool {
    let mut header = format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\n", body.len());
    for (name, value) in headers {
        header.push_str(&format!("{name}: {value}\r\n"));
    }
    header.push_str("Connection: close\r\n\r\n");
    let deadline = Instant::now() + RESPONSE_WRITE_TIMEOUT;
    write_until(stream, header.as_bytes(), deadline).and_then(|()| write_until(stream, body, deadline)).is_ok()
}

fn write_until(stream: &mut TcpStream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "response write deadline elapsed"));
        }
        stream.set_write_timeout(Some(remaining))?;
        match stream.write(bytes) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "response write made no progress")),
            Ok(count) => bytes = &bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        io::{ErrorKind, Read, Write},
        net::{Shutdown, SocketAddr, TcpListener, TcpStream},
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    use super::{LoopbackPeer, MAX_REQUEST_BYTES, read_request_inner, write_until};

    fn read_request(request: &[u8]) -> io::Result<Option<Vec<u8>>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let mut client = TcpStream::connect(listener.local_addr()?)?;
        let (mut peer, _) = listener.accept()?;
        client.write_all(request)?;
        read_request_inner(&mut peer)
    }

    #[test]
    fn invalid_framing_is_rejected() {
        for request in [
            b"GET / HTTP/1.1\r\nContent-Length: nope\r\n\r\n".as_slice(),
            b"GET / HTTP/1.1\r\nContent-Length : 0\r\n\r\n",
            b"GET / HTTP/1.1\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n",
            b"GET / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
        ] {
            let error = read_request(request).expect_err("invalid request framing must fail");
            assert_eq!(error.kind(), ErrorKind::InvalidData);
        }
    }

    #[test]
    fn request_body_is_framed() {
        let request = b"POST / HTTP/1.1\r\nContent-Length: 4\r\n\r\nbody";
        assert_eq!(read_request(request).expect("read framed request").expect("request is connected"), request);
    }

    #[test]
    fn expired_write_sends_nothing() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind peer");
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).expect("connect peer");
        let (mut peer, _) = listener.accept().expect("accept peer");
        let error = write_until(&mut peer, b"response", Instant::now()).expect_err("expired response must fail");
        assert_eq!(error.kind(), ErrorKind::TimedOut);
        peer.shutdown(Shutdown::Write).expect("close peer output");
        let mut response = Vec::new();
        client.read_to_end(&mut response).expect("read closed output");
        assert!(response.is_empty());
    }

    #[test]
    fn oversized_request_is_rejected() {
        let request = format!("GET / HTTP/1.1\r\nContent-Length: {}\r\n\r\n", MAX_REQUEST_BYTES + 1);
        let error = read_request(request.as_bytes()).expect_err("declared oversized body must fail before reading it");
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn framing_overflow_is_rejected() {
        let request = format!("GET / HTTP/1.1\r\nContent-Length: {}\r\n\r\n", usize::MAX);
        let error = read_request(request.as_bytes()).expect_err("framing arithmetic overflow must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn drop_releases_listener() {
        let peer = LoopbackPeer::new_parallel(Duration::ZERO, |_, _| (200, Vec::new(), Vec::new()));
        let address = SocketAddr::from(([127, 0, 0, 1], peer.url().port().expect("loopback URL has a port")));

        drop(peer);

        let listener = TcpListener::bind(address).expect("dropping the peer releases its listener");
        drop(listener);
    }

    #[test]
    fn drop_finishes_abandoned_write() {
        const RESPONSE_BYTES: usize = 32 * 1024 * 1024;
        let peer = LoopbackPeer::new_parallel(Duration::ZERO, move |_, _| (200, Vec::new(), vec![0; RESPONSE_BYTES]));
        let address = SocketAddr::from(([127, 0, 0, 1], peer.url().port().expect("loopback URL has a port")));
        let mut client = TcpStream::connect(address).expect("connect loopback peer");
        client.set_read_timeout(Some(Duration::from_secs(5))).expect("set test read timeout");
        client.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").expect("write request");
        let mut response = Vec::new();
        let mut chunk = [0; 1024];
        while !response.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let count = client.read(&mut chunk).expect("read response header");
            assert_ne!(count, 0, "peer closes before response header");
            response.extend_from_slice(&chunk[..count]);
        }

        let (finished_tx, finished_rx) = mpsc::channel();
        let cleanup = thread::spawn(move || {
            drop(peer);
            finished_tx.send(()).expect("test waits for peer cleanup");
        });
        finished_rx.recv_timeout(Duration::from_secs(15)).expect("write timeout lets peer cleanup finish");
        cleanup.join().expect("peer cleanup thread completes");
        drop(client);
    }
}
