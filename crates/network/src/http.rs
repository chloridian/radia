use std::{
    error,
    fmt::{self, Debug, Display, Formatter},
    future::Future,
    net::IpAddr,
    time::{Duration, SystemTime},
};

use reqwest::{
    RequestBuilder, StatusCode, Url,
    header::{ACCEPT, ACCEPT_ENCODING, CONTENT_TYPE, HeaderMap, RANGE, RETRY_AFTER},
    redirect::Policy,
};
use tokio::time::{self, Instant};

#[derive(Clone)]
pub(crate) struct Client {
    client: reqwest::Client,
    viewer_asset_client: reqwest::Client,
    timeout: Option<Duration>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RedirectPolicy {
    #[default]
    None,
    ViewerAsset,
}

impl Client {
    pub(crate) fn new(timeout: Option<Duration>, http1_only: bool, user_agent: Option<&str>) -> Result<Self, reqwest::Error> {
        let mut builder = reqwest::Client::builder().redirect(Policy::none()).referer(false);
        let mut viewer_asset_builder = reqwest::Client::builder().redirect(viewer_asset_redirect_policy()).referer(false);
        if http1_only {
            builder = builder.http1_only();
            viewer_asset_builder = viewer_asset_builder.http1_only();
        }
        if let Some(user_agent) = user_agent {
            builder = builder.user_agent(user_agent);
            viewer_asset_builder = viewer_asset_builder.user_agent(user_agent);
        }
        let client = builder.build()?;
        let viewer_asset_client = viewer_asset_builder.build()?;
        Ok(Self { client, viewer_asset_client, timeout })
    }

    pub(crate) fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    pub(crate) async fn get(&self, url: &Url, max_response_bytes: usize) -> Result<Response, Error> {
        validate_http_url(url)?;
        self.send_headers(self.client.get(url.clone()), max_response_bytes).await?.read().await
    }

    pub(crate) async fn get_with_accept(&self, url: &Url, accept: &str, max_response_bytes: usize) -> Result<Response, Error> {
        validate_http_url(url)?;
        self.send_headers(self.client.get(url.clone()).header(ACCEPT, accept), max_response_bytes).await?.read().await
    }

    pub(crate) async fn get_with_accept_and_redirect_policy(
        &self,
        url: &Url,
        accept: &str,
        max_response_bytes: usize,
        redirect_policy: RedirectPolicy,
    ) -> Result<Response, Error> {
        validate_http_url(url)?;
        let client = match redirect_policy {
            RedirectPolicy::None => &self.client,
            RedirectPolicy::ViewerAsset => &self.viewer_asset_client,
        };
        self.send_headers(client.get(url.clone()).header(ACCEPT, accept), max_response_bytes).await?.read().await
    }

    pub(crate) async fn get_with_redirect_policy(
        &self,
        url: &Url,
        max_response_bytes: usize,
        redirect_policy: RedirectPolicy,
    ) -> Result<Response, Error> {
        validate_http_url(url)?;
        let client = match redirect_policy {
            RedirectPolicy::None => &self.client,
            RedirectPolicy::ViewerAsset => &self.viewer_asset_client,
        };
        self.send_headers(client.get(url.clone()), max_response_bytes).await?.read().await
    }

    pub(crate) async fn get_range_with_redirect_policy(
        &self,
        url: &Url,
        range: &str,
        max_response_bytes: usize,
        redirect_policy: RedirectPolicy,
    ) -> Result<Response, Error> {
        validate_http_url(url)?;
        let client = match redirect_policy {
            RedirectPolicy::None => &self.client,
            RedirectPolicy::ViewerAsset => &self.viewer_asset_client,
        };
        self.send_headers(client.get(url.clone()).header(RANGE, range).header(ACCEPT_ENCODING, "identity"), max_response_bytes)
            .await?
            .read()
            .await
    }

    pub(crate) async fn post(
        &self,
        url: &Url,
        content_type: &str,
        accept: Option<&str>,
        body: Vec<u8>,
        max_response_bytes: usize,
    ) -> Result<Response, Error> {
        self.post_headers(url, content_type, accept, body, max_response_bytes).await?.read().await
    }

    pub(crate) async fn post_headers(
        &self,
        url: &Url,
        content_type: &str,
        accept: Option<&str>,
        body: Vec<u8>,
        max_response_bytes: usize,
    ) -> Result<IncomingResponse, Error> {
        validate_http_url(url)?;
        let mut request = self.client.post(url.clone()).header(CONTENT_TYPE, content_type).body(body);
        if let Some(accept) = accept {
            request = request.header(ACCEPT, accept);
        }
        self.send_headers(request, max_response_bytes).await
    }

    pub(crate) async fn patch(
        &self,
        url: &Url,
        content_type: &str,
        accept: Option<&str>,
        body: Vec<u8>,
        max_response_bytes: usize,
    ) -> Result<Response, Error> {
        validate_http_url(url)?;
        let mut request = self.client.patch(url.clone()).header(CONTENT_TYPE, content_type).body(body);
        if let Some(accept) = accept {
            request = request.header(ACCEPT, accept);
        }
        self.send_headers(request, max_response_bytes).await?.read().await
    }

