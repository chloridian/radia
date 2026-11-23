use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::{self, Future},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use futures::{FutureExt, channel::mpsc, pin_mut, sink::SinkExt, stream::StreamExt};
use llsd::{Map, Value};
use network::{
    Error as NetworkError, SimulatorId,
    capabilities::{Client, ErrorKind as HttpErrorKind},
};
use tokio::{runtime::Handle, sync::watch, time};
use tracing::Instrument;
use uuid::Uuid;
use world::{
    LocalAddress, MAX_MEDIA_DIMENSION, MAX_MEDIA_URL_BYTES, MAX_MEDIA_WHITELIST_BYTES, MAX_MEDIA_WHITELIST_ENTRIES, MAX_SURFACE_FACES,
    MediaControls, MediaEntry, ObjectBindingTarget, ObjectMediaError, ObjectMediaUpdate, OrderEvidence, UpdateSource,
};

const REQUEST_QUEUE_CAPACITY: usize = 64;
const RESULT_QUEUE_CAPACITY: usize = 16;
const RESPONSE_LIMIT_BYTES: usize = 256 * 1024;
const MAX_FETCH_ATTEMPTS: u8 = 6;
const MIN_REQUEST_SPACING: Duration = Duration::from_millis(50);
const INITIAL_RETRY_BACKOFF: Duration = Duration::from_millis(250);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ObjectMediaTarget {
    pub(super) simulator_id: SimulatorId,
    pub(super) binding: ObjectBindingTarget,
    pub(super) source: UpdateSource,
    pub(super) version: u32,
}

impl ObjectMediaTarget {
    fn key(self) -> MediaKey {
        MediaKey { simulator_id: self.simulator_id, address: self.binding.address(), object_id: self.binding.object_id() }
    }

