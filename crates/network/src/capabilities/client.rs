use std::{
    collections::HashMap,
    fmt,
    future::Future,
    net::SocketAddr,
    ops::Range,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use llsd::xml;
use reqwest::Url;
use serde::Serialize;
use tokio::{
    runtime::Handle,
    sync,
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::AbortHandle,
    time,
};
use tracing::Instrument;
use uuid::Uuid;

use super::{
    SetupError,
    budget::{
        CapabilityBudget, EVENT_QUEUE_REQUEST_TIMEOUT, MAX_CAPABILITY_REQUEST_BODY_BYTES, MAX_CAPABILITY_RESPONSE_BYTES,
        MAX_SEED_CAPABILITY_RESPONSE_BYTES, REQUEST_TIMEOUT, spawn_blocking_with_permit,
    },
    event_queue::{self, EventQueueContext, EventQueueStatus, EventQueueTask, Stop},
    request::{
        RequestDeadline, decode_capability_value_with_response_limit, decode_seed_capabilities, delete_capability, get_capability_bytes,
        get_capability_range_bytes, patch_capability, post_capability, request_capabilities_response, validate_capability_names,
    },
};
use crate::{
    coverage::CoverageLossLedger,
    error::Error,
    event::{EventEnvelope, SimulatorId},
    http::{self, RedirectPolicy, validate_http_url},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapabilityLastStatus {
    Http(u16),
    Cancelled,
    SessionClosed,
    Transport(http::ErrorKind),
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityRequestStatus {
    pub name: Arc<str>,
    pub granted: bool,
    pub last_status: Option<CapabilityLastStatus>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CapabilityDiagnostics {
    pub event_queue: EventQueueStatus,
    pub capabilities: Vec<CapabilityRequestStatus>,
}

#[derive(Clone)]
pub struct CapabilityUrl {
    pub(super) url: Url,
    name: Option<Arc<str>>,
}

impl CapabilityUrl {
    pub(super) fn from_discovery(url: Url) -> Self {
        Self { url, name: None }
    }

    fn with_name(mut self, name: &str) -> Self {
        self.name = Some(Arc::from(name));
        self
    }

    pub fn parse(value: &str) -> Result<Self, Error> {
        let url = Url::parse(value).map_err(|_| Error::Capability("Capability URL is invalid.".into()))?;
        validate_http_url(&url).map_err(|error| Error::Capability(error.to_string()))?;
        Ok(Self { url, name: None })
    }

    pub fn with_query_pair(&self, name: &str, value: &str) -> Self {
        let mut url = self.url.clone();
        url.query_pairs_mut().append_pair(name, value);
        Self { url, name: self.name.clone() }
    }

    pub fn with_uuid_list_query_pair(&self, name: &str, values: &[Uuid]) -> Self {
        let mut url = self.url.clone();
        let joined = values.iter().map(Uuid::to_string).collect::<Vec<_>>().join(",");
        let updated = match url.query() {
            Some(existing) if !existing.is_empty() => format!("{existing}&{name}={joined}"),
            _ => format!("{name}={joined}"),
        };
        url.set_query(Some(&updated));
        Self { url, name: self.name.clone() }
    }

    pub fn with_path_segments(&self, segments: &[&str]) -> Result<Self, Error> {
        let mut url = self.url.clone();
        let Ok(mut path) = url.path_segments_mut() else {
            return Err(Error::Capability("Capability URL cannot contain path segments.".into()));
        };
        path.pop_if_empty();
        path.extend(segments.iter().copied());
        drop(path);
        Ok(Self { url, name: self.name.clone() })
    }

    pub fn with_trailing_slash(&self) -> Self {
        let mut url = self.url.clone();
        if !url.path().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }
        Self { url, name: self.name.clone() }
    }
}

impl fmt::Debug for CapabilityUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("CapabilityUrl").field(&"[REDACTED]").finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContentRange {
    pub start: u64,
    pub end: u64,
    pub total: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RangeResponse {
    Partial { bytes: Vec<u8>, content_range: Option<ContentRange> },
    Full(Vec<u8>),
}

#[derive(Clone)]
pub struct Client {
    seed: Arc<Url>,
    runtime: Handle,
    http: http::Client,
    session_closed: watch::Sender<bool>,
    requests: RequestTracker,
    pub(super) budget: CapabilityBudget,
    request_timeout: Duration,
    grants: Arc<GrantCache>,
    event_queue_status: watch::Sender<EventQueueStatus>,
}

impl fmt::Debug for Client {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Client").field("seed", &"<redacted>").finish_non_exhaustive()
    }
}

impl Client {
    pub fn new(seed: Url) -> Result<Self, Error> {
        Self::new_with_budget(seed, CapabilityBudget::new()).map_err(|error| Error::Capability(error.to_string()))
    }

    pub(crate) fn new_with_budget(seed: Url, budget: CapabilityBudget) -> Result<Self, SetupError> {
        validate_http_url(&seed).map_err(|error| SetupError::InvalidSeed(Arc::new(error)))?;
        let runtime = Handle::try_current().map_err(|_| SetupError::RequestRuntimeUnavailable)?;
        let http = budget.http_client()?;
        let (session_closed, _) = watch::channel(false);
        let (event_queue_status, _) = watch::channel(EventQueueStatus::default());
        Ok(Self {
            seed: Arc::new(seed),
            runtime,
            http,
            session_closed,
            requests: RequestTracker::new(),
            budget,
            request_timeout: REQUEST_TIMEOUT,
            grants: Arc::default(),
            event_queue_status,
        })
    }

    pub fn diagnostics(&self) -> CapabilityDiagnostics {
        CapabilityDiagnostics { event_queue: *self.event_queue_status.borrow(), capabilities: self.grants.snapshot() }
    }

    pub async fn request_capabilities(&self, names: &[&str]) -> Result<HashMap<String, CapabilityUrl>, Error> {
        validate_capability_names(names)?;
        if *self.session_closed.borrow() {
            return Err(Error::CapabilitySessionClosed);
        }
        match self.grants.cached(names) {
            Ok(granted) => Ok(granted),
            Err(_) => self.discover_missing(names).await,
        }
    }

    async fn discover_missing(&self, names: &[&str]) -> Result<HashMap<String, CapabilityUrl>, Error> {
        let deadline = RequestDeadline::new(self.request_timeout);
        let names = names.iter().map(|name| (*name).to_owned()).collect::<Vec<_>>();
        if deadline.expired() {
            return Err(deadline.error());
        }
        let client = self.clone();
        self.on_runtime(async move {
            let _discovery = client.race_request(deadline, async { Ok(client.grants.discovery.lock().await) }).await?;
            let missing = match client.grants.cached(&names) {
                Ok(granted) => return Ok(granted),
                Err(missing) => missing,
            };
            let discovered = client.discover(deadline, missing.clone()).await?;
            Ok(client.grants.record(&names, missing, &discovered))
        })
        .await
    }

    #[tracing::instrument(name = "capabilities.discover", level = "debug", skip_all, fields(capability_count = names.len()))]
    async fn discover(&self, deadline: RequestDeadline, names: Vec<String>) -> Result<HashMap<String, CapabilityUrl>, Error> {
        let seed = self.seed.as_ref().clone();
        let request_names = names.clone();
        let (response, permit) = self
            .run_with_permit(deadline, move |http, permit| async move {
                let response = request_capabilities_response(&http, &seed, &request_names, MAX_SEED_CAPABILITY_RESPONSE_BYTES).await?;
                Ok((response, permit))
            })
            .await?;
        let decode_guard = self.requests.track();
        let decode = spawn_blocking_with_permit(permit, move || {
            let _decode_guard = decode_guard;
            decode_seed_capabilities(response, &names)
        });
        let (result, permit) = self
            .race_request(deadline, async {
                time::timeout_at(deadline.at, decode)
                    .await
                    .map_err(|_| deadline.error())?
                    .map_err(|error| Error::Capability(format!("Capability response decoder failed: {error}")))
            })
            .await?;
        drop(permit);
        let capabilities = result?;
        Ok(capabilities.into_iter().map(|(name, url)| (name, CapabilityUrl::from_discovery(url))).collect())
    }

    pub async fn post_with_response_limit<Request>(
        &self,
        url: &CapabilityUrl,
        payload: &Request,
        max_response_bytes: usize,
    ) -> Result<llsd::Value, Error>
    where
        Request: Serialize + ?Sized,
    {
        validate_response_limit(max_response_bytes)?;
        let deadline = RequestDeadline::new(self.request_timeout);
        let permit = self.acquire_request_permit_on_runtime(deadline).await?;
        let body = encode_request(payload)?;
        if deadline.expired() {
            return Err(deadline.error());
        }
        let client = self.clone();
        let url = url.clone();
        self.on_runtime(async move {
            let result = client
                .run_with_admitted_permit(deadline, permit, |http, permit| {
                    let url = url.clone();
                    async move {
                        let response = post_capability(&http, &url.url, body, max_response_bytes).await.map_err(Error::CapabilityHttp)?;
                        Ok((response, permit))
                    }
                })
                .await;
            client.record_response_status(&url, result.as_ref().map(|(response, _)| response.status));
            let (response, permit) = result?;
            client.decode_admitted_response(response, max_response_bytes, deadline, permit).await
        })
        .await
    }

    pub async fn post<Request>(&self, url: &CapabilityUrl, payload: &Request) -> Result<llsd::Value, Error>
    where
        Request: Serialize + ?Sized,
    {
        self.post_with_response_limit(url, payload, MAX_CAPABILITY_RESPONSE_BYTES).await
    }

    pub async fn post_bytes_with_response_limit<Request>(
        &self,
        url: &CapabilityUrl,
        payload: &Request,
        max_response_bytes: usize,
    ) -> Result<Vec<u8>, Error>
    where
        Request: Serialize + ?Sized,
    {
        validate_response_limit(max_response_bytes)?;
        let deadline = RequestDeadline::new(self.request_timeout);
        let permit = self.acquire_request_permit_on_runtime(deadline).await?;
        let body = encode_request(payload)?;
        if deadline.expired() {
            return Err(deadline.error());
        }
        let client = self.clone();
        let url = url.clone();
        self.on_runtime(async move {
            let result = client
                .run_with_admitted_permit(deadline, permit, |http, permit| {
                    let url = url.clone();
                    async move {
                        let response = post_capability(&http, &url.url, body, max_response_bytes).await.map_err(Error::CapabilityHttp)?;
                        Ok((response, permit))
                    }
                })
                .await;
            client.record_response_status(&url, result.as_ref().map(|(response, _)| response.status));
            let (response, permit) = result?;
            drop(permit);
            Ok(response.body)
        })
        .await
    }

    pub async fn patch_bytes_with_response_limit<Request>(
        &self,
        url: &CapabilityUrl,
        payload: &Request,
        max_response_bytes: usize,
    ) -> Result<Vec<u8>, Error>
    where
        Request: Serialize + ?Sized,
    {
        validate_response_limit(max_response_bytes)?;
        let deadline = RequestDeadline::new(self.request_timeout);
        let permit = self.acquire_request_permit_on_runtime(deadline).await?;
        let body = encode_request(payload)?;
        if deadline.expired() {
            return Err(deadline.error());
        }
        let client = self.clone();
        let url = url.clone();
        self.on_runtime(async move {
            let result = client
                .run_with_admitted_permit(deadline, permit, |http, permit| {
                    let url = url.clone();
                    async move {
                        let response = patch_capability(&http, &url.url, body, max_response_bytes).await.map_err(Error::CapabilityHttp)?;
                        Ok((response, permit))
                    }
                })
                .await;
            client.record_response_status(&url, result.as_ref().map(|(response, _)| response.status));
            let (response, permit) = result?;
            drop(permit);
            Ok(response.body)
        })
        .await
    }

    pub async fn delete_bytes_with_response_limit(&self, url: &CapabilityUrl, max_response_bytes: usize) -> Result<Vec<u8>, Error> {
        validate_response_limit(max_response_bytes)?;
        let deadline = RequestDeadline::new(self.request_timeout);
        let permit = self.acquire_request_permit_on_runtime(deadline).await?;
        if deadline.expired() {
            return Err(deadline.error());
        }
        let client = self.clone();
        let url = url.clone();
        self.on_runtime(async move {
            let result = client
                .run_with_admitted_permit(deadline, permit, |http, permit| {
                    let url = url.clone();
                    async move {
                        let response = delete_capability(&http, &url.url, max_response_bytes).await.map_err(Error::CapabilityHttp)?;
                        Ok((response, permit))
                    }
                })
                .await;
            client.record_response_status(&url, result.as_ref().map(|(response, _)| response.status));
            let (response, permit) = result?;
            drop(permit);
            Ok(response.body)
        })
        .await
    }

    pub async fn get_bytes_with_response_limit(&self, url: &CapabilityUrl, max_response_bytes: usize) -> Result<Vec<u8>, Error> {
        self.get_bytes_inner(url, None, max_response_bytes, RedirectPolicy::None).await
    }

    pub async fn get_bytes_with_response_limit_and_redirect_policy(
        &self,
        url: &CapabilityUrl,
        max_response_bytes: usize,
        redirect_policy: RedirectPolicy,
    ) -> Result<Vec<u8>, Error> {
        self.get_bytes_inner(url, None, max_response_bytes, redirect_policy).await
    }

    pub async fn get_range_with_response_limit_and_redirect_policy(
        &self,
        url: &CapabilityUrl,
        range: Range<u64>,
        max_response_bytes: usize,
        redirect_policy: RedirectPolicy,
    ) -> Result<RangeResponse, Error> {
        validate_response_limit(max_response_bytes)?;
        if range.start >= range.end {
            return Err(Error::Capability("Capability byte range must be non-empty.".into()));
        }
        let deadline = RequestDeadline::new(self.request_timeout);
        let client = self.clone();
        let url = url.clone();
        self.on_runtime(async move {
            let result = client
                .run_with_get_permit(deadline, redirect_policy, |http, permit| {
                    let url = url.clone();
                    async move {
                        let response = get_capability_range_bytes(&http, &url.url, range, max_response_bytes, redirect_policy)
                            .await
                            .map_err(Error::CapabilityHttp)?;
                        Ok((response, permit))
                    }
                })
                .await;
            client.record_response_status(
                &url,
                result.as_ref().map(|(response, _)| match response {
                    RangeResponse::Partial { .. } => reqwest::StatusCode::PARTIAL_CONTENT,
                    RangeResponse::Full(_) => reqwest::StatusCode::OK,
                }),
            );
            let (response, permit) = result?;
            drop(permit);
            Ok(response)
        })
        .await
    }

    pub async fn get_bytes_with_accept(&self, url: &CapabilityUrl, accept: &str, max_response_bytes: usize) -> Result<Vec<u8>, Error> {
        self.get_bytes_inner(url, Some(accept.to_owned()), max_response_bytes, RedirectPolicy::None).await
    }

    pub async fn get_inventory_bytes_with_accept(
        &self,
        url: &CapabilityUrl,
        accept: &str,
        max_response_bytes: usize,
    ) -> Result<Vec<u8>, Error> {
        validate_response_limit(max_response_bytes)?;
        let deadline = RequestDeadline::new(self.request_timeout);
        let client = self.clone();
        let url = url.clone();
        let accept = accept.to_owned();
        self.on_runtime(async move {
            let budget = Arc::clone(&client.budget.inventory_requests);
            let result = client
                .run_with_budget_permit(deadline, budget, |http, permit| {
                    let url = url.clone();
                    async move {
                        let response =
                            get_capability_bytes(&http, &url.url, Some(accept.as_str()), max_response_bytes, RedirectPolicy::None)
                                .await
                                .map_err(Error::CapabilityHttp)?;
                        Ok((response, permit))
                    }
                })
                .await;
            client.record_response_status(&url, result.as_ref().map(|(response, _)| response.status));
            let (response, permit) = result?;
            drop(permit);
            Ok(response.body)
        })
        .await
    }

    async fn get_bytes_inner(
        &self,
        url: &CapabilityUrl,
        accept: Option<String>,
        max_response_bytes: usize,
        redirect_policy: RedirectPolicy,
    ) -> Result<Vec<u8>, Error> {
        validate_response_limit(max_response_bytes)?;
        let deadline = RequestDeadline::new(self.request_timeout);
        let client = self.clone();
        let url = url.clone();
        self.on_runtime(async move {
            let result = client
                .run_with_get_permit(deadline, redirect_policy, |http, permit| {
                    let url = url.clone();
                    async move {
                        let response = get_capability_bytes(&http, &url.url, accept.as_deref(), max_response_bytes, redirect_policy)
                            .await
                            .map_err(Error::CapabilityHttp)?;
                        Ok((response, permit))
                    }
                })
                .await;
            client.record_response_status(&url, result.as_ref().map(|(response, _)| response.status));
            let (response, permit) = result?;
            drop(permit);
            Ok(response.body)
        })
        .await
    }

    async fn run_with_permit<T, F, Fut>(&self, deadline: RequestDeadline, operation: F) -> Result<T, Error>
    where
        F: FnOnce(http::Client, OwnedSemaphorePermit) -> Fut,
        Fut: Future<Output = Result<T, Error>>,
    {
        self.run_with_budget_permit(deadline, Arc::clone(&self.budget.ordinary_requests), operation).await
    }

    async fn run_with_get_permit<T, F, Fut>(
        &self,
        deadline: RequestDeadline,
        redirect_policy: RedirectPolicy,
        operation: F,
    ) -> Result<T, Error>
    where
        F: FnOnce(http::Client, OwnedSemaphorePermit) -> Fut,
        Fut: Future<Output = Result<T, Error>>,
    {
        let budget = match redirect_policy {
            RedirectPolicy::None => Arc::clone(&self.budget.ordinary_requests),
            RedirectPolicy::ViewerAsset => Arc::clone(&self.budget.asset_requests),
        };
        self.run_with_budget_permit(deadline, budget, operation).await
    }

    async fn run_with_budget_permit<T, F, Fut>(&self, deadline: RequestDeadline, budget: Arc<Semaphore>, operation: F) -> Result<T, Error>
    where
        F: FnOnce(http::Client, OwnedSemaphorePermit) -> Fut,
        Fut: Future<Output = Result<T, Error>>,
    {
        let permit = self.acquire_request_permit_from(budget, deadline).await?;
        self.run_with_admitted_permit(deadline, permit, operation).await
    }

    async fn run_with_admitted_permit<T, F, Fut>(
        &self,
        deadline: RequestDeadline,
        permit: OwnedSemaphorePermit,
        operation: F,
    ) -> Result<T, Error>
    where
        F: FnOnce(http::Client, OwnedSemaphorePermit) -> Fut,
        Fut: Future<Output = Result<T, Error>>,
    {
        self.race_request(deadline, operation(self.http.clone(), permit)).await
    }

    async fn acquire_request_permit(&self, deadline: RequestDeadline) -> Result<OwnedSemaphorePermit, Error> {
        self.acquire_request_permit_from(Arc::clone(&self.budget.ordinary_requests), deadline).await
    }

    async fn acquire_request_permit_from(&self, budget: Arc<Semaphore>, deadline: RequestDeadline) -> Result<OwnedSemaphorePermit, Error> {
        let mut session_closed = self.session_closed.subscribe();
        if *session_closed.borrow() {
            return Err(Error::CapabilitySessionClosed);
        }
        tokio::select! {
            biased;
            _ = session_closed.wait_for(|closed| *closed) => Err(Error::CapabilitySessionClosed),
            _ = time::sleep_until(deadline.at) => Err(deadline.error()),
            permit = budget.acquire_owned() => permit.map_err(|_| Error::CapabilityRequestCanceled),
        }
    }

    async fn acquire_request_permit_on_runtime(&self, deadline: RequestDeadline) -> Result<OwnedSemaphorePermit, Error> {
        let client = self.clone();
        self.on_runtime(async move { client.acquire_request_permit(deadline).await }).await
    }

    async fn race_request<T>(&self, deadline: RequestDeadline, operation: impl Future<Output = Result<T, Error>>) -> Result<T, Error> {
        let mut session_closed = self.session_closed.subscribe();
        tokio::select! {
            biased;
            _ = session_closed.wait_for(|closed| *closed) => Err(Error::CapabilitySessionClosed),
            _ = time::sleep_until(deadline.at) => Err(deadline.error()),
            result = operation => result,
        }
    }

    async fn decode_admitted_response(
        &self,
        response: http::Response,
        max_response_bytes: usize,
        deadline: RequestDeadline,
        permit: OwnedSemaphorePermit,
    ) -> Result<llsd::Value, Error> {
        let decode_guard = self.requests.track();
        let decode = spawn_blocking_with_permit(permit, move || {
            let _decode_guard = decode_guard;
            decode_capability_value_with_response_limit(&response.body, max_response_bytes)
        });
        let (result, permit) = self
            .race_request(deadline, async {
                time::timeout_at(deadline.at, decode)
                    .await
                    .map_err(|_| deadline.error())?
                    .map_err(|error| Error::Capability(format!("LLSD response decoder task failed: {error}")))
            })
            .await?;
        drop(permit);
        result
    }

    fn on_runtime<T, F>(&self, future: F) -> impl Future<Output = Result<T, Error>> + '_
    where
        T: Send + 'static,
        F: Future<Output = Result<T, Error>> + Send + 'static,
    {
        let requests = self.requests.clone();
        let runtime = self.runtime.clone();
        let session_closed = self.session_closed.subscribe();
        async move {
            let request = requests.track();
            if *session_closed.borrow() {
                drop(request);
                return Err(Error::CapabilitySessionClosed);
            }
            let task = runtime.spawn(
                async move {
                    let _request = request;
                    future.await
                }
                .in_current_span(),
            );
            let abort = AbortOnDrop(task.abort_handle());
            let result = task.await;
            drop(abort);
            match result {
                Ok(result) => result,
                Err(error) => {
                    if error.is_panic() {
                        return Err(Error::Capability("The capability request task failed.".into()));
                    }
                    if *session_closed.borrow() { Err(Error::CapabilitySessionClosed) } else { Err(Error::CapabilityRequestCanceled) }
                }
            }
        }
    }

    pub(crate) fn close_session(&self) {
        self.session_closed.send_replace(true);
    }

    pub(crate) async fn close_session_and_wait(&self) {
        self.close_session();
        self.wait_for_requests().await;
    }

    pub(crate) async fn wait_for_requests(&self) {
        let mut session_closed = self.session_closed.subscribe();
        if let Ok(closed) = session_closed.wait_for(|closed| *closed).await {
            drop(closed);
        }
        self.requests.wait_until_idle().await;
    }

    pub(crate) fn start_event_queue(
        &self,
        simulator_id: SimulatorId,
        source: SocketAddr,
        events: mpsc::Sender<EventEnvelope>,
        shutdown: watch::Receiver<bool>,
        coverage_losses: CoverageLossLedger,
    ) -> Result<EventQueueTask, SetupError> {
        self.start_event_queue_with_role(simulator_id, source, events, shutdown, coverage_losses, false)
    }

    pub(crate) fn start_primary_event_queue(
        &self,
        simulator_id: SimulatorId,
        source: SocketAddr,
        events: mpsc::Sender<EventEnvelope>,
        shutdown: watch::Receiver<bool>,
        coverage_losses: CoverageLossLedger,
    ) -> Result<EventQueueTask, SetupError> {
        self.start_event_queue_with_role(simulator_id, source, events, shutdown, coverage_losses, true)
    }

    pub(super) fn start_event_queue_with_role(
        &self,
        simulator_id: SimulatorId,
        source: SocketAddr,
        events: mpsc::Sender<EventEnvelope>,
        shutdown: watch::Receiver<bool>,
        coverage_losses: CoverageLossLedger,
        is_primary: bool,
    ) -> Result<EventQueueTask, SetupError> {
        let runtime = Handle::try_current().map_err(|_| SetupError::EventQueueRuntimeUnavailable)?;
        let http = self.http.clone().with_timeout(Some(EVENT_QUEUE_REQUEST_TIMEOUT));
        let seed = self.seed.as_ref().clone();
        let (role, primary) = event_queue::EventQueueRoleHandle::channel(is_primary);
        let session_closed = self.session_closed.subscribe();
        let (completed, completion) = oneshot::channel();
        let context = EventQueueContext {
            simulator_id,
            source,
            seed,
            http,
            events,
            budget: self.budget.clone(),
            coverage_losses,
            status: self.event_queue_status.clone(),
        };
        let stop = Stop::new(shutdown, session_closed);
        let task = runtime.spawn(
            async move {
                let exit = event_queue::run_event_queue(context, stop, primary).await;
                completed.send(exit).ok();
            }
            .in_current_span(),
        );
        Ok(EventQueueTask::new(task, completion, role))
    }
    fn record_response_status(&self, url: &CapabilityUrl, result: Result<reqwest::StatusCode, &Error>) {
        let Some(name) = url.name.as_ref() else { return };
        let status = match result {
            Ok(code) => CapabilityLastStatus::Http(code.as_u16()),
            Err(Error::CapabilityHttp(error)) => match error.status {
                Some(code) => CapabilityLastStatus::Http(code.as_u16()),
                None => CapabilityLastStatus::Transport(error.kind),
            },
            Err(Error::CapabilityRequestCanceled) => CapabilityLastStatus::Cancelled,
            Err(Error::CapabilitySessionClosed) => CapabilityLastStatus::SessionClosed,
            Err(_) => CapabilityLastStatus::Error,
        };
        self.grants.record_status(name, status);
    }
}

#[derive(Default)]
struct GrantCache {
    grants: Mutex<HashMap<String, Option<CapabilityUrl>>>,
    last_statuses: Mutex<HashMap<Arc<str>, CapabilityLastStatus>>,
    discovery: sync::Mutex<()>,
}

impl GrantCache {
    fn cached<N: AsRef<str>>(&self, names: &[N]) -> Result<HashMap<String, CapabilityUrl>, Vec<String>> {
        let grants = self.grants.lock().unwrap_or_else(PoisonError::into_inner);
        let missing = names.iter().map(AsRef::as_ref).filter(|name| !grants.contains_key(*name)).map(str::to_owned).collect::<Vec<_>>();
        if missing.is_empty() { Ok(granted(&grants, names)) } else { Err(missing) }
    }

    fn record(
        &self,
        names: &[String],
        missing: Vec<String>,
        discovered: &HashMap<String, CapabilityUrl>,
    ) -> HashMap<String, CapabilityUrl> {
        let mut grants = self.grants.lock().unwrap_or_else(PoisonError::into_inner);
        for name in missing {
            let url = discovered.get(&name).cloned().map(|url| url.with_name(&name));
            grants.insert(name, url);
        }
        granted(&grants, names)
    }

    fn record_status(&self, name: &Arc<str>, status: CapabilityLastStatus) {
        let mut statuses = self.last_statuses.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(slot) = statuses.get_mut(name.as_ref()) {
            *slot = status;
        } else {
            statuses.insert(Arc::clone(name), status);
        }
    }

    fn snapshot(&self) -> Vec<CapabilityRequestStatus> {
        let grants = self.grants.lock().unwrap_or_else(PoisonError::into_inner);
        let statuses = self.last_statuses.lock().unwrap_or_else(PoisonError::into_inner);
        let mut items = grants
            .iter()
            .map(|(name, url)| CapabilityRequestStatus {
                name: Arc::from(name.as_str()),
                granted: url.is_some(),
                last_status: statuses.get(name.as_str()).copied(),
            })
            .collect::<Vec<_>>();
        items.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        items
    }
}

struct RequestTracker {
    active: watch::Sender<usize>,
}

impl RequestTracker {
    fn new() -> Self {
        let (active, _) = watch::channel(0);
        Self { active }
    }

    fn track(&self) -> RequestGuard {
        self.active.send_modify(|active| *active = active.saturating_add(1));
        RequestGuard(self.active.clone())
    }

    async fn wait_until_idle(&self) {
        let mut active = self.active.subscribe();
        if let Ok(active) = active.wait_for(|active| *active == 0).await {
            drop(active);
        }
    }
}

impl Clone for RequestTracker {
    fn clone(&self) -> Self {
        Self { active: self.active.clone() }
    }
}

struct RequestGuard(watch::Sender<usize>);

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.0.send_modify(|active| *active = active.saturating_sub(1));
    }
}

struct AbortOnDrop(AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn granted<N: AsRef<str>>(grants: &HashMap<String, Option<CapabilityUrl>>, names: &[N]) -> HashMap<String, CapabilityUrl> {
    names.iter().map(AsRef::as_ref).filter_map(|name| grants.get(name).cloned().flatten().map(|url| (name.to_owned(), url))).collect()
}

fn validate_response_limit(max_response_bytes: usize) -> Result<(), Error> {
    if max_response_bytes == 0 || max_response_bytes > MAX_CAPABILITY_RESPONSE_BYTES {
        return Err(Error::Capability(format!("Capability response limit must be between 1 and {MAX_CAPABILITY_RESPONSE_BYTES} bytes.")));
    }
    Ok(())
}

fn encode_request<Request: Serialize + ?Sized>(payload: &Request) -> Result<Vec<u8>, Error> {
    xml::to_vec_with_max_bytes(payload, MAX_CAPABILITY_REQUEST_BODY_BYTES)
        .map_err(|error| Error::Capability(format!("Could not encode an LLSD request: {error}")))
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
