use std::{
    collections::{HashMap, HashSet, VecDeque},
    error::Error,
    fmt, iter,
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
    FolderFetch, FolderFetchProvenance, InventoryFetchError, InventoryFetchEvent, MAX_AIS_DEPTH, MAX_FETCH_RETRIES, RequestFailure, ais,
    classify_request_error, legacy,
    state::{BackgroundRequest, SourceKind, enqueue_background, next_background_job},
};
use crate::inventory::InventoryEditHandle;

const REQUEST_CAPACITY: usize = 1024;
const RESULT_CAPACITY: usize = 2;
const BACKGROUND_CONCURRENCY: usize = 2;
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
    refresh_incomplete: watch::Sender<u64>,
}

impl InventoryFetchHandle {
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
            TrySendError::Full(_) => InventoryFetchRequestError::QueueFull,
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
        let requests = InventoryFetchRequests { sender: request_sender, refresh_incomplete };
        Self {
            handle: InventoryFetchHandle { requests, status: status_rx, edits },
            requests: Some(requests_rx),
            events_tx,
            events_rx,
            status_tx,
            refresh_receiver: Some(refresh_receiver),
            stop_tx: None,
            task: None,
        }
    }

    pub(crate) fn handle(&self) -> InventoryFetchHandle {
        self.handle.clone()
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
struct BackgroundJob {
    id: u64,
    folders: Vec<CategoryKey>,
    depth: u8,
}

struct JobCompletion {
    job: BackgroundJob,
    result: Result<Vec<FolderFetch>, InventoryFetchError>,
}

async fn run_scheduler(
    client: Client,
    snapshot: InventorySnapshot,
    model: watch::Receiver<Option<InventorySnapshot>>,
    mut requests: mpsc::Receiver<ForegroundRequest>,
    events: mpsc::Sender<InventoryFetchEvent>,
    mut refresh_incomplete: watch::Receiver<u64>,
    cancellation: Cancellation,
    mut stop: oneshot::Receiver<()>,
) {
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
        enqueue_background(BackgroundRequest { folder, depth: 0 }, &current, &failed, &mut background, &mut queued);
    }
    let mut jobs = FuturesUnordered::<BoxFuture<'static, JobCompletion>>::new();
    let mut active = HashMap::<u64, BackgroundJob>::new();
    let mut next_job_id = 0u64;
    let mut requests_open = true;
    let mut background_done = false;

    loop {
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

        if let Some(folder) = foreground.pop_front() {
            foreground_queued.remove(&folder);
            let completions = foreground_completions.remove(&folder).unwrap_or_default();
            if !active.is_empty() {
                let current = latest_snapshot(&model, &snapshot);
                for job in active.values() {
                    for key in &job.folders {
                        in_flight.remove(key);
                        enqueue_background(
                            BackgroundRequest { folder: *key, depth: job.depth },
                            &current,
                            &failed,
                            &mut background,
                            &mut queued,
                        );
                    }
                }
                active.clear();
                jobs = FuturesUnordered::new();
            }
            failed.remove(&folder);
            let Some(source) = capabilities.source(folder.owner()) else {
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
                finish_foreground_request(completions, InventoryFetchOutcome::Failed);
                continue;
            };
            if latest_snapshot(&model, &snapshot).is_complete(folder) {
                finish_foreground_request(completions, InventoryFetchOutcome::Succeeded);
                continue;
            }
            in_flight.insert(folder);
            let job = BackgroundJob { id: next_job_id, folders: vec![folder], depth: 0 };
            next_job_id = next_job_id.wrapping_add(1);
            let fetch = execute_job(&client, source.clone(), job.clone(), &model, &snapshot, &cancellation);
            futures::pin_mut!(fetch);
            let result = loop {
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
                    result = &mut fetch => break result,
                }
            };
            in_flight.remove(&folder);
            let completion = JobCompletion { job, result };
            if !process_completion(
                completion,
                &model,
                &snapshot,
                &mut background,
                &mut queued,
                &mut failed,
                &events,
                &cancellation,
                &mut stop,
                &mut background_done,
                Some((folder, completions)),
            )
            .await
            {
                return;
            }
            continue;
        }

        while active.len() < BACKGROUND_CONCURRENCY {
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
            let job = BackgroundJob { id: next_job_id, folders: plan.folders, depth: plan.depth };
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
                    let result = execute_job(&client, source, active_job.clone(), &model, &initial, &job_cancellation).await;
                    JobCompletion { job: active_job, result }
                }
                .boxed(),
            );
            active.insert(job.id, job);
        }

        if active.is_empty() && background.is_empty() && !background_done {
            background_done = true;
            if !emit(&events, InventoryFetchEvent::BackgroundDone, &cancellation, &mut stop).await {
                return;
            }
            continue;
        }

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
                for folder in &completion.job.folders {
                    in_flight.remove(folder);
                }
                if !process_completion(
                    completion,
                    &model,
                    &snapshot,
                    &mut background,
                    &mut queued,
                    &mut failed,
                    &events,
                    &cancellation,
                    &mut stop,
                &mut background_done,
                    None,
                ).await {
                    return;
                }
            }
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
    job: BackgroundJob,
    model: &watch::Receiver<Option<InventorySnapshot>>,
    initial: &InventorySnapshot,
    cancellation: &Cancellation,
) -> Result<Vec<FolderFetch>, InventoryFetchError> {
    let Some(folder) = job.folders.first().copied() else {
        return Err(InventoryFetchError::InvalidResponse(DecodeError::UnexpectedStructure("empty fetch job")));
    };
    if let Some(capability) = source.ais {
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
            let mut retries = 0;
            loop {
                match ais::fetch_item_once(client, capability, owner, *item_id).await {
                    Ok(item) => {
                        items.push(item);
                        break;
                    }
                    Err(RequestFailure::Cancelled) => return Err(InventoryFetchError::Cancelled),
                    Err(RequestFailure::Transient { retry_after, .. }) if retries < MAX_FETCH_RETRIES => {
                        wait_retry_cancellable(retries, retry_after, None).await?;
                        retries += 1;
                    }
                    Err(RequestFailure::Transient { cause, .. }) => return Err(InventoryFetchError::RetryExhausted(cause)),
                    Err(RequestFailure::Missing(_)) => break,
                    Err(RequestFailure::FolderTooLarge) => return Err(InventoryFetchError::FolderTooLarge),
                    Err(RequestFailure::InvalidResponse(decode)) => return Err(InventoryFetchError::InvalidResponse(decode)),
                    Err(RequestFailure::Forbidden) => return Err(InventoryFetchError::HttpStatus(403)),
                    Err(RequestFailure::HttpStatus(status)) => return Err(InventoryFetchError::HttpStatus(status)),
                    Err(RequestFailure::Failed) => return Err(InventoryFetchError::RequestFailed),
                }
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
    let mut retries = 0;
    loop {
        let request_snapshot = capture_snapshot();
        match ais::fetch_once(client, capability, folder, depth, is_library).await {
            Ok(mut folders) => {
                stamp_request_provenance(&mut folders, request_snapshot.as_ref());
                return Ok(folders);
            }
            Err(RequestFailure::Cancelled) => return Err(InventoryFetchError::Cancelled),
            Err(RequestFailure::Forbidden | RequestFailure::FolderTooLarge) if depth > 0 => {
                depth /= 2;
                retries = 0;
            }
            Err(RequestFailure::Forbidden | RequestFailure::FolderTooLarge) => return Err(InventoryFetchError::FolderTooLarge),
            Err(RequestFailure::Transient { retry_after, .. }) if retries < MAX_FETCH_RETRIES => {
                wait_retry_cancellable(retries, retry_after, cancellation).await?;
                retries += 1;
            }
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
    let mut retries = 0;
    loop {
        let request_snapshot = capture_snapshot();
        match legacy::fetch_batch(client, capability, folders, is_library).await {
            Ok(mut folders) => {
                stamp_request_provenance(&mut folders, request_snapshot.as_ref());
                return Ok(folders);
            }
            Err(RequestFailure::Cancelled) => return Err(InventoryFetchError::Cancelled),
            Err(RequestFailure::Transient { retry_after, .. }) if retries < MAX_FETCH_RETRIES => {
                wait_retry_cancellable(retries, retry_after, cancellation).await?;
                retries += 1;
            }
            Err(RequestFailure::Transient { cause, .. }) => return Err(InventoryFetchError::RetryExhausted(cause)),
            Err(RequestFailure::FolderTooLarge) => return Err(InventoryFetchError::FolderTooLarge),
            Err(RequestFailure::InvalidResponse(decode)) => return Err(InventoryFetchError::InvalidResponse(decode)),
            Err(RequestFailure::Forbidden) => return Err(InventoryFetchError::HttpStatus(403)),
            Err(RequestFailure::Missing(status) | RequestFailure::HttpStatus(status)) => {
                return Err(InventoryFetchError::HttpStatus(status));
            }
            Err(RequestFailure::Failed) => return Err(InventoryFetchError::RequestFailed),
        }
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

async fn wait_retry_cancellable(
    retries: u32,
    retry_after: Option<Duration>,
    cancellation: Option<&Cancellation>,
) -> Result<(), InventoryFetchError> {
    let delay = retry_delay(retries, retry_after);
    if let Some(cancellation) = cancellation {
        tokio::select! {
            biased;
            _ = cancellation.clone() => Err(InventoryFetchError::Cancelled),
            _ = time::sleep(delay) => Ok(()),
        }
    } else {
        time::sleep(delay).await;
        Ok(())
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
    completion: JobCompletion,
    model: &watch::Receiver<Option<InventorySnapshot>>,
    initial: &InventorySnapshot,
    background: &mut VecDeque<BackgroundRequest>,
    queued: &mut HashSet<CategoryKey>,
    failed: &mut HashSet<CategoryKey>,
    events: &mpsc::Sender<InventoryFetchEvent>,
    cancellation: &Cancellation,
    stop: &mut oneshot::Receiver<()>,
    background_done: &mut bool,
    foreground_request: Option<(CategoryKey, Vec<oneshot::Sender<InventoryFetchOutcome>>)>,
) -> bool {
    let foreground_folder = foreground_request.as_ref().map(|(folder, _)| *folder);
    let folders = match completion.result {
        Ok(folders) => folders,
        Err(InventoryFetchError::Cancelled) => return false,
        Err(error) => {
            for folder in completion.job.folders {
                failed.insert(folder);
                if !emit(events, InventoryFetchEvent::Failed { folder, error: error.clone() }, cancellation, stop).await {
                    return false;
                }
            }
            if let Some((_, completions)) = foreground_request {
                finish_foreground_request(completions, InventoryFetchOutcome::Failed);
            }
            return true;
        }
    };
    let mut foreground_outcome = None;
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
                if foreground_folder == Some(folder_key) {
                    foreground_outcome = Some(InventoryFetchOutcome::Failed);
                }
                continue;
            }
            super::FolderApplyOutcome::Stale => {
                failed.remove(&folder_key);
                let current = latest_snapshot(model, initial);
                let complete = current.is_complete(folder_key);
                if foreground_folder == Some(folder_key) {
                    foreground_outcome = Some(if complete { InventoryFetchOutcome::Succeeded } else { InventoryFetchOutcome::Failed });
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
        if foreground_folder == Some(folder_key) {
            foreground_outcome =
                Some(if current.is_complete(folder_key) { InventoryFetchOutcome::Succeeded } else { InventoryFetchOutcome::Failed });
        }
        for category in current.children(folder_key).filter_map(|node| match node.id() {
            inventory::InventoryNodeId::Category(id) => Some(CategoryKey::new(folder_key.owner(), id)),
            inventory::InventoryNodeId::Item(_) => None,
        }) {
            let was_queued = queued.contains(&category);
            enqueue_background(BackgroundRequest { folder: category, depth: 0 }, &current, failed, background, queued);
            if !was_queued && queued.contains(&category) {
                *background_done = false;
            }
        }
        if !current.is_complete(folder_key) && completion.job.depth > 0 {
            let was_queued = queued.contains(&folder_key);
            enqueue_background(BackgroundRequest { folder: folder_key, depth: 0 }, &current, failed, background, queued);
            if !was_queued && queued.contains(&folder_key) {
                *background_done = false;
            }
        }
    }
    if let Some((_, completions)) = foreground_request {
        finish_foreground_request(completions, foreground_outcome.unwrap_or(InventoryFetchOutcome::Failed));
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
