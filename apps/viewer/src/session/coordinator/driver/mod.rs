pub(super) mod inventory_sync;

use std::{
    collections::{HashMap, VecDeque},
    future::pending,
    mem,
    net::SocketAddr,
    ops::ControlFlow,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use assets::cache::AssetCache;
use futures::{
    FutureExt,
    channel::oneshot,
    future::{BoxFuture, Shared},
};
use inventory::{CategoryKey, Inventory, InventoryChild, InventoryNodeKey, InventoryPreparationCancellation, InventorySnapshot};
use network::{
    AgentSession, CircuitMotionStatus, CircuitStats, CoverageLoss, CoverageLossReason, CoverageSource, Event, Grid, PacketOrder,
    SimulatorId, capabilities::Client,
};
use protocol::{
    lludp::Message,
    login::ResponseData,
    messages::{AgentMovementComplete, CacheMissType, RequestAgentData},
};
use tokio::{
    runtime::Handle,
    sync::{oneshot as tokio_oneshot, watch},
    task,
    task::{JoinError, JoinHandle},
};
use tracing::field;
use utils::Summary;
use uuid::Uuid;
use world::{MAX_OBJECT_LOOKUP_WORK_ITEMS, ObjectLookupCursor, ObjectLookupProgress, UpdateSource};

use super::{
    events::{
        EntityUpdateContinuation, EntityUpdateProgress, MovementCompleteOutcome, ObjectUpdateFinishContinuation,
        ObjectUpdateFinishProgress, ObjectUpdatePreparation, PromotionProgress, SimulatorMessageOutcome, SimulatorScope, WorldHandoff,
        handle_simulator_message, order_evidence, route_recovery_targets,
    },
    input::{Input, OrdinaryInputTurn, select_drain_input_fair},
    inventory_bootstrap::{InventoryBootstrapReport, InventoryWorkerError, spawn_inventory_build},
    inventory_cache::{InventoryCacheEvent, InventoryCacheEventOutcome, InventoryCacheSession, InventoryDisposalPermit},
    media::{ObjectMediaApplyProgress, ObjectMediaIntake, PendingObjectMediaApply},
    presentation::{AuthenticationUpdate, FavoritesUpdate, PresentationMailbox, SessionEnd, SessionReadyUpdate, ViewerUpdate},
};
use crate::{
    asset_coordinator::AssetCoordinator,
    inspection::{InventoryFetchStats, ObjectCacheStats, RegionNetworkHandle, RegionSimStats, SessionDiagnostics, SessionQueuesSnapshot},
    inventory::{
        FolderApplyOutcome, FolderFetch, InventoryEditController, InventoryEditEvent, InventoryFetchController, InventoryFetchError,
        InventoryFetchEvent, MoveExpectation,
    },
    remembered_login::RememberedLogin,
    session::{
        AgentMotionPublisher, CameraUplink, RegionLookup, Session, SessionHandle,
        environment::{Completion, EnvironmentIntake},
        experience_environment::{EnvironmentInbox, EnvironmentQueueError, EnvironmentSender, PushOutcome},
        inbound::{ObjectMediaRequest, object_media_requests},
        name_cache::NameCache,
        object_cache::{CachedObjectContinuation, CachedObjectProgress, ObjectCacheEvent, ObjectCacheSession},
        object_media::{ObjectMediaFetcher, ObjectMediaResult},
        recovery::{RecoveryCompletion, RecoveryIntake, ScheduledRecovery, retryable_send_error, simulator_lost_error},
        terrain::{DecodedTerrain, TerrainDecodeQueue},
        world::{
            IngestSummary, MAX_OBJECTS_PER_INTAKE_QUANTUM, ObjectBatchContinuation, ObjectBatchProgress, ObjectMediaPreparationProgress,
            SessionWorld, viewer_world_limits,
        },
    },
    world_reader::{ReadRequest, WorldReadHost},
};

type Cancellation = Shared<BoxFuture<'static, ()>>;

fn network_end_after_stream_close(disconnect_requested: bool, failure: Option<String>, primary_disconnected: bool) -> SessionEnd {
    if let Some(failure) = failure {
        SessionEnd::Failed(failure)
    } else if disconnect_requested {
        SessionEnd::Requested
    } else if primary_disconnected {
        SessionEnd::Failed("The primary simulator disconnected.".into())
    } else {
        SessionEnd::Failed("The network session ended.".into())
    }
}

fn record_verified_login_async(runtime: &Handle, grid: Grid, entered_username: String, session: AgentSession) {
    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_millis() as u64);
    drop(runtime.spawn(async move {
        let (cache, _) = match NameCache::open_default() {
            Ok(opened) => opened,
            Err(error) => {
                tracing::warn!(%error, grid = grid.id(), "could not open names.sqlite to record verified login");
                return;
            }
        };
        let receipt = match cache.record_verified_login(grid, &entered_username, &session, now_ms) {
            Ok(receipt) => receipt,
            Err(error) => {
                tracing::warn!(%error, grid = grid.id(), "could not queue verified login in names.sqlite");
                if let Err(shutdown_error) = cache.shutdown() {
                    tracing::warn!(error = %shutdown_error, "could not shut down names.sqlite");
                }
                return;
            }
        };
        if let Err(error) = receipt.wait().await {
            tracing::warn!(%error, grid = grid.id(), "could not persist verified login in names.sqlite");
        }
        if let Err(error) = cache.shutdown() {
            tracing::warn!(%error, "could not shut down names.sqlite");
        }
    }));
}

enum DriverInput {
    Session(Input),
    InventoryCache(Option<InventoryCacheEvent>),
    ObjectCache(Option<ObjectCacheEvent>),
    InventoryEdit(Option<InventoryEditEvent>),
    InventoryPreparation(Result<Result<inventory_sync::PreparedInventoryFetch, inventory::InventoryError>, JoinError>),
    InventoryOwnerRefresh(Result<Result<Inventory, inventory::InventoryError>, JoinError>),
    InventoryDisposal(Option<InventoryDisposalPermit>),
    InventoryBootstrap(Result<Result<(Inventory, InventoryBootstrapReport), InventoryWorkerError>, JoinError>),
    Favorites(Result<FavoritesUpdate, JoinError>),
    Relationships,
    Removals,
    MediaWorkTurn,
}

struct PendingSimulatorObjectUpdate {
    simulator_id: SimulatorId,
    packet_order: PacketOrder,
    source: SocketAddr,
    received_at: Instant,
    primary_origin_before: Option<math::DVec3>,
    region: Option<world::Region>,
    message: Message,
    media_requests: Vec<ObjectMediaRequest>,
    agent_id: Option<Uuid>,
    stage: PendingSimulatorObjectUpdateStage,
}

enum PendingSimulatorObjectUpdateStage {
    Cache(CachedObjectContinuation),
    Batch(ObjectBatchContinuation),
    Entity(EntityUpdateContinuation),
    Finish(ObjectUpdateFinishContinuation),
    Finalize(ObjectMessageFinalization),
    Finished,
}

enum ObjectMessageFinalizationStage {
    Recovery,
    Media,
}

struct ObjectMessageFinalization {
    outcome: SimulatorMessageOutcome,
    has_summary: bool,
    recovery_targets: Vec<(Option<SimulatorId>, world::LocalAddress, CacheMissType)>,
    cancellations: Vec<(Option<world::Entity>, world::LocalAddress)>,
    cancellation_index: usize,
    cancellation_cursor: Option<ObjectLookupCursor>,
    updated: Vec<world::LocalAddress>,
    media_requests: VecDeque<ObjectMediaRequest>,
    current_media: Option<ObjectMediaRequest>,
    media_cursor: Option<ObjectLookupCursor>,
    stage: ObjectMessageFinalizationStage,
}

enum ObjectMessageFinalizationProgress {
    Pending,
    Complete,
}

impl ObjectMessageFinalization {
    fn new(world: &mut SessionWorld, mut outcome: SimulatorMessageOutcome, media_requests: Vec<ObjectMediaRequest>) -> Self {
        let has_summary = outcome.summary.is_some();
        let mut recovery_targets = Vec::new();
        let mut cancellations = Vec::new();
        if let Some((simulator_id, summary)) = outcome.summary.take() {
            world.record_ingest(&summary, simulator_id);
            recovery_targets = route_recovery_targets(world, summary.recovery_requests());
            cancellations.extend(summary.recovery_cancellations());
        }
        Self {
            outcome,
            has_summary,
            recovery_targets,
            cancellations,
            cancellation_index: 0,
            cancellation_cursor: None,
            updated: Vec::new(),
            media_requests: media_requests.into(),
            current_media: None,
            media_cursor: None,
            stage: ObjectMessageFinalizationStage::Recovery,
        }
    }
}

enum OwnerTurnInput {
    Continue,
    Cancelled,
    WorldRead(ReadRequest),
}

#[derive(Debug)]
enum OwnerTurnProgress {
    Continue,
    Cancelled,
    ReadServed,
}

async fn select_owner_turn_input(reads: &mut WorldReadHost, cancellation: &Cancellation) -> OwnerTurnInput {
    tokio::select! {
        biased;
        _ = cancellation.clone() => OwnerTurnInput::Cancelled,
        request = reads.next_request() => OwnerTurnInput::WorldRead(request),
        () = task::yield_now() => OwnerTurnInput::Continue,
    }
}

async fn serve_owner_turn(reads: &mut WorldReadHost, world: &mut SessionWorld, cancellation: &Cancellation) -> OwnerTurnProgress {
    match select_owner_turn_input(reads, cancellation).await {
        OwnerTurnInput::Continue => OwnerTurnProgress::Continue,
        OwnerTurnInput::Cancelled => OwnerTurnProgress::Cancelled,
        OwnerTurnInput::WorldRead(request) => {
            request.serve_from_owner(world.world_mut());
            OwnerTurnProgress::ReadServed
        }
    }
}

async fn serve_owner_turn_with_advance<T>(
    reads: &mut WorldReadHost,
    world: &mut SessionWorld,
    cancellation: &Cancellation,
    advance: impl FnOnce(&mut SessionWorld) -> T,
) -> (OwnerTurnProgress, Option<T>) {
    match serve_owner_turn(reads, world, cancellation).await {
        OwnerTurnProgress::Continue => (OwnerTurnProgress::Continue, Some(advance(world))),
        OwnerTurnProgress::ReadServed => (OwnerTurnProgress::ReadServed, Some(advance(world))),
        OwnerTurnProgress::Cancelled => (OwnerTurnProgress::Cancelled, None),
    }
}

pub(super) const MAX_DEFERRED_INVENTORY_EVENTS: usize = 128;
const MAX_DEFERRED_INVENTORY_BYTES: usize = 96 * 1024 * 1024;
pub(super) const MAX_DEFERRED_NETWORK_EVENT_BYTES: usize = 9 * 1024 * 1024;
const SEND_FAILURE_REPORT_INTERVAL: Duration = Duration::from_secs(10);
const DECODE_REJECTION_LOSS_REPORT_INTERVAL: Duration = Duration::from_secs(10);
const DROPPED_PUSH_REPORT_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Default, Debug, Eq, PartialEq)]
struct DecodeRejectionLosses {
    count: u64,
    sample_simulator: Option<SimulatorId>,
    sample_region: Option<u64>,
    sample_source: Option<&'static str>,
}

impl DecodeRejectionLosses {
    fn record(&mut self, simulator_id: SimulatorId, region: Option<u64>, source: &'static str) {
        self.count = self.count.saturating_add(1);
        self.sample_simulator = Some(simulator_id);
        self.sample_region = region;
        self.sample_source = Some(source);
    }

    fn report(self) {
        let Some(simulator) = self.sample_simulator else { return };
        tracing::warn!(
            count = self.count,
            simulator = simulator.get(),
            region = self.sample_region,
            source = self.sample_source,
            reason = ?CoverageLossReason::DecodeRejected,
            "world data may be incomplete"
        );
    }
}

#[derive(Default)]
struct SendFailures {
    failed: u64,
    last_error: Option<String>,
}

