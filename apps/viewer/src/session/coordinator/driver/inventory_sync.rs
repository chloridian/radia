use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use inventory::{
    CategoryKey, ChangeSet, FolderFetchCommit, Inventory, InventoryChild, InventoryDelta, InventoryError, InventoryNodeId,
    InventoryNodeKey, InventoryPreparationCancellation, InventorySnapshot, PreparedFolderFetch, ServerVersion,
};
use network::{AgentSession, CapabilityEvent, capabilities::Client};
use protocol::lludp::{MAX_PACKET_RETAINED_CAPACITY_BYTES, Message};
use tokio::{
    sync::{oneshot, watch},
    task::{JoinError, JoinHandle},
};
use tracing::{Instrument, field};

use super::{DeferredInventoryEvent, PendingFolderPreparation, SessionDriver};
use crate::{
    inventory::{
        AisDelta, FolderApplyOutcome, FolderFetch, FolderFetchProvenance, InventoryEditEvent, InventoryFetchError, InventoryFetchEvent,
        InventoryLoadingStatus, MoveExpectation, favorites::load_favorites,
    },
    session::{
        RegionLookup,
        coordinator::{
            inventory_bootstrap::InventoryBootstrapReport,
            inventory_cache::{InventoryCacheSession, InventoryDisposalPermit},
            inventory_intake::{self, InventoryIntakeError, InventoryIntakeOutcome},
            presentation::{FavoritesUpdate, PresentationMailbox, ViewerUpdate},
        },
    },
};

pub(super) const INVENTORY_FAILURE_REPORT_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Default)]
pub(super) struct InventoryRejections {
    rejected_blocks: u64,
    last_error: Option<InventoryIntakeError>,
}

impl InventoryRejections {
    fn report(self) {
        tracing::warn!(
            rejected_blocks = self.rejected_blocks,
            last_error = self.last_error.as_ref().map(field::display),
            "inventory updates contain rejected blocks"
        );
    }
}

#[derive(Default)]
pub(super) struct InventoryFetchFailures {
    failed: u64,
    sample: Option<(CategoryKey, InventoryFetchError)>,
}

impl InventoryFetchFailures {
    fn report(self) {
        let Some((folder, error)) = self.sample else { return };
        tracing::warn!(
            failed = self.failed,
            sample_owner = %folder.owner().id(),
            sample_folder = %folder.id().uuid(),
            last_error = %error,
            "inventory folder fetches failed"
        );
    }
}

impl SessionDriver {
    pub(super) fn handle_inventory_udp(&mut self, message: &Message) -> bool {
        let is_inventory =
            self.active_session.as_ref().is_some_and(|session| inventory_intake::is_inventory_udp_message(message, session.agent_id));
        if !is_inventory {
            return false;
        }
        if self.inventory_observations_deferred() {
            return !self.defer_network_inventory(MAX_PACKET_RETAINED_CAPACITY_BYTES, || DeferredInventoryEvent::Udp(message.clone()))
                && self.note_inventory_overload();
        }
        let Some(disposal) = self.inventory_cache.try_reserve_disposal() else {
            return !self.defer_network_inventory(MAX_PACKET_RETAINED_CAPACITY_BYTES, || DeferredInventoryEvent::Udp(message.clone()))
                && self.note_inventory_overload();
        };
        self.handle_inventory_udp_now(message, disposal);
        false
    }

    pub(super) fn handle_inventory_udp_now(&mut self, message: &Message, disposal: InventoryDisposalPermit) {
        let Some(agent_id) = self.active_session.as_ref().map(|session| session.agent_id) else {
            drop(disposal);
            return;
        };
        let Some(before) = self.inventory.as_ref().map(Inventory::snapshot) else {
            if self.inventory_bootstrap.is_some() && inventory_intake::is_inventory_udp_message(message, agent_id) {
                self.inventory_bootstrap_dirty = true;
            }
            drop(disposal);
            return;
        };
        let Some(inventory) = self.inventory.as_mut() else {
            drop(disposal);
            return;
        };
        let result = inventory_intake::apply_udp(inventory, agent_id, message);
        self.apply_inventory_intake_result(result, before, disposal);
    }

