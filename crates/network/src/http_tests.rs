use std::{
    future::Future,
    task::Poll,
    time::{Duration, UNIX_EPOCH},
};

use reqwest::{
    StatusCode,
    header::{HeaderMap, HeaderValue},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net,
    task::JoinHandle,
    time::{self, Instant},
};
use url::Url;

use crate::fixtures::http::{LocalServer, response};

use super::{Client, Error, ErrorKind, RedirectPolicy, Response, retry_after_delay, validate_http_url};

#[test]
fn retry_after_values() {
    let date_now = UNIX_EPOCH + Duration::from_secs(784_111_700);
    for (value, now, expected) in [
        ("120", UNIX_EPOCH, Some(Duration::from_secs(120))),
        ("Sun, 06 Nov 1994 08:49:37 GMT", date_now, Some(Duration::from_secs(77))),
        ("Sunday, 06-Nov-94 08:49:37 GMT", date_now, Some(Duration::from_secs(77))),
        ("Sun Nov  6 08:49:37 1994", date_now, Some(Duration::from_secs(77))),
        ("soon", UNIX_EPOCH, None),
        ("", UNIX_EPOCH, None),
    ] {
        assert_eq!(retry_after_delay(value, now), expected, "{value:?}");
    }
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let redirect = "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:0/redirected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let server = LocalServer::start(vec![redirect.as_bytes().to_vec()]);
    let client = Client::new(Some(Duration::from_secs(2)), false, None).expect("build HTTP client");

    let response = client.get(&server.url("/start"), 10).await.expect("return redirect response without following it");
    let requests = server.finish();

    assert_eq!(response.status, StatusCode::FOUND);
    assert_eq!(requests.len(), 1);
}

#[tokio::test]
async fn viewer_redirect_omits_referer() {
    let redirect = b"HTTP/1.1 302 Found\r\nLocation: /settings\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let settings = b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nsettings";
    let server = LocalServer::start(vec![redirect.to_vec(), settings.to_vec()]);
    let client = Client::new(Some(Duration::from_secs(2)), false, None).expect("build HTTP client");

    let response = client
        .get_with_redirect_policy(&server.url("/asset?capability-secret"), 16, RedirectPolicy::ViewerAsset)
        .await
        .expect("follow loopback ViewerAsset redirect");
    let requests = server.finish();

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, b"settings");
    assert_eq!(requests.len(), 2);
    assert!(String::from_utf8_lossy(&requests[1]).starts_with("GET /settings HTTP/1.1"));
    assert!(!String::from_utf8_lossy(&requests[1]).to_ascii_lowercase().contains("referer:"));
}