    pub(crate) async fn delete(&self, url: &Url, accept: Option<&str>, max_response_bytes: usize) -> Result<Response, Error> {
        validate_http_url(url)?;
        let mut request = self.client.delete(url.clone());
        if let Some(accept) = accept {
            request = request.header(ACCEPT, accept);
        }
        self.send_headers(request, max_response_bytes).await?.read().await
    }

    async fn send_headers(&self, request: RequestBuilder, max_response_bytes: usize) -> Result<IncomingResponse, Error> {
        let started = Instant::now();
        let deadline = self.timeout.map(|timeout| started + timeout);
        let response = send_with_deadline(request.send(), deadline, started).await?;
        let retry_after = response_retry_after(response.headers());
        let incoming = IncomingResponse { response, started, deadline, max_response_bytes, retry_after };
        incoming.check_deadline()?;
        Ok(incoming)
    }
}

pub(crate) struct IncomingResponse {
    response: reqwest::Response,
    started: Instant,
    deadline: Option<Instant>,
    max_response_bytes: usize,
    retry_after: Option<Duration>,
}

impl IncomingResponse {
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub(crate) fn timeout_error(&self) -> Error {
        Error::deadline(
            self.started,
            Some(self.response.status()),
            self.retry_after,
            "The HTTP operation exceeded its deadline after receiving response headers.",
        )
    }

    fn check_deadline(&self) -> Result<(), Error> {
        if self.deadline.is_some_and(|deadline| Instant::now() >= deadline) { Err(self.timeout_error()) } else { Ok(()) }
    }

    pub(crate) async fn read(mut self) -> Result<Response, Error> {
        let status = self.response.status();
        let headers = self.response.headers().clone();
        let body = read_bounded_body(&mut self).await?;
        let elapsed = self.started.elapsed();
        Ok(Response { status, headers, body, elapsed })
    }
}

fn viewer_asset_redirect_policy() -> Policy {
    Policy::custom(|attempt| {
        if attempt.previous().len() >= 6 {
            return attempt.stop();
        }
        let target = attempt.url();
        if target.scheme() == "https"
            || attempt
                .previous()
                .last()
                .is_some_and(|source| source.scheme() == target.scheme() && is_loopback_url(source) && is_loopback_url(target))
        {
            attempt.follow()
        } else {
            attempt.stop()
        }
    })
}

fn is_loopback_url(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().is_ok_and(|address| address.is_loopback())
    })
}

pub(crate) struct Response {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Vec<u8>,
    pub(crate) elapsed: Duration,
}

impl Response {
    pub(crate) fn retry_after(&self) -> Option<Duration> {
        let value = self.headers.get(RETRY_AFTER)?.to_str().ok()?;
        retry_after_delay(value, SystemTime::now())
    }
}

impl Debug for Response {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Response")
            .field("status", &self.status)
            .field("header_count", &self.headers.len())
            .field("body_bytes", &self.body.len())
            .field("elapsed", &self.elapsed)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ErrorKind {
    Transport,
    Timeout,
    Cancelled,
    ResourceExhausted,
    HttpStatus,
    InvalidResponse,
    InvalidUrl,
}

impl ErrorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::ResourceExhausted => "resource_exhausted",
            Self::HttpStatus => "http_status",
            Self::InvalidResponse => "invalid_response",
            Self::InvalidUrl => "invalid_url",
        }
    }
}

#[non_exhaustive]
pub struct Error {
    pub kind: ErrorKind,
    pub elapsed: Duration,
    pub status: Option<StatusCode>,
    pub retry_after: Option<Duration>,
    message: String,
}

impl Error {
    fn new(kind: ErrorKind, elapsed: Duration, status: Option<StatusCode>, retry_after: Option<Duration>, message: String) -> Self {
        Self { kind, elapsed, status, retry_after, message }
    }

    fn request(error: reqwest::Error, elapsed: Duration) -> Self {
        let kind = if error.is_timeout() { ErrorKind::Timeout } else { ErrorKind::Transport };
        Self::new(kind, elapsed, None, None, error.without_url().to_string())
    }