impl SendFailures {
    fn report(self) {
        tracing::warn!(failed = self.failed, last_error = self.last_error.as_deref(), "simulator sends failed");
    }
}

#[derive(Default)]
struct DroppedPushes {
    dropped: u64,
    invalid: u64,
    sample: Option<(SimulatorId, world::Region)>,
    last_error: Option<EnvironmentQueueError>,
}

impl DroppedPushes {
    fn record(&mut self, simulator_id: SimulatorId, region: world::Region, outcome: Result<PushOutcome, EnvironmentQueueError>) {
        match outcome {
            Ok(PushOutcome::Accepted) => return,
            Ok(PushOutcome::Invalid) => self.invalid = self.invalid.saturating_add(1),
            Err(error) => {
                self.dropped = self.dropped.saturating_add(1);
                self.last_error = Some(error);
            }
        }
        self.sample = Some((simulator_id, region));
    }

    fn report(self) {
        let Some((simulator, region)) = self.sample else { return };
        tracing::warn!(
            dropped = self.dropped,
            invalid = self.invalid,
            sample_simulator = simulator.get(),
            sample_region = region.get(),
            last_error = self.last_error.as_ref().map(field::display),
            "experience environment pushes dropped"
        );
    }
}

pub(super) fn inventory_queue_is_full(count: usize, bytes: usize) -> bool {
    count >= MAX_DEFERRED_INVENTORY_EVENTS || bytes > 0
}

enum DeferredInventoryEvent {
    Udp(Message),
    EventQueue { name: String, body: llsd::Value },
    Fetch(InventoryFetchEvent),
    Edit(InventoryEditEvent),
    RefreshFolders(Vec<CategoryKey>),
    Cache(InventoryCacheEvent),
}

struct PendingFolderPreparation {
    folder: CategoryKey,
    before: InventorySnapshot,
    applied: tokio_oneshot::Sender<FolderApplyOutcome>,
    cancellation: InventoryPreparationCancellation,
    disposal: InventoryDisposalPermit,
    task: JoinHandle<Result<inventory_sync::PreparedInventoryFetch, inventory::InventoryError>>,
}

struct PendingOwnerRefresh {
    before: InventorySnapshot,
    disposal: InventoryDisposalPermit,
    task: JoinHandle<Result<Inventory, inventory::InventoryError>>,
}

#[derive(Default)]
struct PendingHandshakeReplies {
    by_simulator: HashMap<SimulatorId, PendingHandshakeReply>,
}

struct PendingHandshakeReply {
    flags: u32,
    remaining: usize,
}

impl PendingHandshakeReplies {
    fn queue(&mut self, simulator_id: SimulatorId, flags: u32) -> bool {
        match self.by_simulator.get_mut(&simulator_id) {
            Some(reply) => {
                reply.flags = flags;
                reply.remaining = reply.remaining.saturating_add(1);
                false
            }
            None => {
                self.by_simulator.insert(simulator_id, PendingHandshakeReply { flags, remaining: 1 });
                true
            }
        }
    }

    fn take_next(&mut self, simulator_id: SimulatorId) -> Option<u32> {
        if self.by_simulator.get(&simulator_id).is_some_and(|reply| reply.remaining == 0) {
            self.by_simulator.remove(&simulator_id);
            return None;
        }
        let reply = self.by_simulator.get_mut(&simulator_id)?;
        reply.remaining -= 1;
        Some(reply.flags)
    }
}

pub(crate) struct CredentialPersistence {
    pub(crate) grid: Grid,
    pub(crate) username: String,
    pub(crate) password: String,
    pub(crate) remember_login: bool,
}

struct SessionDriver {
    session: Session,
    session_handle: SessionHandle,
    runtime: Handle,
    mailbox: PresentationMailbox,
    cancellation: Cancellation,
    network_failure: Option<String>,
    primary_disconnected: bool,
    credentials: CredentialPersistence,
    session_world: SessionWorld,
    reads: WorldReadHost,
    ordinary_input_turn: OrdinaryInputTurn,
    terrain: TerrainDecodeQueue,
    environments: EnvironmentIntake,
    environment_pushes: EnvironmentSender,
    environment_inbox: EnvironmentInbox,
    dropped_pushes: Summary<DroppedPushes>,
    send_failures: Summary<SendFailures>,
    decode_rejection_losses: Summary<DecodeRejectionLosses>,
    assets: AssetCoordinator,
    terrain_received: bool,
    primary_simulator: Option<SimulatorId>,
    handoff: WorldHandoff,
    active_session: Option<AgentSession>,
    active_login_data: Option<ResponseData>,
    active_capabilities: Option<Client>,
    simulator_capabilities: HashMap<SimulatorId, Client>,
    favorites_enabled: bool,
    favorites_load: Option<JoinHandle<FavoritesUpdate>>,
    inventory_bootstrap: Option<JoinHandle<Result<(Inventory, InventoryBootstrapReport), InventoryWorkerError>>>,
    inventory_bootstrap_dirty: bool,
    inventory: Option<Inventory>,
    inventory_cache: InventoryCacheSession,
    object_cache: ObjectCacheSession,
    object_cache_stats: watch::Sender<ObjectCacheStats>,
    pending_handshake_replies: Arc<Mutex<PendingHandshakeReplies>>,
    inventory_changes: watch::Sender<Option<InventorySnapshot>>,
    rejected_inventory_blocks: Summary<inventory_sync::InventoryRejections>,
    inventory_fetch_failures: Summary<inventory_sync::InventoryFetchFailures>,
    inventory_fetch: InventoryFetchController,
    inventory_fetch_stats: watch::Sender<InventoryFetchStats>,
    inventory_edits: InventoryEditController,
    inventory_pending_moves: HashMap<InventoryNodeKey, (MoveExpectation, tokio_oneshot::Sender<()>)>,
    inventory_failed: HashMap<CategoryKey, InventoryFetchError>,
    pending_folder_preparation: Option<PendingFolderPreparation>,
    pending_owner_refresh: Option<PendingOwnerRefresh>,
    deferred_inventory: VecDeque<(DeferredInventoryEvent, usize)>,
    deferred_inventory_bytes: usize,
    inventory_background_done: bool,
    inventory_overload_recovery_pending: bool,
    pending_lifecycle_barrier: Option<Event>,
    inventory_last_failure: Option<(CategoryKey, InventoryFetchError)>,
    camera: CameraUplink,
    agent_motion: AgentMotionPublisher,
    motion_statuses: HashMap<SimulatorId, watch::Receiver<CircuitMotionStatus>>,
    circuit_stats: HashMap<SimulatorId, watch::Receiver<CircuitStats>>,
    region_motion_mapping: HashMap<world::Region, SimulatorId>,
    region_motion_sender: watch::Sender<Vec<(world::Region, watch::Receiver<CircuitMotionStatus>)>>,
    network_diagnostics: watch::Sender<Vec<RegionNetworkHandle>>,
    region_sim_stats: HashMap<world::Region, RegionSimStats>,
    sim_stats: watch::Sender<HashMap<world::Region, RegionSimStats>>,
    session_queues: watch::Sender<SessionQueuesSnapshot>,
    region_lookup: Option<RegionLookup>,
    media: ObjectMediaIntake,
    recovery: RecoveryIntake,
    pending_object_update: Option<PendingSimulatorObjectUpdate>,
    pending_media_apply: Option<PendingObjectMediaApply>,
}

impl SessionDriver {
    fn inventory_observations_deferred(&self) -> bool {
        self.pending_folder_preparation.is_some()
            || self.pending_owner_refresh.is_some()
            || !self.deferred_inventory.is_empty()
            || self.inventory_overload_recovery_pending
    }

    fn defer_inventory(&mut self, event: DeferredInventoryEvent) {
        debug_assert!(self.deferred_inventory.len() < MAX_DEFERRED_INVENTORY_EVENTS);
        let bytes = deferred_inventory_event_bytes(&event);
        debug_assert!(self.can_defer_network_inventory(1, bytes));
        self.deferred_inventory_bytes = self.deferred_inventory_bytes.saturating_add(bytes);
        self.deferred_inventory.push_back((event, bytes));
    }

    fn can_defer_network_inventory(&self, count: usize, bytes: usize) -> bool {
        self.deferred_inventory.len().checked_add(count).is_some_and(|len| len <= MAX_DEFERRED_INVENTORY_EVENTS)
            && bytes <= MAX_DEFERRED_INVENTORY_BYTES.saturating_sub(self.deferred_inventory_bytes)
    }

    fn defer_network_inventory(&mut self, bytes: usize, event: impl FnOnce() -> DeferredInventoryEvent) -> bool {
        if bytes > MAX_DEFERRED_NETWORK_EVENT_BYTES || !self.can_defer_network_inventory(1, bytes) {
            return false;
        }
        self.deferred_inventory_bytes = self.deferred_inventory_bytes.saturating_add(bytes);
        self.deferred_inventory.push_back((event(), bytes));
        true
    }

    fn handle_deferred_inventory(&mut self, (event, bytes): (DeferredInventoryEvent, usize), disposal: InventoryDisposalPermit) {
        self.deferred_inventory_bytes = self.deferred_inventory_bytes.saturating_sub(bytes);
        match event {
            DeferredInventoryEvent::Udp(message) => self.handle_inventory_udp_now(&message, disposal),
            DeferredInventoryEvent::EventQueue { name, body } => self.handle_inventory_event_queue_now(&name, &body, disposal),
            DeferredInventoryEvent::Fetch(event) => self.handle_inventory_fetch_now(event, disposal),
            DeferredInventoryEvent::Edit(event) => self.handle_inventory_edit_event_now(event, disposal),
            DeferredInventoryEvent::RefreshFolders(folders) => {
                if !self.mark_inventory_folders_incomplete(folders, disposal) {
                    tracing::debug!("inventory folder refresh could not be queued after a stale edit confirmation");
                }
            }
            DeferredInventoryEvent::Cache(event) => {
                drop(disposal);
                self.handle_inventory_cache_event_now(event);
            }
        }
    }

    async fn reject_deferred_inventory(&mut self) {
        if self.deferred_inventory.is_empty() {
            return;
        }
        let disposal = self.inventory_cache.reserve_disposal().await.expect("inventory reclaimer remains open until session close");
        let deferred_inventory = mem::take(&mut self.deferred_inventory);
        let mut payloads: Vec<Box<dyn Send>> = Vec::new();
        for (event, _) in deferred_inventory {
            match event {
                DeferredInventoryEvent::Fetch(InventoryFetchEvent::Folder { fetch, applied }) => {
                    applied.send(FolderApplyOutcome::Rejected).ok();
                    payloads.push(Box::new(fetch));
                }
                DeferredInventoryEvent::Edit(InventoryEditEvent::Apply { confirmation, applied }) => {
                    applied.send(false).ok();
                    payloads.push(Box::new(confirmation));
                }
                DeferredInventoryEvent::Edit(InventoryEditEvent::RefreshFolders { folders, refreshed }) => {
                    refreshed.send(false).ok();
                    payloads.push(Box::new(folders));
                }
                DeferredInventoryEvent::Edit(InventoryEditEvent::MoveTimedOut { refreshed, .. }) => {
                    refreshed.send(false).ok();
                }
                DeferredInventoryEvent::Edit(InventoryEditEvent::MoveAborted { removed, .. }) => {
                    removed.send(()).ok();
                }
                DeferredInventoryEvent::Edit(InventoryEditEvent::MoveStarted { expectation, confirmed, registered }) => {
                    payloads.push(Box::new((expectation, confirmed, registered)));
                }
                DeferredInventoryEvent::RefreshFolders(folders) => payloads.push(Box::new(folders)),
                DeferredInventoryEvent::Udp(message) => payloads.push(Box::new(message)),
                DeferredInventoryEvent::EventQueue { name, body } => payloads.push(Box::new((name, body))),
                DeferredInventoryEvent::Fetch(InventoryFetchEvent::Failed { .. }) => {}
                DeferredInventoryEvent::Fetch(InventoryFetchEvent::BackgroundDone) | DeferredInventoryEvent::Cache(_) => {}
            }
        }
        self.deferred_inventory_bytes = 0;
        disposal.reclaim(payloads);
    }

