use std::{
    collections::{HashMap, HashSet, VecDeque},
    num::NonZeroU32,
    ops::Bound::{Excluded, Unbounded},
};

use math::Vec3;
use uuid::Uuid;

use crate::{
    Binding, Entity, Kind, LocalAddress, Parent, Recovery, RecoveryCancellation, Region, World, WorldChange, WorldEntityRemovalReason,
    WorldRevision,
    history::WorldChange::{EntityRemoved, SubtreeRemovalPending},
    mutation::Effects,
    object::relationships::PendingChildKey,
    update::{FieldStamp, Stamp},
};

pub const MAX_REMOVAL_WORK_PER_TURN: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RemovalFence {
    region: Region,
    order: u64,
    block_ordinal: u32,
}

impl RemovalFence {
    pub const fn new(region: Region, order: u64, block_ordinal: u32) -> Self {
        Self { region, order, block_ordinal }
    }

    pub const fn region(self) -> Region {
        self.region
    }

    pub const fn order(self) -> u64 {
        self.order
    }

    pub const fn block_ordinal(self) -> u32 {
        self.block_ordinal
    }
}

#[must_use]
#[derive(Debug, Default)]
pub struct RemovalProgress {
    pub work_units: usize,
    pub pending: bool,
    pub removed_entities: usize,
    pub revision: Option<WorldRevision>,
    pub recovery: Recovery,
    pub recovery_cancellations: Vec<RecoveryCancellation>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum RemovalCause {
    Killed(Stamp),
    AddressReplaced(Stamp),
    ParentCascade(Stamp),
    RegionRetired,
}

impl RemovalCause {
    pub(crate) fn stamp(self) -> Option<Stamp> {
        match self {
            Self::Killed(stamp) | Self::AddressReplaced(stamp) | Self::ParentCascade(stamp) => Some(stamp),
            Self::RegionRetired => None,
        }
    }

    fn reason(self) -> WorldEntityRemovalReason {
        match self {
            Self::Killed(_) => WorldEntityRemovalReason::Killed,
            Self::AddressReplaced(_) => WorldEntityRemovalReason::AddressReplaced,
            Self::ParentCascade(_) => WorldEntityRemovalReason::ParentCascade,
            Self::RegionRetired => WorldEntityRemovalReason::RegionRetired,
        }
    }

    fn tombstone_stamp(self) -> Option<Stamp> {
        match self {
            Self::Killed(stamp) | Self::ParentCascade(stamp) => Some(stamp),
            Self::AddressReplaced(_) | Self::RegionRetired => None,
        }
    }

    fn supersedes(self, other: Self) -> bool {
        match (self.stamp(), other.stamp()) {
            (Some(stamp), Some(other)) => stamp >= other,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => true,
        }
    }

    fn removal_fence(self, region: Region) -> Option<RemovalFence> {
        self.stamp().map(|stamp| RemovalFence { region, order: stamp.order, block_ordinal: stamp.block_ordinal })
    }