#[tokio::test]
async fn viewer_follows_five_redirects() {
    let mut responses = (1..=5)
        .map(|hop| format!("HTTP/1.1 302 Found\r\nLocation: /hop{hop}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes())
        .collect::<Vec<_>>();
    responses.push(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_vec());
    let server = LocalServer::start(responses);
    let client = Client::new(Some(Duration::from_secs(2)), false, None).expect("build HTTP client");

    let response = client
        .get_with_redirect_policy(&server.url("/start"), 16, RedirectPolicy::ViewerAsset)
        .await
        .expect("follow up to five ViewerAsset redirects");
    let requests = server.finish();

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, b"ok");
    assert_eq!(requests.len(), 6);
}

#[tokio::test]
async fn body_at_limit_is_accepted() {
    let server = LocalServer::start(vec![response(200, "OK", "text/plain", None, "12345")]);
    let client = Client::new(Some(Duration::from_secs(2)), false, None).expect("build HTTP client");

    let response = client.get(&server.url("/body"), 5).await.expect("body at limit");

    assert_eq!(response.body, b"12345");
    server.finish();
}

#[tokio::test]
async fn declared_length_is_bounded() {
    let server = LocalServer::start(vec![response(200, "OK", "text/plain", None, "123456")]);
    let client = Client::new(Some(Duration::from_secs(2)), false, None).expect("build HTTP client");

    let error = client.get(&server.url("/body"), 5).await.expect_err("declared body exceeds limit");

    assert_eq!(error.kind, ErrorKind::ResourceExhausted);
    assert_eq!(error.status, Some(StatusCode::OK));
    assert!(error.to_string().contains("5-byte limit"));
    assert_eq!(server.finish().len(), 1);
}

#[tokio::test]
async fn chunked_body_limit() {
    let wire_response = concat!(
        "HTTP/1.1 200 OK\r\n",
        "Transfer-Encoding: chunked\r\n",
        "Retry-After: 2\r\n",
        "Connection: close\r\n\r\n",
        "3\r\nabc\r\n",
        "3\r\ndef\r\n",
        "0\r\n\r\n"
    );
    let server = LocalServer::start(vec![wire_response.as_bytes().to_vec()]);
    let client = Client::new(Some(Duration::from_secs(2)), false, None).expect("build HTTP client");

    let error = client.get(&server.url("/body"), 5).await.expect_err("chunked body exceeds limit");

    assert_eq!(error.kind, ErrorKind::ResourceExhausted);
    assert_eq!(error.status, Some(StatusCode::OK));
    assert_eq!(error.retry_after, Some(Duration::from_secs(2)));
    assert!(error.to_string().contains("5-byte limit"));
    server.finish();
}

#[test]
fn body_growth_stays_bounded() {
    let max_size = 37;
    let chunks = [b"abc".as_slice(), b"defgh", b"ijklmno", b"pqrstuvwxyz", b"ABCDEFGHIJK"];
    let mut body = Vec::new();

    for chunk in chunks {
        super::append_bounded_body(&mut body, chunk, max_size).expect("append body within the limit");
        assert!(body.capacity() <= max_size, "capacity {} exceeds limit {max_size}", body.capacity());
    }

    assert_eq!(body, b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJK");

    let mut tiny_body = Vec::new();
    let mut capacity_changes = 0;
    for _ in 0..max_size {
        let capacity = tiny_body.capacity();
        super::append_bounded_body(&mut tiny_body, b"x", max_size).expect("append tiny body chunk");
        capacity_changes += usize::from(tiny_body.capacity() != capacity);
        assert!(tiny_body.capacity() <= max_size, "capacity {} exceeds limit {max_size}", tiny_body.capacity());
    }
    assert_eq!(tiny_body, vec![b'x'; max_size]);
    assert!(capacity_changes <= 7, "capacity changed {capacity_changes} times for {max_size} tiny chunks");

    let capacity = tiny_body.capacity();
    let body_before_overflow = tiny_body.clone();
    let error = super::append_bounded_body(&mut tiny_body, b"y", max_size).expect_err("overflow must be rejected");

    assert_eq!(error.kind, ErrorKind::ResourceExhausted);
    assert_eq!(tiny_body, body_before_overflow);
    assert_eq!(tiny_body.capacity(), capacity);
}

#[tokio::test]
async fn gzip_content_encoding_remains_raw() {
    const GZIP_BODY: &[u8] = &[
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0a, 0xab, 0xa8, 0x18, 0x05, 0xa3, 0x60, 0x14, 0x54, 0x8c, 0x50, 0x00, 0x00,
        0x63, 0xf0, 0xd7, 0x48, 0x00, 0x04, 0x00, 0x00,
    ];
    let mut wire_response =
        format!("HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", GZIP_BODY.len())
            .into_bytes();
    wire_response.extend_from_slice(GZIP_BODY);
    let server = LocalServer::start(vec![wire_response]);
    let client = Client::new(Some(Duration::from_secs(2)), false, None).expect("build HTTP client");

    let response = client.get(&server.url("/compressed"), GZIP_BODY.len()).await.expect("read raw gzip bytes");

    assert_eq!(response.body, GZIP_BODY);
    assert_eq!(server.finish().len(), 1);
}

#[tokio::test]
async fn truncated_body_keeps_response_metadata() {
    let wire_response = concat!("HTTP/1.1 200 OK\r\n", "Content-Length: 6\r\n", "Retry-After: 2\r\n", "Connection: close\r\n\r\n", "abc");
    let server = LocalServer::start(vec![wire_response.as_bytes().to_vec()]);
    let client = Client::new(Some(Duration::from_secs(2)), false, None).expect("build HTTP client");

    let error = client.get(&server.url("/body"), 6).await.expect_err("truncated Content-Length response must fail");

    assert_eq!(error.kind, ErrorKind::Transport);
    assert_eq!(error.status, Some(StatusCode::OK));
    assert_eq!(error.retry_after, Some(Duration::from_secs(2)));
    assert!(error.elapsed > Duration::ZERO && error.elapsed < Duration::from_secs(2));
    assert_eq!(server.finish().len(), 1);
}

async fn held_body_response() -> (Url, tokio::sync::oneshot::Sender<()>, JoinHandle<()>) {
    let listener = net::TcpListener::bind("127.0.0.1:0").await.expect("bind local HTTP server");
    let address = listener.local_addr().expect("read local listener address");
    let (release_body, body_released) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept HTTP request");
        let mut request = Vec::new();
        let mut chunk = [0; 1024];
        loop {
            let size = stream.read(&mut chunk).await.expect("read HTTP request");
            if size == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..size]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 4\r\nRetry-After: 2\r\nConnection: close\r\n\r\n")
            .await
            .expect("write response headers");
        body_released.await.expect("release held response body");
        if let Err(error) = stream.write_all(b"data").await {
            std::assert_matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted,
                "write response body after the client stopped reading: {error}"
            );
        }
    });
    let url = Url::parse(&format!("http://{address}/body")).expect("local HTTP URL");

    (url, release_body, server)
}