    fn handle_inventory_cache_event(&mut self, event: InventoryCacheEvent) {
        if self.inventory_observations_deferred() && matches!(&event, InventoryCacheEvent::RestoreBatch(_)) {
            self.defer_inventory(DeferredInventoryEvent::Cache(event));
        } else {
            self.handle_inventory_cache_event_now(event);
        }
    }

    fn handle_inventory_cache_event_now(&mut self, event: InventoryCacheEvent) {
        let outcome = self.inventory.as_mut().map(|inventory| self.inventory_cache.handle_event(&self.runtime, event, inventory));
        if let Some(InventoryCacheEventOutcome::Changed { before, after, disposal }) = outcome {
            self.process_inventory_changes(&before, &after);
            disposal.reclaim(before);
            self.inventory_changes.send_replace(Some(after));
        }
    }

    fn handle_inventory_edit_event(&mut self, event: InventoryEditEvent) {
        if self.inventory_observations_deferred() {
            self.defer_inventory(DeferredInventoryEvent::Edit(event));
        } else if let Some(disposal) = self.inventory_cache.try_reserve_disposal() {
            self.handle_inventory_edit_event_now(event, disposal);
        } else {
            self.defer_inventory(DeferredInventoryEvent::Edit(event));
        }
    }

    fn handle_inventory_fetch(&mut self, event: InventoryFetchEvent) {
        if self.inventory_observations_deferred() {
            self.defer_inventory(DeferredInventoryEvent::Fetch(event));
        } else if let Some(disposal) = self.inventory_cache.try_reserve_disposal() {
            self.handle_inventory_fetch_now(event, disposal);
        } else {
            self.defer_inventory(DeferredInventoryEvent::Fetch(event));
        }
    }

    fn finish_inventory_fetch_preparation(
        &mut self,
        result: Result<Result<inventory_sync::PreparedInventoryFetch, inventory::InventoryError>, JoinError>,
    ) {
        inventory_sync::finish_inventory_fetch_preparation(self, result);
    }

    fn new(
        session: Session,
        session_handle: SessionHandle,
        runtime: Handle,
        mailbox: PresentationMailbox,
        cancellation: oneshot::Receiver<()>,
        credentials: CredentialPersistence,
        cache: Option<Arc<AssetCache>>,
        asset_budget: assets::Budget,
    ) -> Self {
        let cancellation = async move {
            match cancellation.await {
                Ok(()) | Err(_) => {}
            }
        }
        .boxed()
        .shared();
        let session_world = SessionWorld::new(viewer_world_limits());
        let reads = WorldReadHost::new(session_world.world(), runtime.clone());
        let terrain = TerrainDecodeQueue::default();
        let environments = EnvironmentIntake::new(runtime.clone());
        let (environment_pushes, environment_inbox) = EnvironmentInbox::channel();
        let assets = AssetCoordinator::with_budget(runtime.clone(), cancellation.clone(), asset_budget, cache);
        let camera = CameraUplink::new();
        let (region_motion_sender, _) = watch::channel(Vec::new());
        let (network_diagnostics, _) = watch::channel(Vec::new());
        let (sim_stats, _) = watch::channel(HashMap::new());
        let (session_queues, _) = watch::channel(SessionQueuesSnapshot::default());
        let media = ObjectMediaIntake::new(ObjectMediaFetcher::new(runtime.clone(), cancellation.clone()));
        let recovery = RecoveryIntake::new();
        let (inventory_changes, _) = watch::channel(None);
        let inventory_edits = InventoryEditController::new();
        let inventory_fetch = InventoryFetchController::new(inventory_edits.handle());
        let (inventory_fetch_stats, _) = watch::channel(InventoryFetchStats::default());
        let inventory_cache = InventoryCacheSession::new(&runtime);
        let mut object_cache = ObjectCacheSession::new(credentials.grid);
        object_cache.start(&runtime);
        let (object_cache_stats, _) = watch::channel(object_cache.stats());
        let pending_handshake_replies = Arc::new(Mutex::new(PendingHandshakeReplies::default()));

        Self {
            session,
            session_handle,
            runtime,
            mailbox,
            cancellation,
            network_failure: None,
            primary_disconnected: false,
            credentials,
            session_world,
            reads,
            ordinary_input_turn: OrdinaryInputTurn::default(),
            terrain,
            environments,
            environment_pushes,
            environment_inbox,
            dropped_pushes: Summary::new(DROPPED_PUSH_REPORT_INTERVAL),
            send_failures: Summary::new(SEND_FAILURE_REPORT_INTERVAL),
            decode_rejection_losses: Summary::new(DECODE_REJECTION_LOSS_REPORT_INTERVAL),
            assets,
            terrain_received: false,
            primary_simulator: None,
            handoff: WorldHandoff::default(),
            active_session: None,
            active_login_data: None,
            active_capabilities: None,
            simulator_capabilities: HashMap::new(),
            favorites_enabled: false,
            favorites_load: None,
            inventory_bootstrap: None,
            inventory_bootstrap_dirty: false,
            inventory: None,
            inventory_cache,
            object_cache,
            object_cache_stats,
            pending_handshake_replies,
            inventory_changes,
            rejected_inventory_blocks: Summary::new(inventory_sync::INVENTORY_FAILURE_REPORT_INTERVAL),
            inventory_fetch_failures: Summary::new(inventory_sync::INVENTORY_FAILURE_REPORT_INTERVAL),
            inventory_fetch,
            inventory_fetch_stats,
            inventory_edits,
            inventory_pending_moves: HashMap::new(),
            inventory_failed: HashMap::new(),
            pending_folder_preparation: None,
            pending_owner_refresh: None,
            deferred_inventory: VecDeque::new(),
            deferred_inventory_bytes: 0,
            inventory_background_done: false,
            inventory_overload_recovery_pending: false,
            pending_lifecycle_barrier: None,
            inventory_last_failure: None,
            camera,
            agent_motion: AgentMotionPublisher::new(),
            motion_statuses: HashMap::new(),
            circuit_stats: HashMap::new(),
            region_motion_mapping: HashMap::new(),
            region_motion_sender,
            network_diagnostics,
            region_sim_stats: HashMap::new(),
            sim_stats,
            session_queues,
            region_lookup: None,
            media,
            recovery,
            pending_object_update: None,
            pending_media_apply: None,
        }
    }

