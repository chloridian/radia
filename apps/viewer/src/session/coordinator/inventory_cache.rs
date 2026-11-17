use std::{
    collections::HashSet,
    fmt::Display,
    mem,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use inventory::{
    Category, CategoryKey, ChangeSet, Inventory, InventoryCache, InventoryCacheOpenStatus, InventoryCacheRestore, InventorySnapshot,
};
use network::Grid;
use tokio::{
    runtime::Handle,
    sync::{mpsc, oneshot},
    task,
    task::JoinHandle,
};
use uuid::Uuid;

pub(super) const MAX_PENDING_WRITES: usize = 32;
pub(super) const MAX_PENDING_ROWS: usize = 65_536;
pub(super) const MAX_PENDING_BYTES: usize = 16 * 1024 * 1024;
pub(super) const CACHE_WRITER_QUEUE_CAPACITY: usize = MAX_PENDING_WRITES + 2;
pub(super) const RECLAIMER_QUEUE_CAPACITY: usize = MAX_PENDING_WRITES + 4;
const MAX_BOOTSTRAP_BYTES: usize = 64 * 1024 * 1024;
const CATEGORY_BATCH_MEMORY: usize = size_of::<Category>().saturating_mul(2);

pub(super) type ReclaimJob = Box<dyn FnOnce() + Send + 'static>;

pub(super) struct InventoryDisposalPermit(mpsc::OwnedPermit<ReclaimJob>);

impl InventoryDisposalPermit {
    pub(super) fn from_owned(permit: mpsc::OwnedPermit<ReclaimJob>) -> Self {
        Self(permit)
    }

    pub(super) fn reclaim<T: Send + 'static>(self, value: T) {
        drop(self.0.send(Box::new(move || drop(value))));
    }
}

pub(super) struct InventoryCacheSession {
    event_sender: mpsc::UnboundedSender<InventoryCacheEvent>,
    event_receiver: mpsc::UnboundedReceiver<InventoryCacheEvent>,
    warning_emitted: Arc<AtomicBool>,
    reclaimer_sender: Option<mpsc::Sender<ReclaimJob>>,
    reclaimer_task: Option<JoinHandle<()>>,
    open_task: Option<JoinHandle<()>>,
    cleanup_task: Option<JoinHandle<()>>,
    lifecycle: CacheLifecycle,
    restore: RestoreLifecycle,
    pending_write_count: usize,
    pending_rows: usize,
    pending_bytes: usize,
    pending_exclusions: usize,
}

enum CacheLifecycle {
    Dormant,
    Opening { pending_writes: Vec<PendingWrite> },
    Active { writer: CacheWriter },
    Disabled { writer: Option<CacheWriter> },
}

enum RestoreLifecycle {
    Available,
    Invalidated,
    Applying(PendingRestore),
}

struct CacheWriter {
    sender: mpsc::Sender<CacheWriterCommand>,
    task: JoinHandle<()>,
}

enum CacheWriterCommand {
    Changeset {
        changes: ChangeSet,
        before: InventorySnapshot,
        after: InventorySnapshot,
        rows: usize,
        bytes: usize,
        disposal: InventoryDisposalPermit,
    },
    Bootstrap {
        snapshot: InventorySnapshot,
        disposal: InventoryDisposalPermit,
    },
    Discard,
    Close(oneshot::Sender<()>),
}

struct PendingRestore {
    restore: InventoryCacheRestore,
    excluded: HashSet<CategoryKey>,
    disposal: InventoryDisposalPermit,
}

struct PendingWrite {
    changes: ChangeSet,
    before: InventorySnapshot,
    after: InventorySnapshot,
    rows: usize,
    bytes: usize,
    disposal: InventoryDisposalPermit,
}

pub(super) enum InventoryCacheEvent {
    Ready {
        cache: InventoryCache,
        restore: InventoryCacheRestore,
        status: InventoryCacheOpenStatus,
        disposal: InventoryDisposalPermit,
    },
    OpenFailed(String),
    RestoreBatch(InventoryDisposalPermit),
    WriteCompleted {
        rows: usize,
        bytes: usize,
        affected: Vec<CategoryKey>,
        bootstrap: bool,
        result: Result<(), String>,
        disposal: InventoryDisposalPermit,
    },
}

pub(super) enum InventoryCacheEventOutcome {
    Unchanged,
    Deferred,
    Stale,
    Changed { before: InventorySnapshot, after: InventorySnapshot, disposal: InventoryDisposalPermit },
}

impl InventoryCacheSession {
    pub(super) fn new(runtime: &Handle) -> Self {
        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        let (reclaimer_sender, mut reclaimer_receiver) = mpsc::channel(RECLAIMER_QUEUE_CAPACITY);
        let reclaimer_task = runtime.spawn(async move {
            while let Some(reclaim) = reclaimer_receiver.recv().await {
                if let Err(error) = task::spawn_blocking(reclaim).await {
                    tracing::error!(%error, "inventory payload reclaimer failed");
                }
            }
        });
        Self {
            event_sender,
            event_receiver,
            warning_emitted: Arc::new(AtomicBool::new(false)),
            reclaimer_sender: Some(reclaimer_sender),
            reclaimer_task: Some(reclaimer_task),
            open_task: None,
            cleanup_task: None,
            lifecycle: CacheLifecycle::Dormant,
            restore: RestoreLifecycle::Available,
            pending_write_count: 0,
            pending_rows: 0,
            pending_bytes: 0,
            pending_exclusions: 0,
        }
    }

    pub(super) fn try_reserve_disposal(&self) -> Option<InventoryDisposalPermit> {
        self.reclaimer_sender.as_ref()?.clone().try_reserve_owned().ok().map(InventoryDisposalPermit)
    }

    pub(super) fn reclaimer(&self) -> Option<mpsc::Sender<ReclaimJob>> {
        self.reclaimer_sender.clone()
    }

    pub(super) fn start(&mut self, runtime: &Handle, grid: Grid, agent_id: Uuid, library_owner: Option<Uuid>, inventory: &Inventory) {
        let grid_identity = grid.id();
        self.start_with_open(runtime, inventory, move || InventoryCache::open(grid_identity, agent_id, library_owner));
    }