#[tokio::test]
async fn post_headers_precede_body() {
    let (url, release_body, server) = held_body_response().await;
    let client = Client::new(Some(Duration::from_secs(20)), false, None).expect("build HTTP client");
    let incoming =
        match time::timeout(Duration::from_secs(45), client.post_headers(&url, "text/plain", Some("text/plain"), b"request".to_vec(), 16))
            .await
        {
            Ok(Ok(incoming)) => incoming,
            Ok(Err(error)) => {
                release_body.send(()).expect("release body after request failure");
                server.await.expect("server should finish");
                panic!("receive response headers: {error}");
            }
            Err(error) => {
                release_body.send(()).expect("release body after header timeout");
                server.await.expect("server should finish");
                panic!("post_headers should finish before response body: {error}");
            }
        };
    let header_timeout = incoming.timeout_error();

    assert_eq!(header_timeout.status, Some(StatusCode::OK));
    assert_eq!(header_timeout.retry_after, Some(Duration::from_secs(2)));
    release_body.send(()).expect("release response body");
    let response = incoming.read().await.expect("read released response body");
    server.await.expect("server should finish");

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.headers.get(reqwest::header::CONTENT_TYPE).and_then(|value| value.to_str().ok()), Some("text/plain"));
    assert_eq!(response.retry_after(), Some(Duration::from_secs(2)));
    assert_eq!(response.body, b"data");
}

#[tokio::test]
async fn expired_headers_reject_buffered_body() {
    let server = LocalServer::start(vec![response(200, "OK", "text/plain", Some("7"), "data")]);
    let deadline = Duration::from_secs(20);
    let client = Client::new(Some(deadline), false, None).expect("build HTTP client");
    let incoming = client.post_headers(&server.url("/body"), "text/plain", None, Vec::new(), 16).await.expect("parsed response headers");
    time::pause();
    time::advance(deadline).await;
    let error = incoming.read().await.expect_err("a buffered body cannot extend an expired operation");
    assert_eq!(error.kind, ErrorKind::Timeout);
    assert_eq!(error.status, Some(StatusCode::OK));
    assert_eq!(error.retry_after, Some(Duration::from_secs(7)));
    assert_eq!(server.finish().len(), 1);
}