    pub(super) fn handle_inventory_event_queue(&mut self, name: &str, body: &llsd::Value) -> bool {
        let is_inventory =
            self.active_session.as_ref().is_some_and(|session| inventory_intake::is_valid_inventory_event(name, body, session.agent_id));
        if !is_inventory {
            return false;
        }
        let retained_bytes = size_of::<(String, llsd::Value)>().saturating_add(name.len()).saturating_add(body.estimated_heap_bytes());
        if self.inventory_observations_deferred() {
            return !self.defer_network_inventory(retained_bytes, || DeferredInventoryEvent::EventQueue {
                name: name.to_owned(),
                body: body.clone(),
            }) && self.note_inventory_overload();
        }
        let Some(disposal) = self.inventory_cache.try_reserve_disposal() else {
            return !self.defer_network_inventory(retained_bytes, || DeferredInventoryEvent::EventQueue {
                name: name.to_owned(),
                body: body.clone(),
            }) && self.note_inventory_overload();
        };
        self.handle_inventory_event_queue_now(name, body, disposal);
        false
    }

    pub(super) fn handle_inventory_event_queue_now(&mut self, name: &str, body: &llsd::Value, disposal: InventoryDisposalPermit) {
        let Some(agent_id) = self.active_session.as_ref().map(|session| session.agent_id) else {
            drop(disposal);
            return;
        };
        let Some(before) = self.inventory.as_ref().map(Inventory::snapshot) else {
            if self.inventory_bootstrap.is_some() && inventory_intake::is_valid_inventory_event(name, body, agent_id) {
                self.inventory_bootstrap_dirty = true;
            }
            drop(disposal);
            return;
        };
        let Some(inventory) = self.inventory.as_mut() else {
            drop(disposal);
            return;
        };
        let result = inventory_intake::apply_event_queue(inventory, agent_id, name, body);
        self.apply_inventory_intake_result(result, before, disposal);
    }

    pub(super) fn handle_inventory_event_queue_batch(&mut self, events: &[CapabilityEvent]) -> bool {
        let Some(agent_id) = self.active_session.as_ref().map(|session| session.agent_id) else { return false };
        for event in events.iter().filter(|event| inventory_intake::is_valid_inventory_event(&event.message, &event.body, agent_id)) {
            let retained_bytes =
                size_of::<(String, llsd::Value)>().saturating_add(event.message.len()).saturating_add(event.body.estimated_heap_bytes());
            if self.inventory_observations_deferred() {
                if !self.defer_network_inventory(retained_bytes, || DeferredInventoryEvent::EventQueue {
                    name: event.message.clone(),
                    body: event.body.clone(),
                }) {
                    return self.note_inventory_overload();
                }
                continue;
            }
            if let Some(disposal) = self.inventory_cache.try_reserve_disposal() {
                self.handle_inventory_event_queue_now(&event.message, &event.body, disposal);
            } else if !self.defer_network_inventory(retained_bytes, || DeferredInventoryEvent::EventQueue {
                name: event.message.clone(),
                body: event.body.clone(),
            }) {
                return self.note_inventory_overload();
            }
        }
        false
    }

    fn apply_inventory_intake_result(
        &mut self,
        result: Result<Option<InventoryIntakeOutcome>, InventoryIntakeError>,
        before: InventorySnapshot,
        disposal: InventoryDisposalPermit,
    ) {
        match result {
            Ok(Some(outcome)) => {
                self.note_rejected_inventory_blocks(outcome.rejected_blocks);
                if !outcome.changes.is_empty()
                    && let Some(snapshot) = self.inventory.as_ref().map(Inventory::snapshot)
                {
                    self.process_inventory_changes(&before, &snapshot);
                    self.request_inventory_refetches(&outcome.changes, &snapshot);
                    self.publish_inventory_status();
                    publish_inventory_change(
                        &self.inventory_changes,
                        &mut self.inventory_cache,
                        outcome.changes,
                        before,
                        snapshot,
                        disposal,
                    );
                } else {
                    drop(disposal);
                }
            }
            Ok(None) => drop(disposal),
            Err(error) => {
                drop(disposal);
                self.note_rejected_inventory_blocks_with_error(error);
            }
        }
    }