    pub(crate) fn resource_exhausted(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::ResourceExhausted, Duration::ZERO, None, None, message.into())
    }

    pub(crate) fn timeout(elapsed: Duration, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Timeout, elapsed, None, None, message.into())
    }

    fn deadline(started: Instant, status: Option<StatusCode>, retry_after: Option<Duration>, message: &str) -> Self {
        Self::new(ErrorKind::Timeout, started.elapsed(), status, retry_after, message.into())
    }

    fn with_response(mut self, status: StatusCode, retry_after: Option<Duration>, elapsed: Duration) -> Self {
        self.status = Some(status);
        self.retry_after = retry_after;
        self.elapsed = elapsed;
        self
    }

    pub(crate) fn is_timeout_after(&self, minimum: Duration) -> bool {
        self.kind == ErrorKind::Timeout && self.status.is_none() && self.elapsed >= minimum
    }

    pub(crate) fn is_transport_without_response_after(&self, minimum: Duration) -> bool {
        self.kind == ErrorKind::Transport && self.status.is_none() && self.elapsed >= minimum
    }

    pub(crate) fn is_transient(&self) -> bool {
        matches!(self.kind, ErrorKind::Transport | ErrorKind::Timeout)
    }

    pub(crate) fn is_resource_exhausted(&self) -> bool {
        self.kind == ErrorKind::ResourceExhausted
    }

    pub(crate) fn http_status(status: StatusCode, elapsed: Duration, retry_after: Option<Duration>, context: &str) -> Self {
        Self::new(ErrorKind::HttpStatus, elapsed, Some(status), retry_after, format!("{context}: HTTP {status}"))
    }

    pub(crate) fn invalid_response(status: StatusCode, elapsed: Duration, context: &str) -> Self {
        Self::new(ErrorKind::InvalidResponse, elapsed, Some(status), None, context.into())
    }
}

impl Debug for Error {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Error")
            .field("kind", &self.kind)
            .field("elapsed", &self.elapsed)
            .field("status", &self.status)
            .field("retry_after", &self.retry_after)
            .field("message", &"[REDACTED]")
            .finish()
    }
}

impl Display for Error {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl error::Error for Error {}

fn retry_after_delay(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        let seconds = value.parse::<u64>().unwrap_or(u64::MAX);
        return Some(Duration::from_secs(seconds));
    }

    let retry_at = httpdate::parse_http_date(value).ok()?;
    Some(retry_at.duration_since(now).unwrap_or_default())
}

fn response_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?;
    retry_after_delay(value, SystemTime::now())
}

async fn send_with_deadline<F>(send: F, deadline: Option<Instant>, started: Instant) -> Result<reqwest::Response, Error>
where
    F: Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    let result = if let Some(deadline) = deadline {
        time::timeout_at(deadline, send)
            .await
            .map_err(|_| Error::deadline(started, None, None, "The HTTP operation exceeded its deadline."))?
    } else {
        send.await
    };
    result.map_err(|error| Error::request(error, started.elapsed()))
}

async fn read_bounded_body(incoming: &mut IncomingResponse) -> Result<Vec<u8>, Error> {
    let status = incoming.response.status();
    if let Some(length) = incoming.response.content_length()
        && usize::try_from(length).map_or(true, |length| length > incoming.max_response_bytes)
    {
        return Err(Error::resource_exhausted(format!("HTTP response body exceeds the {}-byte limit.", incoming.max_response_bytes))
            .with_response(status, incoming.retry_after, incoming.started.elapsed()));
    }

    let mut body = Vec::new();
    loop {
        incoming.check_deadline()?;
        let next = if let Some(deadline) = incoming.deadline() {
            match time::timeout_at(deadline, incoming.response.chunk()).await {
                Ok(next) => next,
                Err(_) => return Err(incoming.timeout_error()),
            }
        } else {
            incoming.response.chunk().await
        };
        let Some(chunk) = next.map_err(|error| {
            let elapsed = incoming.started.elapsed();
            Error::request(error, elapsed).with_response(status, incoming.retry_after, elapsed)
        })?
        else {
            break;
        };
        let elapsed = incoming.started.elapsed();
        append_bounded_body(&mut body, &chunk, incoming.max_response_bytes)
            .map_err(|error| error.with_response(status, incoming.retry_after, elapsed))?;
    }
    Ok(body)
}

fn append_bounded_body(body: &mut Vec<u8>, chunk: &[u8], max_size: usize) -> Result<(), Error> {
    let required = body
        .len()
        .checked_add(chunk.len())
        .filter(|size| *size <= max_size)
        .ok_or_else(|| Error::resource_exhausted(format!("HTTP response body exceeds the {max_size}-byte limit.")))?;
    if required > body.capacity() {
        let target_capacity = required.max(body.capacity().saturating_mul(2)).min(max_size);
        body.try_reserve_exact(target_capacity - body.len())
            .map_err(|_| Error::resource_exhausted("Could not grow the HTTP response body."))?;
    }
    body.extend_from_slice(chunk);
    Ok(())
}

pub(crate) fn validate_http_url(url: &Url) -> Result<(), Error> {
    let has_userinfo = url.username() != ""
        || url.password().is_some()
        || url.as_str().split_once("://").is_some_and(|(_, authority_and_path)| {
            authority_and_path.split(['/', '?', '#']).next().is_some_and(|authority| authority.contains('@'))
        });
    if !matches!(url.scheme(), "http" | "https") || !url.has_host() || has_userinfo {
        return Err(Error::new(
            ErrorKind::InvalidUrl,
            Duration::ZERO,
            None,
            None,
            "URL must use HTTP or HTTPS and include a host without embedded credentials.".into(),
        ));
    }
    if url.fragment().is_some() {
        return Err(Error::new(ErrorKind::InvalidUrl, Duration::ZERO, None, None, "URL must not include a fragment.".into()));
    }
    Ok(())
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