#[tokio::test]
async fn body_deadline_preserves_response_metadata() {
    let (url, release_body, server) = held_body_response().await;
    let timeout = Duration::from_secs(20);
    let client = Client::new(Some(timeout), false, None).expect("build HTTP client");
    let incoming = match time::timeout(Duration::from_secs(45), client.post_headers(&url, "text/plain", None, Vec::new(), 16)).await {
        Ok(Ok(incoming)) => incoming,
        Ok(Err(error)) => {
            release_body.send(()).expect("release body after request failure");
            server.await.expect("server should finish");
            panic!("receive response headers: {error}");
        }
        Err(error) => {
            release_body.send(()).expect("release body after header timeout");
            server.await.expect("server should finish");
            panic!("post_headers should finish before response body: {error}");
        }
    };
    let deadline = incoming.deadline().expect("client configured a request deadline");
    let header_timeout = incoming.timeout_error();
    assert_eq!(header_timeout.status, Some(StatusCode::OK));
    assert_eq!(header_timeout.retry_after, Some(Duration::from_secs(2)));

    time::pause();
    time::advance(Duration::from_secs(5)).await;
    let mut body = Box::pin(incoming.read());
    let body_read_pending = std::future::poll_fn(|context| Poll::Ready(body.as_mut().poll(context).is_pending())).await;
    assert!(body_read_pending, "body read should be pending with response metadata available");
    time::advance(deadline.saturating_duration_since(Instant::now())).await;
    let error = body.await.expect_err("body read must hit its deadline");

    assert_eq!(error.kind, ErrorKind::Timeout);
    assert_eq!(error.status, Some(StatusCode::OK));
    assert_eq!(error.retry_after, header_timeout.retry_after);
    assert!(error.elapsed >= timeout, "elapsed {} should include the request's original deadline", error.elapsed.as_secs_f64());
    assert!(!error.is_timeout_after(timeout), "a body timeout has response metadata");
    release_body.send(()).expect("release held response body");
    server.await.expect("server should finish");
}

#[test]
fn long_transport_is_empty_poll() {
    let long_transport = Error::new(ErrorKind::Transport, Duration::from_secs(30), None, None, "closed".into());
    let fast_transport = Error::new(ErrorKind::Transport, Duration::from_secs(9), None, None, "closed".into());
    let response_transport = Error::new(ErrorKind::Transport, Duration::from_secs(30), Some(StatusCode::OK), None, "truncated".into());

    assert!(long_transport.is_transport_without_response_after(Duration::from_secs(10)));
    assert!(!fast_transport.is_transport_without_response_after(Duration::from_secs(10)));
    assert!(!response_transport.is_transport_without_response_after(Duration::from_secs(10)));
    assert!(long_transport.is_transient());
}

#[tokio::test]
async fn transport_errors_hide_urls() {
    let url = "http://127.0.0.1:0/cap?token=secret-token";
    let client = Client::new(Some(Duration::from_secs(2)), false, None).expect("build HTTP client");

    let error = client.get(&Url::parse(url).expect("parse capability URL"), 10).await.expect_err("closed port should fail");

    assert!(!error.to_string().contains("secret-token"));
    assert!(!format!("{error:?}").contains("secret-token"));
    assert!(!format!("{error:?}").contains(url));
}

#[test]
fn unsafe_urls_are_rejected() {
    for url in [
        Url::parse("ftp://example.com/cap").expect("parse FTP URL"),
        Url::parse("http://user:password@example.com/cap").expect("parse credential URL"),
        Url::parse("https://example.com/cap#fragment").expect("parse fragment URL"),
        Url::parse("file:///tmp/cap").expect("parse file URL"),
    ] {
        assert!(validate_http_url(&url).is_err(), "accepted {url}");
    }

    for url in [
        Url::parse("http://127.0.0.1:9000/cap?token=opaque").expect("parse local HTTP URL"),
        Url::parse("https://caps.example.net:8443/arbitrary/path").expect("parse HTTPS URL"),
    ] {
        assert!(validate_http_url(&url).is_ok(), "rejected {url}");
    }
}

#[test]
fn response_debug_hides_private_details() {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", HeaderValue::from_static("Bearer header-secret"));
    headers.insert("cookie", HeaderValue::from_static("session=cookie-secret"));
    let response = Response { status: StatusCode::OK, headers, body: b"body-session-secret".to_vec(), elapsed: Duration::from_millis(12) };

    let debug = format!("{response:?}");

    for secret in ["authorization", "header-secret", "cookie", "cookie-secret", "body-session-secret"] {
        assert!(!debug.contains(secret));
    }
    assert!(debug.contains("body_bytes"));
    assert!(debug.contains("header_count"));
}