    fn note_rejected_inventory_blocks(&mut self, count: usize) {
        if count > 0 {
            self.record_rejected_inventory_blocks(count as u64, None);
        }
    }

    fn note_rejected_inventory_blocks_with_error(&mut self, error: InventoryIntakeError) {
        self.record_rejected_inventory_blocks(1, Some(error));
    }

    fn record_rejected_inventory_blocks(&mut self, count: u64, error: Option<InventoryIntakeError>) {
        let due = self.rejected_inventory_blocks.record(Instant::now(), |rejections| {
            rejections.rejected_blocks = rejections.rejected_blocks.saturating_add(count);
            if error.is_some() {
                rejections.last_error = error;
            }
        });
        if let Some(due) = due {
            due.report();
        }
    }

    pub(super) fn flush_inventory_summaries(&mut self) {
        if let Some(due) = self.rejected_inventory_blocks.flush() {
            due.report();
        }
        if let Some(due) = self.inventory_fetch_failures.flush() {
            due.report();
        }
    }

    pub(super) fn finish_inventory_bootstrap(&mut self, inventory: Inventory, report: InventoryBootstrapReport) {
        if report.rejected_personal_entries > 0 || report.rejected_library_entries > 0 {
            tracing::warn!(
                personal_rejected = report.rejected_personal_entries,
                library_rejected = report.rejected_library_entries,
                "login inventory skeleton contained rejected folders"
            );
        }
        if self.inventory_bootstrap_dirty {
            self.inventory_cache.invalidate_restore();
            tracing::debug!("inventory updates arrived before login inventory; refreshing cached contents");
        }
        if let Some(session) = &self.active_session {
            let library_owner = inventory.roots().library().map(|root| root.owner().id());
            self.inventory_cache.start(&self.runtime, self.credentials.grid, session.agent_id, library_owner, &inventory);
        }
        self.inventory_changes.send_replace(Some(inventory.snapshot()));
        self.inventory = Some(inventory);
        self.inventory_bootstrap_dirty = false;
        if let Some(capabilities) = self.active_capabilities.clone() {
            self.start_inventory_edits(capabilities.clone());
            self.start_inventory_fetch(capabilities);
        }
        if let Some(simulator_id) = self.primary_simulator
            && let Some(session) = self.active_session.clone()
        {
            self.publish_session_ready_update(simulator_id, session);
        }
        self.start_favorites_load_for_active();
    }

    pub(super) fn start_favorites_load_for_active(&mut self) {
        cancel_favorites_refresh(&mut self.favorites_load);
        if !self.favorites_enabled {
            return;
        }
        let (Some(session), Some(capabilities), Some(lookup)) =
            (self.active_session.clone(), self.active_capabilities.clone(), self.region_lookup.clone())
        else {
            return;
        };
        let inventory = match self.inventory.as_ref() {
            Some(inventory) => Some(inventory.snapshot()),
            None if self.inventory_bootstrap.is_some() => return,
            None => None,
        };
        self.favorites_load = Some(self.spawn_favorites_load(session, capabilities, lookup, inventory));
    }

    fn spawn_favorites_load(
        &self,
        session: AgentSession,
        capabilities: Client,
        lookup: RegionLookup,
        inventory: Option<InventorySnapshot>,
    ) -> JoinHandle<FavoritesUpdate> {
        let username = self.credentials.username.clone();
        self.runtime.spawn(
            async move {
                let result = load_favorites(&capabilities, &lookup, &session, inventory.as_ref()).await;
                FavoritesUpdate { grid: session.grid, agent_id: session.agent_id, session_id: session.session_id, username, result }
            }
            .in_current_span(),
        )
    }

    fn start_inventory_fetch(&mut self, capabilities: Client) {
        let Some(inventory) = &self.inventory else { return };
        let snapshot = inventory.snapshot();
        self.inventory_changes.send_replace(Some(snapshot.clone()));
        self.inventory_failed.clear();
        self.inventory_background_done = false;
        self.inventory_last_failure = None;
        self.publish_inventory_status();
        let model = self.inventory_changes.subscribe();
        self.inventory_fetch.start(&self.runtime, capabilities, snapshot, model, self.cancellation.clone());
    }