    fn cascades(self, parent_region: Region, child_region: Region, child_kind: Kind, child_fence: Stamp) -> bool {
        match self {
            Self::RegionRetired => child_region == parent_region,
            Self::Killed(stamp) | Self::AddressReplaced(stamp) | Self::ParentCascade(stamp) => {
                child_region == parent_region && child_kind != Kind::Avatar && stamp > child_fence
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct RemovalFrame {
    pub(crate) entity: Entity,
    pub(crate) cause: RemovalCause,
    pub(crate) preserve_child: Option<Entity>,
    cancelled_at_invalidation: bool,
    after_child: Option<Uuid>,
    after_work: Option<(Uuid, LocalAddress)>,
    active_work: Option<(Uuid, LocalAddress)>,
    after_work_child: Option<(WorldRevision, Uuid)>,
    pending_child: Option<PendingRemovalCandidate>,
    phase: RemovalPhase,
}

#[derive(Clone, Copy)]
struct PendingRemovalCandidate {
    entity: Entity,
    link_admitted: bool,
    cascade: bool,
    cycle_cursor: Option<Entity>,
    cycle_steps: usize,
    cycle_checked: bool,
    cycle: bool,
}

impl RemovalFrame {
    pub(crate) fn new(entity: Entity, cause: RemovalCause, preserve_child: Option<Entity>, cancelled_at_invalidation: bool) -> Self {
        Self {
            entity,
            cause,
            preserve_child,
            cancelled_at_invalidation,
            after_child: None,
            after_work: None,
            active_work: None,
            after_work_child: None,
            pending_child: None,
            phase: RemovalPhase::Children,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct RegionFrame {
    pub(crate) region: Region,
    after_object: Option<Uuid>,
    phase: RegionPhase,
}

impl RegionFrame {
    pub(crate) fn new(region: Region) -> Self {
        Self { region, after_object: None, phase: RegionPhase::Objects }
    }
}

#[derive(Clone, Copy)]
enum RemovalPhase {
    Children,
    RelationshipWork,
    Finalize,
}

#[derive(Clone, Copy)]
enum RegionPhase {
    Objects,
    State,
    Tombstones,
}

enum PendingRemovalStep {
    Progress(Option<RemovalFrame>),
    Deferred,
}

#[derive(Clone, Copy)]
pub(crate) enum ReclamationFrame {
    Entity(RemovalFrame),
    Region(RegionFrame),
    Transfer(Entity),
}

#[derive(Clone, Copy)]
pub(crate) enum ReclamationTask {
    Entity(RemovalFrame),
    Region(RegionFrame),
    Transfer(Entity),
}

#[derive(Default)]
pub(crate) struct TransferProgress {
    after_child: Option<Uuid>,
    phase: TransferPhase,
    after_work: Option<LocalAddress>,
    active_work: Option<LocalAddress>,
    after_pending_child: Option<PendingChildKey>,
    old_addresses: HashMap<LocalAddress, Binding>,
    release_addresses: VecDeque<LocalAddress>,
    latest_addresses: HashMap<Region, LocalAddress>,
    queued: bool,
}

#[derive(Clone, Copy, Default)]
enum TransferPhase {
    #[default]
    Children,
    Relationships,
    Release,
}

pub(crate) struct ParentTransferReservation {
    entity: Entity,
    old_address: LocalAddress,
    old_binding: Binding,
    progress: Option<TransferProgress>,
}

#[derive(Clone, Copy)]
pub(crate) struct Visibility {
    pub(crate) visible: bool,
    pub(crate) cause: Option<RemovalCause>,
}

#[derive(Default)]
pub(crate) struct VisibilityCache {
    entities: HashMap<Entity, Visibility>,
    pending_causes: HashMap<(LocalAddress, Option<Uuid>), Vec<PendingParentCause>>,
}

#[derive(Clone, Copy)]
struct CachedVisibility {
    revision: WorldRevision,
    visibility: Visibility,
}

#[derive(Default)]
pub(crate) struct VisibilityLookupCache {
    entries: HashMap<Entity, CachedVisibility>,
    order: VecDeque<(Entity, WorldRevision)>,
}

const MAX_VISIBILITY_LOOKUP_CACHE_ENTRIES: usize = 256;

impl VisibilityLookupCache {
    pub(crate) fn get(&self, entity: Entity, revision: WorldRevision) -> Option<Visibility> {
        self.entries.get(&entity).filter(|entry| entry.revision == revision).map(|entry| entry.visibility)
    }

    pub(crate) fn insert(&mut self, entity: Entity, revision: WorldRevision, visibility: Visibility) {
        self.entries.insert(entity, CachedVisibility { revision, visibility });
        self.order.push_back((entity, revision));
        while self.order.len() > MAX_VISIBILITY_LOOKUP_CACHE_ENTRIES {
            if let Some((old_entity, old_revision)) = self.order.pop_front()
                && self.entries.get(&old_entity).is_some_and(|entry| entry.revision == old_revision)
            {
                self.entries.remove(&old_entity);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct VisibilityCursor {
    entity: Entity,
    id: Uuid,
    current: Entity,
    steps: usize,
    path: VisibilityPath,
    pending: Option<PendingVisibility>,
}

impl VisibilityCursor {
    pub(crate) fn new(entity: Entity, id: Uuid) -> Self {
        Self { entity, id, current: entity, steps: 0, path: VisibilityPath::default(), pending: None }
    }

    pub(crate) const fn entity(self) -> Entity {
        self.entity
    }

    pub(crate) const fn id(self) -> Uuid {
        self.id
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingVisibility {
    address: LocalAddress,
    expected_uuid: Option<Uuid>,
    current_address_entity: Option<Entity>,
    parent_field: Option<FieldStamp>,
    after_parent: Option<Uuid>,
    fallback_parent_seen: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VisibilityPath {
    same_region: bool,
    contains_avatar: bool,
    max_child_fence: Option<Stamp>,
}

impl Default for VisibilityPath {
    fn default() -> Self {
        Self { same_region: true, contains_avatar: false, max_child_fence: None }
    }
}

impl VisibilityPath {
    fn include_edge(&mut self, child: &crate::ObjectRecord, parent: Option<&crate::ObjectRecord>) {
        self.same_region &= parent.is_some_and(|parent| parent.state.binding.address.region() == child.state.binding.address.region());
        self.contains_avatar |= child.kind == Kind::Avatar;
        let fence = child.state.binding.fence();
        self.max_child_fence = Some(self.max_child_fence.map_or(fence, |previous| previous.max(fence)));
    }

    fn inherit(self, visibility: Visibility, parent_region: Option<Region>) -> Visibility {
        if !visibility.visible && visibility.cause.is_none() {
            return visibility;
        }
        let Some(cause) = visibility.cause else { return visibility };
        let cascades = self.same_region
            && match cause {
                RemovalCause::RegionRetired => true,
                RemovalCause::Killed(stamp) | RemovalCause::AddressReplaced(stamp) | RemovalCause::ParentCascade(stamp) => {
                    !self.contains_avatar && self.max_child_fence.is_none_or(|fence| stamp > fence)
                }
            }
            && parent_region.is_some();
        if cascades { Visibility { visible: false, cause: Some(cause) } } else { Visibility { visible: true, cause: None } }
    }
}

pub(crate) enum VisibilityStep {
    Pending,
    Complete(Visibility),
}

struct PendingParentCause {
    cause: RemovalCause,
    parent_region: Region,
    parent: Entity,
    id: Uuid,
    binding: Binding,
}

impl World {
    #[cfg(test)]
    pub(crate) fn transfer_retention_is_valid(&self) -> bool {
        self.transfer_address_count == self.transfer_cursors.values().map(|progress| progress.old_addresses.len()).sum::<usize>()
            && self.transfer_address_count <= self.limits.max_entities
            && self.transfer_cursors.values().all(|progress| {
                progress.release_addresses.len() == progress.old_addresses.len()
                    && progress.release_addresses.iter().all(|address| progress.old_addresses.contains_key(address))
                    && progress.release_addresses.iter().collect::<HashSet<_>>().len() == progress.release_addresses.len()
                    && progress.latest_addresses.len() <= progress.old_addresses.len()
                    && progress
                        .latest_addresses
                        .iter()
                        .all(|(region, address)| address.region() == *region && progress.old_addresses.contains_key(address))
            })
    }

    pub fn has_pending_removals(&self) -> bool {
        !self.removal_tasks.is_empty() || !self.reclamation.is_empty() || !self.transfer_cursors.is_empty()
    }

    pub fn reclaim_removals(&mut self, max_work: usize) -> RemovalProgress {
        let limit = max_work.min(MAX_REMOVAL_WORK_PER_TURN);
        let mut progress = RemovalProgress::default();
        let mut changes = Vec::with_capacity(limit.saturating_mul(2));
        let mut recovery_targets = Vec::with_capacity(limit);
        let mut recovery_cancellations = Vec::with_capacity(limit);

        while progress.work_units < limit {
            if self.reclamation.is_empty() {
                let Some(next) = self.removal_tasks.pop_front() else { break };
                self.reclamation.push(match next {
                    ReclamationTask::Entity(frame) => ReclamationFrame::Entity(frame),
                    ReclamationTask::Region(frame) => ReclamationFrame::Region(frame),
                    ReclamationTask::Transfer(entity) if self.transfer_cursors.contains_key(&entity) => ReclamationFrame::Transfer(entity),
                    ReclamationTask::Transfer(_) => {
                        progress.work_units += 1;
                        continue;
                    }
                });
            }

            let Some(frame) = self.reclamation.pop() else { break };
            match frame {
                ReclamationFrame::Entity(mut frame) => match frame.phase {
                    RemovalPhase::Children => {
                        let next_child = self.children.get(&frame.entity).and_then(|children| {
                            let lower = frame.after_child.map_or(Unbounded, Excluded);
                            children.range((lower, Unbounded)).next().map(|(id, child)| (*id, *child))
                        });
                        if let Some((child_id, child)) = next_child {
                            if self.limits.max_recovery_targets != 0
                                && recovery_targets.len() >= self.limits.max_recovery_targets
                                && self.child_requires_recovery(frame, child)
                            {
                                self.reclamation.push(ReclamationFrame::Entity(frame));
                                break;
                            }
                            frame.after_child = Some(child_id);
                            self.reclamation.push(ReclamationFrame::Entity(frame));
                            self.reclaim_child(frame, child, &mut changes, &mut recovery_targets, &mut recovery_cancellations);
                            progress.work_units += 1;
                        } else {
                            frame.phase = RemovalPhase::RelationshipWork;
                            self.reclamation.push(ReclamationFrame::Entity(frame));
                        }
                    }
                    RemovalPhase::RelationshipWork => {
                        let Some(id) = self.store.object_record(frame.entity).map(|record| record.id) else {
                            frame.phase = RemovalPhase::Finalize;
                            self.reclamation.push(ReclamationFrame::Entity(frame));
                            continue;
                        };
                        if let Some(key) = frame.active_work {
                            if frame.pending_child.is_some() {
                                match self.advance_pending_removal(&mut frame, &mut changes, &mut recovery_targets) {
                                    PendingRemovalStep::Deferred => {
                                        self.reclamation.push(ReclamationFrame::Entity(frame));
                                        break;
                                    }
                                    PendingRemovalStep::Progress(child_frame) => {
                                        self.reclamation.push(ReclamationFrame::Entity(frame));
                                        if let Some(child_frame) = child_frame {
                                            self.reclamation.push(ReclamationFrame::Entity(child_frame));
                                        }
                                        progress.work_units += 1;
                                    }
                                }
                            } else {
                                let next_child = self.pending_children.get(&key.1).and_then(|children| {
                                    let lower = frame.after_work_child.map_or(Unbounded, Excluded);
                                    children.range((lower, Unbounded)).next().map(|(key, child)| (*key, *child))
                                });
                                if let Some((child_key, child)) = next_child {
                                    frame.after_work_child = Some(child_key);
                                    frame.pending_child = self.plan_pending_removal(frame, key, child);
                                    self.reclamation.push(ReclamationFrame::Entity(frame));
                                    progress.work_units += 1;
                                } else {
                                    if self.relationship_work.get(&key).is_some_and(|work| work.parent == frame.entity) {
                                        self.remove_relationship_work(&key);
                                    }
                                    frame.after_work = Some(key);
                                    frame.active_work = None;
                                    frame.after_work_child = None;
                                    self.reclamation.push(ReclamationFrame::Entity(frame));
                                    progress.work_units += 1;
                                }
                            }
                        } else {
                            let start = (id, LocalAddress::new(Region::minimum(), NonZeroU32::MIN));
                            let lower = frame.after_work.map_or(Excluded(start), Excluded);
                            let next = self.relationship_work.range((lower, Unbounded)).next().map(|(key, work)| (*key, *work));
                            match next {
                                Some((key, work)) if key.0 == id => {
                                    if matches!(frame.cause, RemovalCause::RegionRetired) {
                                        self.remove_relationship_work(&key);
                                        frame.after_work = Some(key);
                                        progress.work_units += 1;
                                    } else if work.parent == frame.entity {
                                        frame.active_work = Some(key);
                                        frame.after_work_child = None;
                                    } else {
                                        frame.after_work = Some(key);
                                        progress.work_units += 1;
                                    }
                                    self.reclamation.push(ReclamationFrame::Entity(frame));
                                }
                                _ => {
                                    frame.phase = RemovalPhase::Finalize;
                                    self.reclamation.push(ReclamationFrame::Entity(frame));
                                }
                            }
                        }
                    }
                    RemovalPhase::Finalize => {
                        self.finalize_removal(frame, &mut changes, &mut recovery_cancellations);
                        progress.removed_entities += 1;
                        progress.work_units += 1;
                    }
                },
                ReclamationFrame::Region(mut frame) => match frame.phase {
                    RegionPhase::Objects => {
                        let next = self.store.region_ids(frame.region).and_then(|objects| {
                            let lower = frame.after_object.map_or(Unbounded, Excluded);
                            objects.range((lower, Unbounded)).next().map(|(id, entity)| (*id, *entity))
                        });
                        match next {
                            Some((id, entity)) => {
                                frame.after_object = Some(id);
                                self.reclamation.push(ReclamationFrame::Region(frame));
                                if let Some(record) = self.store.object_record(entity)
                                    && !self.invalidations.contains_key(&entity)
                                {
                                    let address = record.state.binding.address;
                                    self.mark_invalidation(entity, RemovalCause::RegionRetired);
                                    recovery_cancellations.push(RecoveryCancellation::new(entity, address));
                                    self.reclamation.push(ReclamationFrame::Entity(RemovalFrame::new(
                                        entity,
                                        RemovalCause::RegionRetired,
                                        None,
                                        true,
                                    )));
                                }
                                progress.work_units += 1;
                            }
                            None => {
                                frame.phase = RegionPhase::State;
                                self.reclamation.push(ReclamationFrame::Region(frame));
                            }
                        }
                    }
                    RegionPhase::State => {
                        let Some(state) = self.retiring_regions.get_mut(&frame.region) else {
                            frame.phase = RegionPhase::Tombstones;
                            self.reclamation.push(ReclamationFrame::Region(frame));
                            continue;
                        };
                        let old_terrain = state.terrain.patch_count();
                        let old_environment = state.environment.source_bytes();
                        let old_parcel = state.parcels.retained_bytes();
                        if state.reclaim_one() {
                            self.terrain_patches -= old_terrain - state.terrain.patch_count();
                            self.environment_bytes -= old_environment - state.environment.source_bytes();
                            self.parcel_bytes -= old_parcel - state.parcels.retained_bytes();
                            self.reclamation.push(ReclamationFrame::Region(frame));
                            progress.work_units += 1;
                        } else {
                            frame.phase = RegionPhase::Tombstones;
                            self.reclamation.push(ReclamationFrame::Region(frame));
                        }
                    }
                    RegionPhase::Tombstones => {
                        if let Some(address) = self.tombstones.first_region_address(frame.region) {
                            self.tombstones.remove(&address);
                            self.reclamation.push(ReclamationFrame::Region(frame));
                            progress.work_units += 1;
                        } else if let Some(address) = self.retained.first_region_address(frame.region) {
                            self.retained.remove(&address);
                            self.reclamation.push(ReclamationFrame::Region(frame));
                            progress.work_units += 1;
                        } else {
                            self.floors.remove(&frame.region);
                            self.retiring_regions.remove(&frame.region);
                            progress.work_units += 1;
                        }
                    }
                },
                ReclamationFrame::Transfer(entity) => {
                    let Some(state) = self.transfer_cursors.get(&entity) else {
                        progress.work_units += 1;
                        continue;
                    };
                    let phase = state.phase;
                    let after_child = state.after_child;
                    let after_work = state.after_work;
                    let active_work = state.active_work;
                    let after_pending_child = state.after_pending_child;
                    let parent = self.store.object_record(entity).map(|record| (record.id, record.state.binding));
                    match phase {
                        TransferPhase::Children => {
                            let next_child = self.children.get(&entity).and_then(|children| {
                                let lower = after_child.map_or(Unbounded, Excluded);
                                children.range((lower, Unbounded)).next().map(|(id, child)| (*id, *child))
                            });
                            if let Some((id, child)) = next_child {
                                let transfer = parent
                                    .zip(self.store.object_record(child))
                                    .filter(|(_, child)| child.state.parent == Parent::Resolved(entity))
                                    .map(|((parent_id, binding), child)| (parent_id, binding, child.id, child.state.binding.address));
                                if let Some((parent_id, binding, child_id, child_address)) = transfer
                                    && child_address.region() != binding.address.region()
                                {
                                    let pending_address = self
                                        .transfer_cursors
                                        .get(&entity)
                                        .and_then(|state| state.latest_addresses.get(&child_address.region()).copied())
                                        .unwrap_or_else(|| LocalAddress::new(child_address.region(), binding.address.local_id()));
                                    if self.limits.max_recovery_targets != 0 && recovery_targets.len() >= self.limits.max_recovery_targets {
                                        self.reclamation.push(ReclamationFrame::Transfer(entity));
                                        break;
                                    }
                                    if let Some(state) = self.transfer_cursors.get_mut(&entity) {
                                        state.after_child = Some(id);
                                    }
                                    self.set_parent(child, Parent::Pending { address: pending_address, expected_uuid: Some(parent_id) });
                                    let mut effects = Effects::default();
                                    self.relink_changed(child, child_id, &mut effects);
                                    if let Some(replacement) = self.entity_at(pending_address)
                                        && self.store.object_record(replacement).is_some_and(|record| record.id == parent_id)
                                    {
                                        self.queue_pending_children(pending_address, replacement, parent_id);
                                    } else {
                                        recovery_targets.push(child_address);
                                    }
                                    changes.extend(effects.changes);
                                } else if let Some(state) = self.transfer_cursors.get_mut(&entity) {
                                    state.after_child = Some(id);
                                }
                                self.reclamation.push(ReclamationFrame::Transfer(entity));
                                progress.work_units += 1;
                            } else {
                                if let Some(state) = self.transfer_cursors.get_mut(&entity) {
                                    state.phase = TransferPhase::Relationships;
                                }
                                self.reclamation.push(ReclamationFrame::Transfer(entity));
                            }
                        }
                        TransferPhase::Relationships => {
                            let Some((parent_id, parent_binding)) = parent else {
                                if let Some(state) = self.transfer_cursors.get_mut(&entity) {
                                    state.phase = TransferPhase::Release;
                                }
                                self.reclamation.push(ReclamationFrame::Transfer(entity));
                                progress.work_units += 1;
                                continue;
                            };
                            if let Some(address) = active_work {
                                if address == parent_binding.address {
                                    if let Some(state) = self.transfer_cursors.get_mut(&entity) {
                                        state.after_work = Some(address);
                                        state.active_work = None;
                                        state.after_pending_child = None;
                                    }
                                    self.reclamation.push(ReclamationFrame::Transfer(entity));
                                    progress.work_units += 1;
                                    continue;
                                }
                                let next_child = self.pending_children.get(&address).and_then(|children| {
                                    let lower = after_pending_child.map_or(Unbounded, Excluded);
                                    children.range((lower, Unbounded)).next().map(|(key, child)| (*key, *child))
                                });
                                if let Some((key, child)) = next_child {
                                    let work =
                                        self.relationship_work.get(&(parent_id, address)).copied().filter(|work| work.parent == entity);
                                    let old_binding =
                                        self.transfer_cursors.get(&entity).and_then(|state| state.old_addresses.get(&address).copied());
                                    let pending = self.store.object_record(child).and_then(|record| {
                                        let Parent::Pending { address: child_parent_address, expected_uuid } = record.state.parent else {
                                            return None;
                                        };
                                        (child_parent_address == address).then_some((
                                            record.id,
                                            record.state.binding.address,
                                            expected_uuid,
                                            self.store.ordering(child.0).fences.parent,
                                        ))
                                    });
                                    if let (Some(work), Some(old_binding), Some((child_id, child_address, expected_uuid, parent_stamp))) =
                                        (work, old_binding, pending)
                                    {
                                        let admitted =
                                            Self::pending_link_admitted(old_binding, work.binding, expected_uuid, parent_stamp, parent_id);
                                        if admitted && expected_uuid != Some(parent_id) {
                                            if self.limits.max_recovery_targets != 0
                                                && recovery_targets.len() >= self.limits.max_recovery_targets
                                            {
                                                self.reclamation.push(ReclamationFrame::Transfer(entity));
                                                break;
                                            }
                                            self.set_parent(child, Parent::Pending { address, expected_uuid: Some(parent_id) });
                                            recovery_targets.push(child_address);
                                            let mut effects = Effects::default();
                                            self.relink_changed(child, child_id, &mut effects);
                                            changes.extend(effects.changes);
                                        }
                                    }
                                    if let Some(state) = self.transfer_cursors.get_mut(&entity) {
                                        state.after_pending_child = Some(key);
                                    }
                                    self.reclamation.push(ReclamationFrame::Transfer(entity));
                                    progress.work_units += 1;
                                } else {
                                    self.remove_relationship_work(&(parent_id, address));
                                    if let Some(state) = self.transfer_cursors.get_mut(&entity) {
                                        state.after_work = Some(address);
                                        state.active_work = None;
                                        state.after_pending_child = None;
                                    }
                                    self.reclamation.push(ReclamationFrame::Transfer(entity));
                                    progress.work_units += 1;
                                }
                            } else {
                                let start = LocalAddress::new(Region::minimum(), NonZeroU32::MIN);
                                let lower = after_work.map_or(Excluded((parent_id, start)), |address| Excluded((parent_id, address)));
                                let next = self.relationship_work.range((lower, Unbounded)).next().map(|(key, work)| (*key, *work));
                                match next {
                                    Some(((id, address), work)) if id == parent_id => {
                                        let old_address = self
                                            .transfer_cursors
                                            .get(&entity)
                                            .is_some_and(|state| state.old_addresses.contains_key(&address));
                                        if old_address && address != parent_binding.address && work.parent == entity {
                                            if let Some(state) = self.transfer_cursors.get_mut(&entity) {
                                                state.active_work = Some(address);
                                            }
                                        } else if let Some(state) = self.transfer_cursors.get_mut(&entity) {
                                            state.after_work = Some(address);
                                        }
                                        self.reclamation.push(ReclamationFrame::Transfer(entity));
                                    }
                                    _ => {
                                        if let Some(state) = self.transfer_cursors.get_mut(&entity) {
                                            state.phase = TransferPhase::Release;
                                        }
                                        self.reclamation.push(ReclamationFrame::Transfer(entity));
                                    }
                                }
                                progress.work_units += 1;
                            }
                        }
                        TransferPhase::Release => {
                            let address = self.transfer_cursors.get(&entity).and_then(|state| state.release_addresses.front().copied());
                            if let Some(address) = address {
                                if let Some(state) = self.transfer_cursors.get_mut(&entity) {
                                    state.release_addresses.pop_front();
                                    state.old_addresses.remove(&address);
                                    if state.latest_addresses.get(&address.region()) == Some(&address) {
                                        state.latest_addresses.remove(&address.region());
                                    }
                                }
                                self.transfer_address_count -= 1;
                                self.reclamation.push(ReclamationFrame::Transfer(entity));
                            } else {
                                self.transfer_cursors.remove(&entity);
                            }
                            progress.work_units += 1;
                        }
                    }
                }
            }
        }

        progress.revision = self.publish_changes(changes);
        progress.pending = self.has_pending_removals();
        if !progress.pending
            && self.invalidations.is_empty()
            && self.counted_invalidations.is_empty()
            && self.removal_fences.is_empty()
            && self.invalidated_region_counts.is_empty()
            && self.transfer_address_count == 0
        {
            self.release_empty_removal_capacity();
        }
        progress.recovery = Recovery::from_targets(recovery_targets, self.limits.max_recovery_targets);
        progress.recovery_cancellations = recovery_cancellations;
        progress
    }

    pub(crate) fn schedule_removal(
        &mut self,
        entity: Entity,
        cause: RemovalCause,
        preserve_child: Option<Entity>,
        changes: &mut Vec<WorldChange>,
        cancellations: &mut Vec<RecoveryCancellation>,
    ) -> Result<(), crate::ApplyError> {
        let record = self.store.object_record(entity).ok_or(crate::ApplyError::RemovalCapacityExhausted)?;
        let id = record.id;
        let region = record.state.binding.address.region();
        let address = record.state.binding.address;
        self.reserve_removal_capacity(entity, cause)?;
        changes.try_reserve(usize::from(cause.removal_fence(region).is_some())).map_err(|_| crate::ApplyError::RemovalCapacityExhausted)?;
        cancellations.try_reserve(1).map_err(|_| crate::ApplyError::RemovalCapacityExhausted)?;
        self.mark_invalidation(entity, cause);
        self.removal_tasks.push_back(ReclamationTask::Entity(RemovalFrame::new(entity, cause, preserve_child, true)));
        cancellations.push(RecoveryCancellation::new(entity, address));
        if let Some(fence) = cause.removal_fence(region) {
            changes.push(SubtreeRemovalPending {
                id,
                entity,
                region: fence.region,
                order: fence.order,
                block_ordinal: fence.block_ordinal,
            });
        }
        Ok(())
    }

    pub(crate) fn schedule_region_retirement(&mut self, region: Region) -> Result<(), crate::ApplyError> {
        self.removal_tasks.try_reserve(1).map_err(|_| crate::ApplyError::RemovalCapacityExhausted)?;
        self.retiring_regions.try_reserve(1).map_err(|_| crate::ApplyError::RemovalCapacityExhausted)?;
        self.removal_tasks.push_back(ReclamationTask::Region(RegionFrame::new(region)));
        Ok(())
    }

    pub(crate) fn prepare_parent_transfer(
        &mut self,
        entity: Entity,
        old_binding: Binding,
    ) -> Result<ParentTransferReservation, crate::ApplyError> {
        let old_address = old_binding.address;
        if let Some(progress) = self.transfer_cursors.get_mut(&entity) {
            if !progress.old_addresses.contains_key(&old_address) {
                if self.transfer_address_count >= self.limits.max_entities {
                    return Err(crate::ApplyError::RemovalCapacityExhausted);
                }
                progress.old_addresses.try_reserve(1).map_err(|_| crate::ApplyError::RemovalCapacityExhausted)?;
                progress.release_addresses.try_reserve(1).map_err(|_| crate::ApplyError::RemovalCapacityExhausted)?;
            }
            if !progress.latest_addresses.contains_key(&old_address.region()) {
                if progress.latest_addresses.len() >= self.limits.max_regions {
                    return Err(crate::ApplyError::RemovalCapacityExhausted);
                }
                progress.latest_addresses.try_reserve(1).map_err(|_| crate::ApplyError::RemovalCapacityExhausted)?;
            }
            Ok(ParentTransferReservation { entity, old_address, old_binding, progress: None })
        } else {
            if self.transfer_address_count >= self.limits.max_entities {
                return Err(crate::ApplyError::RemovalCapacityExhausted);
            }
            let mut progress = TransferProgress::default();
            progress.old_addresses.try_reserve(1).map_err(|_| crate::ApplyError::RemovalCapacityExhausted)?;
            progress.release_addresses.try_reserve(1).map_err(|_| crate::ApplyError::RemovalCapacityExhausted)?;
            progress.latest_addresses.try_reserve(1).map_err(|_| crate::ApplyError::RemovalCapacityExhausted)?;
            self.transfer_cursors.try_reserve(1).map_err(|_| crate::ApplyError::RemovalCapacityExhausted)?;
            self.removal_tasks.try_reserve(1).map_err(|_| crate::ApplyError::RemovalCapacityExhausted)?;
            Ok(ParentTransferReservation { entity, old_address, old_binding, progress: Some(progress) })
        }
    }

    pub(crate) fn schedule_parent_transfer(&mut self, reservation: ParentTransferReservation) {
        let ParentTransferReservation { entity, old_address, old_binding, progress } = reservation;
        if let Some(progress) = progress {
            self.transfer_cursors.insert(entity, progress);
        }
        if let Some(progress) = self.transfer_cursors.get_mut(&entity) {
            progress.after_child = None;
            progress.phase = TransferPhase::Children;
            progress.after_work = None;
            progress.active_work = None;
            progress.after_pending_child = None;
            if progress.old_addresses.insert(old_address, old_binding).is_none() {
                self.transfer_address_count += 1;
                progress.release_addresses.push_back(old_address);
            }
            progress.latest_addresses.insert(old_address.region(), old_address);
            if !progress.queued {
                progress.queued = true;
                self.removal_tasks.push_back(ReclamationTask::Transfer(entity));
            }
        } else {
            unreachable!();
        }
    }

    pub(crate) fn retired_region_capacity_available(&self) -> bool {
        self.retiring_regions.len() < self.limits.max_regions
    }

    pub(crate) fn region_object_count_upper_bound(&self, region: Region) -> usize {
        self.store
            .region_ids(region)
            .map_or(0, |objects| objects.len())
            .saturating_sub(self.invalidated_region_counts.get(&region).copied().unwrap_or_default())
    }

    pub(crate) fn visible_entity_count(&self) -> usize {
        if !self.has_pending_removals() {
            return self.store.len();
        }
        let mut cache = VisibilityCache::default();
        self.store.identity_entities().filter(|entity| self.visibility(*entity, &mut cache).visible).count()
    }

    pub(crate) fn visible_entity_upper_bound(&self) -> usize {
        let retired =
            self.retiring_regions.keys().map(|region| self.store.region_ids(*region).map_or(0, |objects| objects.len())).sum::<usize>();
        let invalidated = self
            .invalidated_region_counts
            .iter()
            .filter(|(region, _)| !self.retiring_regions.contains_key(region))
            .map(|(_, count)| *count)
            .sum::<usize>();
        self.store.len().saturating_sub(retired.saturating_add(invalidated))
    }

    pub(crate) fn entity_is_visible(&self, entity: Entity) -> bool {
        if !self.store.is_current_entity(entity) {
            return false;
        }
        if !self.has_pending_removals() {
            return true;
        }
        if let Some(visibility) = self.lookup_visibility.get(entity, self.revision()) {
            return visibility.visible;
        }
        self.visibility_single(entity).visible
    }

    pub(crate) fn logical_parent(&self, parent: Parent) -> Parent {
        let Parent::Resolved(parent_entity) = parent else { return parent };
        if self.entity_is_visible(parent_entity) {
            return parent;
        }
        self.store
            .object_record(parent_entity)
            .map_or(Parent::Unknown, |record| Parent::Pending { address: record.state.binding.address, expected_uuid: Some(record.id) })
    }

    pub(crate) fn logical_parent_for(&self, child: Entity, parent: Parent) -> Parent {
        let Parent::Resolved(parent_entity) = parent else { return parent };
        let Some(child_record) = self.store.object_record(child) else { return Parent::Unknown };
        let Some(parent_record) = self.store.object_record(parent_entity) else { return Parent::Unknown };
        let child_region = child_record.state.binding.address.region();
        if child_region != parent_record.state.binding.address.region() {
            let address = self
                .transfer_cursors
                .get(&parent_entity)
                .filter(|state| !matches!(state.phase, TransferPhase::Release))
                .and_then(|state| state.latest_addresses.get(&child_region).copied())
                .unwrap_or_else(|| LocalAddress::new(child_region, parent_record.state.binding.address.local_id()));
            Parent::Pending { address, expected_uuid: Some(parent_record.id) }
        } else {
            self.logical_parent(parent)
        }
    }

    pub(crate) fn logical_parent_for_removal(&self, child: Entity, parent: Parent, removed_parent: Entity) -> Parent {
        let Parent::Resolved(parent_entity) = parent else { return parent };
        let same_region = self
            .store
            .object_record(child)
            .zip(self.store.object_record(parent_entity))
            .is_some_and(|(child, parent)| child.state.binding.address.region() == parent.state.binding.address.region());
        if parent_entity == removed_parent && same_region { parent } else { self.logical_parent_for(child, parent) }
    }

    pub(crate) fn removal_cause_at(&self, address: LocalAddress) -> Option<RemovalCause> {
        if !self.has_pending_removals() {
            return None;
        }
        let entity = self.store.entity_at(address)?;
        self.visibility_single(entity).cause
    }

    pub(crate) fn physical_entity_at(&self, address: LocalAddress) -> Option<Entity> {
        self.store.entity_at(address)
    }

    pub(crate) fn parent_removal_fence(&self, parent: Parent, child_region: Region) -> Option<Stamp> {
        match parent {
            Parent::Resolved(entity) => {
                let record = self.store.object_record(entity)?;
                (record.state.binding.address.region() == child_region)
                    .then(|| self.invalidations.get(&entity).and_then(|cause| cause.stamp()))
                    .flatten()
            }
            Parent::Pending { address, expected_uuid } if address.region() == child_region => self
                .removal_fences
                .get(&address)
                .filter(|(_, id, _)| expected_uuid.is_none_or(|expected| expected == *id))
                .and_then(|(_, _, cause)| cause.stamp()),
            Parent::Pending { .. } | Parent::Unknown | Parent::Root => None,
        }
    }

    fn reserve_removal_capacity(&mut self, entity: Entity, cause: RemovalCause) -> Result<(), crate::ApplyError> {
        self.removal_tasks.try_reserve(1).map_err(|_| crate::ApplyError::RemovalCapacityExhausted)?;
        self.invalidations.try_reserve(1).map_err(|_| crate::ApplyError::RemovalCapacityExhausted)?;
        if self.store.is_current_entity(entity) {
            self.counted_invalidations.try_reserve(1).map_err(|_| crate::ApplyError::RemovalCapacityExhausted)?;
            let region = self.store.object_record(entity).expect("current entity has a record").state.binding.address.region();
            if !self.invalidated_region_counts.contains_key(&region) {
                self.invalidated_region_counts.try_reserve(1).map_err(|_| crate::ApplyError::RemovalCapacityExhausted)?;
            }
        }
        if cause
            .removal_fence(self.store.object_record(entity).expect("removal target has a record").state.binding.address.region())
            .is_some()
        {
            self.removal_fences.try_reserve(1).map_err(|_| crate::ApplyError::RemovalCapacityExhausted)?;
        }
        Ok(())
    }

    fn release_empty_removal_capacity(&mut self) {
        self.removal_tasks = VecDeque::new();
        self.reclamation = Vec::new();
        self.invalidations = HashMap::new();
        self.counted_invalidations = HashSet::new();
        self.removal_fences = HashMap::new();
        self.invalidated_region_counts = HashMap::new();
        self.transfer_cursors = HashMap::new();
    }

    pub(crate) fn mark_invalidation(&mut self, entity: Entity, cause: RemovalCause) -> bool {
        let current = self.store.is_current_entity(entity);
        if self.invalidations.insert(entity, cause).is_some() {
            return false;
        }
        if let Some(record) = self.store.object_record(entity) {
            let address = record.state.binding.address;
            if self.removal_fences.get(&address).is_none_or(|(_, _, previous)| cause.supersedes(*previous)) {
                self.removal_fences.insert(address, (entity, record.id, cause));
            }
        }
        if current && self.counted_invalidations.insert(entity) {
            self.track_invalidated_entity(entity, true);
        }
        true
    }

    pub(crate) fn untrack_current_invalidation(&mut self, entity: Entity) {
        if self.counted_invalidations.remove(&entity) {
            self.track_invalidated_entity(entity, false);
        }
    }

    fn track_invalidated_entity(&mut self, entity: Entity, adding: bool) {
        let Some(region) = self.store.object_record(entity).map(|record| record.state.binding.address.region()) else { return };
        if adding {
            *self.invalidated_region_counts.entry(region).or_default() += 1;
        } else if let Some(count) = self.invalidated_region_counts.get_mut(&region) {
            *count -= 1;
            if *count == 0 {
                self.invalidated_region_counts.remove(&region);
            }
        }
    }

    fn reclaim_child(
        &mut self,
        frame: RemovalFrame,
        child: Entity,
        changes: &mut Vec<WorldChange>,
        recovery_targets: &mut Vec<LocalAddress>,
        recovery_cancellations: &mut Vec<RecoveryCancellation>,
    ) {
        if Some(child) == frame.preserve_child {
            self.set_parent(child, Parent::Unknown);
            return;
        }
        let Some((parent_id, parent_address, cause, child_id, child_address, child_kind, child_fence)) =
            self.store.object_record(frame.entity).zip(self.store.object_record(child)).map(|(parent, child)| {
                (
                    parent.id,
                    parent.state.binding.address,
                    frame.cause,
                    child.id,
                    child.state.binding.address,
                    child.kind,
                    child.state.binding.fence(),
                )
            })
        else {
            return;
        };
        if self.store.object_record(child).is_none_or(|record| record.state.parent != Parent::Resolved(frame.entity)) {
            return;
        }
        let region_retired = self.retiring_regions.contains_key(&child_address.region());
        if child_kind == Kind::Avatar
            && !region_retired
            && cause.stamp().is_some_and(|stamp| self.detach_avatar_child(frame.entity, child, stamp, changes))
        {
            return;
        }
        if region_retired || cause.cascades(parent_address.region(), child_address.region(), child_kind, child_fence) {
            let child_cause = if region_retired {
                RemovalCause::RegionRetired
            } else {
                match cause {
                    RemovalCause::RegionRetired => RemovalCause::RegionRetired,
                    RemovalCause::Killed(stamp) | RemovalCause::AddressReplaced(stamp) | RemovalCause::ParentCascade(stamp) => {
                        RemovalCause::ParentCascade(stamp)
                    }
                }
            };
            if self.mark_invalidation(child, child_cause) {
                self.reclamation.push(ReclamationFrame::Entity(RemovalFrame::new(child, child_cause, None, false)));
            }
        } else {
            let pending_address = LocalAddress::new(child_address.region(), parent_address.local_id());
            self.set_parent(child, Parent::Pending { address: pending_address, expected_uuid: Some(parent_id) });
            self.advance_parent_fence(child, cause.stamp(), parent_address.region());
            let mut effects = Effects::default();
            self.relink_changed(child, child_id, &mut effects);
            if let Some(replacement) = self.entity_at(pending_address)
                && self.store.object_record(replacement).is_some_and(|record| record.id == parent_id)
            {
                self.queue_pending_children(pending_address, replacement, parent_id);
            } else {
                recovery_targets.push(child_address);
            }
            changes.extend(effects.changes);
            recovery_cancellations.extend(effects.recovery_cancellations);
        }
    }

    fn detach_avatar_child(&mut self, parent: Entity, child: Entity, stamp: Stamp, changes: &mut Vec<WorldChange>) -> bool {
        let Some((id, address, parent_region)) = self
            .store
            .object_record(child)
            .zip(self.store.object_record(parent))
            .map(|(child, parent)| (child.id, child.state.binding.address, parent.state.binding.address.region()))
        else {
            return false;
        };
        if address.region() != parent_region {
            return false;
        }
        let field_stamp = FieldStamp { authority: crate::UpdateAuthority::Tracked, stamp };
        if !crate::newer_field(field_stamp, self.store.ordering(child.0).fences.parent) {
            return false;
        }
        let pose = self
            .resolve_pose_before_parent_removal(child, parent)
            .ok()
            .and_then(|pose| match pose {
                crate::PoseState::Resolved(pose) => Some(pose),
                crate::PoseState::Unresolved(_) | crate::PoseState::Provisional { .. } => None,
            })
            .and_then(|pose| {
                let origin = self.region(address.region()).and_then(crate::RegionInfo::resolved_origin)?;
                let position: Vec3 = (pose.position - origin).as_vec3();
                (position.is_finite() && pose.rotation.is_finite()).then_some((position, pose.rotation))
            });
        self.store.set_detached_pose(child, pose);
        self.set_parent(child, Parent::Root);
        self.advance_parent_fence(child, Some(stamp), parent_region);
        let ordering = self.store.ordering_mut(child.0);
        if crate::newer_field(field_stamp, ordering.fences.pose) {
            ordering.fences.pose = Some(field_stamp);
        }
        if crate::newer_field(field_stamp, ordering.fences.frame) {
            ordering.fences.frame = Some(field_stamp);
        }
        let mut effects = Effects::default();
        self.relink_changed(child, id, &mut effects);
        changes.extend(effects.changes);
        true
    }

    fn plan_pending_removal(&self, frame: RemovalFrame, key: (Uuid, LocalAddress), child: Entity) -> Option<PendingRemovalCandidate> {
        let work = self.relationship_work.get(&key)?;
        if work.parent != frame.entity {
            return None;
        }
        let parent = self.store.object_record(frame.entity)?;
        let child_record = self.store.object_record(child)?;
        let Parent::Pending { address, expected_uuid } = child_record.state.parent else { return None };
        if address != key.1 {
            return None;
        }
        let child_fence = child_record.state.binding.fence();
        let cause_cascades = frame.cause.cascades(
            parent.state.binding.address.region(),
            child_record.state.binding.address.region(),
            child_record.kind,
            child_fence,
        );
        let ordering = self.store.ordering(child.0);
        let link_admitted =
            World::pending_link_admitted(parent.state.binding, work.binding, expected_uuid, ordering.fences.parent, parent.id);
        let cascade = link_admitted && cause_cascades;
        Some(PendingRemovalCandidate {
            entity: child,
            link_admitted,
            cascade,
            cycle_cursor: cascade.then_some(frame.entity),
            cycle_steps: 0,
            cycle_checked: !cascade,
            cycle: false,
        })
    }

    fn advance_pending_removal(
        &mut self,
        frame: &mut RemovalFrame,
        changes: &mut Vec<WorldChange>,
        recovery_targets: &mut Vec<LocalAddress>,
    ) -> PendingRemovalStep {
        let Some(mut candidate) = frame.pending_child else { return PendingRemovalStep::Progress(None) };
        if !candidate.cycle_checked {
            if candidate.cycle_cursor == Some(candidate.entity) || candidate.cycle_steps >= self.store.physical_len() {
                candidate.cycle = true;
                candidate.cycle_checked = true;
                candidate.cycle_cursor = None;
                frame.pending_child = Some(candidate);
                return PendingRemovalStep::Progress(None);
            }
            let Some(cursor) = candidate.cycle_cursor else {
                candidate.cycle_checked = true;
                frame.pending_child = Some(candidate);
                return PendingRemovalStep::Progress(None);
            };
            let next = self.store.object_record(cursor).and_then(|record| match record.state.parent {
                Parent::Resolved(parent) => Some(parent),
                Parent::Root | Parent::Pending { .. } | Parent::Unknown => None,
            });
            candidate.cycle_steps += 1;
            candidate.cycle_cursor = next;
            if next.is_none() {
                candidate.cycle_checked = true;
            }
            frame.pending_child = Some(candidate);
            return PendingRemovalStep::Progress(None);
        }

        let Some(key) = frame.active_work else {
            frame.pending_child = None;
            return PendingRemovalStep::Progress(None);
        };
        let Some((id, address, parent_region, stamp)) = self
            .relationship_work
            .get(&key)
            .filter(|work| work.parent == frame.entity)
            .and_then(|_| self.store.object_record(frame.entity))
            .map(|parent| {
                (
                    self.store.object_record(candidate.entity).map(|record| record.id),
                    key.1,
                    parent.state.binding.address.region(),
                    frame.cause.stamp(),
                )
            })
            .and_then(|(id, address, region, stamp)| Some((id?, address, region, stamp)))
        else {
            frame.pending_child = None;
            return PendingRemovalStep::Progress(None);
        };
        let Some((child_address, child_parent)) =
            self.store.object_record(candidate.entity).map(|record| (record.state.binding.address, record.state.parent))
        else {
            frame.pending_child = None;
            return PendingRemovalStep::Progress(None);
        };
        let Parent::Pending { address: pending_address, expected_uuid } = child_parent else {
            frame.pending_child = None;
            return PendingRemovalStep::Progress(None);
        };
        if pending_address != address {
            frame.pending_child = None;
            return PendingRemovalStep::Progress(None);
        }
        let should_cascade = candidate.cascade && !candidate.cycle;
        if !should_cascade && self.limits.max_recovery_targets != 0 && recovery_targets.len() >= self.limits.max_recovery_targets {
            return PendingRemovalStep::Deferred;
        }
        frame.pending_child = None;
        if should_cascade {
            let cause = stamp.map(RemovalCause::ParentCascade).unwrap_or(RemovalCause::RegionRetired);
            let child_frame =
                self.mark_invalidation(candidate.entity, cause).then(|| RemovalFrame::new(candidate.entity, cause, None, false));
            return PendingRemovalStep::Progress(child_frame);
        }
        if candidate.link_admitted && expected_uuid != Some(id) {
            self.set_parent(candidate.entity, Parent::Pending { address: pending_address, expected_uuid: Some(id) });
            self.advance_parent_fence(candidate.entity, stamp, parent_region);
            let mut effects = Effects::default();
            self.relink_changed(candidate.entity, id, &mut effects);
            changes.extend(effects.changes);
        }
        if candidate.link_admitted
            && let Some(replacement) = self.entity_at(pending_address)
            && self.store.object_record(replacement).is_some_and(|record| record.id == key.0)
        {
            self.queue_pending_children(pending_address, replacement, key.0);
        } else {
            recovery_targets.push(child_address);
        }
        PendingRemovalStep::Progress(None)
    }

    fn child_requires_recovery(&self, frame: RemovalFrame, child: Entity) -> bool {
        if Some(child) == frame.preserve_child {
            return false;
        }
        let Some((parent, child)) = self.store.object_record(frame.entity).zip(self.store.object_record(child)) else { return false };
        if child.state.parent != Parent::Resolved(frame.entity) {
            return false;
        }
        let parent_region = parent.state.binding.address.region();
        let child_region = child.state.binding.address.region();
        !self.retiring_regions.contains_key(&child_region)
            && !frame.cause.cascades(parent_region, child_region, child.kind, child.state.binding.fence())
    }

    fn finalize_removal(
        &mut self,
        frame: RemovalFrame,
        changes: &mut Vec<WorldChange>,
        recovery_cancellations: &mut Vec<RecoveryCancellation>,
    ) {
        let (id, state, media_bytes) = self.store.removal_data(frame.entity.0);
        let address = state.binding.address;
        let address_is_replaced = self.physical_entity_at(address).is_some_and(|current| current != frame.entity);
        self.detach_parent(frame.entity);
        self.object_media_bytes = self.object_media_bytes.saturating_sub(media_bytes);
        self.untrack_current_invalidation(frame.entity);
        self.store.remove(frame.entity);
        self.invalidations.remove(&frame.entity);
        self.counted_invalidations.remove(&frame.entity);
        if let Some(progress) = self.transfer_cursors.get_mut(&frame.entity) {
            progress.phase = TransferPhase::Release;
        }
        if self.removal_fences.get(&address).is_some_and(|(entity, _, _)| *entity == frame.entity) {
            self.removal_fences.remove(&address);
        }
        if !address_is_replaced && let Some(stamp) = frame.cause.tombstone_stamp() {
            self.insert_tombstone(address, stamp);
        }
        changes.push(EntityRemoved { id, entity: frame.entity, address, reason: frame.cause.reason() });
        if !frame.cancelled_at_invalidation {
            recovery_cancellations.push(RecoveryCancellation::new(frame.entity, address));
        }
    }

    pub(crate) fn visibility(&self, entity: Entity, cache: &mut VisibilityCache) -> Visibility {
        if let Some(known) = cache.entities.get(&entity) {
            return *known;
        }
        let mut path = Vec::new();
        let mut visited = HashSet::new();
        let mut current = entity;
        let mut visibility = loop {
            if let Some(known) = cache.entities.get(&current).copied() {
                break known;
            }
            if let Some(cause) = self.invalidations.get(&current).copied() {
                let found = Visibility { visible: false, cause: Some(cause) };
                cache.entities.insert(current, found);
                break found;
            }
            if !visited.insert(current) {
                let found = Visibility { visible: false, cause: None };
                cache.entities.insert(current, found);
                break found;
            }
            let Some(record) = self.store.object_record(current) else {
                let found = Visibility { visible: false, cause: None };
                cache.entities.insert(current, found);
                break found;
            };
            if self.retiring_regions.contains_key(&record.state.binding.address.region())
                || !self.regions.contains_key(&record.state.binding.address.region())
            {
                let found = Visibility { visible: false, cause: Some(RemovalCause::RegionRetired) };
                cache.entities.insert(current, found);
                break found;
            }
            if !self.store.is_current_entity(current) {
                let found = Visibility { visible: false, cause: None };
                cache.entities.insert(current, found);
                break found;
            }
            match record.state.parent {
                Parent::Resolved(parent) => {
                    path.push((current, parent));
                    current = parent;
                }
                Parent::Pending { address, expected_uuid } => {
                    let current_address_entity = self.store.entity_at(address);
                    let current_parent = current_address_entity.and_then(|parent| {
                        let parent_record = self.store.object_record(parent)?;
                        expected_uuid.is_none_or(|expected| expected == parent_record.id).then_some((parent, parent_record))
                    });
                    let direct_cause = current_parent.and_then(|(parent, parent_record)| {
                        self.invalidations
                            .get(&parent)
                            .copied()
                            .or_else(|| {
                                self.retiring_regions
                                    .contains_key(&parent_record.state.binding.address.region())
                                    .then_some(RemovalCause::RegionRetired)
                            })
                            .map(|cause| (cause, parent_record.state.binding.address.region(), parent, parent_record.id))
                    });
                    let parent_field = self.store.ordering(current.0).fences.parent;
                    let parent_fence = parent_field.map(|field| field.stamp).unwrap_or(record.state.binding.fence());
                    let removal = direct_cause
                        .or_else(|| self.pending_removal_cause(address, expected_uuid, current_address_entity, parent_field, cache));
                    if let Some((cause, _, _, _)) = removal.filter(|(cause, parent_region, _, _)| {
                        cause.cascades(*parent_region, record.state.binding.address.region(), record.kind, parent_fence)
                    }) {
                        let found = Visibility { visible: false, cause: Some(cause) };
                        cache.entities.insert(current, found);
                        break found;
                    }
                    let found = Visibility { visible: true, cause: None };
                    cache.entities.insert(current, found);
                    break found;
                }
                Parent::Root | Parent::Unknown => {
                    let found = Visibility { visible: true, cause: None };
                    cache.entities.insert(current, found);
                    break found;
                }
            }
        };
        for (child, parent) in path.into_iter().rev() {
            let Some(record) = self.store.object_record(child) else {
                visibility = Visibility { visible: false, cause: None };
                cache.entities.insert(child, visibility);
                continue;
            };
            let parent_region = self.store.object_record(parent).map(|parent| parent.state.binding.address.region());
            let cascades = visibility.cause.is_some_and(|cause| {
                parent_region.is_some_and(|region| {
                    cause.cascades(region, record.state.binding.address.region(), record.kind, record.state.binding.fence())
                })
            });
            visibility = if cascades {
                Visibility { visible: false, cause: visibility.cause }
            } else if visibility.cause.is_some() {
                Visibility { visible: true, cause: None }
            } else if !visibility.visible {
                Visibility { visible: false, cause: None }
            } else {
                Visibility { visible: true, cause: None }
            };
            cache.entities.insert(child, visibility);
        }
        cache.entities.get(&entity).copied().unwrap_or(visibility)
    }

    fn visibility_single(&self, entity: Entity) -> Visibility {
        if let Some(visibility) = self.lookup_visibility.get(entity, self.revision()) {
            return visibility;
        }
        let Some(record) = self.store.object_record(entity) else {
            return Visibility { visible: false, cause: None };
        };
        let mut cursor = self.begin_visibility(entity, record.id);
        let mut cache = VisibilityCache::default();
        loop {
            match self.advance_visibility_with_lookup_cache(&mut cursor, &mut cache, &self.lookup_visibility) {
                VisibilityStep::Pending => {}
                VisibilityStep::Complete(visibility) => return visibility,
            }
        }
    }

    pub(crate) fn begin_visibility(&self, entity: Entity, id: Uuid) -> VisibilityCursor {
        VisibilityCursor::new(entity, id)
    }

    pub(crate) fn advance_visibility(&self, cursor: &mut VisibilityCursor, cache: &mut VisibilityCache) -> VisibilityStep {
        self.advance_visibility_inner(cursor, cache, None)
    }

    pub(crate) fn advance_visibility_with_lookup_cache(
        &self,
        cursor: &mut VisibilityCursor,
        cache: &mut VisibilityCache,
        lookup_cache: &VisibilityLookupCache,
    ) -> VisibilityStep {
        self.advance_visibility_inner(cursor, cache, Some(lookup_cache))
    }

    fn advance_visibility_inner(
        &self,
        cursor: &mut VisibilityCursor,
        cache: &mut VisibilityCache,
        lookup_cache: Option<&VisibilityLookupCache>,
    ) -> VisibilityStep {
        if let Some(pending) = cursor.pending {
            return self.advance_pending_visibility(cursor, pending, cache);
        }
        if cursor.steps >= self.store.physical_len() {
            return VisibilityStep::Complete(Visibility { visible: false, cause: None });
        }
        cursor.steps += 1;
        let Some(record) = self.store.object_record(cursor.current) else {
            return self.finish_visibility(cursor, Visibility { visible: false, cause: None }, None, cache);
        };
        if let Some(known) = cache.entities.get(&cursor.current).copied() {
            return self.finish_visibility(cursor, known, Some(record.state.binding.address.region()), cache);
        }
        if let Some(known) = lookup_cache.and_then(|lookup_cache| lookup_cache.get(cursor.current, self.revision())) {
            return self.finish_visibility(cursor, known, Some(record.state.binding.address.region()), cache);
        }
        if let Some(cause) = self.invalidations.get(&cursor.current).copied() {
            return self.finish_visibility(
                cursor,
                Visibility { visible: false, cause: Some(cause) },
                Some(record.state.binding.address.region()),
                cache,
            );
        }
        let region = record.state.binding.address.region();
        if self.retiring_regions.contains_key(&region) || !self.regions.contains_key(&region) {
            return self.finish_visibility(
                cursor,
                Visibility { visible: false, cause: Some(RemovalCause::RegionRetired) },
                Some(region),
                cache,
            );
        }
        if !self.store.is_current_entity(cursor.current) {
            return self.finish_visibility(cursor, Visibility { visible: false, cause: None }, Some(region), cache);
        }
        match record.state.parent {
            Parent::Resolved(parent) => {
                cursor.path.include_edge(record, self.store.object_record(parent));
                cursor.current = parent;
                VisibilityStep::Pending
            }
            Parent::Pending { address, expected_uuid } => {
                let current_address_entity = self.store.entity_at(address);
                let current_parent = current_address_entity.and_then(|parent| {
                    let parent_record = self.store.object_record(parent)?;
                    expected_uuid.is_none_or(|expected| expected == parent_record.id).then_some((parent, parent_record))
                });
                let direct_cause = current_parent.and_then(|(parent, parent_record)| {
                    self.invalidations
                        .get(&parent)
                        .copied()
                        .or_else(|| {
                            self.retiring_regions
                                .contains_key(&parent_record.state.binding.address.region())
                                .then_some(RemovalCause::RegionRetired)
                        })
                        .map(|cause| (cause, parent_record.state.binding.address.region(), parent, parent_record.id))
                });
                let parent_field = self.store.ordering(cursor.current.0).fences.parent;
                let parent_fence = parent_field.map(|field| field.stamp).unwrap_or(record.state.binding.fence());
                if let Some((cause, parent_region, _, _)) = direct_cause {
                    let result = if cause.cascades(parent_region, region, record.kind, parent_fence) {
                        Visibility { visible: false, cause: Some(cause) }
                    } else {
                        Visibility { visible: true, cause: None }
                    };
                    return self.finish_visibility(cursor, result, Some(region), cache);
                }
                let pending = PendingVisibility {
                    address,
                    expected_uuid,
                    current_address_entity,
                    parent_field,
                    after_parent: None,
                    fallback_parent_seen: false,
                };
                cursor.pending = Some(pending);
                self.advance_pending_visibility(cursor, pending, cache)
            }
            Parent::Root | Parent::Unknown => {
                self.finish_visibility(cursor, Visibility { visible: true, cause: None }, Some(region), cache)
            }
        }
    }

    fn advance_pending_visibility(
        &self,
        cursor: &mut VisibilityCursor,
        mut pending: PendingVisibility,
        cache: &mut VisibilityCache,
    ) -> VisibilityStep {
        if let Some((id, parent)) = self.relationship_work_parents.get(&pending.address).and_then(|parents| {
            let lower = pending.after_parent.map_or(Unbounded, Excluded);
            parents.range((lower, Unbounded)).next().map(|(id, parent)| (*id, *parent))
        }) {
            pending.after_parent = Some(id);
            let cause = self.invalidations.get(&parent).copied().and_then(|cause| {
                let parent_record = self.store.object_record(parent)?;
                let work = self.relationship_work.get(&(id, pending.address))?;
                if self.removal_fences.get(&pending.address).is_some_and(|(fallback, _, _)| *fallback == parent) {
                    pending.fallback_parent_seen = true;
                }
                let pinned_identity_replaced =
                    pending.expected_uuid == Some(id) && pending.current_address_entity.is_some_and(|current| current != parent);
                (!pinned_identity_replaced
                    && Self::pending_link_admitted(
                        parent_record.state.binding,
                        work.binding,
                        pending.expected_uuid,
                        pending.parent_field,
                        id,
                    ))
                .then_some((cause, parent_record.state.binding.address.region()))
            });
            cursor.pending = Some(pending);
            if let Some((cause, parent_region)) = cause {
                cursor.pending = None;
                let Some(record) = self.store.object_record(cursor.current) else {
                    return self.finish_visibility(cursor, Visibility { visible: false, cause: None }, None, cache);
                };
                let child_region = record.state.binding.address.region();
                let parent_fence = pending.parent_field.map(|field| field.stamp).unwrap_or(record.state.binding.fence());
                let result = if cause.cascades(parent_region, child_region, record.kind, parent_fence) {
                    Visibility { visible: false, cause: Some(cause) }
                } else {
                    Visibility { visible: true, cause: None }
                };
                return self.finish_visibility(cursor, result, Some(child_region), cache);
            }
            return VisibilityStep::Pending;
        }

        let fallback = self
            .removal_fences
            .get(&pending.address)
            .copied()
            .filter(|(_, id, _)| !pending.fallback_parent_seen && pending.expected_uuid.is_none_or(|expected| expected == *id));
        if let Some((parent, id, cause)) = fallback
            && let Some(parent_record) = self.store.object_record(parent)
        {
            let pinned_identity_replaced =
                pending.expected_uuid == Some(id) && pending.current_address_entity.is_some_and(|current| current != parent);
            if !pinned_identity_replaced
                && Self::pending_link_admitted(
                    parent_record.state.binding,
                    parent_record.state.binding,
                    pending.expected_uuid,
                    pending.parent_field,
                    id,
                )
                && let Some(record) = self.store.object_record(cursor.current)
            {
                let child_region = record.state.binding.address.region();
                let parent_region = parent_record.state.binding.address.region();
                let parent_fence = pending.parent_field.map(|field| field.stamp).unwrap_or(record.state.binding.fence());
                let result = if cause.cascades(parent_region, child_region, record.kind, parent_fence) {
                    Visibility { visible: false, cause: Some(cause) }
                } else {
                    Visibility { visible: true, cause: None }
                };
                cursor.pending = None;
                return self.finish_visibility(cursor, result, Some(child_region), cache);
            }
        }

        cursor.pending = None;
        self.finish_visibility(cursor, Visibility { visible: true, cause: None }, None, cache)
    }

    fn finish_visibility(
        &self,
        cursor: &VisibilityCursor,
        base: Visibility,
        parent_region: Option<Region>,
        cache: &mut VisibilityCache,
    ) -> VisibilityStep {
        cache.entities.insert(cursor.current, base);
        let result = cursor.path.inherit(base, parent_region);
        cache.entities.insert(cursor.entity, result);
        VisibilityStep::Complete(result)
    }

    fn pending_removal_cause(
        &self,
        address: LocalAddress,
        expected_uuid: Option<Uuid>,
        current_address_entity: Option<Entity>,
        parent_field: Option<FieldStamp>,
        cache: &mut VisibilityCache,
    ) -> Option<(RemovalCause, Region, Entity, Uuid)> {
        let key = (address, expected_uuid);
        cache.pending_causes.entry(key).or_insert_with(|| {
            let mut causes = Vec::new();
            if let Some(parents) = self.relationship_work_parents.get(&address) {
                for (id, parent) in parents {
                    if expected_uuid.is_some_and(|expected| expected != *id) {
                        continue;
                    }
                    let Some(cause) = self.invalidations.get(parent).copied() else { continue };
                    let Some(parent_record) = self.store.object_record(*parent) else { continue };
                    let Some(work) = self.relationship_work.get(&(*id, address)) else { continue };
                    causes.push(PendingParentCause {
                        cause,
                        parent_region: parent_record.state.binding.address.region(),
                        parent: *parent,
                        id: *id,
                        binding: work.binding,
                    });
                }
            }
            if let Some((parent, id, cause)) = self.removal_fences.get(&address)
                && expected_uuid.is_none_or(|expected| expected == *id)
                && !causes.iter().any(|candidate| candidate.parent == *parent)
                && let Some(parent_record) = self.store.object_record(*parent)
            {
                causes.push(PendingParentCause {
                    cause: *cause,
                    parent_region: parent_record.state.binding.address.region(),
                    parent: *parent,
                    id: *id,
                    binding: parent_record.state.binding,
                });
            }
            causes
        });
        cache.pending_causes.get(&key).and_then(|causes| {
            causes.iter().find_map(|candidate| {
                let pinned_identity_replaced =
                    expected_uuid == Some(candidate.id) && current_address_entity.is_some_and(|current| current != candidate.parent);
                let parent_binding = self.store.object_record(candidate.parent)?.state.binding;
                (!pinned_identity_replaced
                    && Self::pending_link_admitted(parent_binding, candidate.binding, expected_uuid, parent_field, candidate.id))
                .then_some((candidate.cause, candidate.parent_region, candidate.parent, candidate.id))
            })
        })
    }
}
