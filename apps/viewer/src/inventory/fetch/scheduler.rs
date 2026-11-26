use std::{
    collections::{HashMap, HashSet, VecDeque},
    error::Error,
    fmt, iter,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures::{
    FutureExt, StreamExt,
    future::{BoxFuture, Shared},
    stream::FuturesUnordered,
};
use inventory::{CategoryKey, InventoryOwner, InventorySnapshot, Item, MAX_INVENTORY_ITEMS};
use network::capabilities::{CapabilityUrl, Client};
use protocol::inventory::DecodeError;
use tokio::{
    runtime::Handle,
    sync::{mpsc, mpsc::error::TrySendError, oneshot, watch},
    task,
    task::JoinHandle,
    time,
};

use super::{
    FetchFailureCause, FolderFetch, FolderFetchProvenance, InventoryFetchError, InventoryFetchEvent, MAX_AIS_DEPTH, MAX_FETCH_RETRIES,
    RequestFailure, ais, classify_request_error, legacy,
    state::{BackgroundRequest, SourceKind, enqueue_background, next_background_job},
};
use crate::{inspection::BoundedQueueStats, inventory::InventoryEditHandle};

const REQUEST_CAPACITY: usize = 1024;
const RESULT_CAPACITY: usize = 2;
const MAX_TOTAL_CONCURRENCY: usize = 20;
const MAX_BACKGROUND_CONCURRENCY: usize = MAX_TOTAL_CONCURRENCY - 1;
const MIN_BACKGROUND_CONCURRENCY: usize = 1;
const RECOVERY_SUCCESS_THRESHOLD: usize = 2;
const OWNER_REFRESH_SCAN_CHUNK: usize = 512;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(250);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

pub(crate) type Cancellation = Shared<BoxFuture<'static, ()>>;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InventoryLoadingStatus {
    pub folders_known: usize,
    pub folders_complete: usize,
    pub folders_failed: usize,
    pub background_done: bool,
    pub last_failure: Option<(CategoryKey, InventoryFetchError)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InventoryFetchRequestError {
    QueueFull,
    SessionEnded,
}

impl fmt::Display for InventoryFetchRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::QueueFull => "inventory request queue is full",
            Self::SessionEnded => "inventory session has ended",
        })
    }
}

impl Error for InventoryFetchRequestError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InventoryFetchOutcome {
    Succeeded,
    Failed,
}

struct ForegroundRequest {
    folder: CategoryKey,
    completion: Option<oneshot::Sender<InventoryFetchOutcome>>,
}

impl ForegroundRequest {
    fn background(folder: CategoryKey) -> Self {
        Self { folder, completion: None }
    }
}

#[derive(Clone)]
pub struct InventoryFetchHandle {
    requests: InventoryFetchRequests,
    status: watch::Receiver<InventoryLoadingStatus>,
    edits: InventoryEditHandle,
}

#[derive(Clone)]
pub(crate) struct InventoryFetchRequests {
    sender: mpsc::Sender<ForegroundRequest>,
    full_rejections: Arc<AtomicU64>,
    refresh_incomplete: watch::Sender<u64>,
    background_queue_depth: Arc<AtomicUsize>,
    background_queue_peak: Arc<AtomicUsize>,
}