    fn start_inventory_edits(&mut self, capabilities: Client) {
        if self.inventory.is_none() {
            return;
        }
        let Some(session) = self.active_session.clone() else { return };
        let model = self.inventory_changes.subscribe();
        self.inventory_edits.start(&self.runtime, capabilities, self.session_handle.clone(), session, model, self.cancellation.clone());
    }

    pub(super) fn handle_inventory_edit_event_now(&mut self, event: InventoryEditEvent, disposal: InventoryDisposalPermit) {
        match event {
            InventoryEditEvent::Apply { confirmation, applied } => {
                let result = self.apply_inventory_confirmation(confirmation, disposal);
                applied.send(result).ok();
            }
            InventoryEditEvent::RefreshFolders { folders, refreshed } => {
                let folder_count = folders.len();
                let refreshed_inventory = self.mark_inventory_folders_incomplete(folders, disposal);
                if !refreshed_inventory {
                    tracing::debug!(folder_count, "inventory folder refresh could not be queued after an edit failure");
                }
                refreshed.send(refreshed_inventory).ok();
            }
            InventoryEditEvent::MoveStarted { expectation, confirmed, registered } => {
                self.inventory_pending_moves.insert(expectation.node, (expectation, confirmed));
                drop(disposal);
                registered.send(()).ok();
            }
            InventoryEditEvent::MoveAborted { expectation, removed } => {
                if self.inventory_pending_moves.get(&expectation.node).is_some_and(|(current, _)| *current == expectation) {
                    self.inventory_pending_moves.remove(&expectation.node);
                }
                drop(disposal);
                removed.send(()).ok();
            }
            InventoryEditEvent::MoveTimedOut { expectation, refreshed } => {
                if self.inventory_pending_moves.get(&expectation.node).is_some_and(|(current, _)| *current == expectation) {
                    self.inventory_pending_moves.remove(&expectation.node);
                }
                let applied = self.mark_inventory_folders_incomplete([expectation.from, expectation.to], disposal);
                refreshed.send(applied).ok();
            }
        }
    }

    fn apply_inventory_confirmation(&mut self, confirmation: AisDelta, disposal: InventoryDisposalPermit) -> bool {
        let Some(current) = self.inventory.as_ref().map(Inventory::snapshot) else {
            disposal.reclaim(confirmation);
            return false;
        };
        if !confirmation.is_current(&current) {
            let folders = confirmation.refetch_folders().to_vec();
            if let Some(refresh_disposal) = self.inventory_cache.try_reserve_disposal() {
                disposal.reclaim(confirmation);
                return self.mark_inventory_folders_incomplete(folders, refresh_disposal);
            }
            if !folders.is_empty() {
                let bytes = folders.capacity().saturating_mul(size_of::<CategoryKey>());
                if self.can_defer_network_inventory(1, bytes) {
                    disposal.reclaim(confirmation);
                    self.defer_inventory(DeferredInventoryEvent::RefreshFolders(folders));
                    return true;
                }
            }
            disposal.reclaim((confirmation, folders));
            self.inventory_overload_recovery_pending = true;
            return true;
        }
        self.apply_inventory_edit_delta(confirmation.into_delta(&current), disposal)
    }

    fn apply_inventory_edit_delta(&mut self, delta: InventoryDelta, disposal: InventoryDisposalPermit) -> bool {
        let Some(before) = self.inventory.as_ref().map(Inventory::snapshot) else {
            disposal.reclaim(delta);
            return false;
        };
        let Some(inventory) = self.inventory.as_mut() else {
            disposal.reclaim((delta, before));
            return false;
        };
        let changes = match inventory.apply_delta(delta) {
            Ok(changes) => changes,
            Err(error) => {
                disposal.reclaim(before);
                tracing::warn!(%error, "inventory edit confirmation could not be applied");
                return false;
            }
        };
        if changes.is_empty() {
            disposal.reclaim((changes, before));
            return true;
        }
        let Some(snapshot) = self.inventory.as_ref().map(Inventory::snapshot) else {
            disposal.reclaim((changes, before));
            return false;
        };
        self.process_inventory_changes(&before, &snapshot);
        self.request_inventory_refetches(&changes, &snapshot);
        self.publish_inventory_status();
        publish_inventory_change(&self.inventory_changes, &mut self.inventory_cache, changes, before, snapshot, disposal);
        true
    }