    pub(super) fn is_newer_than(self, other: Self) -> bool {
        self.version > other.version
            || (self.version == other.version
                && matches!((self.source.order(), other.source.order()), (OrderEvidence::Ordered(a), OrderEvidence::Ordered(b)) if a > b))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct MediaKey {
    simulator_id: SimulatorId,
    address: LocalAddress,
    object_id: Uuid,
}

struct FetchRequest {
    target: ObjectMediaTarget,
    client: Client,
}

struct PendingMedia {
    active: ObjectMediaTarget,
    retry: Option<ObjectMediaTarget>,
}

impl PendingMedia {
    fn coalesce(&mut self, target: ObjectMediaTarget) {
        let latest = self.retry.unwrap_or(self.active);
        if target.binding != latest.binding || target.is_newer_than(latest) {
            self.retry = Some(target);
        }
    }
}

pub(super) struct ObjectMediaResult {
    pub(super) target: ObjectMediaTarget,
    pub(super) update: Result<ObjectMediaUpdate, FetchError>,
}

#[derive(Default)]
struct RetirementState {
    outstanding: HashMap<SimulatorId, usize>,
    retired: HashSet<SimulatorId>,
}

impl RetirementState {
    fn reserve(&mut self, simulator_id: SimulatorId) -> bool {
        if self.retired.contains(&simulator_id) {
            return false;
        }
        *self.outstanding.entry(simulator_id).or_default() += 1;
        true
    }

    fn retire(&mut self, simulator_id: SimulatorId) -> bool {
        if !self.outstanding.contains_key(&simulator_id) {
            return false;
        }
        self.retired.insert(simulator_id)
    }

    fn finish(&mut self, simulator_id: SimulatorId) {
        let Some(count) = self.outstanding.get_mut(&simulator_id) else { return };
        if *count > 1 {
            *count -= 1;
            return;
        }
        self.outstanding.remove(&simulator_id);
        self.retired.remove(&simulator_id);
    }

    fn is_retired(&self, simulator_id: SimulatorId) -> bool {
        self.retired.contains(&simulator_id)
    }
}

pub(super) struct ObjectMediaFetcher {
    requests: mpsc::Sender<FetchRequest>,
    results: mpsc::Receiver<ObjectMediaResult>,
    pending: HashMap<MediaKey, PendingMedia>,
    retirement: Arc<Mutex<RetirementState>>,
    retirement_generation: watch::Sender<u64>,
}

impl ObjectMediaFetcher {
    pub(super) fn new<F>(runtime: Handle, cancellation: F) -> Self
    where
        F: Future<Output = ()> + Clone + Send + 'static,
    {
        let (request_sender, request_receiver) = mpsc::channel(REQUEST_QUEUE_CAPACITY);
        let (result_sender, result_receiver) = mpsc::channel(RESULT_QUEUE_CAPACITY);
        let retirement = Arc::new(Mutex::new(RetirementState::default()));
        let (retirement_generation, retirement_updates) = watch::channel(0);
        drop(runtime.spawn(run(request_receiver, result_sender, retirement.clone(), retirement_updates, cancellation).in_current_span()));
        Self { requests: request_sender, results: result_receiver, pending: HashMap::new(), retirement, retirement_generation }
    }

    pub(super) fn enqueue(&mut self, target: ObjectMediaTarget, client: Client) -> bool {
        if self.retirement.lock().unwrap_or_else(PoisonError::into_inner).is_retired(target.simulator_id) {
            return false;
        }
        let key = target.key();
        if let Some(pending) = self.pending.get_mut(&key) {
            pending.coalesce(target);
            return true;
        }
        if !self.retirement.lock().unwrap_or_else(PoisonError::into_inner).reserve(target.simulator_id) {
            return false;
        }
        if self.requests.try_send(FetchRequest { target, client }).is_err() {
            self.retirement.lock().unwrap_or_else(PoisonError::into_inner).finish(target.simulator_id);
            return false;
        }
        self.pending.insert(key, PendingMedia { active: target, retry: None });
        true
    }

    pub(super) async fn next_result(&mut self) -> Option<ObjectMediaResult> {
        self.results.next().await
    }

    pub(super) fn finish(&mut self, result: &ObjectMediaResult) -> Option<ObjectMediaTarget> {
        self.pending.remove(&result.target.key()).and_then(|pending| pending.retry)
    }

    pub(super) fn forget_simulator(&mut self, simulator_id: SimulatorId) {
        self.pending.retain(|key, _| key.simulator_id != simulator_id);
        if self.retirement.lock().unwrap_or_else(PoisonError::into_inner).retire(simulator_id) {
            self.retirement_generation.send_modify(|generation| *generation = generation.saturating_add(1));
        }
    }
}

enum FetchOutcome {
    Cancelled,
    Retired,
    Complete(AttemptOutcome),
}

enum AttemptOutcome {
    Success(ObjectMediaUpdate),
    Retryable { error: FetchError, retry_after: Option<Duration>, throttled: bool },
    Fatal(FetchError),
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FetchError {
    Capability,
    HttpStatus(u16),
    Timeout,
    Malformed,
    ObjectMismatch,
    Invalid(ObjectMediaError),
    Transport,
    StaleVersion { requested: u32, received: u32 },
}

impl From<ParseError> for FetchError {
    fn from(error: ParseError) -> Self {
        match error {
            ParseError::Malformed => Self::Malformed,
            ParseError::ObjectMismatch => Self::ObjectMismatch,
            ParseError::Invalid(error) => Self::Invalid(error),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParseError {
    Malformed,
    ObjectMismatch,
    Invalid(ObjectMediaError),
}

pub(super) fn response_is_stale(response_version: u32, requested_version: u32) -> bool {
    response_version < requested_version
}

struct QueuedRequest {
    request: FetchRequest,
    attempts: u8,
}

struct SimulatorPacing {
    next_allowed: time::Instant,
    consecutive_throttles: u32,
}

impl SimulatorPacing {
    fn ready_at(&self) -> time::Instant {
        self.next_allowed
    }

    fn record_success(&mut self, now: time::Instant) {
        self.consecutive_throttles = 0;
        self.next_allowed = now + MIN_REQUEST_SPACING;
    }

    fn record_fatal(&mut self, now: time::Instant) {
        self.next_allowed = now + MIN_REQUEST_SPACING;
    }

    fn record_retryable(&mut self, now: time::Instant, retry_after: Option<Duration>, throttled: bool) {
        if throttled {
            self.consecutive_throttles = self.consecutive_throttles.saturating_add(1);
        }
        let backoff = exponential_backoff(self.consecutive_throttles.max(1));
        let delay = retry_after.unwrap_or(backoff).min(MAX_RETRY_BACKOFF).max(MIN_REQUEST_SPACING);
        self.next_allowed = now + delay;
    }
}

fn exponential_backoff(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(16);
    INITIAL_RETRY_BACKOFF.saturating_mul(1u32 << shift).min(MAX_RETRY_BACKOFF)
}

fn select_next_request(
    queued: &VecDeque<QueuedRequest>,
    pacing: &HashMap<SimulatorId, SimulatorPacing>,
    now: time::Instant,
) -> Option<(usize, time::Instant)> {
    let mut best: Option<(usize, time::Instant)> = None;
    for (index, item) in queued.iter().enumerate() {
        let ready_at = pacing.get(&item.request.target.simulator_id).map_or(now, SimulatorPacing::ready_at);
        if ready_at <= now {
            return Some((index, now));
        }
        if best.is_none_or(|(_, earliest)| ready_at < earliest) {
            best = Some((index, ready_at));
        }
    }
    best
}

fn drain_retired(
    queued: &mut VecDeque<QueuedRequest>,
    pacing: &mut HashMap<SimulatorId, SimulatorPacing>,
    retirement: &Mutex<RetirementState>,
) {
    let mut index = 0;
    while index < queued.len() {
        let simulator_id = queued[index].request.target.simulator_id;
        if is_retired(retirement, simulator_id) {
            queued.remove(index);
            pacing.remove(&simulator_id);
            finish_request(retirement, simulator_id);
        } else {
            index += 1;
        }
    }
    pacing.retain(|simulator_id, _| !is_retired(retirement, *simulator_id));
}

fn finish_all_queued(queued: &mut VecDeque<QueuedRequest>, retirement: &Mutex<RetirementState>) {
    while let Some(item) = queued.pop_front() {
        finish_request(retirement, item.request.target.simulator_id);
    }
}

async fn run<F>(
    mut requests: mpsc::Receiver<FetchRequest>,
    mut results: mpsc::Sender<ObjectMediaResult>,
    retirement: Arc<Mutex<RetirementState>>,
    mut retirement_updates: watch::Receiver<u64>,
    cancellation: F,
) where
    F: Future<Output = ()> + Clone + Send + 'static,
{
    let mut queued = VecDeque::new();
    let mut pacing = HashMap::<SimulatorId, SimulatorPacing>::new();
    let mut requests_closed = false;

    loop {
        while !requests_closed {
            match requests.try_recv() {
                Ok(request) => {
                    if is_retired(&retirement, request.target.simulator_id) {
                        pacing.remove(&request.target.simulator_id);
                        finish_request(&retirement, request.target.simulator_id);
                    } else {
                        queued.push_back(QueuedRequest { request, attempts: 0 });
                    }
                }
                Err(mpsc::TryRecvError::Closed) => {
                    requests_closed = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }

        drain_retired(&mut queued, &mut pacing, &retirement);

        let now = time::Instant::now();
        let Some((index, ready_at)) = select_next_request(&queued, &pacing, now) else {
            if requests_closed {
                return;
            }
            let cancelled = cancellation.clone().fuse();
            let retired_change = retirement_updates.changed().fuse();
            let next_request = requests.next().fuse();
            pin_mut!(cancelled, retired_change, next_request);
            futures::select! {
                _ = cancelled => return,
                _ = retired_change => continue,
                request = next_request => match request {
                    Some(request) => {
                        if is_retired(&retirement, request.target.simulator_id) {
                            pacing.remove(&request.target.simulator_id);
                            finish_request(&retirement, request.target.simulator_id);
                        } else {
                            queued.push_back(QueuedRequest { request, attempts: 0 });
                        }
                    }
                    None => return,
                },
            }
            continue;
        };

        if ready_at > now {
            let cancelled = cancellation.clone().fuse();
            let retired_change = retirement_updates.changed().fuse();
            let sleep = time::sleep_until(ready_at).fuse();
            let next_request =
                async { if requests_closed { future::pending::<Option<FetchRequest>>().await } else { requests.next().await } }.fuse();
            pin_mut!(cancelled, retired_change, sleep, next_request);
            futures::select! {
                _ = cancelled => {
                    finish_all_queued(&mut queued, &retirement);
                    return;
                }
                _ = retired_change => continue,
                request = next_request => {
                    match request {
                        Some(request) => {
                            if is_retired(&retirement, request.target.simulator_id) {
                                pacing.remove(&request.target.simulator_id);
                                finish_request(&retirement, request.target.simulator_id);
                            } else {
                                queued.push_back(QueuedRequest { request, attempts: 0 });
                            }
                        }
                        None => requests_closed = true,
                    }
                    continue;
                }
                _ = sleep => {}
            }
        }

        let Some(mut item) = queued.remove(index) else { continue };
        let simulator_id = item.request.target.simulator_id;
        if is_retired(&retirement, simulator_id) {
            pacing.remove(&simulator_id);
            finish_request(&retirement, simulator_id);
            continue;
        }

        let outcome = {
            let fetch = fetch(&item.request).fuse();
            pin_mut!(fetch);
            loop {
                let cancelled = cancellation.clone().fuse();
                let retired_change = retirement_updates.changed().fuse();
                pin_mut!(cancelled, retired_change);
                futures::select! {
                    _ = cancelled => break FetchOutcome::Cancelled,
                    changed = retired_change => {
                        if changed.is_err() || is_retired(&retirement, simulator_id) {
                            break FetchOutcome::Retired;
                        }
                    }
                    attempt = fetch => break FetchOutcome::Complete(attempt),
                }
            }
        };

        let now = time::Instant::now();
        let update = match outcome {
            FetchOutcome::Cancelled => {
                finish_request(&retirement, simulator_id);
                finish_all_queued(&mut queued, &retirement);
                return;
            }
            FetchOutcome::Retired => {
                pacing.remove(&simulator_id);
                finish_request(&retirement, simulator_id);
                continue;
            }
            FetchOutcome::Complete(AttemptOutcome::Cancelled) => {
                finish_request(&retirement, simulator_id);
                continue;
            }
            FetchOutcome::Complete(AttemptOutcome::Success(update)) => {
                pacing.entry(simulator_id).or_insert(SimulatorPacing { next_allowed: now, consecutive_throttles: 0 }).record_success(now);
                Ok(update)
            }
            FetchOutcome::Complete(AttemptOutcome::Fatal(error)) => {
                pacing.entry(simulator_id).or_insert(SimulatorPacing { next_allowed: now, consecutive_throttles: 0 }).record_fatal(now);
                Err(error)
            }
            FetchOutcome::Complete(AttemptOutcome::Retryable { error, retry_after, throttled }) => {
                pacing.entry(simulator_id).or_insert(SimulatorPacing { next_allowed: now, consecutive_throttles: 0 }).record_retryable(
                    now,
                    retry_after,
                    throttled,
                );
                item.attempts = item.attempts.saturating_add(1);
                if item.attempts < MAX_FETCH_ATTEMPTS {
                    queued.push_back(item);
                    continue;
                }
                Err(error)
            }
        };

        let result = ObjectMediaResult { target: item.request.target, update };
        let send = results.send(result).fuse();
        pin_mut!(send);
        let cancelled = cancellation.clone().fuse();
        pin_mut!(cancelled);
        if futures::select! { _ = cancelled => true, sent = send => sent.is_err() } {
            finish_request(&retirement, simulator_id);
            finish_all_queued(&mut queued, &retirement);
            return;
        }
        finish_request(&retirement, simulator_id);
    }
}

fn is_retired(retirement: &Mutex<RetirementState>, simulator_id: SimulatorId) -> bool {
    retirement.lock().unwrap_or_else(PoisonError::into_inner).is_retired(simulator_id)
}

fn finish_request(retirement: &Mutex<RetirementState>, simulator_id: SimulatorId) {
    retirement.lock().unwrap_or_else(PoisonError::into_inner).finish(simulator_id);
}

fn classify_network_error(error: NetworkError, fallback: FetchError) -> AttemptOutcome {
    match error {
        NetworkError::CapabilityRequestCanceled | NetworkError::CapabilitySessionClosed | NetworkError::SessionClosed => {
            AttemptOutcome::Cancelled
        }
        NetworkError::CapabilityHttp(error) => {
            if error.kind == HttpErrorKind::Cancelled {
                return AttemptOutcome::Cancelled;
            }
            if let Some(status) = error.status {
                let code = status.as_u16();
                let fetch_error = FetchError::HttpStatus(code);
                return if code == 429 || (500..600).contains(&code) {
                    AttemptOutcome::Retryable { error: fetch_error, retry_after: error.retry_after, throttled: code == 429 || code == 503 }
                } else {
                    AttemptOutcome::Fatal(fetch_error)
                };
            }
            match error.kind {
                HttpErrorKind::Timeout => {
                    AttemptOutcome::Retryable { error: FetchError::Timeout, retry_after: error.retry_after, throttled: false }
                }
                HttpErrorKind::InvalidResponse => AttemptOutcome::Fatal(FetchError::Malformed),
                _ => AttemptOutcome::Retryable { error: FetchError::Transport, retry_after: error.retry_after, throttled: false },
            }
        }
        _ => AttemptOutcome::Retryable { error: fallback, retry_after: None, throttled: false },
    }
}

async fn fetch(request: &FetchRequest) -> AttemptOutcome {
    let mut grants = match request.client.request_capabilities(&["ObjectMedia"]).await {
        Ok(grants) => grants,
        Err(error) => return classify_network_error(error, FetchError::Capability),
    };
    let Some(capability) = grants.remove("ObjectMedia") else {
        return AttemptOutcome::Retryable { error: FetchError::Capability, retry_after: None, throttled: false };
    };

    let mut payload = Map::new();
    payload.insert("verb".into(), Value::String("GET".into()));
    payload.insert("object_id".into(), Value::Uuid(request.target.binding.object_id()));
    let response = match request.client.post_with_response_limit(&capability, &payload, RESPONSE_LIMIT_BYTES).await {
        Ok(response) => response,
        Err(error) => return classify_network_error(error, FetchError::Transport),
    };
    match parse_response(&response, request.target.binding.object_id()) {
        Ok(update) if update.version() >= request.target.version => AttemptOutcome::Success(update),
        Ok(update) => AttemptOutcome::Retryable {
            error: FetchError::StaleVersion { requested: request.target.version, received: update.version() },
            retry_after: None,
            throttled: false,
        },
        Err(error) => AttemptOutcome::Fatal(FetchError::from(error)),
    }
}

fn parse_response(response: &Value, expected_id: Uuid) -> Result<ObjectMediaUpdate, ParseError> {
    let response = response.as_map().ok_or(ParseError::Malformed)?;
    if response.contains_key("error") {
        return Err(ParseError::Malformed);
    }
    if response.get("object_id").and_then(Value::as_uuid) != Some(expected_id) {
        return Err(ParseError::ObjectMismatch);
    }
    let version = response.get("object_media_version").and_then(Value::as_str).and_then(media_version).ok_or(ParseError::Malformed)?;
    let faces = response.get("object_media_data").and_then(Value::as_array).ok_or(ParseError::Malformed)?;
    if faces.len() > MAX_SURFACE_FACES {
        return Err(ParseError::Malformed);
    }
    let faces = faces
        .iter()
        .map(|face| match face {
            Value::Undefined => Ok(None),
            Value::Map(fields) => parse_entry(fields).map(Some),
            _ => Err(ParseError::Malformed),
        })
        .collect::<Result<Vec<_>, _>>()?;
    ObjectMediaUpdate::new(version, faces).map_err(ParseError::Invalid)
}

fn parse_entry(fields: &Map) -> Result<MediaEntry, ParseError> {
    let mut entry = MediaEntry::default();
    entry.alt_image_enable = bool_field(fields, "alt_image_enable", entry.alt_image_enable)?;
    entry.controls = match fields.get("controls") {
        None | Some(Value::Integer(0)) => MediaControls::Standard,
        Some(Value::Integer(1)) => MediaControls::Mini,
        Some(_) => return Err(ParseError::Malformed),
    };
    entry.current_url = owned_string(fields, "current_url", &entry.current_url)?;
    entry.home_url = owned_string(fields, "home_url", &entry.home_url)?;
    entry.auto_loop = bool_field(fields, "auto_loop", entry.auto_loop)?;
    entry.auto_play = bool_field(fields, "auto_play", entry.auto_play)?;
    entry.auto_scale = bool_field(fields, "auto_scale", entry.auto_scale)?;
    entry.auto_zoom = bool_field(fields, "auto_zoom", entry.auto_zoom)?;
    entry.first_click_interact = bool_field(fields, "first_click_interact", entry.first_click_interact)?;
    entry.width_pixels = dimension_field(fields, "width_pixels", entry.width_pixels)?;
    entry.height_pixels = dimension_field(fields, "height_pixels", entry.height_pixels)?;
    entry.whitelist_enable = bool_field(fields, "whitelist_enable", entry.whitelist_enable)?;
    entry.whitelist = whitelist_field(fields)?;
    entry.perms_interact = permission_field(fields, "perms_interact", entry.perms_interact)?;
    entry.perms_control = permission_field(fields, "perms_control", entry.perms_control)?;
    Ok(entry)
}

fn bool_field(fields: &Map, key: &str, default: bool) -> Result<bool, ParseError> {
    match fields.get(key) {
        None => Ok(default),
        Some(value) => value.as_bool().ok_or(ParseError::Malformed),
    }
}

fn string_field<'a>(fields: &'a Map, key: &str, default: &'a str) -> Result<&'a str, ParseError> {
    match fields.get(key) {
        None => Ok(default),
        Some(value) => value.as_str().ok_or(ParseError::Malformed),
    }
}

fn owned_string(fields: &Map, key: &str, default: &str) -> Result<String, ParseError> {
    let value = string_field(fields, key, default)?;
    if value.len() > MAX_MEDIA_URL_BYTES {
        return Err(ParseError::Invalid(ObjectMediaError::UrlTooLong));
    }
    Ok(value.to_owned())
}

fn dimension_field(fields: &Map, key: &str, default: u16) -> Result<u16, ParseError> {
    let Some(value) = fields.get(key) else { return Ok(default) };
    let value = value.as_integer().ok_or(ParseError::Malformed)?;
    let value = u16::try_from(value).map_err(|_| ParseError::Malformed)?;
    if value > MAX_MEDIA_DIMENSION {
        return Err(ParseError::Invalid(ObjectMediaError::DimensionTooLarge));
    }
    Ok(value)
}

fn permission_field(fields: &Map, key: &str, default: u8) -> Result<u8, ParseError> {
    let Some(value) = fields.get(key) else { return Ok(default) };
    u8::try_from(value.as_integer().ok_or(ParseError::Malformed)?).map_err(|_| ParseError::Malformed)
}

fn whitelist_field(fields: &Map) -> Result<Vec<String>, ParseError> {
    let Some(value) = fields.get("whitelist") else { return Ok(Vec::new()) };
    let values = value.as_array().ok_or(ParseError::Malformed)?;
    if values.len() > MAX_MEDIA_WHITELIST_ENTRIES {
        return Err(ParseError::Invalid(ObjectMediaError::WhitelistTooLarge));
    }
    let mut total_bytes = 0usize;
    values
        .iter()
        .map(|value| {
            let value = value.as_str().ok_or(ParseError::Malformed)?;
            total_bytes = total_bytes.saturating_add(value.len());
            if total_bytes > MAX_MEDIA_WHITELIST_BYTES {
                return Err(ParseError::Invalid(ObjectMediaError::WhitelistTooLarge));
            }
            Ok(value.to_owned())
        })
        .collect()
}

pub(super) fn media_version(value: &str) -> Option<u32> {
    let value = value.strip_prefix("x-mv:")?;
    let (version, agent_id) = value.split_once('/')?;
    Uuid::parse_str(agent_id).ok()?;
    version.parse().ok()
}

#[cfg(test)]
mod tests {
    use std::{
        assert_matches,
        future::pending,
        num::NonZeroU32,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use llsd::{Map, Value, xml};
    use network::{SimulatorId, capabilities::Client};
    use test_support::http_peer::LoopbackPeer;
    use tokio::{runtime::Handle, time};
    use url::Url;
    use uuid::Uuid;
    use world::{
        Appearance, FieldUpdate, Kind, MAX_MEDIA_URL_BYTES, MAX_MEDIA_WHITELIST_BYTES, MediaControls, ObjectMediaError, ObjectObservation,
        OrderEvidence, ParentUpdate, Shape, UpdateSource,
    };

    use super::{
        FetchError, MAX_FETCH_ATTEMPTS, ObjectMediaFetcher, ObjectMediaTarget, PendingMedia, RetirementState, media_version, parse_entry,
        parse_response, response_is_stale,
    };
    use crate::{
        session::{
            inbound::{ObjectBatch, ObjectMediaRequest, ObjectOperation},
            world::{SessionWorld, viewer_world_limits},
        },
        test_helpers::simulator_id,
    };

    async fn replacement_targets() -> (ObjectMediaTarget, ObjectMediaTarget) {
        let simulator_id = simulator_id();
        let mut world = SessionWorld::new(viewer_world_limits());
        let region = world.register_simulator(simulator_id, None).expect("register simulator");
        let local_id = NonZeroU32::new(7).expect("nonzero local ID");
        let other_local_id = NonZeroU32::new(8).expect("nonzero replacement local ID");
        let object_id = Uuid::from_u128(7);
        let observation = |order, local_id| ObjectObservation {
            shape: FieldUpdate::Set(Shape::Prim(Default::default())),
            appearance: FieldUpdate::Set(Appearance::Surface),
            ..ObjectObservation::new(
                UpdateSource::new(region, order, OrderEvidence::Ordered(u64::from(order)), 0),
                object_id,
                local_id,
                Kind::Object,
                ParentUpdate::Root,
            )
        };
        let apply = |world: &mut SessionWorld, operation| {
            world
                .apply_object_batch(
                    simulator_id,
                    ObjectBatch { region, region_handle: None, blocks: vec![Some(operation)], invalid_blocks: 0, invalid_fields: 0 },
                )
                .expect("registered simulator")
        };
        let request = |order, local_id| ObjectMediaRequest {
            source: UpdateSource::new(region, order, OrderEvidence::Ordered(u64::from(order)), 0),
            object_id,
            local_id,
            version: 5,
        };

        apply(&mut world, ObjectOperation::Observe(observation(1, local_id)));
        let active = world.prepare_object_media(simulator_id, request(2, local_id)).await.expect("current surface target");
        apply(&mut world, ObjectOperation::Observe(observation(3, other_local_id)));
        apply(&mut world, ObjectOperation::Observe(observation(4, local_id)));
        let replacement = world.prepare_object_media(simulator_id, request(5, local_id)).await.expect("current rebound target");
        (active, replacement)
    }

    fn response(object_id: Uuid) -> Value {
        let mut fields = Map::new();
        fields.insert("object_id".into(), Value::Uuid(object_id));
        fields.insert("object_media_version".into(), Value::String("x-mv:8/00000000-0000-0000-0000-000000000001".into()));
        fields.insert("object_media_data".into(), Value::Array(vec![Value::Map(Map::new())]));
        Value::Map(fields)
    }

    #[test]
    fn marker_requires_version_and_agent() {
        assert_eq!(media_version("x-mv:12/00000000-0000-0000-0000-000000000001"), Some(12));
        assert_eq!(media_version("https://example.test/media"), None);
        assert_eq!(media_version("x-mv:bad/00000000-0000-0000-0000-000000000001"), None);
    }

    #[test]
    fn retry_discards_older_media_version() {
        assert!(response_is_stale(7, 8));
        assert!(!response_is_stale(8, 8));
    }

    #[tokio::test]
    async fn replacement_target_is_retried() {
        let (active, replacement) = replacement_targets().await;
        assert_eq!(active.binding.entity(), replacement.binding.entity());
        assert_eq!(active.key(), replacement.key());
        assert_ne!(active.binding, replacement.binding);
        assert_eq!(active.version, replacement.version);

        let mut pending = PendingMedia { active, retry: None };
        pending.coalesce(replacement);

        assert_eq!(pending.retry, Some(replacement));
    }

    #[tokio::test]
    async fn older_marker_keeps_retry() {
        let (active, _) = replacement_targets().await;
        let latest = ObjectMediaTarget { version: 8, ..active };
        let older = ObjectMediaTarget { version: 7, ..active };
        let mut pending = PendingMedia { active, retry: Some(latest) };

        pending.coalesce(older);

        assert_eq!(pending.retry, Some(latest));
    }

    #[test]
    fn retirement_lives_with_requests() {
        let simulator_id = simulator_id();
        let mut retirement = RetirementState::default();

        assert!(!retirement.retire(simulator_id));
        assert!(retirement.reserve(simulator_id));
        assert!(retirement.reserve(simulator_id));
        assert!(retirement.retire(simulator_id));
        assert!(!retirement.reserve(simulator_id));

        retirement.finish(simulator_id);
        assert!(retirement.is_retired(simulator_id));
        retirement.finish(simulator_id);
        assert!(!retirement.is_retired(simulator_id));
        assert!(retirement.outstanding.is_empty());
        assert!(retirement.reserve(simulator_id));
    }

    #[test]
    fn response_checks_identity_and_faces() {
        let id = Uuid::from_u128(7);
        assert_eq!(parse_response(&response(id), id).unwrap().version(), 8);
        assert_eq!(FetchError::from(parse_response(&response(Uuid::from_u128(9)), id).unwrap_err()), FetchError::ObjectMismatch);
        let mut value = response(id);
        let Value::Map(fields) = &mut value else { panic!("test response is a map") };
        fields.insert("object_media_data".into(), Value::String("bad".into()));
        assert_eq!(FetchError::from(parse_response(&value, id).unwrap_err()), FetchError::Malformed);
    }

    #[test]
    fn media_controls_values() {
        for (value, expected) in [
            (Some(Value::Integer(0)), Some(MediaControls::Standard)),
            (Some(Value::Integer(1)), Some(MediaControls::Mini)),
            (None, Some(MediaControls::Standard)),
            (Some(Value::Integer(-1)), None),
            (Some(Value::Integer(2)), None),
            (Some(Value::String("MINI".into())), None),
        ] {
            let case = format!("{value:?}");
            let fields: Map = value.into_iter().map(|value| ("controls".into(), value)).collect();
            assert_eq!(parse_entry(&fields).ok().map(|entry| entry.controls), expected, "{case}");
        }
    }

    #[test]
    fn response_rejects_oversized_fields() {
        let id = Uuid::from_u128(7);
        let mut value = response(id);
        let Value::Map(fields) = &mut value else { panic!("test response is a map") };
        let Value::Array(faces) = fields.get_mut("object_media_data").unwrap() else { panic!("test faces are an array") };
        let mut media = Map::new();
        media.insert("current_url".into(), Value::String("x".repeat(MAX_MEDIA_URL_BYTES + 1)));
        faces[0] = Value::Map(media);
        assert_eq!(FetchError::from(parse_response(&value, id).unwrap_err()), FetchError::Invalid(ObjectMediaError::UrlTooLong));

        let mut value = response(id);
        let Value::Map(fields) = &mut value else { panic!("test response is a map") };
        let Value::Array(faces) = fields.get_mut("object_media_data").unwrap() else { panic!("test faces are an array") };
        let mut media = Map::new();
        media.insert("whitelist".into(), Value::Array(vec![Value::String("x".repeat(MAX_MEDIA_WHITELIST_BYTES + 1))]));
        faces[0] = Value::Map(media);
        assert_eq!(FetchError::from(parse_response(&value, id).unwrap_err()), FetchError::Invalid(ObjectMediaError::WhitelistTooLarge));
    }

    async fn media_target(simulator_id: SimulatorId, local_id: u32, object_id: Uuid, version: u32) -> ObjectMediaTarget {
        let mut world = SessionWorld::new(viewer_world_limits());
        let region = world.register_simulator(simulator_id, None).expect("register simulator");
        let local_id = NonZeroU32::new(local_id).expect("nonzero local ID");
        let observation = ObjectObservation {
            shape: FieldUpdate::Set(Shape::Prim(Default::default())),
            appearance: FieldUpdate::Set(Appearance::Surface),
            ..ObjectObservation::new(
                UpdateSource::new(region, 1, OrderEvidence::Ordered(1), 0),
                object_id,
                local_id,
                Kind::Object,
                ParentUpdate::Root,
            )
        };
        world
            .apply_object_batch(
                simulator_id,
                ObjectBatch {
                    region,
                    region_handle: None,
                    blocks: vec![Some(ObjectOperation::Observe(observation))],
                    invalid_blocks: 0,
                    invalid_fields: 0,
                },
            )
            .expect("registered simulator");
        world
            .prepare_object_media(
                simulator_id,
                ObjectMediaRequest { source: UpdateSource::new(region, 2, OrderEvidence::Ordered(2), 0), object_id, local_id, version },
            )
            .await
            .expect("current surface target")
    }

    fn grants_body(base: &Url) -> Vec<u8> {
        let mut grants = Map::new();
        grants.insert("ObjectMedia".into(), Value::String(base.join("object_media").expect("capability url").to_string()));
        xml::to_vec(&Value::Map(grants)).expect("encode grants")
    }

    fn media_body(object_id: Uuid, version: u32) -> Vec<u8> {
        let mut fields = Map::new();
        fields.insert("object_id".into(), Value::Uuid(object_id));
        fields.insert("object_media_version".into(), Value::String(format!("x-mv:{version}/00000000-0000-0000-0000-000000000001")));
        fields.insert("object_media_data".into(), Value::Array(vec![Value::Map(Map::new())]));
        xml::to_vec(&Value::Map(fields)).expect("encode media response")
    }

    #[tokio::test]
    async fn retry_after_honored() {
        for (status, retry_after_secs) in [(429u16, 1u64), (503u16, 1u64)] {
            let sim = simulator_id();
            let object_id = Uuid::from_u128(u128::from(status));
            let target = media_target(sim, 10, object_id, 4).await;
            let body = media_body(object_id, 4);
            let peer = LoopbackPeer::new_with_headers(3, move |index, _, base| match index {
                0 => (200, Vec::new(), grants_body(base)),
                1 => (status, vec![("Retry-After".into(), retry_after_secs.to_string())], Vec::new()),
                2 => (200, Vec::new(), body.clone()),
                _ => unreachable!(),
            });
            let client = Client::new(peer.url().join("seed").expect("seed url")).expect("client");
            let mut fetcher = ObjectMediaFetcher::new(Handle::current(), pending());
            let started = time::Instant::now();
            assert!(fetcher.enqueue(target, client));

            let result = fetcher.next_result().await.expect("media result");
            let elapsed = started.elapsed();
            assert!(
                elapsed >= Duration::from_secs(retry_after_secs),
                "status {status}: elapsed {elapsed:?} < retry-after {retry_after_secs}s"
            );
            assert_matches!(result.update, Ok(update) if update.version() == 4);
            assert_eq!(peer.finish().len(), 3);
        }
    }

    #[tokio::test]
    async fn throttle_isolates_simulators() {
        let sim_a = SimulatorId::new(1);
        let sim_b = SimulatorId::new(2);
        let obj_a1 = Uuid::from_u128(101);
        let obj_a2 = Uuid::from_u128(102);
        let obj_b1 = Uuid::from_u128(201);
        let target_a1 = media_target(sim_a, 11, obj_a1, 1).await;
        let target_a2 = media_target(sim_a, 12, obj_a2, 1).await;
        let target_b1 = media_target(sim_b, 21, obj_b1, 1).await;

        let body_a1 = media_body(obj_a1, 1);
        let body_a2 = media_body(obj_a2, 1);
        let body_b1 = media_body(obj_b1, 1);

        let a_hits = Arc::new(AtomicUsize::new(0));
        let a_hits_peer = Arc::clone(&a_hits);
        let peer_a = LoopbackPeer::new_with_headers(4, move |index, request, base| {
            let text = String::from_utf8_lossy(request);
            if index == 0 {
                return (200, Vec::new(), grants_body(base));
            }
            let hit = a_hits_peer.fetch_add(1, Ordering::SeqCst);
            if hit == 0 {
                return (429, vec![("Retry-After".into(), "1".into())], Vec::new());
            }
            if text.contains(&obj_a1.to_string()) { (200, Vec::new(), body_a1.clone()) } else { (200, Vec::new(), body_a2.clone()) }
        });
        let peer_b = LoopbackPeer::new_with_headers(2, move |index, _, base| match index {
            0 => (200, Vec::new(), grants_body(base)),
            1 => (200, Vec::new(), body_b1.clone()),
            _ => unreachable!(),
        });

        let client_a = Client::new(peer_a.url().join("seed").expect("seed url")).expect("client a");
        let client_b = Client::new(peer_b.url().join("seed").expect("seed url")).expect("client b");
        let mut fetcher = ObjectMediaFetcher::new(Handle::current(), pending());
        let started = time::Instant::now();

        assert!(fetcher.enqueue(target_a1, client_a.clone()));
        assert!(fetcher.enqueue(target_a2, client_a));
        assert!(fetcher.enqueue(target_b1, client_b));

        let completed = Arc::new(Mutex::new(Vec::new()));
        for _ in 0..3 {
            let result = fetcher.next_result().await.expect("media result");
            assert!(result.update.is_ok());
            completed.lock().expect("lock").push((result.target.simulator_id, started.elapsed()));
        }

        let completed = completed.lock().expect("lock");
        assert_eq!(completed[0].0, sim_b);
        assert!(completed[0].1 < Duration::from_secs(1), "simulator B must not wait on simulator A throttle");
        assert_eq!(completed[1].0, sim_a);
        assert!(completed[1].1 >= Duration::from_secs(1));
        assert_eq!(completed[2].0, sim_a);
        assert!(completed[2].1 >= Duration::from_secs(1));
        assert_eq!(peer_a.finish().len(), 4);
        assert_eq!(peer_b.finish().len(), 2);
    }

    #[tokio::test]
    async fn failure_preserves_http_status() {
        let sim = simulator_id();
        let object_404 = Uuid::from_u128(404);
        let target_404 = media_target(sim, 30, object_404, 1).await;
        let peer_404 = LoopbackPeer::new_with_headers(2, |index, _, base| match index {
            0 => (200, Vec::new(), grants_body(base)),
            1 => (404, Vec::new(), Vec::new()),
            _ => unreachable!(),
        });
        let client_404 = Client::new(peer_404.url().join("seed").expect("seed url")).expect("client 404");
        let mut fetcher = ObjectMediaFetcher::new(Handle::current(), pending());
        assert!(fetcher.enqueue(target_404, client_404));
        let result = fetcher.next_result().await.expect("404 result");
        assert_matches!(result.update, Err(FetchError::HttpStatus(404)));
        assert_eq!(peer_404.finish().len(), 2);

        let object_503 = Uuid::from_u128(503);
        let target_503 = media_target(sim, 31, object_503, 1).await;
        let peer_503 = LoopbackPeer::new_with_headers(usize::from(MAX_FETCH_ATTEMPTS) + 1, |index, _, base| match index {
            0 => (200, Vec::new(), grants_body(base)),
            _ => (503, vec![("Retry-After".into(), "0".into())], Vec::new()),
        });
        let client_503 = Client::new(peer_503.url().join("seed").expect("seed url")).expect("client 503");
        assert!(fetcher.enqueue(target_503, client_503));
        let result = fetcher.next_result().await.expect("503 result");
        assert_matches!(result.update, Err(FetchError::HttpStatus(503)));
        assert_eq!(peer_503.finish().len(), usize::from(MAX_FETCH_ATTEMPTS) + 1);
    }
}
