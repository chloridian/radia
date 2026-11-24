use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
    fmt,
    ops::{Bound, Range},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    time::{Duration, Instant},
};

use tokio::sync::watch;
use utils::{AssetId, Summary};

use crate::{
    Blob, Budget, Content, TextureEvents, TextureHandle, TextureSize, WeakTexture,
    budget::{Reservation, TEXTURE_WORK_BYTES, WORK_BYTES},
    cache::{AssetCache, AssetCacheError, AssetLookup, CacheWrite, read_work_bytes_for_request},
    content,
    content::{Finished, Prepared, StoredContent},
    mesh::{MAX_MESH_HEADER_BYTES, MeshLod, MeshLodRequest},
};

pub const MAX_ASSET_ROUTES: usize = 33;
pub const MAX_TEXTURE_DECODES: usize = 2;
const MAX_NON_TEXTURE_DECODES: usize = 2;
pub const MAX_NON_MESH_FETCHES: usize = 8;
const MAX_MESH_FETCHES: usize = 32;
const MAX_LEGACY_MATERIAL_FETCHES: usize = 50;
const FETCH_LIMITS: [usize; 3] = [MAX_MESH_FETCHES, MAX_LEGACY_MATERIAL_FETCHES, MAX_NON_MESH_FETCHES];
const DEMAND_DEADLINE: Duration = Duration::from_secs(60);
const NEGATIVE_TTL: Duration = Duration::from_secs(30);
const DISCARD_FAILURE_INTERVAL: Duration = Duration::from_secs(60);
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(300);
const CACHE_PRESSURE_RETRY: Duration = Duration::from_secs(1);
const MAX_ATTEMPTS: u8 = 3;
const DIRTY_QUEUE_CAPACITY: usize = 4096;
const MAX_DIRTY_PER_TURN: usize = 128;
const MAX_FALLBACK_SCAN_PER_TURN: usize = 128;
const MAX_DEADLINES_PER_TURN: usize = 128;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ScheduleKey {
    priority: Reverse<Priority>,
    importance: Reverse<u16>,
    ready: bool,
    used: Instant,
    key: LoadKey,
    route: Route,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum DeadlineKind {
    Demand,
    Retry,
    Recovery,
    CacheRetry,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DeadlineKey {
    at: Instant,
    key: LoadKey,
    route: Option<Route>,
    kind: DeadlineKind,
}

#[derive(Clone, Copy)]
enum RouteEligibility {
    Eligible { attempts: u8 },
    RetryAt { attempts: u8, failure: Failure, retry_at: Instant, expires: Instant },
    Failed { attempts: u8, failure: Failure, expires: Instant },
}

#[derive(Clone, Copy)]
enum CacheEligibility {
    Unchecked,
    Checked,
    RetryAt(Instant),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum MeshFetchMode {
    Range,
    FullFallback,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Kind {
    Texture,
    PbrMaterial,
    LegacyMaterial,
    Environment,
    Mesh,
}

impl Kind {
    const fn work_bytes(self) -> Option<usize> {
        match self {
            Self::Texture => Some(TEXTURE_WORK_BYTES),
            Self::Mesh => None,
            _ => Some(WORK_BYTES),
        }
    }

    pub const fn max_response_bytes(self) -> usize {
        match self {
            Self::Texture => 32 * 1024 * 1024,
            Self::Mesh => 32 * 1024 * 1024,
            Self::PbrMaterial | Self::LegacyMaterial | Self::Environment => 2 * 1024 * 1024,
        }
    }

    pub(crate) const fn max_cache_bytes(self) -> usize {
        match self {
            Self::LegacyMaterial => 4 * 1024 * 1024,
            _ => self.max_response_bytes(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Priority {
    Background,
    Visible,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Resource {
    pub id: AssetId,
    pub kind: Kind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Request {
    Texture { id: AssetId, size: TextureSize },
    PbrMaterial(AssetId),
    LegacyMaterial(AssetId),
    Environment(AssetId),
    MeshHeader(AssetId),
    MeshLod(MeshLodRequest),
}

impl Request {
    pub fn new(resource: Resource) -> Self {
        match resource.kind {
            Kind::Texture => Self::Texture { id: resource.id, size: TextureSize::default() },
            Kind::PbrMaterial => Self::PbrMaterial(resource.id),
            Kind::LegacyMaterial => Self::LegacyMaterial(resource.id),
            Kind::Environment => Self::Environment(resource.id),
            Kind::Mesh => Self::MeshHeader(resource.id),
        }
    }
    pub fn resource(self) -> Resource {
        match self {
            Self::Texture { id, .. } => Resource { id, kind: Kind::Texture },
            Self::PbrMaterial(id) => Resource { id, kind: Kind::PbrMaterial },
            Self::LegacyMaterial(id) => Resource { id, kind: Kind::LegacyMaterial },
            Self::Environment(id) => Resource { id, kind: Kind::Environment },
            Self::MeshHeader(id) => Resource { id, kind: Kind::Mesh },
            Self::MeshLod(block) => Resource { id: block.asset_id(), kind: Kind::Mesh },
        }
    }

    fn size(self) -> Option<TextureSize> {
        match self {
            Self::Texture { size, .. } => Some(size),
            _ => None,
        }
    }

    fn is_compatible_with(self, other: Self) -> bool {
        match (self, other) {
            (Self::Texture { id: left, .. }, Self::Texture { id: right, .. }) => left == right,
            _ => self == other,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct LoadKey {
    resource: Resource,
    lod: Option<MeshLod>,
}

#[derive(Debug)]
pub enum FetchResponse {
    Full(Vec<u8>),
    Partial { bytes: Vec<u8>, range: Range<u64>, total_length: Option<u64> },
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Route {
    store: u64,
    index: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Failure {
    MissingCapability,
    NotFound,
    Denied,
    HttpStatus { status: u16 },
    Network,
    MemoryPressure,
    TooLarge,
    Malformed,
    Unsupported,
    MissingLod(MeshLod),
    MalformedBlock,
    UnsupportedVersion(u32),
    RouteClosed,
    SessionClosed,
    Cancelled,
    Deadline,
    Transient { retry_after: Duration },
    FullAssetRequired,
}

#[derive(Clone, Debug)]
pub enum Status<C = Content> {
    Queued,
    Fetching,
    Decoding,
    Ready(C),
    Refining(C),
    RefinementFailed { content: C, failure: Failure },
    Failed { failure: Failure, retry_at: Option<Instant> },
    StaleSession,
}

impl<C> Status<C> {
    pub fn content(&self) -> Option<&C> {
        match self {
            Self::Ready(content) | Self::Refining(content) | Self::RefinementFailed { content, .. } => Some(content),
            _ => None,
        }
    }

    fn map<T>(self, content: impl FnOnce(C) -> T) -> Status<T> {
        match self {
            Self::Queued => Status::Queued,
            Self::Fetching => Status::Fetching,
            Self::Decoding => Status::Decoding,
            Self::Ready(value) => Status::Ready(content(value)),
            Self::Refining(value) => Status::Refining(content(value)),
            Self::RefinementFailed { content: value, failure } => Status::RefinementFailed { content: content(value), failure },
            Self::Failed { failure, retry_at } => Status::Failed { failure, retry_at },
            Self::StaleSession => Status::StaleSession,
        }
    }
}

impl Status<StoredContent> {
    fn upgrade(&self) -> Status {
        match self {
            Self::Ready(content) => content.upgrade().map_or(Status::Queued, Status::Ready),
            Self::Refining(content) => content.upgrade().map_or(Status::Queued, Status::Refining),
            Self::RefinementFailed { content, failure } => {
                content.upgrade().map_or(Status::Failed { failure: *failure, retry_at: None }, |content| Status::RefinementFailed {
                    content,
                    failure: *failure,
                })
            }
            Self::Queued => Status::Queued,
            Self::Fetching => Status::Fetching,
            Self::Decoding => Status::Decoding,
            Self::Failed { failure, retry_at } => Status::Failed { failure: *failure, retry_at: *retry_at },
            Self::StaleSession => Status::StaleSession,
        }
    }
}

impl<C: Clone> Status<C> {
    pub fn retaining(self, ready: Option<&C>) -> Self {
        match (ready, &self) {
            (Some(content), Self::Fetching | Self::Decoding | Self::Queued) => Self::Refining(content.clone()),
            (Some(content), Self::Failed { failure, .. }) => Self::RefinementFailed { content: content.clone(), failure: *failure },
            _ => self,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionError {
    Closed,
    Route,
    InvalidId,
    CacheProtection,
    ConflictingRequest,
}

struct Counter {
    count: AtomicUsize,
}

struct Lease {
    counter: Arc<Counter>,
    key: LoadKey,
    dirty: Arc<DirtyFlag>,
    signals: Arc<DirtySignals>,
    size: AtomicU16,
    priority: AtomicBool,
    importance: AtomicU16,
    _cache_protection: Option<storage::Protection>,
    _texture: Option<TextureHandle>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.counter.count.fetch_sub(1, Ordering::AcqRel);
        self.signals.mark(self.key, &self.dirty);
    }
}

const SIGNAL_ACTIVE: u8 = 0;
const SIGNAL_DIRTY: u8 = 1;
const SIGNAL_RETIRED: u8 = 2;

struct DirtyFlag(AtomicU8);

impl DirtyFlag {
    fn is_dirty(&self) -> bool {
        self.0.load(Ordering::Acquire) == SIGNAL_DIRTY
    }
}

struct DirtyNotification {
    key: LoadKey,
    dirty: Arc<DirtyFlag>,
}

struct DirtySignals {
    sender: SyncSender<DirtyNotification>,
    queued: AtomicUsize,
    overflow: AtomicBool,
    pending: AtomicUsize,
    change_generation: AtomicU64,
    wake: watch::Sender<()>,
}

impl DirtySignals {
    fn mark(&self, key: LoadKey, dirty: &Arc<DirtyFlag>) {
        self.pending.fetch_add(1, Ordering::AcqRel);
        if dirty.0.compare_exchange(SIGNAL_ACTIVE, SIGNAL_DIRTY, Ordering::AcqRel, Ordering::Acquire).is_err() {
            self.pending.fetch_sub(1, Ordering::AcqRel);
            return;
        }
        self.change_generation.fetch_add(1, Ordering::Release);
        self.queued.fetch_add(1, Ordering::AcqRel);
        match self.sender.try_send(DirtyNotification { key, dirty: Arc::clone(dirty) }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.queued.fetch_sub(1, Ordering::AcqRel);
                self.overflow.store(true, Ordering::Release);
            }
            Err(TrySendError::Disconnected(_)) => {
                self.queued.fetch_sub(1, Ordering::AcqRel);
                self.retire(dirty);
            }
        }
        self.wake.send_replace(());
    }

    fn take(&self, dirty: &DirtyFlag) -> bool {
        if dirty.0.compare_exchange(SIGNAL_DIRTY, SIGNAL_ACTIVE, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return false;
        }
        self.pending.fetch_sub(1, Ordering::AcqRel);
        true
    }

    fn retire(&self, dirty: &DirtyFlag) {
        loop {
            match dirty.0.load(Ordering::Acquire) {
                SIGNAL_ACTIVE => {
                    if dirty.0.compare_exchange(SIGNAL_ACTIVE, SIGNAL_RETIRED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                        return;
                    }
                }
                SIGNAL_DIRTY => {
                    if dirty.0.compare_exchange(SIGNAL_DIRTY, SIGNAL_RETIRED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                        self.pending.fetch_sub(1, Ordering::AcqRel);
                        return;
                    }
                }
                SIGNAL_RETIRED => return,
                _ => unreachable!("dirty signal state is valid"),
            }
        }
    }
}

impl Lease {
    fn size(&self) -> Option<TextureSize> {
        let edge = self.size.load(Ordering::Acquire);
        (edge != 0).then(|| TextureSize::new(u32::from(edge)))
    }

    fn priority(&self) -> Priority {
        if self.priority.load(Ordering::Acquire) { Priority::Visible } else { Priority::Background }
    }
}

#[derive(Clone)]
pub struct Demand {
    _lease: Arc<Lease>,
    status: watch::Receiver<Status<StoredContent>>,
}

impl Demand {
    pub fn status(&self) -> Status {
        self.status.borrow().upgrade()
    }

    pub async fn changed(&mut self) -> Status {
        if self.status.changed().await.is_err() {
            return self.status();
        }
        self.status.borrow_and_update().upgrade()
    }

    pub fn update_importance(&self, importance: u16) {
        self._lease.importance.store(importance, Ordering::Release);
        self._lease.signals.mark(self._lease.key, &self._lease.dirty);
    }

    pub fn update_priority(&self, priority: Priority) {
        self._lease.priority.store(priority == Priority::Visible, Ordering::Release);
        self._lease.signals.mark(self._lease.key, &self._lease.dirty);
    }

    pub fn update_size(&self, size: TextureSize) {
        let size = size.edge() as u16;
        if self._lease.size.try_update(Ordering::AcqRel, Ordering::Acquire, |current| (current != 0).then_some(size)).is_ok() {
            self._lease.signals.mark(self._lease.key, &self._lease.dirty);
        }
    }
}

struct RouteState {
    counter: Arc<Counter>,
    status: watch::Sender<Status<StoredContent>>,
    eligibility: RouteEligibility,
    deadline: Instant,
    demand_deadline_indexed: bool,
    retry_deadline_indexed: bool,
    recovery_deadline_indexed: bool,
    leases: Vec<Weak<Lease>>,
}

impl RouteState {
    fn count(&self) -> usize {
        self.counter.count.load(Ordering::Acquire)
    }

    fn restart(&mut self, deadlines: &mut BTreeSet<DeadlineKey>, key: LoadKey, route: Route, now: Instant) {
        remove_demand_deadline(deadlines, key, route, self);
        remove_failure_deadlines(deadlines, key, route, self);
        self.eligibility = RouteEligibility::Eligible { attempts: 0 };
        self.deadline = now + DEMAND_DEADLINE;
    }

    fn queued_status(&self) -> Status {
        match self.eligibility {
            RouteEligibility::Eligible { .. } => Status::Queued,
            RouteEligibility::RetryAt { failure, retry_at, .. } => Status::Failed { failure, retry_at: Some(retry_at) },
            RouteEligibility::Failed { failure, .. } => Status::Failed { failure, retry_at: None },
        }
    }

    fn fail(
        &mut self,
        deadlines: &mut BTreeSet<DeadlineKey>,
        key: LoadKey,
        route: Route,
        failure: Failure,
        retry_at: Option<Instant>,
        now: Instant,
        ready: Option<&Content>,
    ) {
        let attempts = self.attempts();
        remove_failure_deadlines(deadlines, key, route, self);
        let expires = now
            + match failure {
                Failure::Transient { retry_after } => retry_after.clamp(NEGATIVE_TTL, MAX_RETRY_AFTER),
                _ => NEGATIVE_TTL,
            };
        self.eligibility = match retry_at {
            Some(retry_at) => RouteEligibility::RetryAt { attempts, failure, retry_at, expires },
            None => RouteEligibility::Failed { attempts, failure, expires },
        };
        self.publish_status(Status::Failed { failure, retry_at }.retaining(ready));
    }

    fn attempts(&self) -> u8 {
        match self.eligibility {
            RouteEligibility::Eligible { attempts }
            | RouteEligibility::RetryAt { attempts, .. }
            | RouteEligibility::Failed { attempts, .. } => attempts,
        }
    }

    fn retry_at(&self) -> Option<Instant> {
        match self.eligibility {
            RouteEligibility::RetryAt { retry_at, .. } => Some(retry_at),
            RouteEligibility::Eligible { .. } | RouteEligibility::Failed { .. } => None,
        }
    }

    fn recovery_at(&self) -> Option<Instant> {
        match self.eligibility {
            RouteEligibility::RetryAt { failure: Failure::Transient { .. }, expires, .. }
            | RouteEligibility::Failed { failure: Failure::Transient { .. }, expires, .. } => Some(expires),
            RouteEligibility::Eligible { .. } | RouteEligibility::RetryAt { .. } | RouteEligibility::Failed { .. } => None,
        }
    }

    fn begin_attempt(&mut self, deadlines: &mut BTreeSet<DeadlineKey>, key: LoadKey, route: Route) {
        let attempts = self.attempts().saturating_add(1);
        remove_failure_deadlines(deadlines, key, route, self);
        self.eligibility = RouteEligibility::Eligible { attempts };
    }

    fn refund_attempt(&mut self) {
        self.eligibility = RouteEligibility::Eligible { attempts: self.attempts().saturating_sub(1) };
    }

    fn publish_status(&self, status: Status) {
        self.status.send_replace(status.map(|content| content.downgrade()));
    }
}

struct WorkLife {
    key: LoadKey,
    dirty: Arc<DirtyFlag>,
    signals: Arc<DirtySignals>,
    transport_pending: AtomicBool,
}

impl Drop for WorkLife {
    fn drop(&mut self) {
        self.signals.mark(self.key, &self.dirty);
    }
}

enum LoadPhase {
    Idle,
    Queued,
    Running { generation: u64, route: Route, life: Weak<WorkLife>, cancelled: watch::Sender<bool>, stage: Stage },
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Stage {
    ReadingCache,
    Fetching,
    Cached,
    Decoding,
}

struct AssetEntry {
    request: Request,
    views: BTreeMap<Route, RouteState>,
    phase: LoadPhase,
    dirty: Arc<DirtyFlag>,
    candidate: Option<ScheduleKey>,
    used: Instant,
    ready: Option<StoredContent>,
    texture: Option<WeakTexture>,
    target: Option<TextureSize>,
    priority: Priority,
    importance: u16,
    cache_eligibility: CacheEligibility,
    indexed_cache_retry: Option<Instant>,
    mesh_fetch_mode: MeshFetchMode,
}

impl AssetEntry {
    fn ready(&self) -> Option<Content> {
        self.ready.as_ref().and_then(StoredContent::upgrade)
    }
    fn texture(&self) -> Option<TextureHandle> {
        self.texture.as_ref().and_then(WeakTexture::upgrade)
    }
    fn publish_content(&self, content: Content) {
        let status = if content_satisfies(&content, self.requested_size()) { Status::Ready(content) } else { Status::Refining(content) };
        self.broadcast_status(status);
    }
    fn effective_priority(&self) -> Priority {
        self.views
            .values()
            .flat_map(|view| &view.leases)
            .filter_map(Weak::upgrade)
            .map(|lease| lease.priority())
            .max()
            .unwrap_or(Priority::Background)
    }
    fn effective_importance(&self) -> u16 {
        self.views
            .values()
            .flat_map(|view| &view.leases)
            .filter_map(Weak::upgrade)
            .map(|lease| lease.importance.load(Ordering::Acquire))
            .max()
            .unwrap_or(0)
    }
    fn requested_size(&self) -> Option<TextureSize> {
        self.views.values().flat_map(|view| &view.leases).filter_map(Weak::upgrade).filter_map(|lease| lease.size()).max()
    }

    fn has_active_demand(&self) -> bool {
        self.views.values().any(|view| view.count() > 0)
    }
    fn has_live_demand(&self, now: Instant) -> bool {
        self.views.values().any(|view| view.count() > 0 && now < view.deadline)
    }
    fn broadcast_status(&self, status: Status) {
        let ready = self.ready();
        for view in self.views.values().filter(|view| view.count() > 0) {
            view.publish_status(status.clone().retaining(ready.as_ref()));
        }
    }
}

struct Job {
    resource: Resource,
    key: LoadKey,
    request: Request,
    route: Route,
    generation: u64,
    _life: Arc<WorkLife>,
    reservation: Reservation,
    cancelled: watch::Receiver<bool>,
    size: TextureSize,
    mesh_fetch_mode: MeshFetchMode,
}

impl fmt::Debug for Job {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Job").field("resource", &self.resource).field("route", &self.route).finish_non_exhaustive()
    }
}

impl Job {
    fn is_cancelled(&self) -> bool {
        *self.cancelled.borrow()
    }
    fn cancelled(&self) -> impl Future<Output = ()> + Send + use<> {
        let mut cancelled = self.cancelled.clone();
        async move {
            if *cancelled.borrow() {
                return;
            }
            cancelled.changed().await.unwrap_or(());
        }
    }

    fn fail(self, failure: Failure) -> LoadCompletion {
        let failure = if self.is_cancelled() { Failure::Cancelled } else { failure };
        LoadCompletion { job: self, result: Err(failure), origin: DecodeOrigin::Network, decode_duration: None }
    }
}

#[derive(Debug)]
pub struct FetchFailure {
    job: Box<Job>,
    failure: Failure,
}

impl FetchFailure {
    fn new(job: Job, failure: Failure) -> Self {
        let failure = if job.is_cancelled() { Failure::Cancelled } else { failure };
        Self { job: Box::new(job), failure }
    }

    pub fn failure(&self) -> Failure {
        self.failure
    }
}

impl From<FetchFailure> for LoadCompletion {
    fn from(fetch: FetchFailure) -> Self {
        Self { job: *fetch.job, result: Err(fetch.failure), origin: DecodeOrigin::Network, decode_duration: None }
    }
}

#[derive(Debug)]
pub struct FetchJob {
    job: Job,
}

impl FetchJob {
    pub fn resource(&self) -> Resource {
        self.job.resource
    }
    pub fn request(&self) -> Request {
        self.job.request
    }
    pub fn route(&self) -> Route {
        self.job.route
    }
    pub fn response_limit(&self) -> usize {
        self.job.reservation.bytes()
    }
    pub fn force_full_mesh(&self) -> bool {
        self.job.mesh_fetch_mode == MeshFetchMode::FullFallback
    }
    pub fn is_cancelled(&self) -> bool {
        self.job.is_cancelled()
    }
    pub fn cancelled(&self) -> impl Future<Output = ()> + Send + use<> {
        self.job.cancelled()
    }
    pub fn complete(self, response: Result<Vec<u8>, Failure>) -> Result<DecodeInput, FetchFailure> {
        self.complete_response(response.map(FetchResponse::Full))
    }

    pub fn complete_response(self, response: Result<FetchResponse, Failure>) -> Result<DecodeInput, FetchFailure> {
        let Self { mut job } = self;
        job._life.transport_pending.store(false, Ordering::Release);
        job._life.signals.mark(job.key, &job._life.dirty);
        let response = match response {
            Ok(response) if !job.is_cancelled() => response,
            Ok(_) => return Err(FetchFailure::new(job, Failure::Cancelled)),
            Err(failure) => return Err(FetchFailure::new(job, failure)),
        };
        let (length, capacity) = match &response {
            FetchResponse::Full(bytes) | FetchResponse::Partial { bytes, .. } => (bytes.len(), bytes.capacity()),
        };
        if length > job.reservation.bytes() || capacity > job.resource.kind.max_response_bytes() {
            return Err(FetchFailure::new(job, Failure::TooLarge));
        }
        if !job.reservation.resize(capacity) {
            return Err(FetchFailure::new(job, Failure::Transient { retry_after: Duration::from_secs(1) }));
        }
        Ok(DecodeInput { job, source: DecodeSource::Response(response) })
    }
}

#[derive(Debug)]
pub struct CacheReadJob {
    job: Job,
    cache: Arc<AssetCache>,
}

pub struct CacheReadCompletion {
    job: Job,
    result: Result<AssetLookup, Failure>,
}

impl CacheReadJob {
    pub fn run(self) -> CacheReadCompletion {
        self.run_lookup(|cache, request| cache.lookup(request))
    }

    fn run_lookup(self, lookup: impl FnOnce(&AssetCache, Request) -> AssetLookup) -> CacheReadCompletion {
        let result = if self.job.is_cancelled() {
            Err(Failure::Cancelled)
        } else {
            let lookup = lookup(&self.cache, self.job.request);
            if self.job.is_cancelled() { Err(Failure::Cancelled) } else { Ok(lookup) }
        };
        CacheReadCompletion { job: self.job, result }
    }
}

#[derive(Debug)]
pub enum LoadJob {
    Fetch(FetchJob),
    Decode(DecodeInput),
    ReadCache(CacheReadJob),
}

impl LoadJob {
    pub fn fail(self, failure: Failure) -> LoadCompletion {
        match self {
            Self::Fetch(job) => job.job.fail(failure),
            Self::Decode(input) => input.fail(failure),
            Self::ReadCache(job) => {
                LoadCompletion { job: job.job, result: Err(failure), origin: DecodeOrigin::Disk, decode_duration: None }
            }
        }
    }
}

enum DecodeSource {
    Response(FetchResponse),
    Disk { bytes: Vec<u8>, source_sha256: [u8; 32], cache: Option<Arc<AssetCache>> },
    Cached(Blob),
}

pub struct DecodeInput {
    source: DecodeSource,
    job: Job,
}

impl fmt::Debug for DecodeInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let source = match &self.source {
            DecodeSource::Response(_) => "response",
            DecodeSource::Disk { .. } => "disk",
            DecodeSource::Cached(_) => "cached",
        };
        f.debug_struct("DecodeInput")
            .field("resource", &self.job.resource)
            .field("route", &self.job.route)
            .field("source", &source)
            .finish()
    }
}

impl DecodeInput {
    fn origin(&self) -> DecodeOrigin {
        match &self.source {
            DecodeSource::Response(_) => DecodeOrigin::Network,
            DecodeSource::Disk { .. } => DecodeOrigin::Disk,
            DecodeSource::Cached(_) => DecodeOrigin::Memory,
        }
    }

    pub fn resource(&self) -> Resource {
        self.job.resource
    }
    pub fn is_cancelled(&self) -> bool {
        self.job.is_cancelled()
    }
    pub fn fail(self, failure: Failure) -> LoadCompletion {
        let failure = if self.job.is_cancelled() { Failure::Cancelled } else { failure };
        let origin = self.origin();
        LoadCompletion { job: self.job, result: Err(failure), origin, decode_duration: None }
    }
}

#[derive(Debug)]
pub struct DecodeJob {
    input: DecodeInput,
    persist_prepared: bool,
}

impl DecodeJob {
    pub fn fail(self, failure: Failure) -> LoadCompletion {
        self.input.fail(failure)
    }

    pub fn run(self) -> LoadCompletion {
        let DecodeJob { input: DecodeInput { mut job, source, .. }, persist_prepared } = self;
        let cancelled = &job.cancelled;
        let started = Instant::now();
        let (result, origin) = match source {
            DecodeSource::Response(response) => (
                content::prepare_cancellable(job.request, job.size, response, persist_prepared, &mut job.reservation, || {
                    *cancelled.borrow()
                }),
                DecodeOrigin::Network,
            ),
            DecodeSource::Disk { bytes, source_sha256, cache } => (
                content::prepare_disk(job.request, job.size, bytes, source_sha256, cache.as_deref(), &mut job.reservation, || {
                    *cancelled.borrow()
                }),
                DecodeOrigin::Disk,
            ),
            DecodeSource::Cached(source) => (content::prepare_cached(job.size, &source, || *cancelled.borrow()), DecodeOrigin::Memory),
        };
        LoadCompletion { job, result, origin, decode_duration: Some(started.elapsed()) }
    }

    pub fn resource(&self) -> Resource {
        self.input.resource()
    }

    pub fn is_cancelled(&self) -> bool {
        self.input.is_cancelled()
    }
}

pub struct LoadCompletion {
    result: Result<Prepared, Failure>,
    job: Job,
    origin: DecodeOrigin,
    decode_duration: Option<Duration>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DecodeOrigin {
    Network,
    Disk,
    Memory,
}

impl fmt::Debug for LoadCompletion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadCompletion")
            .field("resource", &self.job.resource)
            .field("route", &self.job.route)
            .field("failure", &self.result.as_ref().err())
            .finish()
    }
}

impl LoadCompletion {
    pub fn failure(&self) -> Option<Failure> {
        self.result.as_ref().err().copied()
    }
}

pub struct Store {
    id: u64,
    sequence: u64,
    routes: BTreeSet<Route>,
    entries: BTreeMap<LoadKey, AssetEntry>,
    candidates: Vec<ScheduleKey>,
    candidate_generation: u64,
    candidate_queues: [BTreeSet<ScheduleKey>; 3],
    deadlines: BTreeSet<DeadlineKey>,
    dirty_signals: Arc<DirtySignals>,
    dirty_receiver: Receiver<DirtyNotification>,
    fallback_scan_active: bool,
    fallback_scan_cursor: Option<LoadKey>,
    candidates_dirty: bool,
    budget: Budget,
    closed: bool,
    maintained_at: Option<Instant>,
    maintained_generation: u64,
    maintenance_pending: bool,
    running_fetches: [usize; 3],
    running_cache_reads: usize,
    running_non_mesh_cache_reads: usize,
    pending_non_mesh_cache_decodes: usize,
    running_texture_decodes: usize,
    running_other_decodes: usize,
    textures: TextureEvents,
    cache: Option<Arc<AssetCache>>,
    discard_failures: Summary<DiscardFailures>,
}

#[derive(Default)]
struct DiscardFailures {
    failed: u64,
    last_error: Option<AssetCacheError>,
}

impl DiscardFailures {
    fn report(self) {
        let Some(last_error) = self.last_error else { return };
        tracing::warn!(failed = self.failed, %last_error, "corrupt asset cache entries could not be discarded");
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        self.close();
    }
}

impl Store {
    pub fn texture_events(&self) -> TextureEvents {
        self.textures.clone()
    }

    pub fn new(budget: Budget) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1)).expect("asset session generation exhausted");
        let (sender, dirty_receiver) = mpsc::sync_channel(DIRTY_QUEUE_CAPACITY);
        let dirty_signals = Arc::new(DirtySignals {
            sender,
            queued: AtomicUsize::new(0),
            overflow: AtomicBool::new(false),
            pending: AtomicUsize::new(0),
            change_generation: AtomicU64::new(0),
            wake: budget.wake(),
        });
        Self {
            id,
            sequence: 0,
            routes: BTreeSet::new(),
            entries: BTreeMap::new(),
            candidates: Vec::with_capacity(FETCH_LIMITS.iter().sum()),
            candidate_generation: 0,
            candidate_queues: Default::default(),
            deadlines: BTreeSet::new(),
            dirty_signals,
            dirty_receiver,
            fallback_scan_active: false,
            fallback_scan_cursor: None,
            candidates_dirty: true,
            budget,
            closed: false,
            maintained_at: None,
            maintained_generation: 0,
            maintenance_pending: false,
            running_fetches: [0; 3],
            running_cache_reads: 0,
            running_non_mesh_cache_reads: 0,
            pending_non_mesh_cache_decodes: 0,
            running_texture_decodes: 0,
            running_other_decodes: 0,
            textures: TextureEvents::default(),
            cache: None,
            discard_failures: Summary::new(DISCARD_FAILURE_INTERVAL),
        }
    }

    pub fn with_cache(budget: Budget, cache: Arc<AssetCache>) -> Self {
        let mut store = Self::new(budget);
        store.cache = Some(cache);
        store
    }

    pub fn wake(&self) -> watch::Receiver<()> {
        self.budget.wake().subscribe()
    }

    pub fn priority(&self, input: &DecodeInput) -> Priority {
        self.entries.get(&input.job.key).map_or(Priority::Background, AssetEntry::effective_priority)
    }

    pub fn importance(&self, input: &DecodeInput) -> u16 {
        self.entries.get(&input.job.key).map_or(0, AssetEntry::effective_importance)
    }

    pub fn register_route(&mut self) -> Result<Route, AdmissionError> {
        if self.closed {
            return Err(AdmissionError::Closed);
        }
        if self.routes.len() >= MAX_ASSET_ROUTES {
            return Err(AdmissionError::Route);
        }
        self.sequence = self.sequence.checked_add(1).ok_or(AdmissionError::Closed)?;
        let route = Route { store: self.id, index: self.sequence };
        self.routes.insert(route);
        Ok(route)
    }

    pub fn retire_route(&mut self, route: Route) {
        self.routes.remove(&route);
        let (entries, deadlines, dirty_signals) = (&mut self.entries, &mut self.deadlines, &self.dirty_signals);
        for (key, entry) in entries {
            let mut changed = false;
            if let Some(mut view) = entry.views.remove(&route) {
                Self::remove_route_deadlines(deadlines, *key, route, &mut view);
                view.publish_status(Status::Failed { failure: Failure::RouteClosed, retry_at: None });
                changed = true;
            }
            if let LoadPhase::Running { route: running, life, cancelled, .. } = &entry.phase
                && (!entry.has_active_demand()
                    || *running == route && life.upgrade().is_some_and(|life| life.transport_pending.load(Ordering::Acquire)))
            {
                cancelled.send_replace(true);
                changed = true;
            }
            if changed {
                dirty_signals.mark(*key, &entry.dirty);
            }
        }
    }

    pub fn request(&mut self, route: Route, request: Request, priority: Priority, now: Instant) -> Result<Demand, AdmissionError> {
        self.request_with_importance(route, request, priority, 0, now)
    }

    pub fn request_with_importance(
        &mut self,
        route: Route,
        request: Request,
        priority: Priority,
        importance: u16,
        now: Instant,
    ) -> Result<Demand, AdmissionError> {
        let resource = request.resource();
        let lod = match request {
            Request::MeshLod(block) => Some(block.lod()),
            Request::Texture { .. }
            | Request::PbrMaterial(_)
            | Request::LegacyMaterial(_)
            | Request::Environment(_)
            | Request::MeshHeader(_) => None,
        };
        let key = LoadKey { resource, lod };
        if self.closed {
            return Err(AdmissionError::Closed);
        }
        if !self.routes.contains(&route) {
            return Err(AdmissionError::Route);
        }
        if resource.id.uuid().is_nil() {
            return Err(AdmissionError::InvalidId);
        }
        let cache_protection =
            self.cache.as_ref().map(|cache| cache.protect(request).map_err(|_| AdmissionError::CacheProtection)).transpose()?;
        let entry = self.entries.entry(key).or_insert_with(|| AssetEntry {
            request,
            views: BTreeMap::new(),
            phase: LoadPhase::Queued,
            dirty: Arc::new(DirtyFlag(AtomicU8::new(SIGNAL_ACTIVE))),
            candidate: None,
            used: now,
            ready: None,
            texture: None,
            target: None,
            priority,
            importance,
            cache_eligibility: CacheEligibility::Unchecked,
            indexed_cache_retry: None,
            mesh_fetch_mode: MeshFetchMode::Range,
        });
        if !entry.request.is_compatible_with(request) {
            return Err(AdmissionError::ConflictingRequest);
        }
        entry.used = now;
        let texture = if resource.kind == Kind::Texture {
            let texture = entry.texture().unwrap_or_else(|| {
                entry.ready = None;
                match &entry.phase {
                    LoadPhase::Running { cancelled, .. } => {
                        cancelled.send_replace(true);
                    }
                    LoadPhase::Idle | LoadPhase::Queued => entry.phase = LoadPhase::Queued,
                }
                TextureHandle::new(resource.id, self.budget.clone(), self.textures.clone())
            });
            entry.texture = Some(texture.downgrade());
            Some(texture)
        } else {
            None
        };
        let ready = entry.ready();
        let view = entry.views.entry(route).or_insert_with(|| {
            let (status, _) = watch::channel(Status::Queued);
            RouteState {
                counter: Arc::new(Counter { count: AtomicUsize::new(0) }),
                status,
                eligibility: RouteEligibility::Eligible { attempts: 0 },
                deadline: now + DEMAND_DEADLINE,
                demand_deadline_indexed: false,
                retry_deadline_indexed: false,
                recovery_deadline_indexed: false,
                leases: Vec::new(),
            }
        });
        let expired = match view.eligibility {
            RouteEligibility::Eligible { .. } => true,
            RouteEligibility::RetryAt { expires, .. } | RouteEligibility::Failed { expires, .. } => expires <= now,
        };
        if view.count() == 0 && expired {
            view.restart(&mut self.deadlines, key, route, now);
        }
        let status = match &entry.phase {
            LoadPhase::Idle => ready.as_ref().map_or(Status::Queued, |content| Status::Ready(content.clone())),
            LoadPhase::Running { stage: Stage::Decoding, .. } => Status::Decoding,
            LoadPhase::Running { stage: Stage::ReadingCache | Stage::Fetching | Stage::Cached, .. } => Status::Fetching,
            LoadPhase::Queued => view.queued_status(),
        };
        view.publish_status(status.retaining(ready.as_ref()));
        view.counter.count.fetch_add(1, Ordering::AcqRel);
        let dirty = Arc::clone(&entry.dirty);
        let lease = Arc::new(Lease {
            counter: Arc::clone(&view.counter),
            key,
            dirty: Arc::clone(&dirty),
            signals: Arc::clone(&self.dirty_signals),
            size: AtomicU16::new(request.size().map_or(0, |size| size.edge() as u16)),
            priority: AtomicBool::new(priority == Priority::Visible),
            importance: AtomicU16::new(importance),
            _cache_protection: cache_protection,
            _texture: texture,
        });
        view.leases.retain(|lease| lease.strong_count() > 0);
        view.leases.push(Arc::downgrade(&lease));
        let status = view.status.subscribe();
        let signals = Arc::clone(&self.dirty_signals);
        signals.mark(key, &dirty);
        Ok(Demand { _lease: lease, status })
    }

    pub fn maintain(&mut self, now: Instant) -> bool {
        if self.closed {
            return false;
        }
        if self.maintained_at == Some(now)
            && !self.maintenance_pending
            && self.maintained_generation == self.dirty_signals.change_generation.load(Ordering::Acquire)
        {
            return false;
        }
        self.maintained_at = Some(now);

        for _ in 0..MAX_DEADLINES_PER_TURN {
            let Some(deadline) = self.deadlines.first().copied().filter(|deadline| deadline.at <= now) else { break };
            self.deadlines.remove(&deadline);
            if self.deadline_is_current(deadline)
                && let Some(entry) = self.entries.get_mut(&deadline.key)
            {
                match (deadline.kind, deadline.route) {
                    (DeadlineKind::Demand, Some(route)) => {
                        if let Some(view) = entry.views.get_mut(&route) {
                            view.demand_deadline_indexed = false;
                        }
                    }
                    (DeadlineKind::Retry, Some(route)) => {
                        if let Some(view) = entry.views.get_mut(&route) {
                            view.retry_deadline_indexed = false;
                        }
                    }
                    (DeadlineKind::Recovery, Some(route)) => {
                        if let Some(view) = entry.views.get_mut(&route) {
                            view.recovery_deadline_indexed = false;
                        }
                    }
                    _ => {}
                }
                self.dirty_signals.mark(deadline.key, &entry.dirty);
            }
        }

        for _ in 0..MAX_DIRTY_PER_TURN {
            match self.dirty_receiver.try_recv() {
                Ok(signal) => {
                    self.dirty_signals.queued.fetch_sub(1, Ordering::AcqRel);
                    self.reconcile_dirty_notification(&signal, now);
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }

        if !self.fallback_scan_active && self.dirty_signals.overflow.swap(false, Ordering::AcqRel) {
            self.fallback_scan_active = true;
            self.fallback_scan_cursor = None;
        }
        for _ in 0..MAX_FALLBACK_SCAN_PER_TURN {
            if !self.fallback_scan_active {
                break;
            }
            let next = match self.fallback_scan_cursor {
                Some(cursor) => self.entries.range((Bound::Excluded(cursor), Bound::Unbounded)).next(),
                None => self.entries.iter().next(),
            }
            .map(|(key, entry)| (*key, entry.dirty.is_dirty()));
            let Some((key, is_dirty)) = next else {
                self.fallback_scan_active = false;
                self.fallback_scan_cursor = None;
                break;
            };
            self.fallback_scan_cursor = Some(key);
            if is_dirty {
                self.reconcile_dirty_key(key, now);
            }
        }

        let generation = self.dirty_signals.change_generation.load(Ordering::Acquire);
        let pending = self.has_pending_maintenance(now);
        self.maintenance_pending = pending;
        self.maintained_generation = generation;
        if pending {
            self.dirty_signals.wake.send_replace(());
        }
        pending
    }

    fn has_pending_maintenance(&self, now: Instant) -> bool {
        self.dirty_signals.pending.load(Ordering::Acquire) > 0
            || self.dirty_signals.queued.load(Ordering::Acquire) > 0
            || self.dirty_signals.overflow.load(Ordering::Acquire)
            || self.fallback_scan_active
            || self.deadlines.first().is_some_and(|deadline| deadline.at <= now)
    }

    fn deadline_is_current(&self, deadline: DeadlineKey) -> bool {
        let Some(entry) = self.entries.get(&deadline.key) else { return false };
        match (deadline.kind, deadline.route) {
            (DeadlineKind::Demand, Some(route)) => {
                entry.views.get(&route).is_some_and(|view| view.demand_deadline_indexed && view.deadline == deadline.at)
            }
            (DeadlineKind::Retry, Some(route)) => {
                entry.views.get(&route).is_some_and(|view| view.retry_deadline_indexed && view.retry_at() == Some(deadline.at))
            }
            (DeadlineKind::Recovery, Some(route)) => {
                entry.views.get(&route).is_some_and(|view| view.recovery_deadline_indexed && view.recovery_at() == Some(deadline.at))
            }
            (DeadlineKind::CacheRetry, None) => entry.indexed_cache_retry == Some(deadline.at),
            _ => false,
        }
    }

    fn reconcile_dirty_notification(&mut self, signal: &DirtyNotification, now: Instant) {
        if !self.dirty_signals.take(&signal.dirty) {
            return;
        }
        let Store {
            entries,
            deadlines,
            dirty_signals,
            candidate_queues,
            candidates_dirty,
            running_fetches,
            running_cache_reads,
            running_non_mesh_cache_reads,
            pending_non_mesh_cache_decodes,
            running_texture_decodes,
            running_other_decodes,
            ..
        } = self;
        let Some(entry) = entries.get_mut(&signal.key) else {
            dirty_signals.retire(&signal.dirty);
            return;
        };
        if !Arc::ptr_eq(&entry.dirty, &signal.dirty) {
            dirty_signals.retire(&signal.dirty);
            return;
        }
        reconcile_entry(
            entry,
            signal.key,
            deadlines,
            now,
            running_fetches,
            running_cache_reads,
            running_non_mesh_cache_reads,
            pending_non_mesh_cache_decodes,
            running_texture_decodes,
            running_other_decodes,
        );
        if entry.dirty.is_dirty() {
            if Self::remove_candidate(candidate_queues, entry) {
                *candidates_dirty = true;
            }
        } else if Self::refresh_candidate_queues(candidate_queues, signal.key, entry, now) {
            *candidates_dirty = true;
        }
        Self::refresh_deadline_index(deadlines, signal.key, entry, now);
        if !entry.has_active_demand()
            && entry.texture().is_none()
            && !matches!(entry.phase, LoadPhase::Running { .. })
            && let Some(entry) = entries.remove(&signal.key)
        {
            dirty_signals.retire(&entry.dirty);
        }
    }

    fn reconcile_dirty_key(&mut self, key: LoadKey, now: Instant) {
        let Some(entry) = self.entries.get(&key) else { return };
        if !self.dirty_signals.take(&entry.dirty) {
            return;
        }
        self.reconcile_resource(key, now);
    }

    fn reconcile_resource(&mut self, key: LoadKey, now: Instant) {
        let Store {
            entries,
            deadlines,
            dirty_signals,
            candidate_queues,
            candidates_dirty,
            running_fetches,
            running_cache_reads,
            running_non_mesh_cache_reads,
            pending_non_mesh_cache_decodes,
            running_texture_decodes,
            running_other_decodes,
            ..
        } = self;
        let Some(entry) = entries.get_mut(&key) else { return };
        reconcile_entry(
            entry,
            key,
            deadlines,
            now,
            running_fetches,
            running_cache_reads,
            running_non_mesh_cache_reads,
            pending_non_mesh_cache_decodes,
            running_texture_decodes,
            running_other_decodes,
        );
        if entry.dirty.is_dirty() {
            if Self::remove_candidate(candidate_queues, entry) {
                *candidates_dirty = true;
            }
        } else if Self::refresh_candidate_queues(candidate_queues, key, entry, now) {
            *candidates_dirty = true;
        }
        Self::refresh_deadline_index(deadlines, key, entry, now);
        if !entry.has_active_demand()
            && entry.texture().is_none()
            && !matches!(entry.phase, LoadPhase::Running { .. })
            && let Some(entry) = entries.remove(&key)
        {
            dirty_signals.retire(&entry.dirty);
        }
    }

    fn refresh_candidate_queues(
        candidate_queues: &mut [BTreeSet<ScheduleKey>; 3],
        key: LoadKey,
        entry: &mut AssetEntry,
        now: Instant,
    ) -> bool {
        let candidate = if matches!(entry.phase, LoadPhase::Queued) && cache_retry_ready(entry.cache_eligibility, now) {
            entry
                .views
                .iter()
                .find(|(_, view)| {
                    view.count() > 0
                        && view.attempts() < MAX_ATTEMPTS
                        && (entry.priority == Priority::Background || now < view.deadline)
                        && route_eligible(view.eligibility, now)
                })
                .map(|(route, _)| ScheduleKey {
                    priority: Reverse(entry.priority),
                    importance: Reverse(entry.importance),
                    ready: entry.ready().is_some(),
                    used: entry.used,
                    key,
                    route: *route,
                })
        } else {
            None
        };
        if entry.candidate == candidate {
            return false;
        }
        Self::remove_candidate(candidate_queues, entry);
        if let Some(candidate) = candidate {
            candidate_queues[fetch_pool(key.resource.kind)].insert(candidate);
            entry.candidate = Some(candidate);
        }
        true
    }

    fn remove_candidate(candidate_queues: &mut [BTreeSet<ScheduleKey>; 3], entry: &mut AssetEntry) -> bool {
        if let Some(previous) = entry.candidate.take() {
            candidate_queues[fetch_pool(previous.key.resource.kind)].remove(&previous);
            true
        } else {
            false
        }
    }

    fn refresh_deadline_index(deadlines: &mut BTreeSet<DeadlineKey>, key: LoadKey, entry: &mut AssetEntry, now: Instant) {
        let running = matches!(entry.phase, LoadPhase::Running { .. });
        let idle = matches!(entry.phase, LoadPhase::Idle);
        for (route, view) in &mut entry.views {
            let demand_deadline =
                (view.count() > 0 && (entry.priority == Priority::Visible || running) && view.deadline > now).then_some(view.deadline);
            update_route_deadline(deadlines, key, *route, DeadlineKind::Demand, view, demand_deadline);
            let retry_at = view.retry_at().filter(|retry_at| view.count() > 0 && *retry_at > now);
            update_route_deadline(deadlines, key, *route, DeadlineKind::Retry, view, retry_at);
            let recovery_at = view.recovery_at().filter(|recovery_at| view.count() > 0 && !idle && *recovery_at > now);
            update_route_deadline(deadlines, key, *route, DeadlineKind::Recovery, view, recovery_at);
        }
        let cache_retry = match entry.cache_eligibility {
            CacheEligibility::RetryAt(retry_at) if entry.has_active_demand() && retry_at > now => Some(retry_at),
            CacheEligibility::Unchecked | CacheEligibility::Checked | CacheEligibility::RetryAt(_) => None,
        };
        update_deadline(deadlines, key, None, DeadlineKind::CacheRetry, &mut entry.indexed_cache_retry, cache_retry);
    }

    fn remove_route_deadlines(deadlines: &mut BTreeSet<DeadlineKey>, key: LoadKey, route: Route, view: &mut RouteState) {
        remove_demand_deadline(deadlines, key, route, view);
        remove_failure_deadlines(deadlines, key, route, view);
    }

    pub fn next_job(&mut self, now: Instant) -> Option<LoadJob> {
        if self.closed {
            return None;
        }
        self.maintain(now);
        self.build_candidates();
        let mut selected = None;
        for index in (0..self.candidates.len()).rev() {
            let candidate = self.candidates[index];
            let key = candidate.key;
            let resource = key.resource;
            let Some(entry) = self.entries.get_mut(&key) else {
                self.quarantine_candidate(candidate);
                continue;
            };
            if entry.dirty.is_dirty() || entry.candidate != Some(candidate) {
                self.quarantine_candidate(candidate);
                continue;
            }
            let request = entry.request;
            let mesh_fetch_mode = entry.mesh_fetch_mode;
            let mut cache_lookup_unchecked =
                cache_unchecked(entry.cache_eligibility, now) && entry.texture().and_then(|texture| texture.source()).is_none();
            if cache_lookup_unchecked && self.cache.as_ref().is_some_and(|cache| !cache.may_contain(request)) {
                entry.cache_eligibility = CacheEligibility::Checked;
                cache_lookup_unchecked = false;
            }
            let cache_read = self.cache.is_some() && cache_lookup_unchecked;
            if cache_read {
                if self.running_cache_reads >= storage::MAX_IN_FLIGHT_READS
                    || resource.kind != Kind::Mesh
                        && self.running_non_mesh_cache_reads + self.pending_non_mesh_cache_decodes
                            >= MAX_TEXTURE_DECODES + MAX_NON_TEXTURE_DECODES
                {
                    continue;
                }
            } else if self.running_fetches[fetch_pool(resource.kind)] >= FETCH_LIMITS[fetch_pool(resource.kind)] {
                continue;
            }
            let prefix_lod = (!cache_read).then(|| self.mesh_lod_prefix(request)).flatten();
            let fetch_bytes = if mesh_fetch_mode == MeshFetchMode::FullFallback && resource.kind == Kind::Mesh {
                Some(resource.kind.max_response_bytes())
            } else {
                fetch_work_bytes(request)
            };
            let Some(bytes) = (if cache_read { read_work_bytes_for_request(request) } else { fetch_bytes })
                .map(|bytes| bytes.max(prefix_lod.as_ref().map_or(0, |(_, bytes)| bytes.capacity())))
            else {
                continue;
            };
            let headroom =
                if self.running_texture_decodes + self.running_other_decodes > 0 { 0 } else { TEXTURE_WORK_BYTES.saturating_sub(bytes) };
            let Some(reservation) = self.budget.reserve_with_headroom(bytes, headroom) else { continue };
            let generation = self.sequence.checked_add(1)?;
            self.sequence = generation;
            selected = Some((index, candidate, prefix_lod, reservation, generation));
            break;
        }
        let (index, candidate, prefix_lod, reservation, generation) = selected?;
        self.candidates.remove(index);
        let key = candidate.key;
        let route = candidate.route;
        let resource = key.resource;
        let dirty = Arc::clone(&self.entries.get(&key).expect("selected demanded entry").dirty);
        let life = Arc::new(WorkLife { key, dirty, signals: Arc::clone(&self.dirty_signals), transport_pending: AtomicBool::new(false) });
        let (cancelled, receiver) = watch::channel(false);
        let entry = self.entries.get_mut(&key).expect("selected demanded entry");
        if matches!(entry.cache_eligibility, CacheEligibility::RetryAt(retry_at) if retry_at <= now) {
            entry.cache_eligibility = CacheEligibility::Unchecked;
        }
        let source = entry.texture().and_then(|texture| texture.source());
        let job = Job {
            resource,
            key,
            request: entry.request,
            route,
            generation,
            reservation,
            _life: life,
            cancelled: receiver,
            size: entry.target.unwrap_or_default(),
            mesh_fetch_mode: entry.mesh_fetch_mode,
        };
        if let Some(source) = source {
            if entry.priority == Priority::Background {
                set_demand_deadline(
                    &mut self.deadlines,
                    key,
                    route,
                    entry.views.get_mut(&route).expect("selected route"),
                    now + DEMAND_DEADLINE,
                );
            }
            entry.views.get_mut(&route).expect("selected route").begin_attempt(&mut self.deadlines, key, route);
            entry.phase = LoadPhase::Running { generation, route, life: Arc::downgrade(&job._life), cancelled, stage: Stage::Fetching };
            start_fetch(resource.kind, &mut self.running_fetches);
            entry.broadcast_status(Status::Fetching);
            Self::refresh_candidate_queues(&mut self.candidate_queues, key, entry, now);
            Self::refresh_deadline_index(&mut self.deadlines, key, entry, now);
            return Some(LoadJob::Decode(DecodeInput { source: DecodeSource::Cached(source), job }));
        }
        if let Some((block, bytes)) = prefix_lod {
            if entry.priority == Priority::Background {
                set_demand_deadline(
                    &mut self.deadlines,
                    key,
                    route,
                    entry.views.get_mut(&route).expect("selected route"),
                    now + DEMAND_DEADLINE,
                );
            }
            entry.views.get_mut(&route).expect("selected route").begin_attempt(&mut self.deadlines, key, route);
            entry.phase = LoadPhase::Running { generation, route, life: Arc::downgrade(&job._life), cancelled, stage: Stage::Fetching };
            start_fetch(resource.kind, &mut self.running_fetches);
            entry.broadcast_status(Status::Fetching);
            Self::refresh_candidate_queues(&mut self.candidate_queues, key, entry, now);
            Self::refresh_deadline_index(&mut self.deadlines, key, entry, now);
            let response = FetchResponse::Partial { bytes, range: block.range(), total_length: Some(block.asset_length()) };
            return Some(LoadJob::Decode(DecodeInput { source: DecodeSource::Response(response), job }));
        }
        if let Some(cache) = &self.cache
            && cache_unchecked(entry.cache_eligibility, now)
        {
            if entry.priority == Priority::Background {
                set_demand_deadline(
                    &mut self.deadlines,
                    key,
                    route,
                    entry.views.get_mut(&route).expect("selected route"),
                    now + DEMAND_DEADLINE,
                );
            }
            entry.phase = LoadPhase::Running { generation, route, life: Arc::downgrade(&job._life), cancelled, stage: Stage::ReadingCache };
            self.running_cache_reads += 1;
            if resource.kind != Kind::Mesh {
                self.running_non_mesh_cache_reads += 1;
            }
            entry.broadcast_status(Status::Fetching);
            Self::refresh_candidate_queues(&mut self.candidate_queues, key, entry, now);
            Self::refresh_deadline_index(&mut self.deadlines, key, entry, now);
            return Some(LoadJob::ReadCache(CacheReadJob { job, cache: Arc::clone(cache) }));
        }
        if entry.priority == Priority::Background {
            set_demand_deadline(
                &mut self.deadlines,
                key,
                route,
                entry.views.get_mut(&route).expect("selected route"),
                now + DEMAND_DEADLINE,
            );
        }
        entry.views.get_mut(&route).expect("selected route").begin_attempt(&mut self.deadlines, key, route);
        entry.phase = LoadPhase::Running { generation, route, life: Arc::downgrade(&job._life), cancelled, stage: Stage::Fetching };
        start_fetch(resource.kind, &mut self.running_fetches);
        entry.broadcast_status(Status::Fetching);
        job._life.transport_pending.store(true, Ordering::Release);
        Self::refresh_candidate_queues(&mut self.candidate_queues, key, entry, now);
        Self::refresh_deadline_index(&mut self.deadlines, key, entry, now);
        Some(LoadJob::Fetch(FetchJob { job }))
    }

    fn mesh_lod_prefix(&self, request: Request) -> Option<(MeshLodRequest, Vec<u8>)> {
        let Request::MeshLod(block) = request else { return None };
        let resource = Resource { id: block.asset_id(), kind: Kind::Mesh };
        let key = LoadKey { resource, lod: None };
        let Content::MeshHeader(header) = self.entries.get(&key)?.ready()? else { return None };
        let source = header.lod_bytes(block)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(source.len()).ok()?;
        bytes.extend_from_slice(source);
        Some((block, bytes))
    }

    #[allow(clippy::result_large_err)]
    pub fn start_decode(&mut self, mut input: DecodeInput) -> Result<DecodeJob, DecodeInput> {
        if self.closed || input.is_cancelled() || input.job.route.store != self.id {
            return Err(input);
        }
        let decode_slots_full = if input.job.resource.kind == Kind::Texture {
            self.running_texture_decodes >= MAX_TEXTURE_DECODES
        } else {
            self.running_other_decodes >= MAX_NON_TEXTURE_DECODES
        };
        if decode_slots_full {
            return Err(input);
        }
        if !self.entries.get(&input.job.key).is_some_and(
            |entry| matches!(entry.phase, LoadPhase::Running { generation, stage: Stage::Fetching | Stage::Cached, .. } if generation == input.job.generation),
        ) {
            return Err(input);
        }
        if let Some(bytes) = input.job.resource.kind.work_bytes()
            && !input.job.reservation.resize(bytes)
        {
            return Err(input);
        }
        let entry = self.entries.get_mut(&input.job.key).expect("validated running job");
        let LoadPhase::Running { stage, .. } = &mut entry.phase else { unreachable!("validated running phase") };
        let cached = *stage == Stage::Cached;
        *stage = Stage::Decoding;
        if cached && input.job.resource.kind != Kind::Mesh {
            finish_cache_read(&mut self.pending_non_mesh_cache_decodes);
        } else {
            if !cached {
                finish_fetch(input.job.resource.kind, &mut self.running_fetches);
            }
        }
        start_decode(input.job.resource.kind, &mut self.running_texture_decodes, &mut self.running_other_decodes);
        entry.broadcast_status(Status::Decoding);
        Ok(DecodeJob { input, persist_prepared: self.cache.is_some() })
    }

    pub fn complete_cache_read(&mut self, completion: CacheReadCompletion, now: Instant) -> Option<LoadJob> {
        let CacheReadCompletion { mut job, result } = completion;
        if self.closed || job.route.store != self.id {
            return None;
        }
        let entry = self.entries.get_mut(&job.key)?;
        if !matches!(entry.phase, LoadPhase::Running { generation, stage: Stage::ReadingCache, .. } if generation == job.generation) {
            return None;
        }
        self.dirty_signals.mark(job.key, &entry.dirty);
        finish_cache_read(&mut self.running_cache_reads);
        if job.resource.kind != Kind::Mesh {
            finish_cache_read(&mut self.running_non_mesh_cache_reads);
        }
        if job.is_cancelled() || !entry.has_live_demand(now) {
            entry.phase = LoadPhase::Queued;
            return None;
        }
        if !matches!(&result, Err(Failure::Cancelled))
            && !matches!(&result, Ok(AssetLookup::Unavailable(storage::UnavailableReason::ReadLimit)))
        {
            entry.cache_eligibility = CacheEligibility::Checked;
        }
        match result {
            Ok(AssetLookup::Hit(asset)) => {
                let decode_bytes = job.resource.kind.work_bytes().unwrap_or(asset.bytes.capacity());
                job.reservation.shrink_to_at_most(decode_bytes.max(asset.bytes.capacity()));
                if job.resource.kind != Kind::Mesh {
                    self.pending_non_mesh_cache_decodes += 1;
                }
                if let LoadPhase::Running { stage, .. } = &mut entry.phase {
                    *stage = Stage::Cached;
                }
                Some(LoadJob::Decode(DecodeInput {
                    source: DecodeSource::Disk { bytes: asset.bytes, source_sha256: asset.sha256, cache: self.cache.clone() },
                    job,
                }))
            }
            Ok(AssetLookup::Unavailable(storage::UnavailableReason::ReadLimit)) => {
                entry.phase = LoadPhase::Queued;
                entry.broadcast_status(Status::Queued);
                None
            }
            _ => {
                if entry.views.get(&job.route).is_none_or(|view| view.count() == 0 || now >= view.deadline) {
                    entry.phase = LoadPhase::Queued;
                    entry.broadcast_status(Status::Queued);
                    return None;
                }
                let fetch = Self::cache_read_fallback(entry, job, now, &mut self.deadlines)?;
                start_fetch(fetch.resource().kind, &mut self.running_fetches);
                Some(LoadJob::Fetch(fetch))
            }
        }
    }

    fn cache_read_fallback(entry: &mut AssetEntry, mut job: Job, now: Instant, deadlines: &mut BTreeSet<DeadlineKey>) -> Option<FetchJob> {
        let bytes = if entry.mesh_fetch_mode == MeshFetchMode::FullFallback && job.resource.kind == Kind::Mesh {
            job.resource.kind.max_response_bytes()
        } else {
            fetch_work_bytes(job.request)?
        };
        job.reservation.shrink_to_at_most(bytes);
        if entry.priority == Priority::Background
            && let Some(view) = entry.views.get_mut(&job.route)
        {
            set_demand_deadline(deadlines, job.key, job.route, view, now + DEMAND_DEADLINE);
        }
        if let Some(view) = entry.views.get_mut(&job.route) {
            view.begin_attempt(deadlines, job.key, job.route);
        }
        if let LoadPhase::Running { stage, .. } = &mut entry.phase {
            *stage = Stage::Fetching;
        }
        job._life.transport_pending.store(true, Ordering::Release);
        Some(FetchJob { job })
    }

    pub fn complete(&mut self, completion: LoadCompletion, now: Instant) -> Vec<CacheWrite> {
        let LoadCompletion { job, result, origin, decode_duration } = completion;
        let mut writes = Vec::new();
        if self.closed || job.route.store != self.id {
            return writes;
        }
        let Some(entry) = self.entries.get_mut(&job.key) else { return writes };
        let stage = match &entry.phase {
            LoadPhase::Running { generation, stage, .. } if *generation == job.generation => *stage,
            _ => return writes,
        };
        if stage == Stage::Decoding
            && result.is_ok()
            && let (Some(cache), Some(duration)) = (&self.cache, decode_duration)
        {
            cache.observe_decode(job.resource.kind, duration);
        }
        match stage {
            Stage::ReadingCache => {
                finish_cache_read(&mut self.running_cache_reads);
                if job.resource.kind != Kind::Mesh {
                    finish_cache_read(&mut self.running_non_mesh_cache_reads);
                }
            }
            Stage::Fetching => {
                finish_fetch(job.resource.kind, &mut self.running_fetches);
            }
            Stage::Cached if job.resource.kind != Kind::Mesh => {
                finish_cache_read(&mut self.pending_non_mesh_cache_decodes);
            }
            Stage::Cached => {}
            Stage::Decoding => finish_decode(job.resource.kind, &mut self.running_texture_decodes, &mut self.running_other_decodes),
        }
        entry.phase = LoadPhase::Queued;
        self.dirty_signals.mark(job.key, &entry.dirty);
        if job.is_cancelled() || !entry.has_live_demand(now) {
            return writes;
        }
        if origin == DecodeOrigin::Disk && result.as_ref().is_err_and(|failure| *failure == Failure::MemoryPressure) {
            entry.cache_eligibility = CacheEligibility::RetryAt(now + CACHE_PRESSURE_RETRY);
            entry.broadcast_status(Status::Queued);
            return writes;
        }
        if result.as_ref().is_err_and(|failure| *failure == Failure::FullAssetRequired)
            && job.resource.kind == Kind::Mesh
            && entry.mesh_fetch_mode == MeshFetchMode::Range
        {
            entry.mesh_fetch_mode = MeshFetchMode::FullFallback;
            if let Some(view) = entry.views.get_mut(&job.route) {
                view.refund_attempt();
            }
            entry.broadcast_status(Status::Queued);
            return writes;
        }
        if origin == DecodeOrigin::Disk && result.as_ref().is_err_and(|failure| *failure != Failure::Cancelled) && !job.is_cancelled() {
            if let Some(cache) = &self.cache
                && let Err(error) = cache.discard_corrupt(job.request)
                && let Some(due) = self.discard_failures.record(now, |failures| {
                    failures.failed = failures.failed.saturating_add(1);
                    failures.last_error = Some(error);
                })
            {
                due.report();
            }
            return writes;
        }
        let persist_encoded = self.cache.is_some() && origin == DecodeOrigin::Network;
        let persist_prepared = self.cache.is_some();
        match result.and_then(|prepared| prepared.finish(job.reservation, persist_encoded, persist_prepared)) {
            Ok(content::FinishedPrepared { content, source, persisted, prepared_texture }) => {
                entry.used = now;
                if let Some(cache) = &self.cache {
                    match (persisted, prepared_texture) {
                        (Some(payload), prepared) => {
                            if let Some(write) = cache.queue_write_with_prepared(job.request, payload, prepared, &self.budget) {
                                writes.push(write);
                            }
                        }
                        (None, Some((key, payload))) => {
                            if let Some(write) = cache.queue_prepared_texture_write(key, payload, &self.budget) {
                                writes.push(write);
                            }
                        }
                        (None, None) => {}
                    }
                }
                if job.resource.kind == Kind::Texture
                    && entry.requested_size().is_some_and(|size| size != job.size)
                    && let Some(ready) = entry.ready()
                {
                    entry.publish_content(ready);
                    return writes;
                }
                let content = match content {
                    Finished::Texture(image) => {
                        let Some(texture) = entry.texture() else { return writes };
                        texture.install(image, source);
                        Content::Texture(texture)
                    }
                    Finished::Other(content) => content,
                };
                let satisfied = content_satisfies(&content, entry.requested_size());
                if satisfied {
                    entry.phase = LoadPhase::Idle;
                }
                entry.publish_content(content.clone());
                entry.ready = Some(content.downgrade());
            }
            Err(failure) => {
                if job.resource.kind == Kind::Texture && entry.requested_size().is_some_and(|size| size != job.size) {
                    return writes;
                }
                let ready = entry.ready();
                let Some(view) = entry.views.get_mut(&job.route) else { return writes };
                let retry_at = match failure {
                    Failure::Transient { retry_after } if view.attempts() < MAX_ATTEMPTS => {
                        let delay = retry_after.max(Duration::from_secs(u64::from(view.attempts().max(1))));
                        if delay > Duration::from_secs(30) {
                            None
                        } else {
                            let retry = now + delay;
                            (retry < view.deadline).then_some(retry)
                        }
                    }
                    _ => None,
                };
                view.fail(&mut self.deadlines, job.key, job.route, failure, retry_at, now, ready.as_ref());
                for (route, view) in &entry.views {
                    if *route != job.route && view.count() > 0 {
                        view.publish_status(view.queued_status().retaining(ready.as_ref()));
                    }
                }
            }
        }
        writes
    }

    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        self.deadlines.first().map(|deadline| deadline.at.max(now))
    }

    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        if let Some(due) = self.discard_failures.flush() {
            due.report();
        }
        self.running_fetches = [0; 3];
        self.running_cache_reads = 0;
        self.running_non_mesh_cache_reads = 0;
        self.pending_non_mesh_cache_decodes = 0;
        self.running_texture_decodes = 0;
        self.running_other_decodes = 0;
        for entry in self.entries.values() {
            self.dirty_signals.retire(&entry.dirty);
            if let LoadPhase::Running { cancelled, .. } = &entry.phase {
                cancelled.send_replace(true);
            }
            entry.broadcast_status(Status::StaleSession);
        }
        self.entries.clear();
        self.candidates.clear();
        self.candidates_dirty = true;
        self.candidate_queues.iter_mut().for_each(BTreeSet::clear);
        self.deadlines.clear();
        self.routes.clear();
    }

    fn build_candidates(&mut self) {
        let generation = self.dirty_signals.change_generation.load(Ordering::Acquire);
        if !self.candidates_dirty && self.candidate_generation == generation {
            return;
        }
        self.candidates.clear();
        let mut more = false;
        let Store { entries, candidates, candidate_queues, .. } = self;
        for (queue, limit) in candidate_queues.iter_mut().zip(FETCH_LIMITS) {
            more |= collect_candidates(queue, entries, candidates, limit);
        }
        self.candidates.sort_unstable_by(|left, right| right.cmp(left));
        self.candidate_generation = generation;
        self.candidates_dirty = more || self.dirty_signals.change_generation.load(Ordering::Acquire) != generation;
    }

    fn quarantine_candidate(&mut self, candidate: ScheduleKey) {
        let Store { entries, candidate_queues, candidates_dirty, .. } = self;
        candidate_queues[fetch_pool(candidate.key.resource.kind)].remove(&candidate);
        if let Some(entry) = entries.get_mut(&candidate.key)
            && entry.candidate == Some(candidate)
        {
            entry.candidate = None;
        }
        *candidates_dirty = true;
    }
}

fn collect_candidates(
    queue: &mut BTreeSet<ScheduleKey>,
    entries: &mut BTreeMap<LoadKey, AssetEntry>,
    candidates: &mut Vec<ScheduleKey>,
    limit: usize,
) -> bool {
    let scan_limit = MAX_DIRTY_PER_TURN.saturating_add(limit);
    let mut cursor = None;
    let mut scanned = 0;
    let mut collected = 0;
    while scanned < scan_limit && collected < limit {
        let next = match cursor {
            Some(cursor) => queue.range((Bound::Excluded(cursor), Bound::Unbounded)).next(),
            None => queue.iter().next(),
        }
        .copied();
        let Some(candidate) = next else { return false };
        cursor = Some(candidate);
        scanned += 1;
        if entries.get(&candidate.key).is_some_and(|entry| !entry.dirty.is_dirty() && entry.candidate == Some(candidate)) {
            candidates.push(candidate);
            collected += 1;
        } else {
            queue.remove(&candidate);
            if let Some(entry) = entries.get_mut(&candidate.key)
                && entry.candidate == Some(candidate)
            {
                entry.candidate = None;
            }
        }
    }
    scanned == scan_limit && cursor.is_some_and(|cursor| queue.range((Bound::Excluded(cursor), Bound::Unbounded)).next().is_some())
}

fn cache_unchecked(eligibility: CacheEligibility, now: Instant) -> bool {
    cache_eligible(eligibility, now)
}

fn cache_eligible(eligibility: CacheEligibility, now: Instant) -> bool {
    match eligibility {
        CacheEligibility::Unchecked => true,
        CacheEligibility::RetryAt(retry_at) => retry_at <= now,
        CacheEligibility::Checked => false,
    }
}

fn cache_retry_ready(eligibility: CacheEligibility, now: Instant) -> bool {
    !matches!(eligibility, CacheEligibility::RetryAt(retry_at) if retry_at > now)
}

fn route_eligible(eligibility: RouteEligibility, now: Instant) -> bool {
    match eligibility {
        RouteEligibility::Eligible { .. } => true,
        RouteEligibility::RetryAt { retry_at, .. } => retry_at <= now,
        RouteEligibility::Failed { .. } => false,
    }
}

fn update_deadline(
    deadlines: &mut BTreeSet<DeadlineKey>,
    key: LoadKey,
    route: Option<Route>,
    kind: DeadlineKind,
    indexed: &mut Option<Instant>,
    next: Option<Instant>,
) {
    if *indexed == next {
        return;
    }
    if let Some(at) = indexed.take() {
        deadlines.remove(&DeadlineKey { at, key, route, kind });
    }
    if let Some(at) = next {
        deadlines.insert(DeadlineKey { at, key, route, kind });
        *indexed = Some(at);
    }
}

fn update_route_deadline(
    deadlines: &mut BTreeSet<DeadlineKey>,
    key: LoadKey,
    route: Route,
    kind: DeadlineKind,
    view: &mut RouteState,
    next: Option<Instant>,
) {
    let (previous, indexed) = match kind {
        DeadlineKind::Demand => (Some(view.deadline), &mut view.demand_deadline_indexed),
        DeadlineKind::Retry => (view.retry_at(), &mut view.retry_deadline_indexed),
        DeadlineKind::Recovery => (view.recovery_at(), &mut view.recovery_deadline_indexed),
        DeadlineKind::CacheRetry => return,
    };
    if *indexed && previous == next {
        return;
    }
    if *indexed {
        if let Some(at) = previous {
            deadlines.remove(&DeadlineKey { at, key, route: Some(route), kind });
        }
        *indexed = false;
    }
    if let Some(at) = next {
        deadlines.insert(DeadlineKey { at, key, route: Some(route), kind });
        *indexed = true;
    }
}

fn set_demand_deadline(deadlines: &mut BTreeSet<DeadlineKey>, key: LoadKey, route: Route, view: &mut RouteState, next: Instant) {
    remove_demand_deadline(deadlines, key, route, view);
    view.deadline = next;
}

fn remove_demand_deadline(deadlines: &mut BTreeSet<DeadlineKey>, key: LoadKey, route: Route, view: &mut RouteState) {
    if view.demand_deadline_indexed {
        deadlines.remove(&DeadlineKey { at: view.deadline, key, route: Some(route), kind: DeadlineKind::Demand });
        view.demand_deadline_indexed = false;
    }
}

fn remove_failure_deadlines(deadlines: &mut BTreeSet<DeadlineKey>, key: LoadKey, route: Route, view: &mut RouteState) {
    if view.retry_deadline_indexed {
        if let Some(at) = view.retry_at() {
            deadlines.remove(&DeadlineKey { at, key, route: Some(route), kind: DeadlineKind::Retry });
        }
        view.retry_deadline_indexed = false;
    }
    if view.recovery_deadline_indexed {
        if let Some(at) = view.recovery_at() {
            deadlines.remove(&DeadlineKey { at, key, route: Some(route), kind: DeadlineKind::Recovery });
        }
        view.recovery_deadline_indexed = false;
    }
}

fn reconcile_entry(
    entry: &mut AssetEntry,
    key: LoadKey,
    deadlines: &mut BTreeSet<DeadlineKey>,
    now: Instant,
    running_fetches: &mut [usize; 3],
    running_cache_reads: &mut usize,
    running_non_mesh_cache_reads: &mut usize,
    pending_non_mesh_cache_decodes: &mut usize,
    running_texture_decodes: &mut usize,
    running_other_decodes: &mut usize,
) {
    let ready = entry.ready();
    let mut desired = None;
    let mut priority = Priority::Background;
    let mut importance = 0;
    for view in entry.views.values_mut() {
        view.leases.retain(|weak| {
            let Some(lease) = weak.upgrade() else { return false };
            desired = desired.max(lease.size());
            priority = priority.max(lease.priority());
            importance = importance.max(lease.importance.load(Ordering::Acquire));
            true
        });
    }
    if priority != entry.priority {
        for (route, view) in &mut entry.views {
            let restart = match view.eligibility {
                RouteEligibility::Eligible { .. } => true,
                RouteEligibility::RetryAt { .. } => false,
                RouteEligibility::Failed { failure, .. } => failure == Failure::Deadline,
            };
            if now >= view.deadline && restart {
                view.restart(deadlines, key, *route, now);
            }
        }
    }
    if !matches!(entry.phase, LoadPhase::Idle) {
        let mut recovered = false;
        for (route, view) in &mut entry.views {
            if view.count() > 0 && view.recovery_at().is_some_and(|recovery_at| recovery_at <= now) {
                view.restart(deadlines, key, *route, now);
                recovered = true;
            }
        }
        if recovered && !matches!(entry.phase, LoadPhase::Running { .. }) {
            entry.phase = LoadPhase::Queued;
        }
    }
    entry.priority = priority;
    entry.importance = importance;
    if entry.target != desired {
        entry.target = desired;
        if let Some(size) = desired
            && let Request::Texture { id, .. } = entry.request
        {
            entry.request = Request::Texture { id, size };
        }
        for (route, view) in &mut entry.views {
            view.restart(deadlines, key, *route, now);
        }
        if !matches!(entry.phase, LoadPhase::Running { .. }) {
            entry.phase = LoadPhase::Queued;
        }
    }
    if matches!(entry.phase, LoadPhase::Queued)
        && let Some(content) = ready.as_ref()
        && content_satisfies(content, desired)
    {
        entry.phase = LoadPhase::Idle;
        entry.broadcast_status(Status::Ready((*content).clone()));
    }
    if let LoadPhase::Running { life, cancelled, route, stage, .. } = &entry.phase {
        let transport_pending = life.upgrade().is_some_and(|life| life.transport_pending.load(Ordering::Acquire));
        if !entry.has_live_demand(now) || transport_pending && entry.views.get(route).is_none_or(|view| now >= view.deadline) {
            cancelled.send_replace(true);
        }
        if life.upgrade().is_none() {
            if let Some(view) = entry.views.get_mut(route) {
                view.fail(deadlines, key, *route, Failure::Cancelled, None, now, ready.as_ref());
            }
            let stage = *stage;
            entry.phase = LoadPhase::Queued;
            release_stage(
                stage,
                entry.request.resource().kind,
                running_fetches,
                running_cache_reads,
                running_non_mesh_cache_reads,
                pending_non_mesh_cache_decodes,
                running_texture_decodes,
                running_other_decodes,
            );
        }
    }
    if !matches!(entry.phase, LoadPhase::Idle) && (entry.priority == Priority::Visible || matches!(entry.phase, LoadPhase::Running { .. }))
    {
        for (route, view) in entry.views.iter_mut().filter(|(_, view)| {
            view.count() > 0
                && now >= view.deadline
                && matches!(view.eligibility, RouteEligibility::Eligible { .. } | RouteEligibility::RetryAt { .. })
        }) {
            view.fail(deadlines, key, *route, Failure::Deadline, None, now, ready.as_ref());
        }
    }
}

fn release_stage(
    stage: Stage,
    kind: Kind,
    running_fetches: &mut [usize; 3],
    running_cache_reads: &mut usize,
    running_non_mesh_cache_reads: &mut usize,
    pending_non_mesh_cache_decodes: &mut usize,
    running_texture_decodes: &mut usize,
    running_other_decodes: &mut usize,
) {
    match stage {
        Stage::ReadingCache => {
            finish_cache_read(running_cache_reads);
            if kind != Kind::Mesh {
                finish_cache_read(running_non_mesh_cache_reads);
            }
        }
        Stage::Cached if kind != Kind::Mesh => finish_cache_read(pending_non_mesh_cache_decodes),
        Stage::Cached => {}
        Stage::Fetching => finish_fetch(kind, running_fetches),
        Stage::Decoding => finish_decode(kind, running_texture_decodes, running_other_decodes),
    }
}

fn content_satisfies(content: &Content, desired: Option<TextureSize>) -> bool {
    match content {
        Content::Texture(image) => {
            desired.is_none_or(|size| image.info().is_some_and(|info| info.discard == size.discard_level(info.source_size)))
        }
        Content::Material(_) | Content::Environment(_) | Content::MeshHeader(_) | Content::Mesh(_) => true,
    }
}

fn fetch_pool(kind: Kind) -> usize {
    match kind {
        Kind::Mesh => 0,
        Kind::LegacyMaterial => 1,
        Kind::Texture | Kind::PbrMaterial | Kind::Environment => 2,
    }
}

fn start_fetch(kind: Kind, running: &mut [usize; 3]) {
    running[fetch_pool(kind)] += 1;
}

fn finish_cache_read(reads: &mut usize) {
    *reads -= 1;
}

fn finish_fetch(kind: Kind, running: &mut [usize; 3]) {
    running[fetch_pool(kind)] -= 1;
}

fn start_decode(kind: Kind, texture: &mut usize, other: &mut usize) {
    if kind == Kind::Texture {
        *texture += 1;
    } else {
        *other += 1;
    }
}

fn finish_decode(kind: Kind, texture: &mut usize, other: &mut usize) {
    if kind == Kind::Texture {
        *texture -= 1;
    } else {
        *other -= 1;
    }
}

fn fetch_work_bytes(request: Request) -> Option<usize> {
    match request {
        Request::MeshHeader(_) => Some(MAX_MESH_HEADER_BYTES),
        Request::MeshLod(block) => usize::try_from(block.range().end.checked_sub(block.range().start)?).ok(),
        _ => Some(request.resource().kind.max_response_bytes()),
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