    pub(super) fn process_inventory_changes(&mut self, before: &InventorySnapshot, after: &InventorySnapshot) {
        resolve_pending_inventory_moves_between(&mut self.inventory_pending_moves, before, after);
        self.inventory_failed.retain(|folder| after.category(*folder).is_some());
    }

    pub(super) fn mark_inventory_folders_incomplete(
        &mut self,
        folders: impl IntoIterator<Item = CategoryKey>,
        disposal: InventoryDisposalPermit,
    ) -> bool {
        let Some(snapshot) = self.inventory.as_ref().map(Inventory::snapshot) else {
            drop(disposal);
            return false;
        };
        let folders = folders.into_iter().filter(|folder| snapshot.category(*folder).is_some()).collect::<HashSet<_>>();
        if folders.is_empty() {
            disposal.reclaim(snapshot);
            return false;
        }
        drop(snapshot);
        let delta = InventoryDelta {
            updated_category_versions: folders.into_iter().map(|folder| (folder, ServerVersion::UNKNOWN)).collect(),
            ..InventoryDelta::default()
        };
        self.apply_inventory_edit_delta(delta, disposal)
    }

    pub(super) fn handle_inventory_fetch_now(&mut self, event: InventoryFetchEvent, disposal: InventoryDisposalPermit) {
        match event {
            InventoryFetchEvent::Folder { fetch, applied } => {
                let folder = fetch.folder;
                let Some(inventory) = self.inventory.as_ref() else {
                    applied.send(FolderApplyOutcome::Rejected).ok();
                    disposal.reclaim(fetch);
                    return;
                };
                let before = inventory.snapshot();
                let worker_snapshot = before.clone();
                let cancellation = InventoryPreparationCancellation::default();
                let worker_cancellation = cancellation.clone();
                let task = self.runtime.spawn_blocking(move || prepare_inventory_fetch(&worker_snapshot, fetch, &worker_cancellation));
                self.pending_folder_preparation = Some(PendingFolderPreparation { folder, before, applied, cancellation, disposal, task });
            }
            InventoryFetchEvent::Failed { folder, error } => {
                drop(disposal);
                self.record_inventory_failure(folder, error);
                self.publish_inventory_status();
            }
            InventoryFetchEvent::BackgroundDone => {
                drop(disposal);
                self.inventory_background_done = true;
                self.publish_inventory_status();
            }
        }
    }

    fn request_inventory_refetches(&mut self, changes: &ChangeSet, snapshot: &InventorySnapshot) {
        let handle = self.inventory_fetch.handle();
        let mut queue_failures = 0usize;
        for folder in &changes.refetch_categories {
            if snapshot.category(*folder).is_none() {
                continue;
            }
            if handle.refetch_folder(*folder).is_err() {
                self.inventory_failed.insert(*folder);
                self.inventory_last_failure = Some((*folder, InventoryFetchError::RequestFailed));
                queue_failures = queue_failures.saturating_add(1);
            }
        }
        if queue_failures > 0 {
            tracing::warn!(queue_failures, "could not queue inventory folder refetches");
        }
    }

    fn record_inventory_failure(&mut self, folder: CategoryKey, error: InventoryFetchError) {
        self.inventory_failed.insert(folder);
        self.inventory_last_failure = Some((folder, error.clone()));
        let due = self.inventory_fetch_failures.record(Instant::now(), |failures| {
            failures.failed = failures.failed.saturating_add(1);
            failures.sample = Some((folder, error));
        });
        if let Some(due) = due {
            due.report();
        }
    }

    pub(super) fn publish_inventory_status(&self) {
        let (folders_known, folders_complete) = self.inventory.as_ref().map_or((0, 0), |inventory| {
            let snapshot = inventory.snapshot();
            let known = snapshot.category_count();
            (known, known.saturating_sub(snapshot.incomplete_category_count_all()))
        });
        self.inventory_fetch.publish(InventoryLoadingStatus {
            folders_known,
            folders_complete,
            folders_failed: self.inventory_failed.len(),
            background_done: self.inventory_background_done,
            last_failure: self.inventory_last_failure.clone(),
        });
    }
}