    fn start_with_open(
        &mut self,
        runtime: &Handle,
        inventory: &Inventory,
        open: impl FnOnce() -> Result<(InventoryCache, InventoryCacheOpenStatus), inventory::InventoryCacheError> + Send + 'static,
    ) {
        if !matches!(&self.lifecycle, CacheLifecycle::Dormant) {
            return;
        }
        let Some(disposal) = self.try_reserve_disposal() else {
            self.lifecycle = CacheLifecycle::Disabled { writer: None };
            self.warn_once(&"inventory cache restore exceeded its disposal admission");
            return;
        };
        self.lifecycle = CacheLifecycle::Opening { pending_writes: Vec::new() };
        let invalidate_cache = matches!(&self.restore, RestoreLifecycle::Invalidated);
        let inventory = inventory.clone();
        let skeleton = inventory.snapshot();
        let sender = self.event_sender.clone();
        self.open_task = Some(runtime.spawn(async move {
            let mut disposal = Some(disposal);
            let opened = task::spawn_blocking(open).await;
            let event = match opened {
                Ok(Ok((cache, status))) => {
                    if invalidate_cache && let Err(error) = cache.discard().await {
                        InventoryCacheEvent::OpenFailed(error.to_string())
                    } else {
                        match cache.load(&inventory).await {
                            Ok(load) => {
                                let prepared = task::spawn_blocking(move || load.prepare_restore(&skeleton)).await;
                                match prepared {
                                    Ok(Ok(restore)) => InventoryCacheEvent::Ready {
                                        cache,
                                        restore,
                                        status,
                                        disposal: disposal.take().unwrap_or_else(|| unreachable!()),
                                    },
                                    Ok(Err(error)) => InventoryCacheEvent::OpenFailed(error.to_string()),
                                    Err(error) => InventoryCacheEvent::OpenFailed(error.to_string()),
                                }
                            }
                            Err(error) => InventoryCacheEvent::OpenFailed(error.to_string()),
                        }
                    }
                }
                Ok(Err(error)) => InventoryCacheEvent::OpenFailed(error.to_string()),
                Err(error) => InventoryCacheEvent::OpenFailed(error.to_string()),
            };
            if let Err(send_error) = sender.send(event) {
                discard_cache_event(send_error.0, false).await;
            }
        }));
    }

    pub(super) async fn next_event(&mut self, allow_restore_batch: bool) -> Option<InventoryCacheEvent> {
        if allow_restore_batch && matches!(&self.restore, RestoreLifecycle::Applying(_)) && self.pending_exclusions == 0 {
            if let Some(disposal) = self.try_reserve_disposal() {
                return Some(InventoryCacheEvent::RestoreBatch(disposal));
            }
            if let Ok(event) = self.event_receiver.try_recv() {
                return Some(event);
            }
            let Some(sender) = self.reclaimer_sender.clone() else {
                return self.event_receiver.recv().await;
            };
            let reserve = async move { sender.reserve_owned().await.ok().map(InventoryDisposalPermit) };
            return tokio::select! {
                biased;
                event = self.event_receiver.recv() => event,
                disposal = reserve => disposal.map(InventoryCacheEvent::RestoreBatch),
            };
        }
        self.event_receiver.recv().await
    }

    pub(super) async fn reserve_disposal(&self) -> Option<InventoryDisposalPermit> {
        self.reclaimer_sender.as_ref()?.clone().reserve_owned().await.ok().map(InventoryDisposalPermit)
    }

    pub(super) fn invalidate_restore(&mut self) {
        self.discard_restore();
        self.restore = RestoreLifecycle::Invalidated;
    }

    pub(super) fn invalidate_for_recovery(&mut self, _runtime: &Handle) {
        self.invalidate_restore();
        let opening = matches!(&self.lifecycle, CacheLifecycle::Opening { .. });
        let active = matches!(&self.lifecycle, CacheLifecycle::Active { .. });
        if opening || active {
            self.disable();
        }
        if active {
            self.request_discard();
        }
    }

    pub(super) fn record_change(
        &mut self,
        changes: ChangeSet,
        before: InventorySnapshot,
        after: InventorySnapshot,
        disposal: InventoryDisposalPermit,
    ) {
        if changes.is_empty() || matches!(&self.lifecycle, CacheLifecycle::Dormant | CacheLifecycle::Disabled { .. }) {
            disposal.reclaim((changes, before, after));
            return;
        }
        let (rows, bytes) = estimate_write(&changes);
        let next_rows = self.pending_rows.saturating_add(rows);
        let next_bytes = self.pending_bytes.saturating_add(bytes);
        if self.pending_write_count >= MAX_PENDING_WRITES || next_rows > MAX_PENDING_ROWS || next_bytes > MAX_PENDING_BYTES {
            if matches!(&self.lifecycle, CacheLifecycle::Active { .. }) {
                self.disable_and_discard("inventory cache write admission was exceeded");
            } else {
                self.disable();
            }
            disposal.reclaim((changes, before, after));
            return;
        }
        self.pending_write_count += 1;
        self.pending_rows = next_rows;
        self.pending_bytes = next_bytes;
        self.pending_exclusions += 1;
        match &mut self.lifecycle {
            CacheLifecycle::Opening { pending_writes } => {
                pending_writes.push(PendingWrite { changes, before, after, rows, bytes, disposal })
            }
            CacheLifecycle::Active { writer } => {
                let command = CacheWriterCommand::Changeset { changes, before, after, rows, bytes, disposal };
                if let Err(error) = writer.sender.try_send(command) {
                    let CacheWriterCommand::Changeset { changes, before, after, disposal, .. } = error.into_inner() else { unreachable!() };
                    disposal.reclaim((changes, before, after));
                    self.pending_write_count = self.pending_write_count.saturating_sub(1);
                    self.pending_rows = self.pending_rows.saturating_sub(rows);
                    self.pending_bytes = self.pending_bytes.saturating_sub(bytes);
                    self.pending_exclusions = self.pending_exclusions.saturating_sub(1);
                    self.disable_and_discard("inventory cache writer stopped accepting changes");
                }
            }
            CacheLifecycle::Dormant | CacheLifecycle::Disabled { .. } => disposal.reclaim((changes, before, after)),
        }
    }

    pub(super) fn handle_event(
        &mut self,
        runtime: &Handle,
        event: InventoryCacheEvent,
        inventory: &mut Inventory,
    ) -> InventoryCacheEventOutcome {
        if matches!(&event, InventoryCacheEvent::RestoreBatch(_))
            && matches!(&self.lifecycle, CacheLifecycle::Active { .. })
            && self.pending_exclusions > 0
        {
            return InventoryCacheEventOutcome::Deferred;
        }
        match event {
            InventoryCacheEvent::OpenFailed(error) => {
                if matches!(&self.lifecycle, CacheLifecycle::Opening { .. }) {
                    self.warn_once(&error);
                    self.disable();
                    InventoryCacheEventOutcome::Unchanged
                } else {
                    InventoryCacheEventOutcome::Stale
                }
            }
            InventoryCacheEvent::Ready { cache, restore, status, disposal } => {
                self.handle_ready(runtime, cache, restore, status, disposal, inventory)
            }
            InventoryCacheEvent::RestoreBatch(disposal) => {
                if !matches!(&self.lifecycle, CacheLifecycle::Active { .. }) {
                    drop(disposal);
                    return InventoryCacheEventOutcome::Stale;
                }
                let before = inventory.snapshot();
                let before_revision = before.revision();
                let result = match &mut self.restore {
                    RestoreLifecycle::Applying(restore) => Some(restore.restore.apply_next(inventory, &restore.excluded)),
                    RestoreLifecycle::Available | RestoreLifecycle::Invalidated => None,
                };
                match result {
                    Some(Ok(progress)) => {
                        let outcome = if progress.changed {
                            InventoryCacheEventOutcome::Changed { before, after: inventory.snapshot(), disposal }
                        } else {
                            disposal.reclaim(before);
                            InventoryCacheEventOutcome::Unchanged
                        };
                        if progress.finished {
                            if let Some(restore) = self.take_restore() {
                                restore.disposal.reclaim((restore.restore, restore.excluded));
                            }
                            tracing::debug!("inventory cache restore finished");
                        }
                        outcome
                    }
                    Some(Err(error)) => {
                        let outcome = if inventory.revision() != before_revision {
                            InventoryCacheEventOutcome::Changed { before, after: inventory.snapshot(), disposal }
                        } else {
                            disposal.reclaim(before);
                            InventoryCacheEventOutcome::Unchanged
                        };
                        if let Some(restore) = self.take_restore() {
                            restore.disposal.reclaim((restore.restore, restore.excluded));
                        }
                        self.warn_once(&error);
                        self.disable_and_discard("inventory cache restore failed");
                        outcome
                    }
                    None => {
                        disposal.reclaim(before);
                        InventoryCacheEventOutcome::Stale
                    }
                }
            }
            InventoryCacheEvent::WriteCompleted { rows, bytes, affected, bootstrap, result, disposal } => {
                let active = matches!(&self.lifecycle, CacheLifecycle::Active { .. });
                if !bootstrap {
                    self.pending_write_count = self.pending_write_count.saturating_sub(1);
                    self.pending_rows = self.pending_rows.saturating_sub(rows);
                    self.pending_bytes = self.pending_bytes.saturating_sub(bytes);
                    self.pending_exclusions = self.pending_exclusions.saturating_sub(1);
                }
                if active && let RestoreLifecycle::Applying(restore) = &mut self.restore {
                    restore.excluded.extend(affected.iter().copied());
                }
                disposal.reclaim(affected);
                if let Err(error) = result {
                    if active {
                        self.warn_once(&error);
                        self.disable_and_discard("inventory cache write failed");
                        return InventoryCacheEventOutcome::Unchanged;
                    }
                    return InventoryCacheEventOutcome::Stale;
                }
                if active { InventoryCacheEventOutcome::Unchanged } else { InventoryCacheEventOutcome::Stale }
            }
        }
    }

