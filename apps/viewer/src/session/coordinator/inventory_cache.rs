use std::{
    collections::HashSet,
    mem::size_of,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use ::inventory::{
    Category, CategoryKey, ChangeSet, Inventory, InventoryCache, InventoryCacheOpenStatus, InventoryCacheRestore, InventorySnapshot,
};
use network::Grid;
use tokio::{
    runtime::Handle,
    sync::{mpsc, oneshot},
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
    writer: Option<CacheWriter>,
    pending_writes: Vec<PendingWrite>,
    pending_write_count: usize,
    pending_rows: usize,
    pending_bytes: usize,
    pending_exclusions: usize,
    open_started: bool,
    pending_overflow: bool,
    disabled: bool,
    restore: Option<PendingRestore>,
    restore_invalidated: bool,
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
    Changed { before: InventorySnapshot, after: InventorySnapshot, disposal: InventoryDisposalPermit },
}

impl InventoryCacheSession {
    pub(super) fn new(runtime: &Handle) -> Self {
        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        let (reclaimer_sender, mut reclaimer_receiver) = mpsc::channel(RECLAIMER_QUEUE_CAPACITY);
        let reclaimer_task = runtime.spawn(async move {
            while let Some(reclaim) = reclaimer_receiver.recv().await {
                if let Err(error) = tokio::task::spawn_blocking(reclaim).await {
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
            writer: None,
            pending_writes: Vec::new(),
            pending_write_count: 0,
            pending_rows: 0,
            pending_bytes: 0,
            pending_exclusions: 0,
            open_started: false,
            pending_overflow: false,
            disabled: false,
            restore: None,
            restore_invalidated: false,
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
        open: impl FnOnce() -> Result<(InventoryCache, InventoryCacheOpenStatus), ::inventory::InventoryCacheError> + Send + 'static,
    ) {
        if self.open_started || self.disabled {
            return;
        }
        let Some(disposal) = self.try_reserve_disposal() else {
            self.disabled = true;
            self.warn_once(&"inventory cache restore exceeded its disposal admission");
            return;
        };
        self.open_started = true;
        let invalidate_cache = self.restore_invalidated;
        let inventory = inventory.clone();
        let skeleton = inventory.snapshot();
        let sender = self.event_sender.clone();
        self.open_task = Some(runtime.spawn(async move {
            let mut disposal = Some(disposal);
            let opened = tokio::task::spawn_blocking(open).await;
            let event = match opened {
                Ok(Ok((cache, status))) => {
                    if invalidate_cache && let Err(error) = cache.discard().await {
                        InventoryCacheEvent::OpenFailed(error.to_string())
                    } else {
                        match cache.load(&inventory).await {
                            Ok(load) => {
                                let prepared = tokio::task::spawn_blocking(move || load.prepare_restore(&skeleton)).await;
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
                discard_cache_event(send_error.0).await;
            }
        }));
    }

    pub(super) async fn next_event(&mut self, allow_restore_batch: bool) -> Option<InventoryCacheEvent> {
        if allow_restore_batch && self.restore.is_some() && self.pending_exclusions == 0 {
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
        self.restore_invalidated = true;
        self.discard_restore();
    }

    pub(super) fn invalidate_for_recovery(&mut self, runtime: &Handle) {
        self.invalidate_restore();
        if self.writer.is_some() {
            self.discard_cache(runtime);
        } else if self.open_started {
            self.clear_pending_writes();
            self.pending_overflow = true;
        }
    }

    pub(super) fn record_change(
        &mut self,
        changes: ChangeSet,
        before: InventorySnapshot,
        after: InventorySnapshot,
        disposal: InventoryDisposalPermit,
    ) {
        if changes.is_empty() || self.disabled {
            disposal.reclaim((changes, before, after));
            return;
        }
        if self.writer.is_none() && (!self.open_started || self.pending_overflow) {
            disposal.reclaim((changes, before, after));
            return;
        }
        let (rows, bytes) = estimate_write(&changes);
        let next_rows = self.pending_rows.saturating_add(rows);
        let next_bytes = self.pending_bytes.saturating_add(bytes);
        if self.pending_write_count >= MAX_PENDING_WRITES || next_rows > MAX_PENDING_ROWS || next_bytes > MAX_PENDING_BYTES {
            if self.writer.is_some() {
                self.disable_and_discard("inventory cache write admission was exceeded");
            } else {
                self.clear_pending_writes();
                self.pending_write_count = 0;
                self.pending_rows = 0;
                self.pending_bytes = 0;
                self.pending_exclusions = 0;
                self.pending_overflow = true;
            }
            disposal.reclaim((changes, before, after));
            return;
        }
        self.pending_write_count += 1;
        self.pending_rows = next_rows;
        self.pending_bytes = next_bytes;
        self.pending_exclusions += 1;
        if let Some(writer) = &self.writer {
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
        } else {
            self.pending_writes.push(PendingWrite { changes, before, after, rows, bytes, disposal });
        }
    }

    pub(super) fn handle_event(
        &mut self,
        runtime: &Handle,
        event: InventoryCacheEvent,
        inventory: &mut Inventory,
    ) -> InventoryCacheEventOutcome {
        if matches!(&event, InventoryCacheEvent::RestoreBatch(_)) && self.pending_exclusions > 0 {
            return InventoryCacheEventOutcome::Unchanged;
        }
        match event {
            InventoryCacheEvent::OpenFailed(error) => {
                self.warn_once(&error);
                self.disabled = true;
                self.clear_pending_writes();
                InventoryCacheEventOutcome::Unchanged
            }
            InventoryCacheEvent::Ready { cache, restore, status, disposal } => {
                self.handle_ready(runtime, cache, restore, status, disposal, inventory);
                InventoryCacheEventOutcome::Unchanged
            }
            InventoryCacheEvent::RestoreBatch(disposal) => {
                let before = inventory.snapshot();
                let before_revision = before.revision();
                let result = self.restore.as_mut().map(|restore| restore.restore.apply_next(inventory, &restore.excluded));
                match result {
                    Some(Ok(progress)) => {
                        let outcome = if progress.changed {
                            InventoryCacheEventOutcome::Changed { before, after: inventory.snapshot(), disposal }
                        } else {
                            drop(disposal);
                            drop(before);
                            InventoryCacheEventOutcome::Unchanged
                        };
                        if progress.finished {
                            if let Some(restore) = self.restore.take() {
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
                        if let Some(restore) = self.restore.take() {
                            restore.disposal.reclaim((restore.restore, restore.excluded));
                        }
                        self.warn_once(&error);
                        self.discard_cache(runtime);
                        outcome
                    }
                    None => {
                        drop(disposal);
                        drop(before);
                        InventoryCacheEventOutcome::Unchanged
                    }
                }
            }
            InventoryCacheEvent::WriteCompleted { rows, bytes, affected, bootstrap, result, disposal } => {
                if !bootstrap {
                    self.pending_write_count = self.pending_write_count.saturating_sub(1);
                    self.pending_rows = self.pending_rows.saturating_sub(rows);
                    self.pending_bytes = self.pending_bytes.saturating_sub(bytes);
                    self.pending_exclusions = self.pending_exclusions.saturating_sub(1);
                }
                if let Some(restore) = &mut self.restore {
                    restore.excluded.extend(affected.iter().copied());
                }
                disposal.reclaim(affected);
                if let Err(error) = result {
                    self.warn_once(&error);
                    self.restore = None;
                    self.disable_and_discard("inventory cache write failed");
                }
                InventoryCacheEventOutcome::Unchanged
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
        self.event_receiver.close();
        if let Some(open_task) = self.open_task.take()
            && let Err(error) = open_task.await
        {
            self.warn_once(&error);
        }
        while let Ok(event) = self.event_receiver.try_recv() {
            discard_cache_event(event).await;
        }
        self.clear_pending_writes();
        self.discard_restore();
        if let Some(writer) = self.writer.take() {
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
    ) {
        if self.pending_overflow {
            self.disabled = true;
            self.clear_pending_writes();
            disposal.reclaim(restore);
            self.discard_in_background(runtime, cache);
            return;
        }
        tracing::debug!(?status, "inventory cache opened");
        self.writer = Some(start_cache_writer(runtime, cache, self.event_sender.clone()));
        let pending_writes = std::mem::take(&mut self.pending_writes);
        if let Some(sender) = self.writer.as_ref().map(|writer| writer.sender.clone()) {
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
            if accepting && !self.disabled {
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
        }
        if self.disabled {
            disposal.reclaim(restore);
        } else if !self.restore_invalidated {
            self.restore = Some(PendingRestore { restore, excluded: HashSet::new(), disposal });
        } else {
            disposal.reclaim(restore);
        }
    }

    fn clear_pending_writes(&mut self) {
        for pending in self.pending_writes.drain(..) {
            pending.disposal.reclaim((pending.changes, pending.before, pending.after));
        }
        self.pending_write_count = 0;
        self.pending_rows = 0;
        self.pending_bytes = 0;
        self.pending_exclusions = 0;
    }

    fn discard_cache(&mut self, _runtime: &Handle) {
        self.disabled = true;
        self.discard_restore();
        if let Some(writer) = &self.writer
            && writer.sender.try_send(CacheWriterCommand::Discard).is_err()
        {
            self.warn_once(&"inventory cache writer stopped before invalidation");
        }
    }

    fn disable_and_discard(&mut self, reason: &str) {
        self.disabled = true;
        self.discard_restore();
        if !self.warning_emitted.swap(true, Ordering::Relaxed) {
            tracing::warn!(reason, "inventory cache is unavailable for this session");
        }
        if let Some(writer) = &self.writer
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

    fn warn_once(&self, error: &impl std::fmt::Display) {
        if !self.warning_emitted.swap(true, Ordering::Relaxed) {
            tracing::warn!(%error, "inventory cache is unavailable for this session");
        }
    }

    fn discard_restore(&mut self) {
        if let Some(restore) = self.restore.take() {
            restore.disposal.reclaim((restore.restore, restore.excluded));
        }
    }
}

fn estimate_write(changes: &ChangeSet) -> (usize, usize) {
    let estimate = changes.cache_write_estimate();
    (estimate.rows(), estimate.bytes())
}

async fn discard_cache_event(event: InventoryCacheEvent) {
    match event {
        InventoryCacheEvent::Ready { cache, restore, disposal, .. } => {
            disposal.reclaim(restore);
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
                    let encoded = tokio::task::spawn_blocking(move || {
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
                    let encoded = tokio::task::spawn_blocking(move || {
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
                    if completion.send(()).is_err() {
                        tracing::trace!("inventory cache session stopped before shutdown acknowledgement");
                    }
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
    use std::{collections::HashSet, sync::Arc};

    use ::inventory::{
        AssetType, Category, CategoryKey, FolderType, Inventory, InventoryChild, InventoryLimits, InventoryOwner, InventoryType, Item,
        ItemKey, PermissionMask, Permissions, SaleInfo, SaleType, ServerVersion,
    };
    use tempfile::tempdir;
    use tokio::runtime::Handle;
    use tokio::sync::mpsc;
    use utils::{AssetId, AvatarId, FolderId, GroupId, ItemId};
    use uuid::Uuid;

    use super::{
        CACHE_WRITER_QUEUE_CAPACITY, CacheWriter, InventoryCache, InventoryCacheEvent, InventoryCacheEventOutcome, InventoryCacheSession,
        MAX_PENDING_WRITES, PendingRestore, RECLAIMER_QUEUE_CAPACITY,
    };

    fn apply_fetch(
        inventory: &mut Inventory,
        folder: CategoryKey,
        version: ServerVersion,
        descendent_count: Option<u32>,
        children: Vec<InventoryChild>,
    ) -> Result<::inventory::ChangeSet, ::inventory::InventoryError> {
        let prepared = inventory.snapshot().prepare_folder_fetch(folder, version, descendent_count, children)?;
        let ::inventory::FolderFetchCommit::Applied(changes) = inventory.commit_folder_fetch(prepared)? else {
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

    #[tokio::test]
    async fn early_delta_invalidation_survives_the_next_login() {
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
        assert!(matches!(event, InventoryCacheEvent::Ready { .. }));
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
    async fn coverage_loss_discards_complete_cache_folders() {
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
        let item_key = ItemKey::new(owner, utils::ItemId::new(uuid(111)));
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
        assert!(matches!(event, InventoryCacheEvent::Ready { .. }));
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
    async fn deferred_restore_keeps_writer_completions() {
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
        let InventoryCacheEvent::RestoreBatch(restore_disposal) = restore else { panic!("restore resumes after writes finish") };
        drop(restore_disposal);
        drop(preparation);
        session.close().await;
    }

    #[tokio::test]
    async fn pending_write_admission_drops_the_overflowing_snapshot() {
        let owner = InventoryOwner::new(uuid(42));
        let (mut inventory, root, _) = cache_skeleton(owner);
        let runtime = Handle::current();
        let mut session = InventoryCacheSession::new(&runtime);
        session.open_started = true;

        for index in 0..=MAX_PENDING_WRITES {
            let before = inventory.snapshot();
            let changes = inventory
                .upsert_category(category(owner, 100 + index as u128, Some(root.id().uuid().as_u128()), 1))
                .expect("category update applies");
            let after = inventory.snapshot();
            let disposal = session.try_reserve_disposal().expect("cache session reserves a bounded disposal slot");
            session.record_change(changes, before, after, disposal);
        }

        assert!(session.pending_overflow);
        assert_eq!(session.pending_write_count, 0);
        assert_eq!(session.pending_rows, 0);
        assert_eq!(session.pending_bytes, 0);
        assert!(session.pending_writes.is_empty());
        session.close().await;
    }

    #[tokio::test]
    async fn disabled_cache_reclaims_changes_and_snapshots_off_owner() {
        let owner = InventoryOwner::new(uuid(44));
        let (mut inventory, root, _) = cache_skeleton(owner);
        let mut session = InventoryCacheSession::new(&Handle::current());
        session.disabled = true;
        let before = inventory.snapshot();
        let changes =
            inventory.upsert_category(category(owner, 300, Some(root.id().uuid().as_u128()), 1)).expect("category update applies");
        let after = inventory.snapshot();
        let disposal = session.try_reserve_disposal().expect("disabled cache still reserves bounded cleanup");

        session.record_change(changes, before, after, disposal);

        assert_eq!(session.pending_write_count, 0);
        assert_eq!(session.pending_writes.len(), 0);
        session.close().await;
    }

    #[tokio::test]
    async fn restore_returns_old_snapshot_with_its_disposal_permit() {
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
        let restore =
            cache.load(&login).await.expect("load cached inventory").prepare_restore(&login.snapshot()).expect("prepare cached restore");
        let mut session = InventoryCacheSession::new(&Handle::current());
        let restore_disposal = session.try_reserve_disposal().expect("restore reserves cleanup capacity");
        session.restore = Some(PendingRestore { restore, excluded: HashSet::new(), disposal: restore_disposal });
        let batch_disposal = session.try_reserve_disposal().expect("restore batch reserves cleanup capacity");
        let outcome = session.handle_event(&Handle::current(), InventoryCacheEvent::RestoreBatch(batch_disposal), &mut login);
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
        cache.close().await.expect("close inventory cache");
        assert!(login.snapshot().category(root).is_some());
    }

    #[tokio::test]
    async fn failed_writer_admission_reclaims_changes_and_disables_cache() {
        let owner = InventoryOwner::new(uuid(45));
        let (mut inventory, root, _) = cache_skeleton(owner);
        let mut session = InventoryCacheSession::new(&Handle::current());
        session.open_started = true;
        let (sender, receiver) = mpsc::channel(CACHE_WRITER_QUEUE_CAPACITY);
        drop(receiver);
        session.writer = Some(CacheWriter { sender, task: tokio::spawn(async {}) });
        let before = inventory.snapshot();
        let changes =
            inventory.upsert_category(category(owner, 301, Some(root.id().uuid().as_u128()), 1)).expect("category update applies");
        let after = inventory.snapshot();
        let disposal = session.try_reserve_disposal().expect("failed write still reserves bounded cleanup");

        session.record_change(changes, before, after, disposal);

        assert!(session.disabled);
        assert_eq!(session.pending_write_count, 0);
        assert_eq!(session.pending_rows, 0);
        assert_eq!(session.pending_bytes, 0);
        assert_eq!(session.pending_exclusions, 0);
        session.close().await;
    }

    #[tokio::test]
    async fn disposal_admission_stops_at_fixed_capacity() {
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
    async fn shutdown_payload_is_dropped_on_reclaimer_worker() {
        struct DropThread(std::sync::mpsc::Sender<std::thread::ThreadId>);

        impl Drop for DropThread {
            fn drop(&mut self) {
                self.0.send(std::thread::current().id()).expect("test waits for payload destruction");
            }
        }

        let owner_thread = std::thread::current().id();
        let (sender, receiver) = std::sync::mpsc::channel();
        InventoryCacheSession::new(&Handle::current()).close_with_payload(DropThread(sender)).await;

        assert_ne!(receiver.recv().expect("shutdown awaits payload destruction"), owner_thread);
    }

    #[tokio::test]
    async fn failed_write_completion_releases_admission_once() {
        let owner = InventoryOwner::new(uuid(43));
        let (mut inventory, _, _) = cache_skeleton(owner);
        let mut session = InventoryCacheSession::new(&Handle::current());
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

        assert!(session.disabled);
        assert_eq!(session.pending_write_count, 0);
        assert_eq!(session.pending_rows, 0);
        assert_eq!(session.pending_bytes, 0);
        assert_eq!(session.pending_exclusions, 0);
        assert!(session.restore.is_none());
        session.close().await;
    }
}