pub(super) fn cancel_favorites_refresh(task: &mut Option<JoinHandle<FavoritesUpdate>>) {
    if let Some(task) = task.take() {
        task.abort();
    }
}

pub(super) fn publish_favorites_refresh(
    task: &mut Option<JoinHandle<FavoritesUpdate>>,
    mailbox: &PresentationMailbox,
    result: Result<FavoritesUpdate, JoinError>,
) {
    *task = None;
    match result {
        Ok(update) => mailbox.publish(ViewerUpdate::Favorites(update)),
        Err(error) if error.is_cancelled() => {}
        Err(error) => tracing::warn!(%error, "favorites refresh worker failed"),
    }
}

pub(super) async fn stop_favorites_refresh(task: &mut Option<JoinHandle<FavoritesUpdate>>) {
    if let Some(task) = task.take() {
        task.abort();
        if let Err(error) = task.await
            && !error.is_cancelled()
        {
            tracing::warn!(%error, "favorites refresh worker failed during shutdown");
        }
    }
}

#[cfg(test)]
pub(crate) fn confirm_pending_moves(
    pending: &mut HashMap<InventoryNodeKey, (MoveExpectation, oneshot::Sender<()>)>,
    before: &InventorySnapshot,
    after: &InventorySnapshot,
) {
    resolve_pending_inventory_moves_between(pending, before, after);
}

fn inventory_node_parent(snapshot: &InventorySnapshot, node: InventoryNodeKey) -> Option<CategoryKey> {
    match node.id() {
        InventoryNodeId::Category(id) => snapshot.category(CategoryKey::new(node.owner(), id)).and_then(|category| category.parent),
        InventoryNodeId::Item(id) => snapshot.item(inventory::ItemKey::new(node.owner(), id)).map(|item| item.parent),
    }
}

fn resolve_pending_inventory_moves_between(
    pending: &mut HashMap<InventoryNodeKey, (MoveExpectation, oneshot::Sender<()>)>,
    before: &InventorySnapshot,
    after: &InventorySnapshot,
) {
    let resolved = pending
        .iter()
        .filter_map(|(node, (expectation, _))| {
            (inventory_node_parent(before, *node) == Some(expectation.from) && inventory_node_parent(after, *node) == Some(expectation.to))
                .then_some(*node)
        })
        .collect::<Vec<_>>();
    for node in resolved {
        if let Some((_, confirmed)) = pending.remove(&node) {
            confirmed.send(()).ok();
        }
    }
}

pub(super) enum PreparedInventoryFetch {
    Prepared(PreparedFolderFetch),
    Stale,
}

pub(super) fn prepare_inventory_fetch(
    current: &InventorySnapshot,
    mut fetch: FolderFetch,
    cancellation: &InventoryPreparationCancellation,
) -> Result<PreparedInventoryFetch, InventoryError> {
    if current.category(fetch.folder).is_none() {
        return Ok(PreparedInventoryFetch::Stale);
    }
    if let FolderFetchProvenance::StartedAt { folder, owner } = fetch.provenance
        && (current.folder_generation(fetch.folder) != folder || current.owner_refresh_generation(fetch.folder.owner()) != owner)
    {
        return Ok(PreparedInventoryFetch::Stale);
    }
    let stale = current.is_complete(fetch.folder)
        && current.category(fetch.folder).is_some_and(|category| {
            category.server_version.is_known() && fetch.version.is_known() && category.server_version.raw() > fetch.version.raw()
        });
    if stale {
        return Ok(PreparedInventoryFetch::Stale);
    }
    for (index, child) in fetch.children.iter_mut().enumerate() {
        if index.is_multiple_of(128) && cancellation.is_cancelled() {
            return Err(InventoryError::Cancelled);
        }
        let InventoryChild::Category(incoming) = child else { continue };
        let Some(existing) = current.category(incoming.key) else { continue };
        if current.is_complete(incoming.key)
            && existing.server_version.is_known()
            && incoming.server_version.is_known()
            && existing.server_version.raw() > incoming.server_version.raw()
        {
            incoming.server_version = existing.server_version;
            incoming.server_descendent_count = existing.server_descendent_count;
        }
    }
    current
        .prepare_folder_fetch_cancellable(fetch.folder, fetch.version, fetch.descendent_count, fetch.children, cancellation)
        .map(PreparedInventoryFetch::Prepared)
}