    #[cfg(test)]
    pub(super) async fn close(self) {
        self.close_inner::<()>(None).await;
    }

    pub(super) async fn close_with_payload<T: Send + 'static>(self, payload: T) {
        self.close_inner(Some(payload)).await;
    }

    async fn close_inner<T: Send + 'static>(mut self, final_payload: Option<T>) {
        if let Some(open_task) = self.open_task.take()
            && let Err(error) = open_task.await
        {
            self.warn_once(&error);
        }
        let discard_late_ready = !matches!(&self.lifecycle, CacheLifecycle::Opening { .. });
        self.event_receiver.close();
        while let Ok(event) = self.event_receiver.try_recv() {
            discard_cache_event(event, discard_late_ready).await;
        }
        self.clear_pending_writes();
        self.discard_restore();
        let writer = match mem::replace(&mut self.lifecycle, CacheLifecycle::Disabled { writer: None }) {
            CacheLifecycle::Active { writer } | CacheLifecycle::Disabled { writer: Some(writer) } => Some(writer),
            CacheLifecycle::Dormant | CacheLifecycle::Opening { .. } | CacheLifecycle::Disabled { writer: None } => None,
        };
        if let Some(writer) = writer {
            let (closed, completion) = oneshot::channel();
            if writer.sender.send(CacheWriterCommand::Close(closed)).await.is_err() {
                self.warn_once(&"inventory cache writer stopped before shutdown");
            } else if completion.await.is_err() {
                self.warn_once(&"inventory cache writer stopped during shutdown");
            }
            if let Err(error) = writer.task.await {
                self.warn_once(&error);
            }
        }
        if let Some(task) = self.cleanup_task.take()
            && let Err(error) = task.await
        {
            self.warn_once(&error);
        }
        if let Some(payload) = final_payload {
            let disposal = self.reserve_disposal().await.expect("inventory reclaimer remains open until session cleanup is complete");
            disposal.reclaim(payload);
        }
        drop(self.reclaimer_sender.take());
        if let Some(task) = self.reclaimer_task.take()
            && let Err(error) = task.await
        {
            self.warn_once(&error);
        }
    }

    fn handle_ready(
        &mut self,
        runtime: &Handle,
        cache: InventoryCache,
        restore: InventoryCacheRestore,
        status: InventoryCacheOpenStatus,
        disposal: InventoryDisposalPermit,
        inventory: &mut Inventory,
    ) -> InventoryCacheEventOutcome {
        let pending_writes = match mem::replace(&mut self.lifecycle, CacheLifecycle::Disabled { writer: None }) {
            CacheLifecycle::Opening { pending_writes } => pending_writes,
            lifecycle => {
                self.lifecycle = lifecycle;
                disposal.reclaim(restore);
                self.discard_in_background(runtime, cache);
                return InventoryCacheEventOutcome::Stale;
            }
        };
        tracing::info!(?status, "inventory cache opened");
        let writer = start_cache_writer(runtime, cache, self.event_sender.clone());
        let sender = writer.sender.clone();
        self.lifecycle = CacheLifecycle::Active { writer };
        let mut accepting = true;
        for pending in pending_writes {
            if accepting {
                let command = CacheWriterCommand::Changeset {
                    changes: pending.changes,
                    before: pending.before,
                    after: pending.after,
                    rows: pending.rows,
                    bytes: pending.bytes,
                    disposal: pending.disposal,
                };
                match sender.try_send(command) {
                    Ok(()) => {}
                    Err(error) => {
                        let CacheWriterCommand::Changeset { changes, before, after, disposal, .. } = error.into_inner() else {
                            unreachable!()
                        };
                        disposal.reclaim((changes, before, after));
                        accepting = false;
                    }
                }
            } else {
                pending.disposal.reclaim((pending.changes, pending.before, pending.after));
            }
        }
        if !accepting {
            self.pending_write_count = 0;
            self.pending_rows = 0;
            self.pending_bytes = 0;
            self.pending_exclusions = 0;
            self.disable_and_discard("inventory cache writer stopped during startup");
        }
        if accepting && matches!(&self.lifecycle, CacheLifecycle::Active { .. }) {
            if let Some(disposal) = self.try_reserve_disposal() {
                let snapshot = inventory.snapshot();
                let bootstrap_bytes = snapshot.category_count().saturating_mul(CATEGORY_BATCH_MEMORY);
                if bootstrap_bytes > MAX_BOOTSTRAP_BYTES {
                    disposal.reclaim(snapshot);
                    self.disable_and_discard("inventory cache bootstrap exceeded its admission budget");
                } else if let Err(error) = sender.try_send(CacheWriterCommand::Bootstrap { snapshot, disposal }) {
                    let CacheWriterCommand::Bootstrap { snapshot, disposal } = error.into_inner() else { unreachable!() };
                    disposal.reclaim(snapshot);
                    self.disable_and_discard("inventory cache writer stopped during bootstrap");
                }
            } else {
                self.disable_and_discard("inventory cache bootstrap exceeded its disposal admission");
            }
        }
        if matches!(&self.lifecycle, CacheLifecycle::Active { .. }) && matches!(&self.restore, RestoreLifecycle::Available) {
            self.restore = RestoreLifecycle::Applying(PendingRestore { restore, excluded: HashSet::new(), disposal });
        } else {
            disposal.reclaim(restore);
        }
        InventoryCacheEventOutcome::Unchanged
    }

    fn clear_pending_writes(&mut self) {
        let pending_writes = match &mut self.lifecycle {
            CacheLifecycle::Opening { pending_writes } => mem::take(pending_writes),
            CacheLifecycle::Dormant | CacheLifecycle::Active { .. } | CacheLifecycle::Disabled { .. } => Vec::new(),
        };
        self.reclaim_pending_writes(pending_writes);
    }

    fn reclaim_pending_writes(&mut self, pending_writes: Vec<PendingWrite>) {
        for pending in pending_writes {
            pending.disposal.reclaim((pending.changes, pending.before, pending.after));
        }
        self.pending_write_count = 0;
        self.pending_rows = 0;
        self.pending_bytes = 0;
        self.pending_exclusions = 0;
    }

    fn disable_and_discard(&mut self, reason: &str) {
        if !self.warning_emitted.swap(true, Ordering::Relaxed) {
            tracing::warn!(reason, "inventory cache is unavailable for this session");
        }
        self.disable();
        self.request_discard();
    }

    fn disable(&mut self) {
        self.lifecycle = match mem::replace(&mut self.lifecycle, CacheLifecycle::Disabled { writer: None }) {
            CacheLifecycle::Dormant => CacheLifecycle::Disabled { writer: None },
            CacheLifecycle::Opening { pending_writes } => {
                self.reclaim_pending_writes(pending_writes);
                CacheLifecycle::Disabled { writer: None }
            }
            CacheLifecycle::Active { writer } => CacheLifecycle::Disabled { writer: Some(writer) },
            CacheLifecycle::Disabled { writer } => CacheLifecycle::Disabled { writer },
        };
        self.invalidate_restore();
    }

    fn request_discard(&self) {
        if let CacheLifecycle::Disabled { writer: Some(writer) } = &self.lifecycle
            && writer.sender.try_send(CacheWriterCommand::Discard).is_err()
        {
            self.warn_once(&"inventory cache writer stopped before invalidation");
        }
    }

    fn discard_in_background(&mut self, runtime: &Handle, cache: InventoryCache) {
        let warning_emitted = Arc::clone(&self.warning_emitted);
        self.cleanup_task = Some(runtime.spawn(async move {
            if let Err(error) = cache.discard().await
                && !warning_emitted.swap(true, Ordering::Relaxed)
            {
                tracing::warn!(%error, "inventory cache was discarded");
            }
            if let Err(error) = cache.close().await
                && !warning_emitted.swap(true, Ordering::Relaxed)
            {
                tracing::warn!(%error, "inventory cache shutdown failed");
            }
        }));
    }

    fn warn_once(&self, error: &impl Display) {
        if !self.warning_emitted.swap(true, Ordering::Relaxed) {
            tracing::warn!(%error, "inventory cache is unavailable for this session");
        }
    }

    fn discard_restore(&mut self) {
        if let Some(restore) = self.take_restore() {
            restore.disposal.reclaim((restore.restore, restore.excluded));
        }
    }

    fn take_restore(&mut self) -> Option<PendingRestore> {
        match mem::replace(&mut self.restore, RestoreLifecycle::Available) {
            RestoreLifecycle::Applying(restore) => Some(restore),
            lifecycle => {
                self.restore = lifecycle;
                None
            }
        }
    }
}