impl InventoryFetchRequests {
    fn new(sender: mpsc::Sender<ForegroundRequest>, refresh_incomplete: watch::Sender<u64>) -> Self {
        Self {
            sender,
            full_rejections: Arc::new(AtomicU64::new(0)),
            refresh_incomplete,
            background_queue_depth: Arc::new(AtomicUsize::new(0)),
            background_queue_peak: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl InventoryFetchHandle {
    pub fn queue_stats(&self) -> BoundedQueueStats {
        let capacity = self.requests.sender.max_capacity();
        BoundedQueueStats {
            depth: capacity.saturating_sub(self.requests.sender.capacity()),
            capacity,
            full_count: self.requests.full_rejections.load(Ordering::Relaxed),
        }
    }

    pub fn background_queue_depth(&self) -> (usize, usize) {
        (self.requests.background_queue_depth.load(Ordering::Relaxed), self.requests.background_queue_peak.load(Ordering::Relaxed))
    }

    pub fn request_folder(&self, folder: CategoryKey) -> Result<oneshot::Receiver<InventoryFetchOutcome>, InventoryFetchRequestError> {
        let (completion, result) = oneshot::channel();
        self.send_request(ForegroundRequest { folder, completion: Some(completion) })?;
        Ok(result)
    }

    pub(crate) fn refetch_folder(&self, folder: CategoryKey) -> Result<(), InventoryFetchRequestError> {
        self.send_request(ForegroundRequest::background(folder))
    }

    fn send_request(&self, request: ForegroundRequest) -> Result<(), InventoryFetchRequestError> {
        self.requests.sender.try_send(request).map_err(|error| match error {
            TrySendError::Full(_) => {
                self.requests.full_rejections.fetch_add(1, Ordering::Relaxed);
                InventoryFetchRequestError::QueueFull
            }
            TrySendError::Closed(_) => InventoryFetchRequestError::SessionEnded,
        })
    }

    pub(crate) fn refetch_incomplete(&self) -> Result<(), InventoryFetchRequestError> {
        if self.requests.sender.is_closed() {
            return Err(InventoryFetchRequestError::SessionEnded);
        }
        self.requests.refresh_incomplete.send_modify(|generation| *generation = generation.wrapping_add(1));
        Ok(())
    }

    pub fn status(&self) -> watch::Receiver<InventoryLoadingStatus> {
        self.status.clone()
    }

    pub fn edits(&self) -> InventoryEditHandle {
        self.edits.clone()
    }
}

pub(crate) struct InventoryFetchController {
    handle: InventoryFetchHandle,
    requests: Option<mpsc::Receiver<ForegroundRequest>>,
    events_tx: mpsc::Sender<InventoryFetchEvent>,
    events_rx: mpsc::Receiver<InventoryFetchEvent>,
    status_tx: watch::Sender<InventoryLoadingStatus>,
    in_flight_count: Arc<AtomicUsize>,
    refresh_receiver: Option<watch::Receiver<u64>>,
    stop_tx: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl InventoryFetchController {
    pub(crate) fn new(edits: InventoryEditHandle) -> Self {
        let (request_sender, requests_rx) = mpsc::channel(REQUEST_CAPACITY);
        let (events_tx, events_rx) = mpsc::channel(RESULT_CAPACITY);
        let (status_tx, status_rx) = watch::channel(InventoryLoadingStatus::default());
        let (refresh_incomplete, refresh_receiver) = watch::channel(0);
        let requests = InventoryFetchRequests::new(request_sender, refresh_incomplete);
        Self {
            handle: InventoryFetchHandle { requests, status: status_rx, edits },
            requests: Some(requests_rx),
            events_tx,
            events_rx,
            status_tx,
            in_flight_count: Arc::new(AtomicUsize::new(0)),
            refresh_receiver: Some(refresh_receiver),
            stop_tx: None,
            task: None,
        }
    }

    pub(crate) fn handle(&self) -> InventoryFetchHandle {
        self.handle.clone()
    }

    pub(crate) fn in_flight_count(&self) -> usize {
        self.in_flight_count.load(Ordering::Relaxed)
    }

    pub(crate) fn publish(&self, status: InventoryLoadingStatus) {
        self.status_tx.send_replace(status);
    }

    pub(crate) fn start(
        &mut self,
        runtime: &Handle,
        client: Client,
        snapshot: InventorySnapshot,
        model: watch::Receiver<Option<InventorySnapshot>>,
        cancellation: Cancellation,
    ) {
        if self.task.is_some() {
            return;
        }
        let Some(requests) = self.requests.take() else { return };
        let Some(refresh_receiver) = self.refresh_receiver.take() else { return };
        let (stop_tx, stop_rx) = oneshot::channel();
        self.stop_tx = Some(stop_tx);
        self.task = Some(runtime.spawn(run_scheduler(
            client,
            snapshot,
            model,
            requests,
            self.events_tx.clone(),
            Arc::clone(&self.in_flight_count),
            Arc::clone(&self.handle.requests.background_queue_depth),
            Arc::clone(&self.handle.requests.background_queue_peak),
            refresh_receiver,
            cancellation,
            stop_rx,
        )));
    }

    pub(crate) async fn next_event(&mut self) -> Option<InventoryFetchEvent> {
        self.events_rx.recv().await
    }

    pub(crate) async fn stop(&mut self) {
        if let Some(stop) = self.stop_tx.take() {
            stop.send(()).ok();
        }
        if let Some(task) = self.task.take()
            && let Err(error) = task.await
        {
            tracing::debug!(%error, "inventory fetch worker ended during session shutdown");
        }
        self.requests.take();
    }
}

#[derive(Clone)]
struct SourceCapabilities {
    owner: InventoryOwner,
    is_library: bool,
    ais: Option<CapabilityUrl>,
    legacy: Option<CapabilityUrl>,
}

#[derive(Clone)]
struct Capabilities {
    personal: SourceCapabilities,
    library: Option<SourceCapabilities>,
}

impl Capabilities {
    fn source(&self, owner: InventoryOwner) -> Option<&SourceCapabilities> {
        if self.personal.owner == owner {
            return Some(&self.personal);
        }
        self.library.as_ref().filter(|source| source.owner == owner)
    }
}

#[derive(Clone, Debug)]
struct FetchJob {
    id: u64,
    parent: Option<CategoryKey>,
    folders: Vec<CategoryKey>,
    depth: u8,
    foreground: bool,
}

enum JobOutcome {
    Ready(Vec<FolderFetch>),
    Transient { cause: FetchFailureCause, retry_after: Option<Duration> },
    SplitBatch,
    Failed(InventoryFetchError),
}

struct JobCompletion {
    job: FetchJob,
    outcome: JobOutcome,
}

struct AdaptiveConcurrency {
    limit: usize,
    backoff_until: Option<time::Instant>,
    consecutive_successes: usize,
}

impl AdaptiveConcurrency {
    fn new() -> Self {
        Self { limit: MAX_BACKGROUND_CONCURRENCY, backoff_until: None, consecutive_successes: 0 }
    }

    fn can_launch_background(&self, active_background: usize, active_total: usize, now: time::Instant) -> bool {
        active_background < self.limit && active_total < MAX_TOTAL_CONCURRENCY && self.backoff_until.is_none_or(|until| now >= until)
    }

    fn backoff_deadline(&self, now: time::Instant) -> Option<time::Instant> {
        self.backoff_until.filter(|&until| until > now)
    }

    fn on_backoff(&mut self, now: time::Instant, retry: u32, retry_after: Option<Duration>) {
        self.limit = (self.limit / 2).max(MIN_BACKGROUND_CONCURRENCY);
        self.consecutive_successes = 0;
        let until = now + retry_delay(retry, retry_after);
        self.backoff_until = Some(self.backoff_until.map_or(until, |existing| existing.max(until)));
    }

    fn on_success(&mut self, now: time::Instant) {
        if self.backoff_until.is_some_and(|until| now < until) {
            return;
        }
        if self.limit < MAX_BACKGROUND_CONCURRENCY {
            self.consecutive_successes += 1;
            if self.consecutive_successes >= RECOVERY_SUCCESS_THRESHOLD {
                self.consecutive_successes = 0;
                self.limit += 1;
            }
        }
    }
}

struct InFlightGauge(Arc<AtomicUsize>);

impl InFlightGauge {
    fn set(&self, count: usize) {
        self.0.store(count, Ordering::Relaxed);
    }
}

impl Drop for InFlightGauge {
    fn drop(&mut self) {
        self.0.store(0, Ordering::Relaxed);
    }
}

struct BackgroundQueueGauge {
    depth: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl BackgroundQueueGauge {
    fn set(&self, len: usize) {
        self.depth.store(len, Ordering::Relaxed);
        self.peak.fetch_max(len, Ordering::Relaxed);
    }
}

impl Drop for BackgroundQueueGauge {
    fn drop(&mut self) {
        self.depth.store(0, Ordering::Relaxed);
    }
}

async fn run_scheduler(
    client: Client,
    snapshot: InventorySnapshot,
    model: watch::Receiver<Option<InventorySnapshot>>,
    mut requests: mpsc::Receiver<ForegroundRequest>,
    events: mpsc::Sender<InventoryFetchEvent>,
    in_flight_count: Arc<AtomicUsize>,
    background_queue_depth: Arc<AtomicUsize>,
    background_queue_peak: Arc<AtomicUsize>,
    mut refresh_incomplete: watch::Receiver<u64>,
    cancellation: Cancellation,
    mut stop: oneshot::Receiver<()>,
) {
    let in_flight_gauge = InFlightGauge(in_flight_count);
    let background_gauge = BackgroundQueueGauge { depth: background_queue_depth, peak: background_queue_peak };
    let discovered = match discover(&client, cancellation.clone(), &mut stop).await {
        Ok(discovered) => discovered,
        Err(InventoryFetchError::Cancelled) => return,
        Err(error) => {
            let roots = iter::once(snapshot.roots().personal()).chain(snapshot.roots().library());
            for folder in roots {
                if !emit(&events, InventoryFetchEvent::Failed { folder, error: error.clone() }, &cancellation, &mut stop).await {
                    return;
                }
            }
            let _ = emit(&events, InventoryFetchEvent::BackgroundDone, &cancellation, &mut stop).await;
            return;
        }
    };
    let capabilities = make_capabilities(&snapshot, discovered);
    let personal_root = snapshot.roots().personal();
    let library_root = snapshot.roots().library();
    let mut background = VecDeque::new();
    let mut queued = HashSet::new();
    let mut foreground = VecDeque::new();
    let mut foreground_queued = HashSet::new();
    let mut foreground_completions = HashMap::<CategoryKey, Vec<oneshot::Sender<InventoryFetchOutcome>>>::new();
    let mut in_flight = HashSet::new();
    let mut failed = HashSet::new();
    let current = latest_snapshot(&model, &snapshot);
    enqueue_background(BackgroundRequest { folder: personal_root, depth: MAX_AIS_DEPTH }, &current, &failed, &mut background, &mut queued);
    if let Some(root) = library_root {
        enqueue_background(BackgroundRequest { folder: root, depth: MAX_AIS_DEPTH }, &current, &failed, &mut background, &mut queued);
    }
    let mut initial_folders = snapshot.categories().map(|category| category.key).collect::<Vec<_>>();
    initial_folders.sort_unstable();
    for folder in initial_folders {
        enqueue_background(BackgroundRequest { folder, depth: MAX_AIS_DEPTH }, &current, &failed, &mut background, &mut queued);
    }
    let mut jobs = FuturesUnordered::<BoxFuture<'static, JobCompletion>>::new();
    let mut active = HashMap::<u64, FetchJob>::new();
    let mut active_background = 0usize;
    let mut retries = HashMap::<CategoryKey, u32>::new();
    let mut concurrency = AdaptiveConcurrency::new();
    let mut next_job_id = 0u64;
    let mut requests_open = true;
    let mut background_done = false;

    loop {
        in_flight_gauge.set(in_flight.len());
        background_gauge.set(background.len());
        while let Ok(request) = requests.try_recv() {
            queue_foreground_request(request, &mut foreground, &mut foreground_queued, &mut foreground_completions);
        }
        if refresh_incomplete.has_changed().unwrap_or(false) {
            refresh_incomplete.borrow_and_update();
            let current = latest_snapshot(&model, &snapshot);
            if !enqueue_incomplete_folders(
                &current,
                &mut background,
                &mut queued,
                &mut failed,
                &mut background_done,
                &cancellation,
                &mut stop,
            )
            .await
            {
                return;
            }
        }
        if requests.is_closed() {
            requests_open = false;
        }

        while active.len() < MAX_TOTAL_CONCURRENCY {
            let Some(folder) = foreground.pop_front() else { break };
            foreground_queued.remove(&folder);
            failed.remove(&folder);
            retries.remove(&folder);
            if latest_snapshot(&model, &snapshot).is_complete(folder) {
                if let Some(completions) = foreground_completions.remove(&folder) {
                    finish_foreground_request(completions, InventoryFetchOutcome::Succeeded);
                }
                continue;
            }
            if in_flight.contains(&folder) {
                continue;
            }
            let Some(source) = capabilities.source(folder.owner()).cloned() else {
                failed.insert(folder);
                if !emit(
                    &events,
                    InventoryFetchEvent::Failed { folder, error: InventoryFetchError::CapabilityUnavailable },
                    &cancellation,
                    &mut stop,
                )
                .await
                {
                    return;
                }
                if let Some(completions) = foreground_completions.remove(&folder) {
                    finish_foreground_request(completions, InventoryFetchOutcome::Failed);
                }
                continue;
            };
            in_flight.insert(folder);
            let job = FetchJob { id: next_job_id, parent: None, folders: vec![folder], depth: 0, foreground: true };
            next_job_id = next_job_id.wrapping_add(1);
            let client = client.clone();
            let model = model.clone();
            let initial = snapshot.clone();
            let active_job = job.clone();
            let job_cancellation = cancellation.clone();
            jobs.push(
                async move {
                    let outcome = execute_job(&client, source, active_job.clone(), &model, &initial, &job_cancellation).await;
                    JobCompletion { job: active_job, outcome }
                }
                .boxed(),
            );
            active.insert(job.id, job);
        }

        let now = time::Instant::now();
        while concurrency.can_launch_background(active_background, active.len(), now) {
            let current = latest_snapshot(&model, &snapshot);
            let Some(plan) = next_background_job(&mut background, &mut queued, &current, &in_flight, &failed, |owner| {
                match capabilities.source(owner) {
                    Some(source) if source.ais.is_some() => SourceKind::Ais,
                    Some(source) if source.legacy.is_some() => SourceKind::Legacy,
                    _ => SourceKind::Unavailable,
                }
            }) else {
                break;
            };
            let job = FetchJob { id: next_job_id, parent: plan.parent, folders: plan.folders, depth: plan.depth, foreground: false };
            next_job_id = next_job_id.wrapping_add(1);
            let Some(source) = job.folders.first().and_then(|folder| capabilities.source(folder.owner())).cloned() else {
                for folder in job.folders {
                    failed.insert(folder);
                    if !emit(
                        &events,
                        InventoryFetchEvent::Failed { folder, error: InventoryFetchError::CapabilityUnavailable },
                        &cancellation,
                        &mut stop,
                    )
                    .await
                    {
                        return;
                    }
                    if let Some(completions) = foreground_completions.remove(&folder) {
                        finish_foreground_request(completions, InventoryFetchOutcome::Failed);
                    }
                }
                continue;
            };
            for folder in &job.folders {
                in_flight.insert(*folder);
            }
            let client = client.clone();
            let model = model.clone();
            let initial = snapshot.clone();
            let active_job = job.clone();
            let job_cancellation = cancellation.clone();
            jobs.push(
                async move {
                    let outcome = execute_job(&client, source, active_job.clone(), &model, &initial, &job_cancellation).await;
                    JobCompletion { job: active_job, outcome }
                }
                .boxed(),
            );
            active.insert(job.id, job);
            active_background += 1;
        }
        in_flight_gauge.set(in_flight.len());
        background_gauge.set(background.len());

        if active_background == 0 && background.is_empty() && !background_done {
            background_done = true;
            if !emit(&events, InventoryFetchEvent::BackgroundDone, &cancellation, &mut stop).await {
                return;
            }
            continue;
        }

        let backoff_deadline = if !background.is_empty() { concurrency.backoff_deadline(time::Instant::now()) } else { None };
        let backoff_sleep = async {
            if let Some(deadline) = backoff_deadline {
                time::sleep_until(deadline).await;
            }
        };

        tokio::select! {
            biased;
            _ = cancellation.clone() => return,
            _ = &mut stop => return,
            request = requests.recv(), if requests_open => {
                match request {
                    Some(request) => queue_foreground_request(
                        request,
                        &mut foreground,
                        &mut foreground_queued,
                        &mut foreground_completions,
                    ),
                    None => requests_open = false,
                }
            }
            _ = refresh_incomplete.changed() => {
                refresh_incomplete.borrow_and_update();
                let current = latest_snapshot(&model, &snapshot);
                if !enqueue_incomplete_folders(
                    &current,
                    &mut background,
                    &mut queued,
                    &mut failed,
                    &mut background_done,
                    &cancellation,
                    &mut stop,
                )
                .await
                {
                    return;
                }
            }
            Some(completion) = jobs.next(), if !active.is_empty() => {
                active.remove(&completion.job.id);
                if !completion.job.foreground {
                    active_background = active_background.saturating_sub(1);
                }
                for folder in &completion.job.folders {
                    in_flight.remove(folder);
                }
                match completion.outcome {
                    JobOutcome::Ready(folders) => {
                        if !completion.job.foreground {
                            concurrency.on_success(time::Instant::now());
                        }
                        for folder in &completion.job.folders {
                            retries.remove(folder);
                        }
                        if !process_completion(
                            completion.job,
                            folders,
                            &model,
                            &snapshot,
                            &mut background,
                            &mut queued,
                            &mut failed,
                            &mut foreground_completions,
                            &events,
                            &cancellation,
                            &mut stop,
                            &mut background_done,
                        )
                        .await
                        {
                            return;
                        }
                    }
                    JobOutcome::SplitBatch => {
                        let current = latest_snapshot(&model, &snapshot);
                        for folder in completion.job.folders {
                            enqueue_background(BackgroundRequest { folder, depth: 0 }, &current, &failed, &mut background, &mut queued);
                        }
                    }
                    JobOutcome::Transient { cause, retry_after } => {
                        let max_retry = completion
                            .job
                            .folders
                            .iter()
                            .map(|folder| retries.get(folder).copied().unwrap_or(0))
                            .max()
                            .unwrap_or(0);
                        concurrency.on_backoff(time::Instant::now(), max_retry, retry_after);
                        let current = latest_snapshot(&model, &snapshot);
                        for folder in completion.job.folders {
                            let attempt = retries.entry(folder).or_insert(0);
                            if *attempt < MAX_FETCH_RETRIES {
                                *attempt += 1;
                                enqueue_background(
                                    BackgroundRequest { folder, depth: completion.job.depth },
                                    &current,
                                    &failed,
                                    &mut background,
                                    &mut queued,
                                );
                            } else {
                                retries.remove(&folder);
                                failed.insert(folder);
                                if !emit(
                                    &events,
                                    InventoryFetchEvent::Failed { folder, error: InventoryFetchError::RetryExhausted(cause) },
                                    &cancellation,
                                    &mut stop,
                                )
                                .await
                                {
                                    return;
                                }
                                if let Some(completions) = foreground_completions.remove(&folder) {
                                    finish_foreground_request(completions, InventoryFetchOutcome::Failed);
                                }
                            }
                        }
                    }
                    JobOutcome::Failed(InventoryFetchError::Cancelled) => return,
                    JobOutcome::Failed(error) => {
                        for folder in completion.job.folders {
                            retries.remove(&folder);
                            failed.insert(folder);
                            if !emit(
                                &events,
                                InventoryFetchEvent::Failed { folder, error: error.clone() },
                                &cancellation,
                                &mut stop,
                            )
                            .await
                            {
                                return;
                            }
                            if let Some(completions) = foreground_completions.remove(&folder) {
                                finish_foreground_request(completions, InventoryFetchOutcome::Failed);
                            }
                        }
                    }
                }
            }
            _ = backoff_sleep, if backoff_deadline.is_some() => {}
        }
    }
}

async fn enqueue_incomplete_folders(
    current: &InventorySnapshot,
    background: &mut VecDeque<BackgroundRequest>,
    queued: &mut HashSet<CategoryKey>,
    failed: &mut HashSet<CategoryKey>,
    background_done: &mut bool,
    cancellation: &Cancellation,
    stop: &mut oneshot::Receiver<()>,
) -> bool {
    for (index, category) in current.categories().enumerate() {
        let folder = category.key;
        if !current.is_complete(folder) {
            failed.remove(&folder);
            let was_queued = queued.contains(&folder);
            enqueue_background(BackgroundRequest { folder, depth: 0 }, current, failed, background, queued);
            if !was_queued && queued.contains(&folder) {
                *background_done = false;
            }
        }
        if (index + 1).is_multiple_of(OWNER_REFRESH_SCAN_CHUNK)
            && tokio::select! {
                biased;
                _ = cancellation.clone() => true,
                _ = &mut *stop => true,
                _ = task::yield_now() => false,
            }
        {
            return false;
        }
    }
    true
}

fn queue_foreground_request(
    request: ForegroundRequest,
    foreground: &mut VecDeque<CategoryKey>,
    foreground_queued: &mut HashSet<CategoryKey>,
    completions: &mut HashMap<CategoryKey, Vec<oneshot::Sender<InventoryFetchOutcome>>>,
) {
    if foreground_queued.insert(request.folder) {
        foreground.push_back(request.folder);
    }
    if let Some(completion) = request.completion {
        completions.entry(request.folder).or_default().push(completion);
    }
}

fn finish_foreground_request(completions: Vec<oneshot::Sender<InventoryFetchOutcome>>, outcome: InventoryFetchOutcome) {
    for completion in completions {
        completion.send(outcome).ok();
    }
}

fn latest_snapshot(model: &watch::Receiver<Option<InventorySnapshot>>, initial: &InventorySnapshot) -> InventorySnapshot {
    model.borrow().clone().unwrap_or_else(|| initial.clone())
}

fn make_capabilities(snapshot: &InventorySnapshot, grants: HashMap<String, CapabilityUrl>) -> Capabilities {
    let personal_owner = snapshot.roots().personal().owner();
    let personal = SourceCapabilities {
        owner: personal_owner,
        is_library: false,
        ais: grants.get("InventoryAPIv3").cloned(),
        legacy: grants.get("FetchInventoryDescendents2").cloned(),
    };
    let library = snapshot.roots().library().map(|root| SourceCapabilities {
        owner: root.owner(),
        is_library: true,
        ais: grants.get("LibraryAPIv3").cloned(),
        legacy: grants.get("FetchLibDescendents2").cloned(),
    });
    Capabilities { personal, library }
}

async fn discover(
    client: &Client,
    cancellation: Cancellation,
    stop: &mut oneshot::Receiver<()>,
) -> Result<HashMap<String, CapabilityUrl>, InventoryFetchError> {
    let names = ["InventoryAPIv3", "LibraryAPIv3", "FetchInventoryDescendents2", "FetchLibDescendents2"];
    let mut retries = 0;
    loop {
        let result = tokio::select! {
            biased;
            _ = cancellation.clone() => return Err(InventoryFetchError::Cancelled),
            _ = &mut *stop => return Err(InventoryFetchError::Cancelled),
            result = client.request_capabilities(&names) => result,
        };
        match result {
            Ok(capabilities) => return Ok(capabilities),
            Err(error) => match classify_request_error(error) {
                RequestFailure::Cancelled => return Err(InventoryFetchError::Cancelled),
                RequestFailure::Transient { retry_after, .. } if retries < MAX_FETCH_RETRIES => {
                    wait_retry(retries, retry_after, cancellation.clone(), stop).await?;
                    retries += 1;
                }
                RequestFailure::Transient { cause, .. } => return Err(InventoryFetchError::RetryExhausted(cause)),
                RequestFailure::Forbidden => return Err(InventoryFetchError::HttpStatus(403)),
                RequestFailure::Missing(status) | RequestFailure::HttpStatus(status) => {
                    return Err(InventoryFetchError::HttpStatus(status));
                }
                RequestFailure::FolderTooLarge => return Err(InventoryFetchError::FolderTooLarge),
                RequestFailure::InvalidResponse(decode) => return Err(InventoryFetchError::InvalidResponse(decode)),
                RequestFailure::Failed => return Err(InventoryFetchError::RequestFailed),
            },
        }
    }
}

async fn execute_job(
    client: &Client,
    source: SourceCapabilities,
    job: FetchJob,
    model: &watch::Receiver<Option<InventorySnapshot>>,
    initial: &InventorySnapshot,
    cancellation: &Cancellation,
) -> JobOutcome {
    let Some(folder) = job.folders.first().copied() else {
        return JobOutcome::Failed(InventoryFetchError::InvalidResponse(DecodeError::UnexpectedStructure("empty fetch job")));
    };
    if job.foreground {
        let result = if let Some(capability) = source.ais {
            fetch_ais_with_provenance(client, &capability, folder, job.depth, source.is_library, Some(cancellation), || {
                Some(latest_snapshot(model, initial))
            })
            .await
        } else if let Some(capability) = source.legacy {
            fetch_legacy_with_provenance(client, &capability, &job.folders, source.is_library, Some(cancellation), || {
                Some(latest_snapshot(model, initial))
            })
            .await
        } else {
            Err(InventoryFetchError::CapabilityUnavailable)
        };
        return match result {
            Ok(folders) => JobOutcome::Ready(folders),
            Err(error) => JobOutcome::Failed(error),
        };
    }
    if let Some(capability) = source.ais {
        execute_background_ais(client, &capability, &job, source.is_library, model, initial).await
    } else if let Some(capability) = source.legacy {
        let request_snapshot = latest_snapshot(model, initial);
        match legacy::fetch_batch(client, &capability, &job.folders, source.is_library).await {
            Ok(mut folders) => {
                stamp_request_provenance(&mut folders, Some(&request_snapshot));
                JobOutcome::Ready(folders)
            }
            Err(RequestFailure::Cancelled) => JobOutcome::Failed(InventoryFetchError::Cancelled),
            Err(RequestFailure::Transient { cause, retry_after }) => JobOutcome::Transient { cause, retry_after },
            Err(RequestFailure::FolderTooLarge) => JobOutcome::Failed(InventoryFetchError::FolderTooLarge),
            Err(RequestFailure::InvalidResponse(decode)) => JobOutcome::Failed(InventoryFetchError::InvalidResponse(decode)),
            Err(RequestFailure::Forbidden) => JobOutcome::Failed(InventoryFetchError::HttpStatus(403)),
            Err(RequestFailure::Missing(status) | RequestFailure::HttpStatus(status)) => {
                JobOutcome::Failed(InventoryFetchError::HttpStatus(status))
            }
            Err(RequestFailure::Failed) => JobOutcome::Failed(InventoryFetchError::RequestFailed),
        }
    } else {
        JobOutcome::Failed(InventoryFetchError::CapabilityUnavailable)
    }
}

async fn execute_background_ais(
    client: &Client,
    capability: &CapabilityUrl,
    job: &FetchJob,
    is_library: bool,
    model: &watch::Receiver<Option<InventorySnapshot>>,
    initial: &InventorySnapshot,
) -> JobOutcome {
    let mut depth = job.depth.min(MAX_AIS_DEPTH);
    loop {
        let request_snapshot = latest_snapshot(model, initial);
        let result = match job.parent {
            Some(parent) if job.folders.len() > 1 => {
                ais::fetch_batch_once(client, capability, parent, &job.folders, depth, is_library).await
            }
            _ => {
                let folder = *job.folders.first().expect("fetch job contains at least one folder");
                ais::fetch_once(client, capability, folder, depth, is_library).await
            }
        };
        match result {
            Ok(mut folders) => {
                stamp_request_provenance(&mut folders, Some(&request_snapshot));
                return JobOutcome::Ready(folders);
            }
            Err(RequestFailure::Cancelled) => return JobOutcome::Failed(InventoryFetchError::Cancelled),
            Err(RequestFailure::Forbidden | RequestFailure::FolderTooLarge) if depth > 0 => {
                depth /= 2;
            }
            Err(
                RequestFailure::Forbidden
                | RequestFailure::FolderTooLarge
                | RequestFailure::Missing(_)
                | RequestFailure::InvalidResponse(_),
            ) if job.folders.len() > 1 => {
                return JobOutcome::SplitBatch;
            }
            Err(RequestFailure::Forbidden | RequestFailure::FolderTooLarge) => {
                return JobOutcome::Failed(InventoryFetchError::FolderTooLarge);
            }
            Err(RequestFailure::Transient { cause, retry_after }) => return JobOutcome::Transient { cause, retry_after },
            Err(RequestFailure::InvalidResponse(decode)) => return JobOutcome::Failed(InventoryFetchError::InvalidResponse(decode)),
            Err(RequestFailure::Missing(status) | RequestFailure::HttpStatus(status)) => {
                return JobOutcome::Failed(InventoryFetchError::HttpStatus(status));
            }
            Err(RequestFailure::Failed) => return JobOutcome::Failed(InventoryFetchError::RequestFailed),
        }
    }
}

pub(crate) async fn fetch_single_folder(
    client: &Client,
    ais_capability: Option<&CapabilityUrl>,
    legacy_capability: Option<&CapabilityUrl>,
    folder: CategoryKey,
) -> Result<FolderFetch, InventoryFetchError> {
    let mut folders = if let Some(capability) = ais_capability {
        fetch_ais(client, capability, folder, 0).await?
    } else if let Some(capability) = legacy_capability {
        fetch_legacy(client, capability, &[folder]).await?
    } else {
        return Err(InventoryFetchError::CapabilityUnavailable);
    };
    folders
        .drain(..)
        .find(|fetch| fetch.folder == folder)
        .ok_or(InventoryFetchError::InvalidResponse(DecodeError::UnexpectedStructure("missing requested folder")))
}

pub(crate) async fn fetch_single_items(
    client: &Client,
    ais_capability: Option<&CapabilityUrl>,
    legacy_capability: Option<&CapabilityUrl>,
    owner: InventoryOwner,
    item_ids: &[uuid::Uuid],
) -> Result<Vec<Item>, InventoryFetchError> {
    if item_ids.len() > MAX_INVENTORY_ITEMS {
        return Err(InventoryFetchError::FolderTooLarge);
    }
    if let Some(capability) = ais_capability {
        let mut items = Vec::with_capacity(item_ids.len());
        for item_id in item_ids {
            match retry_transient(None, || ais::fetch_item_once(client, capability, owner, *item_id)).await {
                Ok(item) => items.push(item),
                Err(RequestFailure::Cancelled) => return Err(InventoryFetchError::Cancelled),
                Err(RequestFailure::Transient { cause, .. }) => return Err(InventoryFetchError::RetryExhausted(cause)),
                Err(RequestFailure::Missing(_)) => {}
                Err(RequestFailure::FolderTooLarge) => return Err(InventoryFetchError::FolderTooLarge),
                Err(RequestFailure::InvalidResponse(decode)) => return Err(InventoryFetchError::InvalidResponse(decode)),
                Err(RequestFailure::Forbidden) => return Err(InventoryFetchError::HttpStatus(403)),
                Err(RequestFailure::HttpStatus(status)) => return Err(InventoryFetchError::HttpStatus(status)),
                Err(RequestFailure::Failed) => return Err(InventoryFetchError::RequestFailed),
            }
        }
        Ok(items)
    } else if let Some(capability) = legacy_capability {
        legacy::fetch_items(client, capability, owner, item_ids).await
    } else {
        Err(InventoryFetchError::CapabilityUnavailable)
    }
}

async fn fetch_ais(
    client: &Client,
    capability: &CapabilityUrl,
    folder: CategoryKey,
    initial_depth: u8,
) -> Result<Vec<FolderFetch>, InventoryFetchError> {
    fetch_ais_with_provenance(client, capability, folder, initial_depth, false, None, || None).await
}

async fn fetch_ais_with_provenance(
    client: &Client,
    capability: &CapabilityUrl,
    folder: CategoryKey,
    initial_depth: u8,
    is_library: bool,
    cancellation: Option<&Cancellation>,
    mut capture_snapshot: impl FnMut() -> Option<InventorySnapshot>,
) -> Result<Vec<FolderFetch>, InventoryFetchError> {
    let mut depth = initial_depth.min(MAX_AIS_DEPTH);
    loop {
        match retry_transient(cancellation, || {
            let request_snapshot = capture_snapshot();
            async move {
                let mut folders = ais::fetch_once(client, capability, folder, depth, is_library).await?;
                stamp_request_provenance(&mut folders, request_snapshot.as_ref());
                Ok(folders)
            }
        })
        .await
        {
            Ok(folders) => return Ok(folders),
            Err(RequestFailure::Cancelled) => return Err(InventoryFetchError::Cancelled),
            Err(RequestFailure::Forbidden | RequestFailure::FolderTooLarge) if depth > 0 => {
                depth /= 2;
            }
            Err(RequestFailure::Forbidden | RequestFailure::FolderTooLarge) => return Err(InventoryFetchError::FolderTooLarge),
            Err(RequestFailure::Transient { cause, .. }) => return Err(InventoryFetchError::RetryExhausted(cause)),
            Err(RequestFailure::InvalidResponse(decode)) => return Err(InventoryFetchError::InvalidResponse(decode)),
            Err(RequestFailure::Missing(status) | RequestFailure::HttpStatus(status)) => {
                return Err(InventoryFetchError::HttpStatus(status));
            }
            Err(RequestFailure::Failed) => return Err(InventoryFetchError::RequestFailed),
        }
    }
}

async fn fetch_legacy(
    client: &Client,
    capability: &CapabilityUrl,
    folders: &[CategoryKey],
) -> Result<Vec<FolderFetch>, InventoryFetchError> {
    fetch_legacy_with_provenance(client, capability, folders, false, None, || None).await
}

async fn fetch_legacy_with_provenance(
    client: &Client,
    capability: &CapabilityUrl,
    folders: &[CategoryKey],
    is_library: bool,
    cancellation: Option<&Cancellation>,
    mut capture_snapshot: impl FnMut() -> Option<InventorySnapshot>,
) -> Result<Vec<FolderFetch>, InventoryFetchError> {
    match retry_transient(cancellation, || {
        let request_snapshot = capture_snapshot();
        async move {
            let mut folders = legacy::fetch_batch(client, capability, folders, is_library).await?;
            stamp_request_provenance(&mut folders, request_snapshot.as_ref());
            Ok(folders)
        }
    })
    .await
    {
        Ok(folders) => Ok(folders),
        Err(RequestFailure::Cancelled) => Err(InventoryFetchError::Cancelled),
        Err(RequestFailure::Transient { cause, .. }) => Err(InventoryFetchError::RetryExhausted(cause)),
        Err(RequestFailure::FolderTooLarge) => Err(InventoryFetchError::FolderTooLarge),
        Err(RequestFailure::InvalidResponse(decode)) => Err(InventoryFetchError::InvalidResponse(decode)),
        Err(RequestFailure::Forbidden) => Err(InventoryFetchError::HttpStatus(403)),
        Err(RequestFailure::Missing(status) | RequestFailure::HttpStatus(status)) => Err(InventoryFetchError::HttpStatus(status)),
        Err(RequestFailure::Failed) => Err(InventoryFetchError::RequestFailed),
    }
}

fn stamp_request_provenance(folders: &mut [FolderFetch], snapshot: Option<&InventorySnapshot>) {
    let Some(snapshot) = snapshot else { return };
    for fetch in folders {
        fetch.provenance = FolderFetchProvenance::StartedAt {
            folder: snapshot.folder_generation(fetch.folder),
            owner: snapshot.owner_refresh_generation(fetch.folder.owner()),
        };
    }
}

async fn retry_transient<T, F>(cancellation: Option<&Cancellation>, mut attempt: impl FnMut() -> F) -> Result<T, RequestFailure>
where
    F: Future<Output = Result<T, RequestFailure>>,
{
    let mut retries = 0;
    loop {
        match attempt().await {
            Err(RequestFailure::Transient { retry_after, .. }) if retries < MAX_FETCH_RETRIES => {
                let delay = retry_delay(retries, retry_after);
                if let Some(cancellation) = cancellation {
                    tokio::select! {
                        biased;
                        _ = cancellation.clone() => return Err(RequestFailure::Cancelled),
                        _ = time::sleep(delay) => {}
                    }
                } else {
                    time::sleep(delay).await;
                }
                retries += 1;
            }
            result => return result,
        }
    }
}

async fn wait_retry(
    retries: u32,
    retry_after: Option<Duration>,
    cancellation: Cancellation,
    stop: &mut oneshot::Receiver<()>,
) -> Result<(), InventoryFetchError> {
    tokio::select! {
        biased;
        _ = cancellation => Err(InventoryFetchError::Cancelled),
        _ = &mut *stop => Err(InventoryFetchError::Cancelled),
        _ = time::sleep(retry_delay(retries, retry_after)) => Ok(()),
    }
}

fn retry_delay(retries: u32, retry_after: Option<Duration>) -> Duration {
    retry_after
        .map(|delay| delay.min(MAX_RETRY_AFTER))
        .unwrap_or_else(|| RETRY_BASE_DELAY.saturating_mul(2_u32.saturating_pow(retries.min(MAX_FETCH_RETRIES))))
}

async fn process_completion(
    job: FetchJob,
    folders: Vec<FolderFetch>,
    model: &watch::Receiver<Option<InventorySnapshot>>,
    initial: &InventorySnapshot,
    background: &mut VecDeque<BackgroundRequest>,
    queued: &mut HashSet<CategoryKey>,
    failed: &mut HashSet<CategoryKey>,
    foreground_completions: &mut HashMap<CategoryKey, Vec<oneshot::Sender<InventoryFetchOutcome>>>,
    events: &mpsc::Sender<InventoryFetchEvent>,
    cancellation: &Cancellation,
    stop: &mut oneshot::Receiver<()>,
    background_done: &mut bool,
) -> bool {
    for folder in folders {
        let folder_key = folder.folder;
        let (applied, outcome) = oneshot::channel();
        if !emit(events, InventoryFetchEvent::Folder { fetch: folder, applied }, cancellation, stop).await {
            return false;
        }
        let outcome = tokio::select! {
            biased;
            _ = cancellation.clone() => return false,
            _ = &mut *stop => return false,
            result = outcome => match result {
                Ok(outcome) => outcome,
                Err(_) => return false,
            },
        };
        match outcome {
            super::FolderApplyOutcome::Rejected => {
                failed.insert(folder_key);
                if let Some(completions) = foreground_completions.remove(&folder_key) {
                    finish_foreground_request(completions, InventoryFetchOutcome::Failed);
                }
                continue;
            }
            super::FolderApplyOutcome::Stale => {
                failed.remove(&folder_key);
                let current = latest_snapshot(model, initial);
                let complete = current.is_complete(folder_key);
                if let Some(completions) = foreground_completions.remove(&folder_key) {
                    finish_foreground_request(
                        completions,
                        if complete { InventoryFetchOutcome::Succeeded } else { InventoryFetchOutcome::Failed },
                    );
                }
                if current.category(folder_key).is_some() && !complete {
                    let was_queued = queued.contains(&folder_key);
                    enqueue_background(BackgroundRequest { folder: folder_key, depth: 0 }, &current, failed, background, queued);
                    if !was_queued && queued.contains(&folder_key) {
                        *background_done = false;
                    }
                }
                continue;
            }
            super::FolderApplyOutcome::Applied => {
                failed.remove(&folder_key);
            }
        }
        let current = latest_snapshot(model, initial);
        if let Some(completions) = foreground_completions.remove(&folder_key) {
            finish_foreground_request(
                completions,
                if current.is_complete(folder_key) { InventoryFetchOutcome::Succeeded } else { InventoryFetchOutcome::Failed },
            );
        }
        let child_depth = if job.depth > 0 { MAX_AIS_DEPTH } else { 0 };
        for category in current.children(folder_key).filter_map(|node| match node.id() {
            inventory::InventoryNodeId::Category(id) => Some(CategoryKey::new(folder_key.owner(), id)),
            inventory::InventoryNodeId::Item(_) => None,
        }) {
            let was_queued = queued.contains(&category);
            enqueue_background(BackgroundRequest { folder: category, depth: child_depth }, &current, failed, background, queued);
            if !was_queued && queued.contains(&category) {
                *background_done = false;
            }
        }
        if !current.is_complete(folder_key) && job.depth > 0 {
            let was_queued = queued.contains(&folder_key);
            enqueue_background(BackgroundRequest { folder: folder_key, depth: 0 }, &current, failed, background, queued);
            if !was_queued && queued.contains(&folder_key) {
                *background_done = false;
            }
        }
    }
    for folder in job.folders {
        if let Some(completions) = foreground_completions.remove(&folder) {
            let complete = latest_snapshot(model, initial).is_complete(folder);
            finish_foreground_request(completions, if complete { InventoryFetchOutcome::Succeeded } else { InventoryFetchOutcome::Failed });
        }
    }
    true
}

async fn emit(
    events: &mpsc::Sender<InventoryFetchEvent>,
    event: InventoryFetchEvent,
    cancellation: &Cancellation,
    stop: &mut oneshot::Receiver<()>,
) -> bool {
    tokio::select! {
        biased;
        _ = cancellation.clone() => false,
        _ = &mut *stop => false,
        result = events.send(event) => result.is_ok(),
    }
}

#[cfg(test)]
#[path = "scheduler_tests.rs"]
mod tests;