pub(super) fn finish_inventory_fetch_preparation(
    driver: &mut SessionDriver,
    result: Result<Result<PreparedInventoryFetch, InventoryError>, JoinError>,
) {
    let Some(preparation) = driver.pending_folder_preparation.take() else { return };
    let PendingFolderPreparation { folder, before, applied, disposal, .. } = preparation;
    let outcome = match result {
        Ok(Ok(PreparedInventoryFetch::Prepared(prepared))) => {
            let Some(inventory) = driver.inventory.as_mut() else {
                disposal.reclaim((prepared, before));
                applied.send(FolderApplyOutcome::Rejected).ok();
                return;
            };
            match inventory.commit_folder_fetch(prepared) {
                Ok(FolderFetchCommit::Applied(changes)) => {
                    let after = inventory.snapshot();
                    driver.process_inventory_changes(&before, &after);
                    driver.inventory_failed.remove(&folder);
                    if driver.inventory_last_failure.as_ref().is_some_and(|(failed, _)| *failed == folder) {
                        driver.inventory_last_failure = None;
                    }
                    publish_inventory_change(&driver.inventory_changes, &mut driver.inventory_cache, changes, before, after, disposal);
                    FolderApplyOutcome::Applied
                }
                Ok(FolderFetchCommit::StaleBase(prepared)) => {
                    disposal.reclaim((prepared, before));
                    FolderApplyOutcome::Stale
                }
                Err(error) => {
                    disposal.reclaim(before);
                    driver.record_inventory_failure(folder, InventoryFetchError::Model(error));
                    FolderApplyOutcome::Rejected
                }
            }
        }
        Ok(Ok(PreparedInventoryFetch::Stale)) => {
            disposal.reclaim(before);
            FolderApplyOutcome::Stale
        }
        Ok(Err(InventoryError::Cancelled)) => {
            disposal.reclaim(before);
            FolderApplyOutcome::Rejected
        }
        Ok(Err(error)) => {
            disposal.reclaim(before);
            driver.record_inventory_failure(folder, InventoryFetchError::Model(error));
            FolderApplyOutcome::Rejected
        }
        Err(error) => {
            disposal.reclaim(before);
            tracing::warn!(folder = %folder.id().uuid(), %error, "inventory folder preparation worker failed");
            driver.record_inventory_failure(folder, InventoryFetchError::RequestFailed);
            FolderApplyOutcome::Rejected
        }
    };
    applied.send(outcome).ok();
    driver.publish_inventory_status();
}

pub(super) fn publish_inventory_change(
    inventory_changes: &watch::Sender<Option<InventorySnapshot>>,
    inventory_cache: &mut InventoryCacheSession,
    changes: ChangeSet,
    before: InventorySnapshot,
    after: InventorySnapshot,
    disposal: InventoryDisposalPermit,
) {
    inventory_changes.send_replace(Some(after.clone()));
    inventory_cache.record_change(changes, before, after, disposal);
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) enum FolderFetchApply {
    Applied(ChangeSet),
    Stale,
}

#[cfg(test)]
pub(crate) fn apply_inventory_fetch(inventory: &mut Inventory, fetch: FolderFetch) -> Result<FolderFetchApply, InventoryError> {
    let current = inventory.snapshot();
    let cancellation = InventoryPreparationCancellation::default();
    let prepared = match prepare_inventory_fetch(&current, fetch, &cancellation)? {
        PreparedInventoryFetch::Prepared(prepared) => prepared,
        PreparedInventoryFetch::Stale => return Ok(FolderFetchApply::Stale),
    };
    match inventory.commit_folder_fetch(prepared)? {
        FolderFetchCommit::Applied(changes) => Ok(FolderFetchApply::Applied(changes)),
        FolderFetchCommit::StaleBase(_) => Ok(FolderFetchApply::Stale),
    }
}