fn estimate_write(changes: &ChangeSet) -> (usize, usize) {
    let estimate = changes.cache_write_estimate();
    (estimate.rows(), estimate.bytes())
}

async fn discard_cache_event(event: InventoryCacheEvent, discard_cache: bool) {
    match event {
        InventoryCacheEvent::Ready { cache, restore, disposal, .. } => {
            disposal.reclaim(restore);
            if discard_cache && let Err(error) = cache.discard().await {
                tracing::warn!(%error, "inventory cache was discarded");
            }
            if let Err(error) = cache.close().await {
                tracing::warn!(%error, "inventory cache shutdown failed");
            }
        }
        InventoryCacheEvent::RestoreBatch(disposal) => drop(disposal),
        InventoryCacheEvent::WriteCompleted { affected, disposal, .. } => {
            disposal.reclaim(affected);
        }
        InventoryCacheEvent::OpenFailed(_) => {}
    }
}

fn send_cache_event(sender: &mpsc::UnboundedSender<InventoryCacheEvent>, event: InventoryCacheEvent) {
    if let Err(error) = sender.send(event)
        && let InventoryCacheEvent::WriteCompleted { affected, disposal, .. } = error.0
    {
        disposal.reclaim(affected);
    }
}

fn start_cache_writer(runtime: &Handle, cache: InventoryCache, sender: mpsc::UnboundedSender<InventoryCacheEvent>) -> CacheWriter {
    let (command_sender, mut command_receiver) = mpsc::channel(CACHE_WRITER_QUEUE_CAPACITY);
    let task = runtime.spawn(async move {
        let mut cache = Some(cache);
        while let Some(command) = command_receiver.recv().await {
            match command {
                CacheWriterCommand::Changeset { changes, before, after, rows, bytes, disposal } => {
                    let Some(current) = cache.take() else {
                        disposal.reclaim((changes, before, after));
                        break;
                    };
                    let encoded = task::spawn_blocking(move || {
                        let result = current.submit_changeset(&changes, &before, &after);
                        (current, result)
                    })
                    .await;
                    let (next_cache, affected, result, disposal) = match encoded {
                        Ok((current, Ok(mut write))) => {
                            let affected = write.take_affected_categories();
                            let result = write.wait().await.map_err(|error| error.to_string());
                            (Some(current), affected, result, disposal)
                        }
                        Ok((current, Err(error))) => {
                            let message = error.to_string();
                            let invalidation = current.discard().await;
                            let result = match invalidation {
                                Ok(()) => Err(message),
                                Err(error) => Err(format!("{message}; cache invalidation failed: {error}")),
                            };
                            (Some(current), Vec::new(), result, disposal)
                        }
                        Err(error) => {
                            let result = Err(error.to_string());
                            send_cache_event(
                                &sender,
                                InventoryCacheEvent::WriteCompleted {
                                    rows,
                                    bytes,
                                    affected: Vec::new(),
                                    bootstrap: false,
                                    result,
                                    disposal,
                                },
                            );
                            break;
                        }
                    };
                    cache = next_cache;
                    send_cache_event(
                        &sender,
                        InventoryCacheEvent::WriteCompleted { rows, bytes, affected, bootstrap: false, result, disposal },
                    );
                }
                CacheWriterCommand::Bootstrap { snapshot, disposal } => {
                    let Some(current) = cache.take() else {
                        disposal.reclaim(snapshot);
                        break;
                    };
                    let encoded = task::spawn_blocking(move || {
                        let result = current.submit_bootstrap(&snapshot);
                        (current, result)
                    })
                    .await;
                    let (next_cache, result, disposal) = match encoded {
                        Ok((current, Ok(write))) => (Some(current), write.wait().await.map_err(|error| error.to_string()), disposal),
                        Ok((current, Err(error))) => {
                            let message = error.to_string();
                            let invalidation = current.discard().await;
                            let result = match invalidation {
                                Ok(()) => Err(message),
                                Err(error) => Err(format!("{message}; cache invalidation failed: {error}")),
                            };
                            (Some(current), result, disposal)
                        }
                        Err(error) => {
                            let result = Err(error.to_string());
                            send_cache_event(
                                &sender,
                                InventoryCacheEvent::WriteCompleted {
                                    rows: 0,
                                    bytes: 0,
                                    affected: Vec::new(),
                                    bootstrap: true,
                                    result,
                                    disposal,
                                },
                            );
                            break;
                        }
                    };
                    cache = next_cache;
                    send_cache_event(
                        &sender,
                        InventoryCacheEvent::WriteCompleted { rows: 0, bytes: 0, affected: Vec::new(), bootstrap: true, result, disposal },
                    );
                }
                CacheWriterCommand::Discard => {
                    if let Some(cache) = &cache
                        && let Err(error) = cache.discard().await
                    {
                        tracing::warn!(%error, "inventory cache invalidation failed");
                    }
                }
                CacheWriterCommand::Close(completion) => {
                    if let Some(cache) = cache.take()
                        && let Err(error) = cache.close().await
                    {
                        tracing::warn!(%error, "inventory cache shutdown failed");
                    }
                    completion.send(()).ok();
                    return;
                }
            }
        }
        if let Some(cache) = cache
            && let Err(error) = cache.close().await
        {
            tracing::warn!(%error, "inventory cache shutdown failed");
        }
    });
    CacheWriter { sender: command_sender, task }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        fs,
        path::Path,
        sync::{Arc, mpsc as std_mpsc},
        thread,
        thread::ThreadId,
    };

    use inventory::{
        AssetType, Category, CategoryKey, FolderType, Inventory, InventoryChild, InventoryLimits, InventoryOwner, InventoryType, Item,
        ItemKey, PermissionMask, Permissions, SaleInfo, SaleType, ServerVersion,
    };
    use tempfile::tempdir;
    use tokio::{
        runtime::Handle,
        sync::{mpsc, oneshot},
    };
    use utils::{AssetId, AvatarId, FolderId, GroupId, ItemId};
    use uuid::Uuid;

    use super::{
        CACHE_WRITER_QUEUE_CAPACITY, CacheLifecycle, CacheWriter, InventoryCache, InventoryCacheEvent, InventoryCacheEventOutcome,
        InventoryCacheSession, MAX_PENDING_WRITES, RECLAIMER_QUEUE_CAPACITY, RestoreLifecycle,
    };

    fn apply_fetch(
        inventory: &mut Inventory,
        folder: CategoryKey,
        version: ServerVersion,
        descendent_count: Option<u32>,
        children: Vec<InventoryChild>,
    ) -> Result<inventory::ChangeSet, inventory::InventoryError> {
        let prepared = inventory.snapshot().prepare_folder_fetch(folder, version, descendent_count, children)?;
        let inventory::FolderFetchCommit::Applied(changes) = inventory.commit_folder_fetch(prepared)? else {
            panic!("single-owner fetch preparation became stale before commit");
        };
        Ok(changes)
    }

    fn uuid(value: u128) -> Uuid {
        Uuid::from_u128(value)
    }

    fn category(owner: InventoryOwner, id: u128, parent: Option<u128>, version: i32) -> Category {
        Category {
            key: CategoryKey::new(owner, FolderId::new(uuid(id))),
            parent: parent.map(|parent| CategoryKey::new(owner, FolderId::new(uuid(parent)))),
            preferred_folder_type: FolderType::None,
            name: Arc::from(format!("folder-{id}")),
            server_version: ServerVersion::from_raw(version),
            server_descendent_count: None,
        }
    }

    fn item(owner: InventoryOwner, id: u128, parent: CategoryKey) -> Item {
        Item {
            key: ItemKey::new(owner, ItemId::new(uuid(id))),
            parent,
            asset_id: AssetId::new(uuid(id + 100)),
            asset_type: AssetType::Notecard,
            inventory_type: InventoryType::Notecard,
            flags: 0,
            name: Arc::from("cached item"),
            description: Arc::from("cached description"),
            created_at: 0,
            permissions: Permissions {
                creator: AvatarId::new(Uuid::nil()),
                owner: AvatarId::new(Uuid::nil()),
                group: GroupId::new(Uuid::nil()),
                last_owner: AvatarId::new(Uuid::nil()),
                base: PermissionMask::new(0),
                owner_mask: PermissionMask::new(0),
                group_mask: PermissionMask::new(0),
                everyone: PermissionMask::new(0),
                next_owner: PermissionMask::new(0),
            },
            sale: SaleInfo { sale_type: SaleType::NotForSale, price: 0 },
        }
    }

    fn cache_skeleton(owner: InventoryOwner) -> (Inventory, CategoryKey, CategoryKey) {
        let root = category(owner, 1, None, 7);
        let child = category(owner, 2, Some(1), 4);
        let root_key = root.key;
        let child_key = child.key;
        let mut inventory = Inventory::new(root, None, InventoryLimits::default()).expect("valid inventory root");
        inventory.upsert_category(child).expect("valid child folder");
        (inventory, root_key, child_key)
    }

    async fn seed_cache_with_item(root: &Path, owner: InventoryOwner, item_id: u128) -> ItemKey {
        let (mut inventory, _, child) = cache_skeleton(owner);
        let (cache, _) = InventoryCache::open_at_root(root, "agni", owner.id(), None).expect("open inventory cache");
        let before = inventory.snapshot();
        cache.submit_bootstrap(&before).expect("write cache skeleton").wait().await.expect("skeleton write completes");
        let child_item = item(owner, item_id, child);
        let item_key = child_item.key;
        let before = inventory.snapshot();
        let changes = apply_fetch(&mut inventory, child, ServerVersion::from_raw(4), Some(1), vec![InventoryChild::Item(child_item)])
            .expect("complete cached child folder");
        let after = inventory.snapshot();
        cache.submit_changeset(&changes, &before, &after).expect("write cached child folder").wait().await.expect("child write completes");
        cache.close().await.expect("close seeded cache");
        item_key
    }

    async fn start_overflowed_open(
        cache_root: &Path,
        owner: InventoryOwner,
        root: CategoryKey,
        inventory: &mut Inventory,
    ) -> (InventoryCacheSession, std_mpsc::Sender<()>) {
        let runtime = Handle::current();
        let mut session = InventoryCacheSession::new(&runtime);
        let cache_root = cache_root.to_path_buf();
        let (opened_tx, opened_rx) = oneshot::channel();
        let (continue_tx, continue_rx) = std_mpsc::channel();
        session.start_with_open(&runtime, inventory, move || {
            opened_tx.send(()).expect("open worker started");
            continue_rx.recv().expect("test releases cache open");
            InventoryCache::open_at_root(cache_root, "agni", owner.id(), None)
        });
        opened_rx.await.expect("cache open reached its controlled gate");

        for index in 0..=MAX_PENDING_WRITES {
            let before = inventory.snapshot();
            let changes = inventory
                .upsert_category(category(owner, 100 + index as u128, Some(root.id().uuid().as_u128()), 1))
                .expect("inventory update applies");
            let after = inventory.snapshot();
            let disposal = session.try_reserve_disposal().expect("cache session reserves a bounded disposal slot");
            session.record_change(changes, before, after, disposal);
        }
        (session, continue_tx)
    }

    #[tokio::test]
    async fn early_delta_invalidates_restore() {
        let directory = tempdir().expect("create inventory cache directory");
        let agent = uuid(10);
        let owner = InventoryOwner::new(agent);
        let (mut seeded, root, child) = cache_skeleton(owner);
        let (cache, _) = InventoryCache::open_at_root(directory.path(), "agni", agent, None).expect("open inventory cache");
        let bootstrap = seeded.snapshot();
        cache.submit_bootstrap(&bootstrap).expect("write login skeleton").wait().await.expect("skeleton write completes");

        let before_root = seeded.snapshot();
        let root_changes = apply_fetch(
            &mut seeded,
            root,
            ServerVersion::from_raw(7),
            Some(1),
            vec![InventoryChild::Category(category(owner, 2, Some(1), 4))],
        )
        .expect("complete root folder");
        let after_root = seeded.snapshot();
        cache
            .submit_changeset(&root_changes, &before_root, &after_root)
            .expect("write root folder completion")
            .wait()
            .await
            .expect("root completion write succeeds");

        let before_child = seeded.snapshot();
        let stale_item = item(owner, 3, child);
        let stale_item_key = stale_item.key;
        let child_changes = apply_fetch(&mut seeded, child, ServerVersion::from_raw(4), Some(1), vec![InventoryChild::Item(stale_item)])
            .expect("complete child folder");
        let after_child = seeded.snapshot();
        cache
            .submit_changeset(&child_changes, &before_child, &after_child)
            .expect("write cached child contents")
            .wait()
            .await
            .expect("child completion write succeeds");
        assert!(after_child.is_complete(child));
        cache.close().await.expect("close seeded cache");

        let (mut next_login, next_root, next_child) = cache_skeleton(owner);
        let mut cache_session = InventoryCacheSession::new(&Handle::current());
        cache_session.invalidate_restore();
        let cache_root = directory.path().to_path_buf();
        cache_session
            .start_with_open(&Handle::current(), &next_login, move || InventoryCache::open_at_root(cache_root, "agni", agent, None));
        let event = cache_session.next_event(true).await.expect("cache open event arrives");
        match &event {
            InventoryCacheEvent::Ready { .. } => {}
            _ => panic!("cache open completes with Ready"),
        }
        cache_session.handle_event(&Handle::current(), event, &mut next_login);
        cache_session.close().await;

        let (cache, _) = InventoryCache::open_at_root(directory.path(), "agni", agent, None).expect("reopen inventory cache");
        let load = cache.load(&next_login).await.expect("load invalidated cache");
        let mut restore = load.prepare_restore(&next_login.snapshot()).expect("prepare next login restore");
        loop {
            let progress = restore.apply_next(&mut next_login, &HashSet::new()).expect("apply cached folder batch");
            if progress.finished {
                break;
            }
        }
        let snapshot = next_login.snapshot();
        assert_eq!(snapshot.roots().personal(), next_root);
        assert_eq!(snapshot.item(stale_item_key).map(|item| item.parent), Some(next_child));
        assert!(!snapshot.is_complete(next_child));
        cache.close().await.expect("close reopened cache");
    }

    #[tokio::test]
    async fn coverage_loss_marks_cache_incomplete() {
        let directory = tempdir().expect("create inventory cache directory");
        let agent = uuid(110);
        let owner = InventoryOwner::new(agent);
        let (mut seeded, root, child) = cache_skeleton(owner);
        let (cache, _) = InventoryCache::open_at_root(directory.path(), "agni", agent, None).expect("open inventory cache");
        let before = seeded.snapshot();
        cache.submit_bootstrap(&before).expect("write login skeleton").wait().await.expect("skeleton write completes");
        let root_changes = apply_fetch(
            &mut seeded,
            root,
            ServerVersion::from_raw(7),
            Some(1),
            vec![InventoryChild::Category(category(owner, 2, Some(1), 4))],
        )
        .expect("complete root folder");
        let after_root = seeded.snapshot();
        cache.submit_changeset(&root_changes, &before, &after_root).expect("write root folder").wait().await.expect("root write completes");
        let before_child = seeded.snapshot();
        let item_key = ItemKey::new(owner, ItemId::new(uuid(111)));
        let child_changes =
            apply_fetch(&mut seeded, child, ServerVersion::from_raw(4), Some(1), vec![InventoryChild::Item(item(owner, 111, child))])
                .expect("complete nested folder");
        let after_child = seeded.snapshot();
        assert!(after_child.is_complete(child));
        cache
            .submit_changeset(&child_changes, &before_child, &after_child)
            .expect("write nested folder")
            .wait()
            .await
            .expect("nested folder write completes");
        cache.close().await.expect("close seeded cache");

        let (mut next_login, _, next_child) = cache_skeleton(owner);
        let mut cache_session = InventoryCacheSession::new(&Handle::current());
        let cache_root = directory.path().to_path_buf();
        cache_session
            .start_with_open(&Handle::current(), &next_login, move || InventoryCache::open_at_root(cache_root, "agni", agent, None));
        let event = cache_session.next_event(true).await.expect("cache open completes");
        match &event {
            InventoryCacheEvent::Ready { .. } => {}
            _ => panic!("cache open completes with Ready"),
        }
        cache_session.handle_event(&Handle::current(), event, &mut next_login);
        cache_session.invalidate_for_recovery(&Handle::current());
        cache_session.close().await;

        let (cache, _) = InventoryCache::open_at_root(directory.path(), "agni", agent, None).expect("reopen inventory cache");
        let load = cache.load(&next_login).await.expect("load cache after invalidation");
        let mut restore = load.prepare_restore(&next_login.snapshot()).expect("prepare next login restore");
        loop {
            let progress = restore.apply_next(&mut next_login, &HashSet::new()).expect("apply remaining cached folders");
            if progress.finished {
                break;
            }
        }
        let restored = next_login.snapshot();
        assert_eq!(restored.item(item_key).map(|item| item.parent), Some(next_child));
        assert!(!restored.is_complete(next_child));
        cache.close().await.expect("close reopened cache");
    }

    #[tokio::test]
    async fn restore_waits_for_writer_completions() {
        let directory = tempdir().expect("create inventory cache directory");
        let agent = uuid(112);
        let owner = InventoryOwner::new(agent);
        let (mut seeded, root, _) = cache_skeleton(owner);
        let (cache, _) = InventoryCache::open_at_root(directory.path(), "agni", agent, None).expect("open inventory cache");
        let before = seeded.snapshot();
        cache.submit_bootstrap(&before).expect("write login skeleton").wait().await.expect("skeleton write completes");
        let changes = apply_fetch(
            &mut seeded,
            root,
            ServerVersion::from_raw(7),
            Some(1),
            vec![InventoryChild::Category(category(owner, 2, Some(1), 0))],
        )
        .expect("complete root folder");
        let after = seeded.snapshot();
        cache.submit_changeset(&changes, &before, &after).expect("write root folder").wait().await.expect("root write completes");
        cache.close().await.expect("close seeded cache");

        let (mut inventory, _, _) = cache_skeleton(owner);
        let mut session = InventoryCacheSession::new(&Handle::current());
        let cache_root = directory.path().to_path_buf();
        session.start_with_open(&Handle::current(), &inventory, move || InventoryCache::open_at_root(cache_root, "agni", agent, None));
        let ready = session.next_event(true).await.expect("cache open completes");
        let InventoryCacheEvent::Ready { .. } = &ready else { panic!("cache open returns ready") };
        session.handle_event(&Handle::current(), ready, &mut inventory);
        let restore = session.next_event(true).await.expect("restore batch becomes available");
        match &restore {
            InventoryCacheEvent::RestoreBatch(_) => {}
            _ => panic!("restore batch becomes available"),
        }
        let preparation = session.try_reserve_disposal().expect("active preparation reserves bounded cleanup");

        for index in 0..MAX_PENDING_WRITES {
            let before = inventory.snapshot();
            let changes = inventory
                .upsert_category(category(owner, 1_000 + index as u128, Some(root.id().uuid().as_u128()), 1))
                .expect("inventory update applies");
            let after = inventory.snapshot();
            let disposal = session.try_reserve_disposal().expect("write reserves bounded cleanup");
            session.record_change(changes, before, after, disposal);
        }

        let InventoryCacheEventOutcome::Deferred = session.handle_event(&Handle::current(), restore, &mut inventory) else {
            panic!("restore waits while cache writes are in flight");
        };
        let mut bootstrap_completions = 0;
        let mut write_completions = 0;
        for _ in 0..=MAX_PENDING_WRITES {
            let event = session.next_event(false).await.expect("cache worker completion remains serviceable");
            let InventoryCacheEvent::WriteCompleted { bootstrap, .. } = &event else {
                panic!("deferred restore must leave writer completions available");
            };
            if *bootstrap {
                bootstrap_completions += 1;
            } else {
                write_completions += 1;
            }
            session.handle_event(&Handle::current(), event, &mut inventory);
        }
        assert_eq!(bootstrap_completions, 1);
        assert_eq!(write_completions, MAX_PENDING_WRITES);
        let restore = session.next_event(true).await.expect("restore resumes after writes finish");
        match &restore {
            InventoryCacheEvent::RestoreBatch(_) => {}
            _ => panic!("restore resumes after writes finish"),
        }
        let InventoryCacheEventOutcome::Unchanged = session.handle_event(&Handle::current(), restore, &mut inventory) else {
            panic!("cached version mismatch keeps the live folder unchanged");
        };
        assert!(!inventory.snapshot().is_complete(root));
        drop(preparation);
        session.close().await;
    }

    #[tokio::test]
    async fn overflow_discards_ready_cache() {
        let directory = tempdir().expect("create inventory cache directory");
        let owner = InventoryOwner::new(uuid(42));
        let stale_item_key = seed_cache_with_item(directory.path(), owner, 43).await;
        let (mut inventory, root, login_child) = cache_skeleton(owner);
        let runtime = Handle::current();
        let (mut session, continue_tx) = start_overflowed_open(directory.path(), owner, root, &mut inventory).await;

        continue_tx.send(()).expect("release cache open");
        let event = session.next_event(false).await.expect("cache open completion arrives");
        match &event {
            InventoryCacheEvent::Ready { .. } => {}
            _ => panic!("overflowed opening emits a late Ready event"),
        }
        let InventoryCacheEventOutcome::Stale = session.handle_event(&runtime, event, &mut inventory) else {
            panic!("late Ready is stale after admission overflow");
        };
        session.close().await;

        let (cache, _) = InventoryCache::open_at_root(directory.path(), "agni", owner.id(), None).expect("reopen invalidated cache");
        let load = cache.load(&inventory).await.expect("load cache after overflow");
        let mut restore = load.prepare_restore(&inventory.snapshot()).expect("prepare post-overflow restore");
        loop {
            let progress = restore.apply_next(&mut inventory, &HashSet::new()).expect("apply post-overflow restore");
            if progress.finished {
                break;
            }
        }
        let after = inventory.snapshot();
        assert_eq!(after.item(stale_item_key).map(|item| item.parent), Some(login_child));
        assert!(!after.is_complete(login_child));
        cache.close().await.expect("close reopened cache");
    }

    #[tokio::test]
    async fn shutdown_discards_overflowed_cache() {
        let directory = tempdir().expect("create inventory cache directory");
        let owner = InventoryOwner::new(uuid(49));
        let stale_item_key = seed_cache_with_item(directory.path(), owner, 50).await;
        let (mut inventory, root, login_child) = cache_skeleton(owner);
        let (session, continue_tx) = start_overflowed_open(directory.path(), owner, root, &mut inventory).await;
        let (closing_tx, closing_rx) = oneshot::channel();
        let closing = tokio::spawn(async move {
            closing_tx.send(()).expect("shutdown task started");
            session.close().await;
        });
        closing_rx.await.expect("shutdown entered cache close");
        continue_tx.send(()).expect("release cache open");
        closing.await.expect("shutdown drains and discards late cache");

        let (cache, _) = InventoryCache::open_at_root(directory.path(), "agni", owner.id(), None).expect("reopen invalidated cache");
        let load = cache.load(&inventory).await.expect("load cache after shutdown");
        let mut restore = load.prepare_restore(&inventory.snapshot()).expect("prepare post-shutdown restore");
        loop {
            let progress = restore.apply_next(&mut inventory, &HashSet::new()).expect("apply post-shutdown restore");
            if progress.finished {
                break;
            }
        }
        let after = inventory.snapshot();
        assert_eq!(after.item(stale_item_key).map(|item| item.parent), Some(login_child));
        assert!(!after.is_complete(login_child));
        cache.close().await.expect("close reopened cache");
    }

    #[tokio::test]
    async fn failed_open_rejects_changes() {
        let directory = tempdir().expect("create inventory cache directory");
        let cache_file = directory.path().join("not-a-directory");
        fs::write(&cache_file, b"file").expect("create invalid cache root");
        let owner = InventoryOwner::new(uuid(46));
        let (mut inventory, root, _) = cache_skeleton(owner);
        let runtime = Handle::current();
        let mut session = InventoryCacheSession::new(&runtime);
        session.start_with_open(&runtime, &inventory, move || InventoryCache::open_at_root(cache_file, "agni", owner.id(), None));

        let event = session.next_event(false).await.expect("cache open failure arrives");
        match &event {
            InventoryCacheEvent::OpenFailed(_) => {}
            _ => panic!("invalid cache root fails to open"),
        }
        let InventoryCacheEventOutcome::Unchanged = session.handle_event(&runtime, event, &mut inventory) else {
            panic!("open failure leaves inventory unchanged");
        };
        let before = inventory.snapshot();
        let changes =
            inventory.upsert_category(category(owner, 47, Some(root.id().uuid().as_u128()), 1)).expect("inventory update applies");
        let after = inventory.snapshot();
        let disposal = session.try_reserve_disposal().expect("failed cache still reserves bounded reclamation");
        session.record_change(changes, before, after, disposal);

        let CacheLifecycle::Disabled { writer: None } = &session.lifecycle else {
            panic!("failed cache stays disabled");
        };
        assert_eq!(session.pending_write_count, 0);
        session.close().await;
    }

    #[tokio::test]
    async fn shutdown_waits_for_open() {
        let directory = tempdir().expect("create inventory cache directory");
        let owner = InventoryOwner::new(uuid(48));
        let (inventory, _, _) = cache_skeleton(owner);
        let runtime = Handle::current();
        let mut session = InventoryCacheSession::new(&runtime);
        let cache_root = directory.path().to_path_buf();
        let (opened_tx, opened_rx) = oneshot::channel();
        let (continue_tx, continue_rx) = std_mpsc::channel();
        session.start_with_open(&runtime, &inventory, move || {
            opened_tx.send(()).expect("open worker started");
            continue_rx.recv().expect("test releases cache open");
            InventoryCache::open_at_root(cache_root, "agni", owner.id(), None)
        });
        opened_rx.await.expect("cache open reached its controlled gate");

        let (closing_tx, closing_rx) = oneshot::channel();
        let closing = tokio::spawn(async move {
            closing_tx.send(()).expect("shutdown task started");
            session.close().await;
        });
        closing_rx.await.expect("shutdown entered cache close");
        continue_tx.send(()).expect("release cache open");
        closing.await.expect("cache shutdown completes");

        let (cache, _) = InventoryCache::open_at_root(directory.path(), "agni", owner.id(), None).expect("shutdown released cache");
        cache.close().await.expect("close cache after shutdown");
    }

    #[tokio::test]
    async fn restore_reclaims_old_snapshot() {
        let directory = tempdir().expect("create inventory cache directory");
        let owner = InventoryOwner::new(uuid(44));
        let (mut seeded, root, child) = cache_skeleton(owner);
        let (cache, _) = InventoryCache::open_at_root(directory.path(), "agni", owner.id(), None).expect("open inventory cache");
        let bootstrap = seeded.snapshot();
        cache.submit_bootstrap(&bootstrap).expect("write cache skeleton").wait().await.expect("skeleton write completes");

        let before = seeded.snapshot();
        let child_item = item(owner, 45, child);
        let child_item_key = child_item.key;
        let changes = apply_fetch(&mut seeded, child, ServerVersion::from_raw(4), Some(1), vec![InventoryChild::Item(child_item)])
            .expect("complete cached child folder");
        let after = seeded.snapshot();
        cache
            .submit_changeset(&changes, &before, &after)
            .expect("write completed child folder")
            .wait()
            .await
            .expect("child write completes");

        let (mut login, login_root, login_child) = cache_skeleton(owner);
        let mut session = InventoryCacheSession::new(&Handle::current());
        let cache_root = directory.path().to_path_buf();
        session.start_with_open(&Handle::current(), &login, move || InventoryCache::open_at_root(cache_root, "agni", owner.id(), None));
        let ready = session.next_event(true).await.expect("cache open completes");
        match &ready {
            InventoryCacheEvent::Ready { .. } => {}
            _ => panic!("cache open completes with Ready"),
        }
        session.handle_event(&Handle::current(), ready, &mut login);
        let batch = session.next_event(true).await.expect("cached restore batch is ready");
        match &batch {
            InventoryCacheEvent::RestoreBatch(_) => {}
            _ => panic!("cached restore batch is ready"),
        }
        let outcome = session.handle_event(&Handle::current(), batch, &mut login);
        let InventoryCacheEventOutcome::Changed { before, after, disposal } = outcome else {
            panic!("restore publishes a changed inventory snapshot");
        };

        assert_eq!(before.roots().personal(), login_root);
        assert!(before.item(child_item_key).is_none());
        assert!(after.item(child_item_key).is_some());
        assert_eq!(after.roots().personal(), login_root);
        assert!(after.category(login_child).is_some());
        disposal.reclaim(before);

        session.close().await;
        assert!(login.snapshot().category(root).is_some());
    }

    #[tokio::test]
    async fn writer_admission_failure_disables_cache() {
        let owner = InventoryOwner::new(uuid(45));
        let (mut inventory, root, _) = cache_skeleton(owner);
        let mut session = InventoryCacheSession::new(&Handle::current());
        let (sender, receiver) = mpsc::channel(CACHE_WRITER_QUEUE_CAPACITY);
        drop(receiver);
        session.lifecycle = CacheLifecycle::Active { writer: CacheWriter { sender, task: tokio::spawn(async {}) } };
        let before = inventory.snapshot();
        let changes =
            inventory.upsert_category(category(owner, 301, Some(root.id().uuid().as_u128()), 1)).expect("category update applies");
        let after = inventory.snapshot();
        let disposal = session.try_reserve_disposal().expect("failed write still reserves bounded cleanup");

        session.record_change(changes, before, after, disposal);

        let CacheLifecycle::Disabled { writer: Some(_) } = &session.lifecycle else {
            panic!("failed writer admission disables cache");
        };
        assert_eq!(session.pending_write_count, 0);
        assert_eq!(session.pending_rows, 0);
        assert_eq!(session.pending_bytes, 0);
        assert_eq!(session.pending_exclusions, 0);
        session.close().await;
    }

    #[tokio::test]
    async fn disposal_admission_is_bounded() {
        let session = InventoryCacheSession::new(&Handle::current());
        let mut permits = (0..RECLAIMER_QUEUE_CAPACITY)
            .map(|_| session.try_reserve_disposal().expect("capacity is available until filled"))
            .collect::<Vec<_>>();

        assert!(session.try_reserve_disposal().is_none());
        drop(permits.pop());
        assert!(session.try_reserve_disposal().is_some());
        drop(permits);
        session.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_reclaims_off_owner() {
        struct DropThread(std_mpsc::Sender<ThreadId>);

        impl Drop for DropThread {
            fn drop(&mut self) {
                self.0.send(thread::current().id()).expect("test waits for payload destruction");
            }
        }

        let owner_thread = thread::current().id();
        let (sender, receiver) = std_mpsc::channel();
        InventoryCacheSession::new(&Handle::current()).close_with_payload(DropThread(sender)).await;

        assert_ne!(receiver.recv().expect("shutdown awaits payload destruction"), owner_thread);
    }

    #[tokio::test]
    async fn write_failure_releases_admission() {
        let owner = InventoryOwner::new(uuid(43));
        let (mut inventory, _, _) = cache_skeleton(owner);
        let mut session = InventoryCacheSession::new(&Handle::current());
        let (sender, receiver) = mpsc::channel(CACHE_WRITER_QUEUE_CAPACITY);
        drop(receiver);
        session.lifecycle = CacheLifecycle::Active { writer: CacheWriter { sender, task: tokio::spawn(async {}) } };
        session.pending_write_count = 1;
        session.pending_rows = 3;
        session.pending_bytes = 192;
        session.pending_exclusions = 1;

        for _ in 0..2 {
            let disposal = session.try_reserve_disposal().expect("cache failure completion reserves bounded cleanup");
            session.handle_event(
                &Handle::current(),
                InventoryCacheEvent::WriteCompleted {
                    rows: 3,
                    bytes: 192,
                    affected: Vec::new(),
                    bootstrap: false,
                    result: Err("worker failed".into()),
                    disposal,
                },
                &mut inventory,
            );
        }

        let CacheLifecycle::Disabled { writer: Some(_) } = &session.lifecycle else {
            panic!("failed completion disables cache");
        };
        assert_eq!(session.pending_write_count, 0);
        assert_eq!(session.pending_rows, 0);
        assert_eq!(session.pending_bytes, 0);
        assert_eq!(session.pending_exclusions, 0);
        let RestoreLifecycle::Invalidated = &session.restore else {
            panic!("disabled cache invalidates pending restore");
        };
        session.close().await;
    }
}