    async fn run(mut self) {
        loop {
            if self.pending_object_update.is_none()
                && self.pending_folder_preparation.is_none()
                && self.pending_owner_refresh.is_none()
                && self.deferred_inventory.is_empty()
                && !self.inventory_overload_recovery_pending
                && let Some(event) = self.pending_lifecycle_barrier.take()
                && self.handle_network_event(Some(event)).await.is_break()
            {
                break;
            }
            self.reads.publish(self.session_world.world());
            self.object_cache.persist_overrides(&self.session_world);
            let (client_events_depth, client_events_capacity) = self.session.event_queue_depth_capacity();
            let queues_snapshot = SessionQueuesSnapshot {
                client_events_depth,
                client_events_capacity,
                experience_environment: self.environment_pushes.queue_stats(),
                object_media: self.media.queue_stats(),
            };
            self.session_queues.send_if_modified(|current| {
                if *current == queues_snapshot {
                    false
                } else {
                    *current = queues_snapshot;
                    true
                }
            });
            let cache_stats = self.object_cache.stats();
            self.object_cache_stats.send_if_modified(|current| {
                if *current == cache_stats {
                    false
                } else {
                    *current = cache_stats;
                    true
                }
            });
            let in_flight = self.inventory_fetch.in_flight_count();
            self.inventory_fetch_stats.send_if_modified(|current| {
                if current.folders_in_flight == in_flight {
                    false
                } else {
                    current.folders_in_flight = in_flight;
                    current.folders_pending = current
                        .folders_known
                        .saturating_sub(current.folders_done)
                        .saturating_sub(in_flight)
                        .saturating_sub(current.folders_failed);
                    true
                }
            });
            if self.pending_object_update.is_some() {
                let flow = self.advance_pending_owner_turn().await;
                if flow.is_break() {
                    break;
                }
                continue;
            }
            let media_flush_pending = self.media.flush_step(&mut self.session_world, self.camera.latest_camera());
            if let Some(pending) = self.pending_media_apply.as_mut()
                && matches!(self.media.advance_apply_result(&mut self.session_world, pending), ObjectMediaApplyProgress::Complete)
            {
                self.pending_media_apply = None;
            }
            let terrain_full = self.terrain.is_full();
            let inventory_queue_full = inventory_queue_is_full(self.deferred_inventory.len(), self.deferred_inventory_bytes);
            let inventory_disposal_pending = self.pending_folder_preparation.is_none()
                && self.pending_owner_refresh.is_none()
                && (!self.deferred_inventory.is_empty() || self.inventory_overload_recovery_pending);
            let disposal_sender = inventory_disposal_pending.then(|| self.inventory_cache.reclaimer()).flatten();
            let inventory_disposal = async move {
                let sender = disposal_sender?;
                sender.reserve_owned().await.ok().map(InventoryDisposalPermit::from_owned)
            };
            let inventory_disposal =
                if inventory_disposal_pending { inventory_disposal.boxed() } else { pending::<Option<InventoryDisposalPermit>>().boxed() };
            let relationships_pending = self.session_world.world().has_pending_relationships();
            let removals_pending = self.session_world.world().has_pending_removals();
            let allow_restore_batch = !self.inventory_observations_deferred();
            let event = async {
                if terrain_full || self.pending_lifecycle_barrier.is_some() {
                    pending::<Option<Event>>().await
                } else {
                    self.session.recv_event().await
                }
            };
            let inventory_fetch = async {
                if inventory_queue_full { pending::<Option<InventoryFetchEvent>>().await } else { self.inventory_fetch.next_event().await }
            };
            let inventory_cache = async { self.inventory_cache.next_event(allow_restore_batch).await };
            let inventory_edit = async {
                if inventory_queue_full { pending::<Option<InventoryEditEvent>>().await } else { self.inventory_edits.next_event().await }
            };
            let inventory_bootstrap = async {
                match self.inventory_bootstrap.as_mut() {
                    Some(task) => Some(task.await),
                    None => pending().await,
                }
            };
            let favorites_load = async {
                match self.favorites_load.as_mut() {
                    Some(task) => Some(task.await),
                    None => pending().await,
                }
            };
            let folder_preparation = async {
                match self.pending_folder_preparation.as_mut() {
                    Some(preparation) => Some((&mut preparation.task).await),
                    None => pending().await,
                }
            };
            let owner_refresh = async {
                match self.pending_owner_refresh.as_mut() {
                    Some(refresh) => Some((&mut refresh.task).await),
                    None => pending().await,
                }
            };
            let input = tokio::select! {
                input = select_drain_input_fair(
                    &mut self.reads,
                    event,
                    self.media.next_result(),
                    self.terrain.next_completion(),
                    self.environments.next_completion(),
                    self.cancellation.clone(),
                    self.camera.next_tick(),
                    self.recovery.next_completion(),
                    inventory_fetch,
                    &mut self.ordinary_input_turn,
                ) => DriverInput::Session(input),
                cache_event = inventory_cache => DriverInput::InventoryCache(cache_event),
                cache_event = self.object_cache.next_event() => DriverInput::ObjectCache(cache_event),
                edit_event = inventory_edit => DriverInput::InventoryEdit(edit_event),
                result = folder_preparation => DriverInput::InventoryPreparation(result.expect("preparation task must be pending")),
                result = owner_refresh => DriverInput::InventoryOwnerRefresh(result.expect("owner refresh task must be pending")),
                disposal = inventory_disposal => DriverInput::InventoryDisposal(disposal),
                bootstrap = inventory_bootstrap => DriverInput::InventoryBootstrap(bootstrap.expect("bootstrap task must be pending")),
                favorites = favorites_load => DriverInput::Favorites(favorites.expect("favorites task must be pending")),
                () = task::yield_now(), if relationships_pending => DriverInput::Relationships,
                () = task::yield_now(), if removals_pending => DriverInput::Removals,
                () = task::yield_now(), if media_flush_pending || self.pending_media_apply.is_some() => DriverInput::MediaWorkTurn,
            };
            let flow = match input {
                DriverInput::Relationships => self.resolve_relationships().await,
                DriverInput::Removals => self.reclaim_removals().await,
                DriverInput::MediaWorkTurn => ControlFlow::Continue(()),
                DriverInput::Favorites(result) => {
                    inventory_sync::publish_favorites_refresh(&mut self.favorites_load, &self.mailbox, result);
                    ControlFlow::Continue(())
                }
                DriverInput::InventoryPreparation(result) => {
                    self.finish_inventory_fetch_preparation(result);
                    ControlFlow::Continue(())
                }
                DriverInput::InventoryOwnerRefresh(result) => {
                    if self.finish_inventory_owner_refresh(result) {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                }
                DriverInput::InventoryDisposal(Some(disposal)) => {
                    let recovery_failed = if let Some(event) = self.deferred_inventory.pop_front() {
                        self.handle_deferred_inventory(event, disposal);
                        false
                    } else {
                        self.recover_inventory_after_overload(disposal)
                    };
                    if recovery_failed { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
                }
                DriverInput::InventoryDisposal(None) => ControlFlow::Continue(()),
                DriverInput::InventoryBootstrap(result) => {
                    self.inventory_bootstrap = None;
                    match result {
                        Ok(Ok((inventory, report))) => self.finish_inventory_bootstrap(inventory, report),
                        Ok(Err(error)) => {
                            tracing::warn!(%error, "login inventory could not be initialized");
                            self.start_favorites_load_for_active();
                        }
                        Err(error) => {
                            tracing::warn!(%error, "login inventory worker failed");
                            self.start_favorites_load_for_active();
                        }
                    }
                    ControlFlow::Continue(())
                }
                DriverInput::InventoryCache(Some(event)) => {
                    self.handle_inventory_cache_event(event);
                    ControlFlow::Continue(())
                }
                DriverInput::InventoryCache(None) => ControlFlow::Continue(()),
                DriverInput::ObjectCache(Some(event)) => {
                    if let Some((simulator_id, flags)) = self.object_cache.handle_event(event) {
                        reply_region_handshake(
                            &self.runtime,
                            Arc::clone(&self.pending_handshake_replies),
                            self.session_handle.clone(),
                            simulator_id,
                            flags,
                        );
                    }
                    ControlFlow::Continue(())
                }
                DriverInput::ObjectCache(None) => ControlFlow::Continue(()),
                DriverInput::InventoryEdit(Some(event)) => {
                    self.handle_inventory_edit_event(event);
                    ControlFlow::Continue(())
                }
                DriverInput::InventoryEdit(None) => ControlFlow::Continue(()),
                DriverInput::Session(input) => match input {
                    Input::Cancelled => {
                        self.mailbox.publish(ViewerUpdate::Ended);
                        ControlFlow::Break(())
                    }
                    Input::Event(Some(event)) if is_inventory_lifecycle_barrier(&event) && self.inventory_observations_deferred() => {
                        self.pending_lifecycle_barrier = Some(event);
                        ControlFlow::Continue(())
                    }
                    Input::Event(event) => self.handle_network_event(event).await,
                    Input::InventoryFetch(Some(event)) => {
                        self.handle_inventory_fetch(event);
                        ControlFlow::Continue(())
                    }
                    Input::InventoryFetch(None) => ControlFlow::Continue(()),
                    Input::Media(result) => {
                        self.begin_media_result(result);
                        ControlFlow::Continue(())
                    }
                    Input::Terrain(decoded) => {
                        self.handle_terrain_completion(decoded);
                        ControlFlow::Continue(())
                    }
                    Input::Environment(completion) => {
                        self.handle_environment_completion(completion);
                        ControlFlow::Continue(())
                    }
                    Input::WorldRead(request) => {
                        self.handle_world_read(request);
                        ControlFlow::Continue(())
                    }
                    Input::CameraTick => {
                        self.handle_camera_tick();
                        ControlFlow::Continue(())
                    }
                    Input::Recovery(completion) => {
                        self.handle_recovery_completion(completion);
                        ControlFlow::Continue(())
                    }
                },
            };
            if flow.is_break() {
                break;
            }
        }
        self.finish().await;
    }

    fn handle_terrain_completion(&mut self, decoded: DecodedTerrain) {
        let applied = self.session_world.apply_terrain(decoded);
        if !self.terrain_received && applied > 0 {
            self.terrain_received = true;
            tracing::info!(patches = applied, "initial terrain patches received");
        }
    }

    fn handle_environment_completion(&mut self, completion: Option<Box<Completion>>) {
        if let Some(completion) = completion {
            self.environments.complete(&mut self.session_world, *completion);
        }
    }

    fn handle_world_read(&mut self, request: ReadRequest) {
        request.serve_from_owner(self.session_world.world_mut());
    }

    async fn wait_for_owner_turn(&mut self) -> bool {
        match serve_owner_turn(&mut self.reads, &mut self.session_world, &self.cancellation).await {
            OwnerTurnProgress::Continue | OwnerTurnProgress::ReadServed => true,
            OwnerTurnProgress::Cancelled => {
                self.mailbox.publish(ViewerUpdate::Ended);
                false
            }
        }
    }

    async fn lookup_entity_on_owner_turn(&mut self, id: Uuid) -> Result<Option<world::Entity>, ()> {
        let mut cursor = None;
        loop {
            match self.session_world.lookup_entity_step(id, cursor, MAX_OBJECT_LOOKUP_WORK_ITEMS) {
                ObjectLookupProgress::Visible { entity, .. } => return Ok(Some(entity)),
                ObjectLookupProgress::Hidden { .. } => return Ok(None),
                ObjectLookupProgress::Pending { cursor: next, .. } => {
                    if !self.wait_for_owner_turn().await {
                        return Err(());
                    }
                    cursor = Some(next);
                }
            }
        }
    }

    async fn lookup_entity_at_on_owner_turn(&mut self, address: world::LocalAddress) -> Result<Option<world::Entity>, ()> {
        let mut cursor = None;
        loop {
            match self.session_world.lookup_entity_at_step(address, cursor, MAX_OBJECT_LOOKUP_WORK_ITEMS) {
                ObjectLookupProgress::Visible { entity, .. } => return Ok(Some(entity)),
                ObjectLookupProgress::Hidden { .. } => return Ok(None),
                ObjectLookupProgress::Pending { cursor: next, .. } => {
                    if !self.wait_for_owner_turn().await {
                        return Err(());
                    }
                    cursor = Some(next);
                }
            }
        }
    }

    async fn recovery_cancellations_on_owner_turn(
        &mut self,
        cancellations: impl IntoIterator<Item = (Option<world::Entity>, world::LocalAddress)>,
    ) -> Result<Vec<world::LocalAddress>, ()> {
        let mut addresses = Vec::new();
        for (entity, address) in cancellations {
            let current = self.lookup_entity_at_on_owner_turn(address).await?;
            if entity.is_some_and(|entity| current.is_some_and(|current| current != entity)) {
                continue;
            }
            addresses.push(address);
        }
        Ok(addresses)
    }

    fn handle_camera_tick(&mut self) {
        let simulator = self.primary_simulator;
        let origin = self.primary_region_origin();
        self.camera.tick(&self.session_handle, simulator, self.active_session.as_ref(), origin);
    }

    fn primary_region_origin(&self) -> Option<math::DVec3> {
        self.primary_simulator
            .and_then(|simulator| self.session_world.region_for(simulator))
            .and_then(|region| self.session_world.world().region(region))
            .and_then(|info| info.resolved_origin())
    }

    fn begin_media_result(&mut self, result: Option<ObjectMediaResult>) {
        if let Some(result) = result {
            self.pending_media_apply = self.media.begin_apply_result(result);
        } else if self.cancellation.clone().now_or_never().is_none() {
            tracing::error!("object media worker stopped; media disabled for this session");
            self.media.disable();
        }
    }

    async fn advance_pending_owner_turn(&mut self) -> ControlFlow<()> {
        if self.pending_object_update.as_ref().is_some_and(|pending| matches!(&pending.stage, PendingSimulatorObjectUpdateStage::Batch(_)))
        {
            return self.advance_pending_object_batch_turn().await;
        }
        match serve_owner_turn(&mut self.reads, &mut self.session_world, &self.cancellation).await {
            OwnerTurnProgress::Continue | OwnerTurnProgress::ReadServed => self.advance_pending_object_update().await,
            OwnerTurnProgress::Cancelled => {
                self.mailbox.publish(ViewerUpdate::Ended);
                ControlFlow::Break(())
            }
        }
    }

    async fn advance_pending_object_batch_turn(&mut self) -> ControlFlow<()> {
        let Some(mut pending) = self.pending_object_update.take() else { return ControlFlow::Continue(()) };
        let stage = mem::replace(&mut pending.stage, PendingSimulatorObjectUpdateStage::Finished);
        let PendingSimulatorObjectUpdateStage::Batch(mut continuation) = stage else {
            pending.stage = stage;
            self.pending_object_update = Some(pending);
            return ControlFlow::Continue(());
        };
        let agent_data =
            self.active_session.as_ref().map(|session| RequestAgentData { agent_id: session.agent_id, session_id: session.session_id });
        let recovery = &mut self.recovery;
        let (turn, progress) = serve_owner_turn_with_advance(&mut self.reads, &mut self.session_world, &self.cancellation, |world| {
            let mut on_object_chunk = |targets, updated| recovery.schedule(targets, updated, agent_data);
            world.advance_object_batch(&mut continuation, &mut on_object_chunk)
        })
        .await;
        match (turn, progress) {
            (OwnerTurnProgress::Cancelled, None) => {
                self.mailbox.publish(ViewerUpdate::Ended);
                ControlFlow::Break(())
            }
            (OwnerTurnProgress::Continue | OwnerTurnProgress::ReadServed, Some(ObjectBatchProgress::Pending)) => {
                pending.stage = PendingSimulatorObjectUpdateStage::Batch(continuation);
                self.pending_object_update = Some(pending);
                ControlFlow::Continue(())
            }
            (OwnerTurnProgress::Continue | OwnerTurnProgress::ReadServed, Some(ObjectBatchProgress::Complete(summary))) => {
                pending.stage = PendingSimulatorObjectUpdateStage::Finish(self.handoff.begin_object_update_finish(
                    pending.simulator_id,
                    pending.agent_id,
                    summary,
                ));
                self.pending_object_update = Some(pending);
                ControlFlow::Continue(())
            }
            (OwnerTurnProgress::Cancelled, Some(_)) | (OwnerTurnProgress::Continue | OwnerTurnProgress::ReadServed, None) => {
                unreachable!("owner turn and object batch progress must agree")
            }
        }
    }

    async fn advance_pending_object_update(&mut self) -> ControlFlow<()> {
        let Some(mut pending) = self.pending_object_update.take() else { return ControlFlow::Continue(()) };
        let stage = mem::replace(&mut pending.stage, PendingSimulatorObjectUpdateStage::Finished);
        let outcome = match stage {
            PendingSimulatorObjectUpdateStage::Cache(mut continuation) => {
                match self.object_cache.advance_resolve_cached(&mut self.session_world, &mut continuation) {
                    CachedObjectProgress::Pending => {
                        pending.stage = PendingSimulatorObjectUpdateStage::Cache(continuation);
                        self.pending_object_update = Some(pending);
                        return ControlFlow::Continue(());
                    }
                    CachedObjectProgress::Complete(batch, media_requests, overrides) => {
                        self.session_world.restore_cached_overrides(overrides);
                        if let Some((continuation, media_requests)) = self.handoff.finish_cached_object_update(
                            &mut self.session_world,
                            pending.simulator_id,
                            pending.agent_id,
                            batch,
                            media_requests,
                        ) {
                            pending.media_requests.extend(media_requests);
                            pending.stage = PendingSimulatorObjectUpdateStage::Batch(continuation);
                            self.pending_object_update = Some(pending);
                            return ControlFlow::Continue(());
                        }
                        None
                    }
                }
            }
            PendingSimulatorObjectUpdateStage::Batch(mut continuation) => {
                let agent_data = self
                    .active_session
                    .as_ref()
                    .map(|session| RequestAgentData { agent_id: session.agent_id, session_id: session.session_id });
                let progress = {
                    let recovery = &mut self.recovery;
                    let mut on_object_chunk = |targets, updated| recovery.schedule(targets, updated, agent_data);
                    self.session_world.advance_object_batch(&mut continuation, &mut on_object_chunk)
                };
                match progress {
                    ObjectBatchProgress::Pending => {
                        pending.stage = PendingSimulatorObjectUpdateStage::Batch(continuation);
                        self.pending_object_update = Some(pending);
                        return ControlFlow::Continue(());
                    }
                    ObjectBatchProgress::Complete(batch_summary) => {
                        pending.stage = PendingSimulatorObjectUpdateStage::Finish(self.handoff.begin_object_update_finish(
                            pending.simulator_id,
                            pending.agent_id,
                            batch_summary,
                        ));
                        self.pending_object_update = Some(pending);
                        return ControlFlow::Continue(());
                    }
                }
            }
            PendingSimulatorObjectUpdateStage::Entity(mut continuation) => {
                match self.handoff.advance_entity_update(&mut self.session_world, &mut continuation) {
                    EntityUpdateProgress::Pending => {
                        pending.stage = PendingSimulatorObjectUpdateStage::Entity(continuation);
                        self.pending_object_update = Some(pending);
                        return ControlFlow::Continue(());
                    }
                    EntityUpdateProgress::Complete(outcome) => {
                        pending.stage = PendingSimulatorObjectUpdateStage::Finalize(ObjectMessageFinalization::new(
                            &mut self.session_world,
                            outcome,
                            mem::take(&mut pending.media_requests),
                        ));
                        self.pending_object_update = Some(pending);
                        return ControlFlow::Continue(());
                    }
                }
            }
            PendingSimulatorObjectUpdateStage::Finish(mut continuation) => {
                match self.handoff.advance_object_update_finish(&mut self.session_world, &mut continuation) {
                    ObjectUpdateFinishProgress::Pending => {
                        pending.stage = PendingSimulatorObjectUpdateStage::Finish(continuation);
                        self.pending_object_update = Some(pending);
                        return ControlFlow::Continue(());
                    }
                    ObjectUpdateFinishProgress::Complete(outcome) => {
                        pending.stage = PendingSimulatorObjectUpdateStage::Finalize(ObjectMessageFinalization::new(
                            &mut self.session_world,
                            outcome,
                            mem::take(&mut pending.media_requests),
                        ));
                        self.pending_object_update = Some(pending);
                        return ControlFlow::Continue(());
                    }
                }
            }
            PendingSimulatorObjectUpdateStage::Finalize(mut continuation) => {
                match self.advance_object_message_finalization(
                    &mut continuation,
                    pending.simulator_id,
                    pending.packet_order,
                    pending.received_at,
                    &pending.message,
                ) {
                    ObjectMessageFinalizationProgress::Pending => {
                        pending.stage = PendingSimulatorObjectUpdateStage::Finalize(continuation);
                        self.pending_object_update = Some(pending);
                        return ControlFlow::Continue(());
                    }
                    ObjectMessageFinalizationProgress::Complete => {
                        self.finish_simulator_message_tail(
                            pending.simulator_id,
                            pending.source,
                            pending.received_at,
                            pending.primary_origin_before,
                            pending.region,
                            pending.message,
                        );
                        return ControlFlow::Continue(());
                    }
                }
            }
            PendingSimulatorObjectUpdateStage::Finished => None,
        };
        self.finish_simulator_message(
            pending.simulator_id,
            pending.packet_order,
            pending.source,
            pending.received_at,
            pending.primary_origin_before,
            pending.region,
            pending.message,
            pending.media_requests,
            outcome,
        )
        .await
    }

    fn advance_object_message_finalization(
        &mut self,
        continuation: &mut ObjectMessageFinalization,
        simulator_id: SimulatorId,
        packet_order: PacketOrder,
        received_at: Instant,
        message: &Message,
    ) -> ObjectMessageFinalizationProgress {
        let mut lookup_work = 0;
        let mut media_requests = usize::from(continuation.current_media.is_some());
        loop {
            if lookup_work >= MAX_OBJECT_LOOKUP_WORK_ITEMS {
                return ObjectMessageFinalizationProgress::Pending;
            }
            match continuation.stage {
                ObjectMessageFinalizationStage::Recovery => {
                    if let Some((expected, address)) = continuation.cancellations.get(continuation.cancellation_index).copied() {
                        match self.session_world.lookup_entity_at_step(
                            address,
                            continuation.cancellation_cursor.take(),
                            MAX_OBJECT_LOOKUP_WORK_ITEMS.saturating_sub(lookup_work),
                        ) {
                            ObjectLookupProgress::Visible { entity, work_units } => {
                                lookup_work = lookup_work.saturating_add(work_units);
                                if expected.is_none_or(|expected| expected == entity) {
                                    continuation.updated.push(address);
                                }
                                continuation.cancellation_index += 1;
                            }
                            ObjectLookupProgress::Hidden { work_units } => {
                                lookup_work = lookup_work.saturating_add(work_units);
                                continuation.updated.push(address);
                                continuation.cancellation_index += 1;
                            }
                            ObjectLookupProgress::Pending { cursor, .. } => {
                                continuation.cancellation_cursor = Some(cursor);
                                return ObjectMessageFinalizationProgress::Pending;
                            }
                        }
                        continue;
                    }

                    if continuation.has_summary {
                        let agent_data = self
                            .active_session
                            .as_ref()
                            .map(|session| RequestAgentData { agent_id: session.agent_id, session_id: session.session_id });
                        self.recovery.schedule(
                            mem::take(&mut continuation.recovery_targets),
                            mem::take(&mut continuation.updated),
                            agent_data,
                        );
                    }
                    if continuation.outcome.touched_agent
                        && self.primary_simulator == Some(simulator_id)
                        && let Some(entity) = continuation.outcome.agent_entity
                    {
                        self.publish_world_agent_motion_for_entity(entity, received_at);
                    }
                    self.environments.simulator_message(
                        &mut self.session_world,
                        simulator_id,
                        packet_order,
                        message,
                        self.primary_simulator,
                    );
                    continuation.stage = ObjectMessageFinalizationStage::Media;
                }
                ObjectMessageFinalizationStage::Media => {
                    if media_requests >= MAX_OBJECTS_PER_INTAKE_QUANTUM {
                        return ObjectMessageFinalizationProgress::Pending;
                    }
                    if continuation.current_media.is_none() {
                        let Some(request) = continuation.media_requests.pop_front() else {
                            return ObjectMessageFinalizationProgress::Complete;
                        };
                        continuation.current_media = Some(request);
                        media_requests += 1;
                    }
                    let Some(request) = continuation.current_media else {
                        return ObjectMessageFinalizationProgress::Complete;
                    };
                    match self.session_world.prepare_object_media_step(
                        simulator_id,
                        request,
                        continuation.media_cursor.take(),
                        MAX_OBJECT_LOOKUP_WORK_ITEMS.saturating_sub(lookup_work),
                    ) {
                        ObjectMediaPreparationProgress::Pending(cursor) => {
                            continuation.media_cursor = Some(cursor);
                            return ObjectMessageFinalizationProgress::Pending;
                        }
                        ObjectMediaPreparationProgress::Complete { target, work_units } => {
                            lookup_work = lookup_work.saturating_add(work_units);
                            continuation.current_media = None;
                            if let Some(target) = target {
                                self.media.queue_current(target);
                            }
                        }
                    }
                }
            }
        }
    }

    async fn handle_network_event(&mut self, event: Option<Event>) -> ControlFlow<()> {
        let Some(event) = event else {
            let end = network_end_after_stream_close(
                self.session_handle.is_disconnect_requested(),
                self.network_failure.take(),
                self.primary_disconnected,
            );
            self.mailbox.publish(match end {
                SessionEnd::Requested => ViewerUpdate::Ended,
                SessionEnd::Failed(error) => ViewerUpdate::Failed(error),
            });
            return ControlFlow::Break(());
        };
        self.handle_event(event).await
    }

    async fn handle_event(&mut self, event: Event) -> ControlFlow<()> {
        match event {
            Event::CoverageLost { loss } => {
                self.handle_coverage_loss(&loss);
                ControlFlow::Continue(())
            }
            Event::MfaRequired { challenge } => {
                self.mailbox.publish(ViewerUpdate::Authentication(AuthenticationUpdate::MfaRequired(challenge)));
                ControlFlow::Continue(())
            }
            Event::LoginAccepted { mfa_hash } => {
                self.handle_login_accepted(mfa_hash);
                ControlFlow::Continue(())
            }
            Event::SessionReady { simulator_id, session, capabilities, login_data } => {
                record_verified_login_async(&self.runtime, self.credentials.grid, self.credentials.username.clone(), (*session).clone());
                self.handoff.session_ready(&mut self.session_world, simulator_id, session.agent_id);
                if self.inventory.is_none() && self.inventory_bootstrap.is_none() {
                    self.inventory_bootstrap = Some(spawn_inventory_build(&self.runtime, session.agent_id, login_data.clone()));
                }
                self.favorites_enabled = true;
                self.environments.session_ready(&mut self.session_world, simulator_id, &capabilities, self.primary_simulator);
                if let Some(region) = self.session_world.region_for(simulator_id) {
                    self.assets.register(region, capabilities.clone());
                }
                self.on_primary_changed(simulator_id, *session, capabilities, Some(login_data));
                ControlFlow::Continue(())
            }
            Event::SimulatorPromoted { previous_simulator_id, simulator_id, session, capabilities } => {
                if let Some(region) = self.session_world.region_for(simulator_id)
                    && let Err(error) = self.environment_pushes.clear_region(region)
                {
                    tracing::error!(simulator = simulator_id.get(), region = region.get(), %error, "experience environment crossing reset could not be admitted");
                    self.mailbox.publish(ViewerUpdate::Failed("The environment update queue is unavailable.".into()));
                    return ControlFlow::Break(());
                }
                self.environments.simulator_promoted(
                    &mut self.session_world,
                    previous_simulator_id,
                    simulator_id,
                    &capabilities,
                    self.primary_simulator,
                );
                let summary = if let Some(mut continuation) = self.handoff.begin_promotion(previous_simulator_id, simulator_id) {
                    loop {
                        match self.handoff.advance_promotion(&mut self.session_world, &mut continuation) {
                            PromotionProgress::Pending => {
                                if !self.wait_for_owner_turn().await {
                                    return ControlFlow::Break(());
                                }
                            }
                            PromotionProgress::Complete(summary) => break summary,
                        }
                    }
                } else {
                    None
                };
                self.on_primary_changed(simulator_id, *session, capabilities, self.active_login_data.clone());
                if let Some(summary) = summary {
                    let touched_agent = match self.summary_touches_agent(&summary).await {
                        Ok(touched_agent) => touched_agent,
                        Err(()) => return ControlFlow::Break(()),
                    };
                    if self.process_ingest_summary(simulator_id, summary).await.is_break() {
                        return ControlFlow::Break(());
                    }
                    if touched_agent && self.publish_world_agent_motion(Instant::now()).await.is_break() {
                        return ControlFlow::Break(());
                    }
                }
                if let Some((movement, received_at)) = self.handoff.apply_promoted_movement_complete(
                    &mut self.session_world,
                    simulator_id,
                    self.active_session.as_ref().map(|session| (session.agent_id, session.session_id)),
                ) {
                    self.publish_arrival_agent_motion(simulator_id, &movement, received_at);
                }
                self.publish_region_motion();
                ControlFlow::Continue(())
            }
            Event::SimulatorConnected { simulator_id, region_handle, .. } => {
                let primary_origin_before = (self.primary_simulator == Some(simulator_id)).then(|| self.primary_region_origin()).flatten();
                self.handoff.simulator_connected(&mut self.session_world, simulator_id, region_handle);
                if region_handle.is_none() {
                    self.primary_simulator = Some(simulator_id);
                    self.camera.request_update();
                }
                if self.primary_simulator == Some(simulator_id) {
                    self.camera.region_origin_changed(primary_origin_before, self.primary_region_origin());
                }
                self.publish_region_motion();
                ControlFlow::Continue(())
            }
            Event::SimulatorMotionStatus { simulator_id, motion_status, circuit_stats } => {
                self.motion_statuses.insert(simulator_id, motion_status);
                self.circuit_stats.insert(simulator_id, circuit_stats);
                self.publish_region_motion();
                ControlFlow::Continue(())
            }
            Event::SimulatorMessage { simulator_id, sequence, packet_order, source, received_at, ping_delay, message } => {
                let terminal_inventory_overload = self.handle_inventory_udp(&message);
                if self
                    .handle_simulator_message(simulator_id, sequence, packet_order, source, received_at, ping_delay, message)
                    .await
                    .is_break()
                {
                    return ControlFlow::Break(());
                }
                if terminal_inventory_overload {
                    self.fail_inventory_overload();
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(())
            }
            Event::SendFailed(error) => {
                let due = self.send_failures.record(Instant::now(), |failures| {
                    failures.failed = failures.failed.saturating_add(1);
                    failures.last_error = Some(error);
                });
                if let Some(due) = due {
                    due.report();
                }
                ControlFlow::Continue(())
            }
            Event::EventQueueCoverageLost { reason, simulator_id, .. } => {
                let region = self.session_world.region_for(simulator_id).map(|region| region.get());
                tracing::warn!(simulator = simulator_id.get(), region, %reason, "simulator event delivery may have a gap; world data may be incomplete");
                ControlFlow::Continue(())
            }
            Event::Failed(error) => {
                self.mailbox.publish(ViewerUpdate::Failed(error));
                ControlFlow::Break(())
            }
            Event::SimulatorDisconnected { simulator_id, .. } => self.handle_simulator_disconnect(simulator_id, None),
            Event::SimulatorFailed { simulator_id, reason, .. } => self.handle_simulator_disconnect(simulator_id, Some(reason)),
            Event::CapabilityEvent { simulator_id, message, body, .. } => {
                let terminal_inventory_overload = self.handle_inventory_event_queue(&message, &body);
                self.environments.capability(&mut self.session_world, simulator_id, &message, &body, self.primary_simulator);
                if terminal_inventory_overload {
                    self.fail_inventory_overload();
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(())
            }
            Event::CapabilityEventBatch { simulator_id, events, .. } => {
                let terminal_inventory_overload = self.handle_inventory_event_queue_batch(&events);
                self.environments.capability_batch(
                    &mut self.session_world,
                    simulator_id,
                    events.iter().map(|event| (event.message.as_str(), &event.body)),
                    self.primary_simulator,
                );
                if terminal_inventory_overload {
                    self.fail_inventory_overload();
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(())
            }
            Event::CapabilityError { .. } | Event::SimulatorConnectFailed { .. } => ControlFlow::Continue(()),
            Event::SimulatorCapabilities { simulator_id, capabilities, .. } => {
                self.environments.simulator_capabilities(&mut self.session_world, simulator_id, &capabilities, self.primary_simulator);
                if let Some(region) = self.session_world.region_for(simulator_id) {
                    self.assets.register(region, capabilities.clone());
                }
                self.simulator_capabilities.insert(simulator_id, capabilities.clone());
                self.publish_network_diagnostics();
                self.media.set_client(simulator_id, capabilities);
                ControlFlow::Continue(())
            }
        }
    }

    fn handle_coverage_loss(&mut self, loss: &CoverageLoss) {
        match loss {
            CoverageLoss::Source(record) => {
                let simulator_id = record.watermark.simulator_id;
                let region = self.session_world.region_for(simulator_id).map(|region| region.get());
                let source = match record.watermark.source {
                    CoverageSource::Lludp => "lludp",
                    CoverageSource::EventQueue { .. } => "event_queue",
                };
                if record.reason == CoverageLossReason::InventoryAdmission {
                    tracing::warn!(simulator = simulator_id.get(), source, "inventory data may be incomplete after intake overload");
                } else if record.reason == CoverageLossReason::DecodeRejected {
                    if let Some(due) = self.decode_rejection_losses.record(Instant::now(), |losses| {
                        losses.record(simulator_id, region, source);
                    }) {
                        due.report();
                    }
                } else {
                    tracing::warn!(simulator = simulator_id.get(), region, source, reason = ?record.reason, "world data may be incomplete");
                }
                if record.reason == CoverageLossReason::InventoryAdmission {
                    self.inventory_overload_recovery_pending = true;
                }
                self.session.acknowledge_coverage_loss(record.watermark);
            }
            CoverageLoss::SessionOverflow { generation } => {
                tracing::warn!(generation, "world event queue overflowed; world data may be incomplete");
                self.session.acknowledge_coverage_overflow(*generation);
            }
        }
    }

    fn recover_inventory_after_overload(&mut self, disposal: InventoryDisposalPermit) -> bool {
        if !self.inventory_overload_recovery_pending {
            drop(disposal);
            return false;
        }
        let Some(before) = self.inventory.as_ref().map(Inventory::snapshot) else {
            self.inventory_overload_recovery_pending = false;
            self.inventory_bootstrap_dirty = true;
            self.inventory_cache.invalidate_for_recovery(&self.runtime);
            drop(disposal);
            return false;
        };
        self.inventory_cache.invalidate_for_recovery(&self.runtime);
        let Some(mut inventory) = self.inventory.clone() else {
            disposal.reclaim(before);
            self.inventory_overload_recovery_pending = false;
            self.fail_inventory_overload();
            return true;
        };
        let task = self.runtime.spawn_blocking(move || {
            inventory.invalidate_all_owners()?;
            Ok(inventory)
        });
        self.pending_owner_refresh = Some(PendingOwnerRefresh { before, disposal, task });
        self.inventory_overload_recovery_pending = false;
        false
    }

    fn finish_inventory_owner_refresh(&mut self, result: Result<Result<Inventory, inventory::InventoryError>, JoinError>) -> bool {
        let Some(refresh) = self.pending_owner_refresh.take() else { return false };
        let PendingOwnerRefresh { before, disposal, .. } = refresh;
        let inventory = match result {
            Ok(Ok(inventory)) => inventory,
            Ok(Err(error)) => {
                disposal.reclaim(before);
                tracing::error!(%error, "inventory owner could not be invalidated after coverage loss");
                self.fail_inventory_overload();
                return true;
            }
            Err(error) => {
                disposal.reclaim(before);
                tracing::error!(%error, "inventory owner refresh worker failed");
                self.fail_inventory_overload();
                return true;
            }
        };
        if self.inventory.as_ref().is_none_or(|current| current.revision() != before.revision()) {
            disposal.reclaim((inventory, before));
            tracing::error!("inventory changed during owner refresh preparation");
            self.fail_inventory_overload();
            return true;
        }
        let after = inventory.snapshot();
        self.process_inventory_changes(&before, &after);
        self.inventory = Some(inventory);
        self.inventory_changes.send_replace(Some(after));
        disposal.reclaim(before);
        self.inventory_failed.clear();
        self.inventory_background_done = false;
        self.inventory_last_failure = None;
        if let Err(error) = self.inventory_fetch.handle().refetch_incomplete() {
            tracing::error!(%error, "inventory folder refetch could not be scheduled after coverage loss");
            self.fail_inventory_overload();
            return true;
        }
        self.publish_inventory_status();
        false
    }

    fn note_inventory_overload(&mut self) -> bool {
        let covered = self.session.report_current_event_loss(CoverageLossReason::InventoryAdmission).is_some();
        if covered {
            self.inventory_overload_recovery_pending = true;
        }
        !covered
    }

    fn fail_inventory_overload(&self) {
        self.mailbox.publish(ViewerUpdate::Failed("Inventory updates exceeded the bounded intake capacity.".into()));
    }

    fn handle_login_accepted(&self, mfa_hash: Option<String>) {
        let remembered_login = self.credentials.remember_login.then(|| RememberedLogin {
            username: self.credentials.username.clone(),
            password: self.credentials.password.clone(),
            mfa_hash: mfa_hash.unwrap_or_default(),
        });
        self.mailbox.publish(ViewerUpdate::Authentication(AuthenticationUpdate::PersistCredentials {
            grid: self.credentials.grid,
            remembered_login,
        }));
    }

    async fn handle_simulator_message(
        &mut self,
        simulator_id: SimulatorId,
        sequence: u32,
        packet_order: PacketOrder,
        source: SocketAddr,
        received_at: Instant,
        ping_delay: Option<Duration>,
        message: Message,
    ) -> ControlFlow<()> {
        let primary_origin_before = (self.primary_simulator == Some(simulator_id)).then(|| self.primary_region_origin()).flatten();
        let region = self.session_world.region_for(simulator_id);
        if let Message::SimStats(stats) = &message
            && let Some(region) = region
        {
            self.region_sim_stats.insert(region, RegionSimStats::from_message(stats));
            self.sim_stats.send_replace(self.region_sim_stats.clone());
        }
        if let Message::RegionHandshake(handshake) = &message {
            self.object_cache.set_region_identity(simulator_id, handshake);
            if let Some(flags) = self.object_cache.begin_region_load(simulator_id, &self.runtime) {
                reply_region_handshake(
                    &self.runtime,
                    Arc::clone(&self.pending_handshake_replies),
                    self.session_handle.clone(),
                    simulator_id,
                    flags,
                );
            }
        }
        if matches!(packet_order, PacketOrder::Ordered(_))
            && Some(simulator_id) == self.primary_simulator
            && let (Some(agent), Some(region)) = (self.active_session.as_ref(), region)
        {
            let outcome = self.environment_pushes.publish(&message, agent.agent_id, region, received_at);
            if !matches!(outcome, Ok(PushOutcome::Accepted))
                && let Some(due) = self.dropped_pushes.record(Instant::now(), |pushes| pushes.record(simulator_id, region, outcome))
            {
                due.report();
            }
        }

        let mut media_requests =
            region.map(|region| object_media_requests(region, sequence, order_evidence(packet_order), &message)).unwrap_or_default();
        if let Some(region) = region {
            self.terrain.enqueue(&message, UpdateSource::new(region, sequence, order_evidence(packet_order), 0), &self.runtime);
        }

        let agent_id = self.active_session.as_ref().map(|session| session.agent_id);
        if is_object_message(&message)
            && let Some(preparation) = self.handoff.begin_object_update(
                &mut self.session_world,
                simulator_id,
                sequence,
                packet_order,
                received_at,
                ping_delay,
                &message,
                agent_id,
                &mut self.object_cache,
            )
        {
            let stage = match preparation {
                ObjectUpdatePreparation::Cached(continuation) => PendingSimulatorObjectUpdateStage::Cache(continuation),
                ObjectUpdatePreparation::Entity(continuation) => PendingSimulatorObjectUpdateStage::Entity(continuation),
                ObjectUpdatePreparation::Ready { continuation, media_requests: recovered } => {
                    media_requests.extend(recovered);
                    PendingSimulatorObjectUpdateStage::Batch(continuation)
                }
            };
            self.pending_object_update = Some(PendingSimulatorObjectUpdate {
                simulator_id,
                packet_order,
                source,
                received_at,
                primary_origin_before,
                region,
                message,
                media_requests,
                agent_id,
                stage,
            });
            return ControlFlow::Continue(());
        }
        let result = self.handoff.apply_message(
            &mut self.session_world,
            simulator_id,
            sequence,
            packet_order,
            received_at,
            ping_delay,
            &message,
            agent_id,
        );
        self.finish_simulator_message(
            simulator_id,
            packet_order,
            source,
            received_at,
            primary_origin_before,
            region,
            message,
            media_requests,
            result,
        )
        .await
    }

    async fn finish_simulator_message(
        &mut self,
        simulator_id: SimulatorId,
        packet_order: PacketOrder,
        source: SocketAddr,
        received_at: Instant,
        primary_origin_before: Option<math::DVec3>,
        region: Option<world::Region>,
        message: Message,
        media_requests: Vec<ObjectMediaRequest>,
        result: Option<SimulatorMessageOutcome>,
    ) -> ControlFlow<()> {
        let mut touched_agent = false;
        let mut agent_entity = None;
        if let Some(outcome) = result {
            touched_agent = outcome.touched_agent;
            agent_entity = outcome.agent_entity;
            if let Some((simulator_id, summary)) = outcome.summary
                && self.process_ingest_summary(simulator_id, summary).await.is_break()
            {
                return ControlFlow::Break(());
            }
        }
        if touched_agent
            && self.primary_simulator == Some(simulator_id)
            && let Some(entity) = agent_entity
        {
            self.publish_world_agent_motion_for_entity(entity, received_at);
        }

        self.environments.simulator_message(&mut self.session_world, simulator_id, packet_order, &message, self.primary_simulator);
        for request in media_requests {
            let mut cursor = None;
            loop {
                match self.session_world.prepare_object_media_step(simulator_id, request, cursor.take(), MAX_OBJECT_LOOKUP_WORK_ITEMS) {
                    ObjectMediaPreparationProgress::Pending(next) => {
                        if !self.wait_for_owner_turn().await {
                            return ControlFlow::Break(());
                        }
                        cursor = Some(next);
                    }
                    ObjectMediaPreparationProgress::Complete { target, .. } => {
                        if let Some(target) = target {
                            self.media.queue_current(target);
                        }
                        break;
                    }
                }
            }
        }

        self.finish_simulator_message_tail(simulator_id, source, received_at, primary_origin_before, region, message);
        ControlFlow::Continue(())
    }

    fn finish_simulator_message_tail(
        &mut self,
        simulator_id: SimulatorId,
        source: SocketAddr,
        received_at: Instant,
        primary_origin_before: Option<math::DVec3>,
        region: Option<world::Region>,
        message: Message,
    ) {
        if let Message::AgentMovementComplete(movement) = &message {
            let outcome = self.handoff.movement_complete(
                &mut self.session_world,
                simulator_id,
                self.active_session.as_ref().map(|session| (session.agent_id, session.session_id)),
                movement,
                received_at,
            );
            if outcome == MovementCompleteOutcome::Applied {
                self.publish_arrival_agent_motion(simulator_id, movement, received_at);
            }
        }
        let scope = if Some(simulator_id) == self.primary_simulator { SimulatorScope::Primary } else { SimulatorScope::Neighbor };
        let region_name = region.and_then(|region| self.session_world.world().region(region)).and_then(|info| info.name.as_deref());
        handle_simulator_message(
            scope,
            source,
            &message,
            self.active_session.as_ref(),
            self.region_lookup.as_ref(),
            region_name,
            &self.mailbox,
        );
        let is_region_handshake = matches!(message, Message::RegionHandshake(_));
        self.object_cache.record_update(simulator_id, message);
        if self.primary_simulator == Some(simulator_id) {
            self.camera.region_origin_changed(primary_origin_before, self.primary_region_origin());
        }
        self.publish_region_motion();
        if is_region_handshake {
            self.publish_network_diagnostics();
        }
    }

    fn on_primary_changed(
        &mut self,
        simulator_id: SimulatorId,
        session: AgentSession,
        capabilities: Client,
        login_data: Option<ResponseData>,
    ) {
        self.camera.request_update();
        self.media.set_client(simulator_id, capabilities.clone());
        self.network_failure = None;
        self.primary_disconnected = false;
        self.primary_simulator = Some(simulator_id);
        self.active_session = Some(session.clone());
        if let Some(login_data) = login_data {
            self.active_login_data = Some(login_data);
        }
        self.active_capabilities = Some(capabilities.clone());
        self.simulator_capabilities.insert(simulator_id, capabilities);
        let lookup = RegionLookup::new(self.session_handle.clone(), simulator_id, &session, self.runtime.clone());
        self.region_lookup = Some(lookup);
        self.publish_network_diagnostics();
        self.publish_session_ready_update(simulator_id, session);
        self.start_favorites_load_for_active();
    }

    fn handle_simulator_disconnect(&mut self, simulator_id: SimulatorId, reason: Option<String>) -> ControlFlow<()> {
        if self.primary_simulator == Some(simulator_id) {
            match reason {
                Some(reason) if !reason.trim().is_empty() => self.network_failure = Some(reason),
                Some(_) => self.network_failure = Some("The primary simulator failed.".into()),
                None => self.primary_disconnected = true,
            }
        }

        self.camera.simulator_disconnected(simulator_id);
        self.environments.simulator_disconnected(&mut self.session_world, simulator_id);
        if let Some(region) = self.session_world.region_for(simulator_id) {
            self.assets.retire(region);
            if self.region_sim_stats.remove(&region).is_some() {
                self.sim_stats.send_replace(self.region_sim_stats.clone());
            }
        }
        self.handoff.simulator_disconnected(&mut self.session_world, simulator_id);
        self.motion_statuses.remove(&simulator_id);
        self.circuit_stats.remove(&simulator_id);
        self.simulator_capabilities.remove(&simulator_id);
        self.publish_region_motion();
        self.publish_network_diagnostics();
        self.object_cache.forget_simulator(simulator_id);
        self.recovery.cancel_simulator(simulator_id);
        self.media.forget_simulator(simulator_id);
        if Some(simulator_id) == self.primary_simulator {
            self.primary_simulator = None;
            inventory_sync::cancel_favorites_refresh(&mut self.favorites_load);
            self.camera.request_update();
        }
        ControlFlow::Continue(())
    }

    fn publish_session_ready_update(&self, simulator_id: SimulatorId, session: AgentSession) {
        self.mailbox.publish(ViewerUpdate::SessionReady(SessionReadyUpdate {
            environments: self.environment_inbox.clone(),
            camera: self.camera.sender(),
            movement: self.camera.movement_sender(),
            agent_motion: self.agent_motion.receiver(),
            motion_circuits: self.region_motion_sender.subscribe(),
            assets: self.assets.handle(),
            session,
            region_name: self.session_world.region_name(simulator_id).map(str::to_owned),
            inventory_updates: self.inventory_changes.subscribe(),
            inventory_fetch: self.inventory.as_ref().map(|_| self.inventory_fetch.handle()),
            world_reader: self.reads.reader(),
            diagnostics: SessionDiagnostics {
                object_cache: self.object_cache_stats.subscribe(),
                inventory: self.inventory_fetch_stats.subscribe(),
                network: self.network_diagnostics.subscribe(),
                sim_stats: self.sim_stats.subscribe(),
                session_queues: self.session_queues.subscribe(),
                network_session: self.session_handle.clone(),
                inventory_fetch: self.inventory_fetch.handle(),
            },
        }));
    }

    async fn process_ingest_summary(&mut self, simulator_id: SimulatorId, summary: IngestSummary) -> ControlFlow<()> {
        self.session_world.record_ingest(&summary, simulator_id);
        let targets = route_recovery_targets(&self.session_world, summary.recovery_requests());
        let updated = match self.recovery_cancellations_on_owner_turn(summary.recovery_cancellations()).await {
            Ok(updated) => updated,
            Err(()) => return ControlFlow::Break(()),
        };
        let agent_data =
            self.active_session.as_ref().map(|session| RequestAgentData { agent_id: session.agent_id, session_id: session.session_id });
        self.recovery.schedule(targets, updated, agent_data);
        ControlFlow::Continue(())
    }

    async fn resolve_relationships(&mut self) -> ControlFlow<()> {
        let resolution = match self.session_world.resolve_relationships() {
            Ok(resolution) => resolution,
            Err(error) => {
                tracing::error!(%error, "world relationship resolution stopped");
                self.mailbox.publish(ViewerUpdate::Failed("The world state could not be updated.".into()));
                return ControlFlow::Break(());
            }
        };
        let targets = route_recovery_targets(
            &self.session_world,
            resolution.recovery.requests.into_iter().map(|address| (address, CacheMissType::Full)),
        );
        let agent_data =
            self.active_session.as_ref().map(|session| RequestAgentData { agent_id: session.agent_id, session_id: session.session_id });
        let cancellations = match self
            .recovery_cancellations_on_owner_turn(
                resolution.recovery_cancellations.into_iter().map(|cancellation| (Some(cancellation.entity()), cancellation.address())),
            )
            .await
        {
            Ok(cancellations) => cancellations,
            Err(()) => return ControlFlow::Break(()),
        };
        self.recovery.schedule(targets, cancellations, agent_data);
        if resolution.revision.is_some() && self.publish_world_agent_motion(Instant::now()).await.is_break() {
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }

    async fn reclaim_removals(&mut self) -> ControlFlow<()> {
        let progress = self.session_world.reclaim_removals();
        let targets = route_recovery_targets(
            &self.session_world,
            progress.recovery.requests.into_iter().map(|address| (address, CacheMissType::Full)),
        );
        let agent_data =
            self.active_session.as_ref().map(|session| RequestAgentData { agent_id: session.agent_id, session_id: session.session_id });
        let cancellations = match self
            .recovery_cancellations_on_owner_turn(
                progress.recovery_cancellations.into_iter().map(|cancellation| (Some(cancellation.entity()), cancellation.address())),
            )
            .await
        {
            Ok(cancellations) => cancellations,
            Err(()) => return ControlFlow::Break(()),
        };
        self.recovery.schedule(targets, cancellations, agent_data);
        if progress.revision.is_some() && self.publish_world_agent_motion(Instant::now()).await.is_break() {
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }

    async fn summary_touches_agent(&mut self, summary: &IngestSummary) -> Result<bool, ()> {
        let Some(agent_id) = self.active_session.as_ref().map(|session| session.agent_id) else { return Ok(false) };
        Ok(self.lookup_entity_on_owner_turn(agent_id).await?.is_some_and(|entity| summary.touches_entity(entity)))
    }

    async fn publish_world_agent_motion(&mut self, received_at: Instant) -> ControlFlow<()> {
        let Some(agent_id) = self.active_session.as_ref().map(|session| session.agent_id) else { return ControlFlow::Continue(()) };
        let entity = match self.lookup_entity_on_owner_turn(agent_id).await {
            Ok(entity) => entity,
            Err(()) => return ControlFlow::Break(()),
        };
        let Some(entity) = entity else { return ControlFlow::Continue(()) };
        self.publish_world_agent_motion_for_entity(entity, received_at);
        ControlFlow::Continue(())
    }

    fn publish_world_agent_motion_for_entity(&mut self, entity: world::Entity, received_at: Instant) {
        let Ok(detail) = self.session_world.world().object_detail(entity) else { return };
        self.agent_motion.publish_world_detail(&detail, received_at);
    }

    fn publish_region_motion(&mut self) {
        let mapping: HashMap<_, _> = self
            .motion_statuses
            .keys()
            .filter_map(|simulator_id| self.session_world.region_for(*simulator_id).map(|region| (region, *simulator_id)))
            .collect();
        if mapping == self.region_motion_mapping {
            return;
        }
        self.region_motion_mapping = mapping;
        let circuits = self
            .region_motion_mapping
            .iter()
            .filter_map(|(region, simulator_id)| self.motion_statuses.get(simulator_id).map(|status| (*region, status.clone())))
            .collect();
        self.region_motion_sender.send_replace(circuits);
        self.publish_network_diagnostics();
    }

    fn publish_network_diagnostics(&self) {
        let mut entries = self
            .region_motion_mapping
            .iter()
            .filter_map(|(region, simulator_id)| {
                let circuit = self.motion_statuses.get(simulator_id)?.clone();
                let stats = self.circuit_stats.get(simulator_id)?.clone();
                let region_name = self.session_world.region_name(*simulator_id).map(str::to_owned);
                let capabilities = self.simulator_capabilities.get(simulator_id).cloned();
                Some(RegionNetworkHandle {
                    region: *region,
                    simulator_id: *simulator_id,
                    region_name,
                    is_primary: self.primary_simulator == Some(*simulator_id),
                    circuit,
                    stats,
                    capabilities,
                })
            })
            .collect::<Vec<_>>();
        entries.sort_unstable_by_key(|entry| (!entry.is_primary, entry.region.get()));
        self.network_diagnostics.send_replace(entries);
    }

    fn publish_arrival_agent_motion(&mut self, simulator_id: SimulatorId, movement: &AgentMovementComplete, received_at: Instant) {
        let Some(session) = self.active_session.as_ref() else { return };
        let Some(region) = self.session_world.region_for(simulator_id) else { return };
        let Some(origin) = self.session_world.world().region(region).and_then(world::RegionInfo::resolved_origin) else {
            return;
        };
        let ping_delay = self.motion_statuses.get(&simulator_id).and_then(|status| status.borrow().measured_ping_delay);
        if self.agent_motion.publish_arrival(session.agent_id, session.session_id, movement, region, origin, received_at, ping_delay) {
            self.camera.request_update();
        }
    }

    fn handle_recovery_completion(&mut self, completion: RecoveryCompletion) {
        match completion {
            RecoveryCompletion::Deadline { aged_out, due, checked_at } => {
                self.recovery.record_aged_out(&aged_out, checked_at);
                let (exhausted, recoveries): (Vec<_>, Vec<_>) = due.into_iter().partition(ScheduledRecovery::is_exhausted);
                self.recovery.degrade_recoveries(&exhausted);
                self.recovery.record_send_limit(&exhausted, checked_at);
                if let Err(oversized) = self.recovery.start_send(&self.session_handle, recoveries) {
                    self.recovery.degrade_recoveries(&oversized.recoveries);
                    if let Some(first) = oversized.recoveries.first() {
                        tracing::error!(
                            simulator = first.simulator_id.get(),
                            region = first.key.region().get(),
                            "object recovery request exceeded its byte budget"
                        );
                    }
                }
            }
            RecoveryCompletion::Send { recoveries, result, completed_at } => {
                let Some(first) = recoveries.first() else { return };
                let simulator_id = first.simulator_id;
                if self.session_world.region_for(simulator_id).is_none() {
                    return;
                }
                match result {
                    Ok(()) => self.recovery.mark_sent(&recoveries, completed_at),
                    Err(error) if retryable_send_error(&error) => self.recovery.defer(&recoveries, completed_at),
                    Err(error) => {
                        if simulator_lost_error(&error) {
                            self.recovery.degrade_simulator(simulator_id);
                        } else {
                            self.recovery.degrade_recoveries(&recoveries);
                        }
                        self.recovery.record_send_failed(&recoveries, error, completed_at);
                    }
                }
            }
        }
    }

    fn flush_summaries(&mut self) {
        self.flush_inventory_summaries();
        if let Some(due) = self.send_failures.flush() {
            due.report();
        }
        if let Some(due) = self.decode_rejection_losses.flush() {
            due.report();
        }
        if let Some(due) = self.dropped_pushes.flush() {
            due.report();
        }
        self.recovery.flush_summaries();
        self.media.flush_summary();
        self.camera.flush_summary();
        self.session_world.flush_summaries();
    }

    async fn finish(mut self) {
        self.flush_summaries();
        if let Some(preparation) = self.pending_folder_preparation.take() {
            cancel_pending_folder_preparation(preparation).await;
        }
        if let Some(refresh) = self.pending_owner_refresh.take() {
            let PendingOwnerRefresh { before, disposal, task } = refresh;
            match task.await {
                Ok(Ok(inventory)) => disposal.reclaim((inventory, before)),
                Ok(Err(error)) => {
                    tracing::warn!(%error, "inventory owner refresh failed during shutdown");
                    disposal.reclaim(before);
                }
                Err(error) => {
                    tracing::warn!(%error, "inventory owner refresh worker failed during shutdown");
                    disposal.reclaim(before);
                }
            }
        }
        self.reject_deferred_inventory().await;
        inventory_sync::stop_favorites_refresh(&mut self.favorites_load).await;
        if let Some(inventory_bootstrap) = self.inventory_bootstrap.take() {
            inventory_bootstrap.abort();
        }
        self.inventory_pending_moves.clear();
        self.inventory_edits.stop().await;
        self.inventory_fetch.stop().await;
        self.recovery.cancel();
        self.reads.finish();
        drop(self.environments);
        drop(self.terrain);
        self.assets.stop().await;
        let inventory = self.inventory.take();
        let inventory_snapshot = self.inventory_changes.send_replace(None);
        self.inventory_cache.close_with_payload((inventory, inventory_snapshot)).await;
        self.object_cache.close().await;
        let mut failures = self.session_world.retire_all();
        while self.session_world.world().has_pending_removals() {
            drop(self.session_world.reclaim_removals());
            task::yield_now().await;
        }
        if !failures.is_empty() {
            failures = self.session_world.retire_all();
            while self.session_world.world().has_pending_removals() {
                drop(self.session_world.reclaim_removals());
                task::yield_now().await;
            }
        }
        for (simulator_id, error) in failures {
            let region = self.session_world.region_for(simulator_id).map(|region| region.get());
            tracing::warn!(simulator = simulator_id.get(), region, %error, "could not retire simulator world region on session shutdown");
        }
    }
}

fn reply_region_handshake(
    runtime: &Handle,
    pending_replies: Arc<Mutex<PendingHandshakeReplies>>,
    session_handle: SessionHandle,
    simulator_id: SimulatorId,
    flags: u32,
) {
    let mut pending = pending_replies.lock().unwrap_or_else(|error| error.into_inner());
    if !pending.by_simulator.contains_key(&simulator_id) {
        match session_handle.try_reply_region_handshake(simulator_id, flags) {
            Ok(response) => {
                drop(pending);
                drop(runtime.spawn(async move {
                    match response.await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            tracing::warn!(simulator = simulator_id.get(), %error, "region handshake reply could not be sent");
                        }
                        Err(_) => {
                            tracing::debug!(simulator = simulator_id.get(), "region handshake reply abandoned at session end");
                        }
                    }
                }));
                return;
            }
            Err(network::Error::CommandQueueFull) => {}
            Err(error) => {
                tracing::warn!(simulator = simulator_id.get(), %error, "region handshake reply could not be sent");
                return;
            }
        }
    }
    let should_drain = pending.queue(simulator_id, flags);
    drop(pending);
    if !should_drain {
        return;
    }
    drop(runtime.spawn(async move {
        loop {
            let next_flags = pending_replies.lock().unwrap_or_else(|error| error.into_inner()).take_next(simulator_id);
            let Some(next_flags) = next_flags else { break };
            if let Err(error) = session_handle.reply_region_handshake(simulator_id, next_flags).await {
                tracing::warn!(simulator = simulator_id.get(), %error, "region handshake reply could not be sent");
                pending_replies.lock().unwrap_or_else(|error| error.into_inner()).by_simulator.remove(&simulator_id);
                break;
            }
        }
    }));
}

fn cancel_pending_folder_preparation(preparation: PendingFolderPreparation) -> impl Future<Output = ()> {
    preparation.cancellation.cancel();
    async move {
        match preparation.task.await {
            Ok(Ok(inventory_sync::PreparedInventoryFetch::Prepared(prepared))) => {
                preparation.disposal.reclaim((prepared, preparation.before));
            }
            Ok(Ok(inventory_sync::PreparedInventoryFetch::Stale)) | Ok(Err(_)) | Err(_) => {
                preparation.disposal.reclaim(preparation.before);
            }
        }
        preparation.applied.send(FolderApplyOutcome::Rejected).ok();
    }
}

fn is_object_message(message: &Message) -> bool {
    matches!(
        message,
        Message::ObjectUpdate(_)
            | Message::ObjectUpdateCompressed(_)
            | Message::ImprovedTerseObjectUpdate(_)
            | Message::ObjectUpdateCached(_)
            | Message::ObjectExtraParams(_)
            | Message::AvatarAppearance(_)
            | Message::AvatarSitResponse(_)
            | Message::KillObject(_)
            | Message::GenericStreamingMessage(_)
    )
}

fn is_inventory_lifecycle_barrier(event: &Event) -> bool {
    matches!(
        event,
        Event::SessionReady { .. }
            | Event::SimulatorConnected { .. }
            | Event::SimulatorDisconnected { .. }
            | Event::SimulatorFailed { .. }
            | Event::SimulatorConnectFailed { .. }
            | Event::SimulatorPromoted { .. }
            | Event::Failed(_)
            | Event::SimulatorMessage { message: Message::AgentMovementComplete(_), .. }
    )
}

fn deferred_inventory_event_bytes(event: &DeferredInventoryEvent) -> usize {
    size_of::<DeferredInventoryEvent>().saturating_add(match event {
        DeferredInventoryEvent::Udp(_) => MAX_DEFERRED_NETWORK_EVENT_BYTES,
        DeferredInventoryEvent::EventQueue { name, body } => name.capacity().saturating_add(body.estimated_heap_bytes()),
        DeferredInventoryEvent::Fetch(InventoryFetchEvent::Folder { fetch, .. }) => estimate_folder_fetch(fetch),
        DeferredInventoryEvent::Edit(InventoryEditEvent::Apply { confirmation, .. }) => confirmation.estimated_heap_bytes(),
        DeferredInventoryEvent::Fetch(InventoryFetchEvent::Failed { .. })
        | DeferredInventoryEvent::Fetch(InventoryFetchEvent::BackgroundDone) => 0,
        DeferredInventoryEvent::Edit(InventoryEditEvent::MoveStarted { .. })
        | DeferredInventoryEvent::Edit(InventoryEditEvent::MoveAborted { .. })
        | DeferredInventoryEvent::Edit(InventoryEditEvent::MoveTimedOut { .. }) => 0,
        DeferredInventoryEvent::RefreshFolders(folders) => folders.capacity().saturating_mul(size_of::<CategoryKey>()),
        DeferredInventoryEvent::Edit(InventoryEditEvent::RefreshFolders { folders, .. }) => {
            folders.capacity().saturating_mul(size_of::<CategoryKey>())
        }
        DeferredInventoryEvent::Cache(_) => 0,
    })
}

fn estimate_folder_fetch(fetch: &FolderFetch) -> usize {
    let children_bytes = fetch.children.capacity().saturating_mul(size_of::<InventoryChild>());
    fetch.children.iter().fold(size_of::<FolderFetch>().saturating_add(children_bytes), |bytes, child| {
        let child_bytes = match child {
            InventoryChild::Category(category) => category.name.len().saturating_add(2 * size_of::<usize>()),
            InventoryChild::Item(item) => item.name.len().saturating_add(item.description.len()).saturating_add(4 * size_of::<usize>()),
        };
        bytes.saturating_add(child_bytes)
    })
}

#[tracing::instrument(name = "viewer.session", level = "info", skip_all, fields(grid = ?credentials.grid))]
pub(crate) async fn drain_session(
    session: Session,
    session_handle: SessionHandle,
    runtime: Handle,
    mailbox: PresentationMailbox,
    cancellation: oneshot::Receiver<()>,
    credentials: CredentialPersistence,
    cache: Option<Arc<AssetCache>>,
    asset_budget: assets::Budget,
) {
    SessionDriver::new(session, session_handle, runtime, mailbox, cancellation, credentials, cache, asset_budget).run().await;
}

#[cfg(test)]
mod tests;
