use std::{
    collections::HashSet,
    num::NonZeroU32,
    ops::Bound::{Excluded, Unbounded},
};

use uuid::Uuid;

use crate::history::{
    WorldChange::{EntityUpdated, SubtreeInvalidated},
    WorldChangeClasses,
};
use crate::mutation::Effects;
use crate::read::{MAX_OBJECT_LOOKUP_WORK_ITEMS, ObjectLookupCursor, ObjectLookupProgress};
use crate::update::{FieldStamp, Stamp};
use crate::{ApplyError, Recovery, RecoveryCancellation, UpdateSource, WorldRevision};
use crate::{AttachmentPoint, Entity, Kind, LocalAddress, ObjectRecord, Parent, ParentUpdate, Region};
use crate::{Binding, World};
use ParentResolution::{Accepted, Cycle};

const IMMEDIATE_RELATIONSHIPS: usize = 64;

pub(crate) type PendingChildKey = (WorldRevision, Uuid);

#[derive(Clone, Copy)]
enum LinkPlan {
    Resolve,
    Cycle,
    Recover,
}

enum ParentAncestry {
    Root(Entity),
    Chain(HashSet<Entity>),
}

impl ParentAncestry {
    fn contains(&self, entity: Entity) -> bool {
        match self {
            Self::Root(root) => *root == entity,
            Self::Chain(ancestors) => ancestors.contains(&entity),
        }
    }
}

struct PlannedChild {
    id: Uuid,
    entity: Entity,
    address: LocalAddress,
    plan: LinkPlan,
}

#[derive(Clone, Copy)]
pub(crate) struct PendingResolution {
    pub(crate) parent: Entity,
    pub(crate) binding: Binding,
    after: Option<PendingChildKey>,
}

#[must_use]
#[derive(Debug, Default)]
pub struct RelationshipResolution {
    pub revision: Option<WorldRevision>,
    pub recovery: Recovery,
    pub recovery_cancellations: Vec<RecoveryCancellation>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ParentResolution {
    Accepted(Parent),
    Cycle,
}

#[derive(Clone, Copy)]
enum ParentResolutionStage {
    BeforeParent,
    Parent,
    Cycle,
    Ready,
}

#[derive(Clone, Copy)]
pub struct ParentResolutionCursor {
    world: crate::identity::WorldId,
    revision: WorldRevision,
    source: UpdateSource,
    local_id: NonZeroU32,
    update: ParentUpdate,
    child: Option<Entity>,
    stage: ParentResolutionStage,
    old_parent: Option<Entity>,
    logical_before_parent: Option<Parent>,
    lookup: Option<ObjectLookupCursor>,
    expected_uuid: Option<Uuid>,
    parent: Option<Entity>,
    cycle_parent: Option<Entity>,
    cycle_steps: usize,
    resolution: Option<ParentResolution>,
}

#[derive(Clone, Copy)]
pub struct ParentResolutionPlan {
    pub(crate) world: crate::identity::WorldId,
    pub(crate) revision: WorldRevision,
    pub(crate) source: UpdateSource,
    pub(crate) local_id: NonZeroU32,
    pub(crate) update: ParentUpdate,
    pub(crate) child: Option<Entity>,
    pub(crate) logical_before_parent: Parent,
    pub(crate) resolution: ParentResolution,
}

impl ParentResolutionPlan {
    pub(crate) fn matches(&self, world: &World, source: UpdateSource, local_id: NonZeroU32, update: ParentUpdate) -> bool {
        self.world == world.id
            && self.revision == world.revision()
            && self.source == source
            && self.local_id == local_id
            && self.update == update
    }
}

#[derive(Clone, Copy)]
pub enum ParentResolutionProgress {
    Ready { plan: ParentResolutionPlan, work_units: usize },
    Pending { cursor: ParentResolutionCursor, work_units: usize },
}

impl World {
    pub(crate) fn insert_relationship_work(&mut self, key: (Uuid, LocalAddress), work: PendingResolution) {
        self.relationship_work.insert(key, work);
        self.relationship_work_parents.entry(key.1).or_default().insert(key.0, work.parent);
    }

    pub(crate) fn queue_pending_children(&mut self, address: LocalAddress, parent: Entity, parent_uuid: Uuid) {
        let key = (parent_uuid, address);
        let Some(binding) = self.store.object_record(parent).map(|record| record.state.binding) else { return };
        let previous = self.relationship_work.get(&key).copied();
        if previous.is_some_and(|work| work.parent == parent) {
            return;
        }
        self.insert_relationship_work(
            key,
            PendingResolution {
                parent,
                binding: previous.map_or(binding, |work| work.binding),
                after: previous.and_then(|work| work.after),
            },
        );
    }

    pub(crate) fn remove_relationship_work(&mut self, key: &(Uuid, LocalAddress)) -> Option<PendingResolution> {
        let work = self.relationship_work.remove(key)?;
        if let Some(parents) = self.relationship_work_parents.get_mut(&key.1) {
            if parents.get(&key.0) == Some(&work.parent) {
                parents.remove(&key.0);
            }
            if parents.is_empty() {
                self.relationship_work_parents.remove(&key.1);
            }
        }
        Some(work)
    }

    pub(crate) fn relationship_capacity_available(&self, address: LocalAddress, parent_uuid: Uuid) -> bool {
        self.relationship_work.contains_key(&(parent_uuid, address))
            || self.pending_children.get(&address).is_none_or(|children| children.is_empty())
            || self.relationship_work.len() < self.limits.max_entities
    }

    pub(crate) fn waiting_children_will_change(
        &self,
        address: LocalAddress,
        entity: Entity,
        parent_uuid: Uuid,
        parent_binding: Binding,
        parent: Parent,
    ) -> bool {
        let previous = self.relationship_work.get(&(parent_uuid, address));
        let work = PendingResolution {
            parent: entity,
            binding: previous.map_or(parent_binding, |work| work.binding),
            after: previous.and_then(|work| work.after),
        };
        let (_, children) = self.plan_children(
            (parent_uuid, address),
            work,
            parent_binding,
            IMMEDIATE_RELATIONSHIPS,
            self.limits.max_recovery_targets,
            Some(parent),
        );
        children.iter().any(|child| matches!(child.plan, LinkPlan::Resolve | LinkPlan::Cycle))
    }

    fn parent_ancestry(&self, start: Entity, parent_override: Option<Parent>) -> ParentAncestry {
        let mut current = start;
        let mut first_parent = parent_override.or_else(|| self.store.object_record(current).map(|record| record.state.parent));
        let mut ancestors = None;
        while let Some(Parent::Resolved(next)) = first_parent.take() {
            let set = ancestors.get_or_insert_with(|| {
                let mut ancestors = HashSet::new();
                ancestors.insert(start);
                ancestors
            });
            if !set.insert(next) {
                break;
            }
            current = next;
            first_parent = self.store.object_record(current).map(|record| record.state.parent);
        }
        match ancestors {
            Some(ancestors) => ParentAncestry::Chain(ancestors),
            None => ParentAncestry::Root(start),
        }
    }

    fn plan_pending_child(
        &self,
        key: (Uuid, LocalAddress),
        work: &PendingResolution,
        current: Binding,
        child: Entity,
        parent_override: Option<Parent>,
        ancestry: &mut Option<ParentAncestry>,
    ) -> Option<PlannedChild> {
        if !self.entity_is_visible(child) {
            return None;
        }
        let mut read = self.store.read(child.0);
        let (record, ordering) = read.get().ok()?;
        let Parent::Pending { address, expected_uuid } = record.state.parent else { return None };
        if address != key.1 {
            return None;
        }
        let plan = if !Self::pending_link_admitted(current, work.binding, expected_uuid, ordering.fences.parent, key.0) {
            LinkPlan::Recover
        } else {
            let ancestry = ancestry.get_or_insert_with(|| self.parent_ancestry(work.parent, parent_override));
            if ancestry.contains(child) {
                if expected_uuid.is_none() { LinkPlan::Cycle } else { LinkPlan::Recover }
            } else {
                LinkPlan::Resolve
            }
        };
        Some(PlannedChild { id: record.id, entity: child, address: record.state.binding.address, plan })
    }

    fn next_relationship_job(&self) -> Option<((Uuid, LocalAddress), PendingResolution)> {
        let next = self.relationship_cursor.and_then(|cursor| self.relationship_work.range((Excluded(cursor), Unbounded)).next());
        next.or_else(|| self.relationship_work.first_key_value()).map(|(key, work)| (*key, *work))
    }

    pub(crate) fn parent_will_be_cascaded(&self, root: Entity, target: Entity, stamp: Stamp) -> bool {
        let Some(root_region) = self.store.object_record(root).map(|record| record.state.binding.address.region()) else { return false };
        let mut current = target;
        let records = self.store.component_view::<ObjectRecord>();

        for _ in 0..=self.store.len() {
            if current == root {
                return true;
            }

            if current.1 != self.id {
                return false;
            }
            let Some(record) = records.get(current.0) else { return false };
            if record.kind == Kind::Avatar || record.state.binding.address.region != root_region || stamp <= record.state.binding.fence() {
                return false;
            }

            let Parent::Resolved(parent) = record.state.parent else { return false };
            current = parent;
        }

        false
    }

    pub(crate) fn invalidate_subtree(&self, root: Entity, effects: &mut Effects) {
        if self.children.get(&root).is_some_and(|children| !children.is_empty()) {
            let id = self.store.object_record(root).expect("a subtree root is live").id;
            effects.changes.push(SubtreeInvalidated { id, entity: root });
        }
    }

    pub(crate) fn resolve_parent(
        &self,
        region: Region,
        local_id: NonZeroU32,
        update: ParentUpdate,
        stamp: FieldStamp,
        child: Option<Entity>,
    ) -> ParentResolution {
        match update {
            ParentUpdate::Root => Accepted(Parent::Root),
            ParentUpdate::Local(parent_local_id) => {
                let address = LocalAddress::new(region, parent_local_id);
                if child.is_some_and(|entity| self.store.object_record(entity).is_some_and(|obj| obj.state.binding.address == address))
                    || local_id == parent_local_id
                {
                    return Cycle;
                }
                let expected_uuid = child.and_then(|entity| self.store.object_record(entity).map(|object| (entity, object))).and_then(
                    |(entity, object)| match self.logical_parent_for(entity, object.state.parent) {
                        Parent::Pending { address: pending_address, expected_uuid } if pending_address == address => expected_uuid,
                        _ => None,
                    },
                );
                if let Some(parent) = self.entity_at(address) {
                    let object = self.store.object_record(parent).expect("address index contains a live parent");
                    if object.state.binding.admits_parent_link(expected_uuid, Some(stamp), object.id) {
                        if child.is_some_and(|child| self.would_cycle(child, parent)) { Cycle } else { Accepted(Parent::Resolved(parent)) }
                    } else {
                        Accepted(Parent::Pending { address, expected_uuid })
                    }
                } else {
                    Accepted(Parent::Pending { address, expected_uuid })
                }
            }
        }
    }

    pub fn prepare_parent_resolution_with_budget(
        &mut self,
        source: UpdateSource,
        local_id: NonZeroU32,
        update: ParentUpdate,
        child: Option<Entity>,
        cursor: Option<ParentResolutionCursor>,
        work_limit: usize,
    ) -> ParentResolutionProgress {
        let revision = self.revision();
        let mut cursor = cursor
            .filter(|cursor| {
                cursor.world == self.id
                    && cursor.revision == revision
                    && cursor.source == source
                    && cursor.local_id == local_id
                    && cursor.update == update
                    && cursor.child == child
            })
            .unwrap_or_else(|| self.begin_parent_resolution(source, local_id, update, child));
        if source.field_stamp().is_none() {
            cursor.logical_before_parent = Some(Parent::Unknown);
            cursor.resolution = Some(Accepted(Parent::Unknown));
            cursor.stage = ParentResolutionStage::Ready;
        }

        let mut work = 0;
        loop {
            match cursor.stage {
                ParentResolutionStage::BeforeParent => {
                    if work >= work_limit.min(MAX_OBJECT_LOOKUP_WORK_ITEMS) {
                        return ParentResolutionProgress::Pending { cursor, work_units: work };
                    }
                    let Some(parent) = cursor.old_parent else {
                        cursor.stage = ParentResolutionStage::Parent;
                        continue;
                    };
                    let Some(record) = self.store.object_record(parent) else {
                        cursor.logical_before_parent = Some(Parent::Unknown);
                        cursor.old_parent = None;
                        cursor.stage = ParentResolutionStage::Parent;
                        continue;
                    };
                    let parent_id = record.id;
                    let address = record.state.binding.address;
                    match self.lookup_entity_with_budget(parent_id, cursor.lookup.take(), work_limit.saturating_sub(work)) {
                        ObjectLookupProgress::Visible { entity, work_units } => {
                            work = work.saturating_add(work_units);
                            cursor.logical_before_parent = Some(if entity == parent && self.store.is_current_entity(parent) {
                                Parent::Resolved(parent)
                            } else {
                                Parent::Pending { address, expected_uuid: Some(parent_id) }
                            });
                            cursor.old_parent = None;
                            cursor.stage = ParentResolutionStage::Parent;
                        }
                        ObjectLookupProgress::Hidden { work_units } => {
                            work = work.saturating_add(work_units);
                            cursor.logical_before_parent = Some(Parent::Pending { address, expected_uuid: Some(parent_id) });
                            cursor.old_parent = None;
                            cursor.stage = ParentResolutionStage::Parent;
                        }
                        ObjectLookupProgress::Pending { cursor: next, work_units } => {
                            work = work.saturating_add(work_units);
                            cursor.lookup = Some(next);
                            return ParentResolutionProgress::Pending { cursor, work_units: work };
                        }
                    }
                }
                ParentResolutionStage::Parent => {
                    let Some(field) = source.field_stamp() else {
                        cursor.resolution = Some(Accepted(Parent::Unknown));
                        cursor.stage = ParentResolutionStage::Ready;
                        continue;
                    };
                    match update {
                        ParentUpdate::Root => {
                            cursor.resolution = Some(Accepted(Parent::Root));
                            cursor.stage = ParentResolutionStage::Ready;
                        }
                        ParentUpdate::Local(parent_local_id) => {
                            let address = LocalAddress::new(source.region(), parent_local_id);
                            cursor.expected_uuid = match cursor.logical_before_parent {
                                Some(Parent::Pending { address: pending_address, expected_uuid }) if pending_address == address => {
                                    expected_uuid
                                }
                                _ => None,
                            };
                            if local_id == parent_local_id
                                || child.is_some_and(|entity| {
                                    self.store.object_record(entity).is_some_and(|object| object.state.binding.address == address)
                                })
                            {
                                cursor.resolution = Some(Cycle);
                                cursor.stage = ParentResolutionStage::Ready;
                                continue;
                            }
                            if work >= work_limit.min(MAX_OBJECT_LOOKUP_WORK_ITEMS) {
                                return ParentResolutionProgress::Pending { cursor, work_units: work };
                            }
                            match self.lookup_entity_at_with_budget(address, cursor.lookup.take(), work_limit.saturating_sub(work)) {
                                ObjectLookupProgress::Visible { entity, work_units } => {
                                    work = work.saturating_add(work_units);
                                    let admits = self.store.object_record(entity).is_some_and(|record| {
                                        record.state.binding.admits_parent_link(cursor.expected_uuid, Some(field), record.id)
                                    });
                                    if admits {
                                        if child.is_some_and(|child| {
                                            self.store
                                                .object_record(child)
                                                .is_some_and(|record| record.state.parent == Parent::Resolved(entity))
                                        }) {
                                            cursor.resolution = Some(Accepted(Parent::Resolved(entity)));
                                            cursor.stage = ParentResolutionStage::Ready;
                                        } else {
                                            cursor.parent = Some(entity);
                                            cursor.cycle_parent = Some(entity);
                                            cursor.stage = if child.is_some() {
                                                ParentResolutionStage::Cycle
                                            } else {
                                                cursor.resolution = Some(Accepted(Parent::Resolved(entity)));
                                                ParentResolutionStage::Ready
                                            };
                                        }
                                    } else {
                                        cursor.resolution =
                                            Some(Accepted(Parent::Pending { address, expected_uuid: cursor.expected_uuid }));
                                        cursor.stage = ParentResolutionStage::Ready;
                                    }
                                }
                                ObjectLookupProgress::Hidden { work_units } => {
                                    work = work.saturating_add(work_units);
                                    cursor.resolution = Some(Accepted(Parent::Pending { address, expected_uuid: cursor.expected_uuid }));
                                    cursor.stage = ParentResolutionStage::Ready;
                                }
                                ObjectLookupProgress::Pending { cursor: next, work_units } => {
                                    work = work.saturating_add(work_units);
                                    cursor.lookup = Some(next);
                                    return ParentResolutionProgress::Pending { cursor, work_units: work };
                                }
                            }
                        }
                    }
                }
                ParentResolutionStage::Cycle => {
                    let Some(parent) = cursor.cycle_parent else {
                        cursor.resolution = Some(Accepted(Parent::Resolved(cursor.parent.expect("cycle parent exists"))));
                        cursor.stage = ParentResolutionStage::Ready;
                        continue;
                    };
                    if Some(parent) == child {
                        cursor.resolution = Some(Cycle);
                        cursor.stage = ParentResolutionStage::Ready;
                        continue;
                    }
                    if parent.1 != self.id || cursor.cycle_steps >= self.store.physical_len() {
                        cursor.resolution = Some(if parent.1 != self.id {
                            Accepted(Parent::Resolved(cursor.parent.expect("cycle parent exists")))
                        } else {
                            Cycle
                        });
                        cursor.stage = ParentResolutionStage::Ready;
                        continue;
                    }
                    if work >= work_limit.min(MAX_OBJECT_LOOKUP_WORK_ITEMS) {
                        return ParentResolutionProgress::Pending { cursor, work_units: work };
                    }
                    work += 1;
                    cursor.cycle_steps += 1;
                    cursor.cycle_parent = self.store.object_record(parent).and_then(|record| match record.state.parent {
                        Parent::Resolved(next) => Some(next),
                        Parent::Pending { .. } | Parent::Unknown | Parent::Root => None,
                    });
                }
                ParentResolutionStage::Ready => {
                    let plan = ParentResolutionPlan {
                        world: self.id,
                        revision,
                        source,
                        local_id,
                        update,
                        child,
                        logical_before_parent: cursor.logical_before_parent.unwrap_or(Parent::Unknown),
                        resolution: cursor.resolution.unwrap_or(Accepted(Parent::Unknown)),
                    };
                    return ParentResolutionProgress::Ready { plan, work_units: work };
                }
            }
        }
    }

    fn begin_parent_resolution(
        &self,
        source: UpdateSource,
        local_id: NonZeroU32,
        update: ParentUpdate,
        child: Option<Entity>,
    ) -> ParentResolutionCursor {
        let (old_parent, logical_before_parent) = child.map_or((None, Some(Parent::Unknown)), |child| {
            let Some(child_record) = self.store.object_record(child) else { return (None, Some(Parent::Unknown)) };
            let before = child_record.state.parent;
            let Parent::Resolved(parent) = before else { return (None, Some(before)) };
            let Some(parent_record) = self.store.object_record(parent) else { return (None, Some(Parent::Unknown)) };
            let parent_id = parent_record.id;
            let address = parent_record.state.binding.address;
            if child_record.state.binding.address.region() != address.region() {
                return (None, Some(self.logical_parent_for(child, before)));
            }
            if !self.store.is_current_entity(parent) {
                return (None, Some(Parent::Pending { address, expected_uuid: Some(parent_id) }));
            }
            if self.has_pending_removals() && self.lookup_visibility.get(parent, self.revision()).is_none() {
                (Some(parent), None)
            } else {
                (None, Some(self.logical_parent_for(child, before)))
            }
        });
        let (stage, resolution) = if source.field_stamp().is_none() {
            (ParentResolutionStage::Ready, Some(Accepted(Parent::Unknown)))
        } else if old_parent.is_some() {
            (ParentResolutionStage::BeforeParent, None)
        } else {
            (ParentResolutionStage::Parent, None)
        };
        ParentResolutionCursor {
            world: self.id,
            revision: self.revision(),
            source,
            local_id,
            update,
            child,
            stage,
            old_parent,
            logical_before_parent,
            lookup: None,
            expected_uuid: None,
            parent: None,
            cycle_parent: None,
            cycle_steps: 0,
            resolution,
        }
    }

    pub(crate) fn relationship_frame_changed(
        &self,
        current: Parent,
        next: Parent,
        current_attachment: Option<AttachmentPoint>,
        next_attachment: Option<AttachmentPoint>,
    ) -> bool {
        self.parent_frame_identity_changed(current, next)
            || (current_attachment != next_attachment && (self.is_avatar_parent(current) || self.is_avatar_parent(next)))
    }

    fn is_avatar_parent(&self, parent: Parent) -> bool {
        matches!(parent, Parent::Resolved(entity) if self.store.object_record(entity).is_some_and(|record| record.kind == Kind::Avatar))
    }

    fn parent_frame_identity_changed(&self, current: Parent, next: Parent) -> bool {
        if current == next {
            return false;
        }
        match (current, next) {
            (Parent::Pending { address, expected_uuid }, Parent::Resolved(entity))
            | (Parent::Resolved(entity), Parent::Pending { address, expected_uuid }) => {
                self.store.object_record(entity).is_none_or(|object| {
                    object.state.binding.address != address
                        || expected_uuid.map_or(object.state.binding.parent_resolution_fence.is_some(), |expected| expected != object.id)
                })
            }
            _ => true,
        }
    }

    pub(crate) fn apply_parent(&mut self, child: Entity, parent: Parent, effects: &mut Effects) {
        self.set_parent(child, parent);
        if let Parent::Pending { address, .. } = parent {
            effects.recovery_targets.push(address);
        }
    }

    pub(crate) fn set_parent(&mut self, child: Entity, parent: Parent) {
        let (child_uuid, previous_parent, previous_since) = {
            let record = self.store.object_record(child).expect("a parented child is live");
            if record.state.parent == parent {
                return;
            }
            (record.id, record.state.parent, record.state.pending_since)
        };
        let pending_since = match (previous_parent, parent) {
            (Parent::Pending { address: previous, .. }, Parent::Pending { address, .. }) if previous == address => previous_since,
            _ => self.journal.revision,
        };
        self.detach_parent(child);
        self.store.set_parent(child, parent, pending_since);
        match parent {
            Parent::Resolved(parent) => {
                self.children.entry(parent).or_default().insert(child_uuid, child);
            }
            Parent::Pending { address, .. } => {
                self.pending_children.entry(address).or_default().insert((pending_since, child_uuid), child);
            }
            Parent::Unknown | Parent::Root => {}
        }
    }

    pub(crate) fn detach_parent(&mut self, child: Entity) {
        let Some((parent, child_uuid, pending_since)) =
            self.store.object_record(child).map(|object| (object.state.parent, object.id, object.state.pending_since))
        else {
            return;
        };
        match parent {
            Parent::Resolved(parent) => {
                if let Some(children) = self.children.get_mut(&parent) {
                    children.remove(&child_uuid);
                    if children.is_empty() {
                        self.children.remove(&parent);
                    }
                }
            }
            Parent::Pending { address, .. } => {
                if let Some(children) = self.pending_children.get_mut(&address) {
                    children.remove(&(pending_since, child_uuid));
                    if children.is_empty() {
                        self.pending_children.remove(&address);
                    }
                }
            }
            Parent::Unknown | Parent::Root => {}
        }
    }

    pub(crate) fn relink_changed(&self, child: Entity, id: Uuid, effects: &mut Effects) {
        effects.changes.push(EntityUpdated { id, entity: child, classes: WorldChangeClasses::RELATIONSHIP | WorldChangeClasses::POSE });
        self.invalidate_subtree(child, effects);
    }

    pub(crate) fn resolve_waiting_children(&mut self, address: LocalAddress, parent: Entity, parent_uuid: Uuid, effects: &mut Effects) {
        let key = (parent_uuid, address);
        let previous = self.remove_relationship_work(&key);
        let binding = self.store.object_record(parent).expect("address index contains a live parent").state.binding;
        let mut resolution = PendingResolution {
            parent,
            binding: previous.map_or(binding, |work| work.binding),
            after: previous.and_then(|work| work.after),
        };
        let (after, children) = self.plan_children(
            key,
            resolution,
            binding,
            IMMEDIATE_RELATIONSHIPS,
            self.limits.max_recovery_targets.saturating_sub(effects.recovery_targets.len()),
            None,
        );
        self.apply_child_plans(address, parent_uuid, parent, &children, effects);
        resolution.after = after;
        if self.next_pending_child(address, resolution.after).is_some() {
            self.insert_relationship_work(key, resolution);
        }
    }

    pub fn has_pending_relationships(&self) -> bool {
        !self.relationship_work.is_empty()
    }

    pub fn resolve_relationships(&mut self, max_children: usize) -> Result<RelationshipResolution, ApplyError> {
        if max_children == 0 || self.relationship_work.is_empty() {
            return Ok(RelationshipResolution::default());
        }
        let Some((key, mut resolution)) = self.next_relationship_job() else {
            return Ok(RelationshipResolution::default());
        };
        let address = key.1;
        let Some(record) = self.store.object_record(resolution.parent) else {
            self.remove_relationship_work(&key);
            self.relationship_cursor = Some(key);
            return Ok(RelationshipResolution::default());
        };
        if !self.entity_is_visible(resolution.parent) {
            if self.relationship_work.get(&key).is_some_and(|work| work.parent == resolution.parent) {
                self.remove_relationship_work(&key);
            }
            self.relationship_cursor = Some(key);
            return Ok(RelationshipResolution::default());
        }
        if record.id != key.0 || record.state.binding.address.region != address.region {
            self.remove_relationship_work(&key);
            self.relationship_cursor = Some(key);
            return Ok(RelationshipResolution::default());
        }
        let parent_uuid = record.id;
        let binding = record.state.binding;
        let (after, children) = self.plan_children(key, resolution, binding, max_children, self.limits.max_recovery_targets, None);
        let changes = children.iter().any(|child| matches!(child.plan, LinkPlan::Resolve | LinkPlan::Cycle));
        if changes {
            self.ensure_revision_available()?;
        }
        let mut effects = Effects::default();
        self.apply_child_plans(address, parent_uuid, resolution.parent, &children, &mut effects);
        resolution.after = after;
        self.relationship_cursor = Some(key);
        if self.next_pending_child(address, resolution.after).is_none() {
            self.remove_relationship_work(&key);
        } else {
            self.insert_relationship_work(key, resolution);
        }
        Ok(RelationshipResolution {
            revision: self.publish_changes(effects.changes),
            recovery: self.recovery(effects.recovery_targets),
            recovery_cancellations: effects.recovery_cancellations,
        })
    }

    fn next_pending_child(&self, address: LocalAddress, after: Option<PendingChildKey>) -> Option<(PendingChildKey, Entity)> {
        let children = self.pending_children.get(&address)?;
        let lower = after.map_or(Unbounded, Excluded);
        children.range((lower, Unbounded)).next().map(|(id, entity)| (*id, *entity))
    }

    fn plan_children(
        &self,
        key: (Uuid, LocalAddress),
        resolution: PendingResolution,
        binding: Binding,
        limit: usize,
        recovery_limit: usize,
        parent_override: Option<Parent>,
    ) -> (Option<PendingChildKey>, Vec<PlannedChild>) {
        let Some(waiting) = self.pending_children.get(&key.1) else { return (resolution.after, Vec::new()) };
        let lower = resolution.after.map_or(Unbounded, Excluded);
        let mut after = resolution.after;
        let mut children = Vec::new();
        let mut ancestry = None;
        let mut remaining = recovery_limit;
        for (&child_key, &child) in waiting.range((lower, Unbounded)).take(limit) {
            if let Some(plan) = self.plan_pending_child(key, &resolution, binding, child, parent_override, &mut ancestry) {
                if self.limits.max_recovery_targets != 0 && matches!(plan.plan, LinkPlan::Cycle | LinkPlan::Recover) {
                    if remaining == 0 {
                        break;
                    }
                    remaining -= 1;
                }
                children.push(plan);
            }
            after = Some(child_key);
        }
        (after, children)
    }

    fn apply_child_plans(
        &mut self,
        address: LocalAddress,
        parent_uuid: Uuid,
        parent: Entity,
        children: &[PlannedChild],
        effects: &mut Effects,
    ) {
        for child in children {
            match child.plan {
                LinkPlan::Resolve => {
                    self.set_parent(child.entity, Parent::Resolved(parent));
                    self.relink_changed(child.entity, child.id, effects);
                    effects.recovery_cancellations.push(RecoveryCancellation::new(child.entity, child.address));
                }
                LinkPlan::Cycle => {
                    self.set_parent(child.entity, Parent::Pending { address, expected_uuid: Some(parent_uuid) });
                    self.relink_changed(child.entity, child.id, effects);
                    effects.recovery_targets.push(child.address);
                }
                LinkPlan::Recover => effects.recovery_targets.push(child.address),
            }
        }
    }

    pub(crate) fn pending_link_admitted(
        current: Binding,
        original: Binding,
        expected: Option<Uuid>,
        stamp: Option<FieldStamp>,
        id: Uuid,
    ) -> bool {
        if (current.address != original.address || current.address_generation != original.address_generation)
            && expected != Some(id)
            && stamp.is_none_or(|stamp| stamp.stamp > original.fence())
        {
            return false;
        }
        original.admits_parent_link(expected, stamp, id)
    }

    pub(crate) fn would_cycle(&self, child: Entity, mut parent: Entity) -> bool {
        if self.store.object_record(child).is_some_and(|object| object.state.parent == Parent::Resolved(parent)) {
            return false;
        }
        for _ in 0..self.store.len() {
            if parent == child {
                return true;
            }
            if parent.1 != self.id {
                return false;
            }
            let Some(record) = self.store.object_record(parent) else { return false };
            match record.state.parent {
                Parent::Resolved(next) => parent = next,
                Parent::Pending { .. } | Parent::Unknown | Parent::Root => return false,
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use crate::{ApplyError, WorldLimits};
    use math::{DVec3, Quat, Vec3};
    use uuid::Uuid;

    use super::*;
    use crate::{
        ApplyOutcome::{Introduced, Removed},
        Kind::{Avatar, Object},
        ObjectObservation,
        OrderEvidence::Ordered,
        Parent::Pending,
        ParentUpdate::{Local, Root},
        Pose, RegionInfo, UpdateSource, WorldRevision,
    };

    fn test_limits() -> WorldLimits {
        WorldLimits {
            max_regions: 1,
            max_entities: 2,
            max_entities_per_region: 2,
            max_tombstones_per_region: 2,
            max_tombstones: 2,
            max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(2),
            max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(2),
            max_journal_entries: 4,
            max_journal_changes: 8,
            max_region_name_bytes: 32,
            max_recovery_targets: 2,
        }
    }

    fn drain_removals(world: &mut World) -> (Vec<LocalAddress>, Vec<LocalAddress>) {
        let mut recovery = Vec::new();
        let mut cancellations = Vec::new();
        while world.has_pending_removals() {
            let progress = world.reclaim_removals(crate::MAX_REMOVAL_WORK_PER_TURN);
            assert!(progress.work_units <= crate::MAX_REMOVAL_WORK_PER_TURN);
            recovery.extend(progress.recovery.requests);
            cancellations.extend(progress.recovery_cancellations.into_iter().map(|cancellation| cancellation.address()));
        }
        (recovery, cancellations)
    }

    #[test]
    fn queued_resolution_reserves_revision() {
        let mut limits = test_limits();
        limits.max_entities = 130;
        limits.max_entities_per_region = 130;
        let mut world = World::new(limits);
        let region = world.register_region().unwrap();
        for child in 2..=130 {
            observation(&mut world, region, u64::from(child), child, Object, Local(NonZeroU32::new(1).unwrap()));
        }
        let parent = observation(&mut world, region, 200, 1, Object, Root);
        let resolved = world.children(parent).count();
        assert!(world.has_pending_relationships());
        world.journal.revision = WorldRevision(u64::MAX);
        assert_eq!(world.resolve_relationships(7).unwrap_err(), ApplyError::RevisionExhausted);
        assert_eq!(world.children(parent).count(), resolved);
        assert!(world.has_pending_relationships());
        assert_eq!(world.check_invariants(), Ok(()));
        world.journal.revision = WorldRevision(u64::MAX - 1);
        let applied = world.resolve_relationships(7).unwrap();
        assert_eq!(applied.revision, Some(WorldRevision(u64::MAX)));
        assert_eq!(world.children(parent).count(), resolved + 7);
        assert_eq!(world.check_invariants(), Ok(()));
    }

    #[test]
    fn recovery_only_drains_at_revision_limit() {
        let mut limits = test_limits();
        limits.max_entities = 130;
        limits.max_entities_per_region = 130;
        limits.max_recovery_targets = 130;
        let mut world = World::new(limits);
        let region = world.register_region().unwrap();
        let _ = world
            .observe_object(ObjectObservation::new(source(region, 1), Uuid::from_u128(1), NonZeroU32::new(1).unwrap(), Object, Root))
            .unwrap();
        let _ = world.remove_object(source(region, 2), NonZeroU32::new(1).unwrap()).unwrap();
        for child in 2..=130 {
            observation(&mut world, region, u64::from(child) + 10, child, Object, Local(NonZeroU32::new(1).unwrap()));
        }
        let parent = observation(&mut world, region, 200, 1, Object, Root);
        assert!(world.has_pending_relationships());
        assert_eq!(world.children(parent).count(), 0);
        world.journal.revision = WorldRevision(u64::MAX);

        let applied = world.resolve_relationships(7).unwrap();

        assert_eq!(applied.revision, None);
        assert_eq!(applied.recovery.requests.len(), 7);
        assert!(applied.recovery_cancellations.is_empty());
        assert_eq!(world.revision(), WorldRevision(u64::MAX));
        assert!(world.has_pending_relationships());
        assert_eq!(world.children(parent).count(), 0);
        assert_eq!(world.check_invariants(), Ok(()));
    }

    #[test]
    fn empty_queued_job_drains_bounded() {
        let mut limits = test_limits();
        limits.max_entities = 130;
        limits.max_entities_per_region = 130;
        let mut world = World::new(limits);
        let region = world.register_region().unwrap();
        for child in 2..=130 {
            observation(&mut world, region, u64::from(child), child, Object, Local(NonZeroU32::new(1).unwrap()));
        }
        let _parent = observation(&mut world, region, 200, 1, Object, Root);
        for (ordinal, local_id) in (66..=130).enumerate() {
            let _ = world.remove_object(source(region, 300 + ordinal as u64), NonZeroU32::new(local_id).unwrap()).unwrap();
        }

        while world.has_pending_removals() {
            let progress = world.reclaim_removals(crate::MAX_REMOVAL_WORK_PER_TURN);
            assert!(progress.work_units <= crate::MAX_REMOVAL_WORK_PER_TURN);
        }
        assert!(world.has_pending_relationships());
        assert_eq!(world.check_invariants(), Ok(()));
        let applied = world.resolve_relationships(1).unwrap();
        assert_eq!(applied.revision, None);
        assert!(!world.has_pending_relationships());
        assert_eq!(world.check_invariants(), Ok(()));
    }

    #[test]
    fn generation_limit_detaches_avatar_without_pose() {
        let mut world = World::new(test_limits());
        let region = world.register_region().unwrap();
        assert!(matches!(
            world.patch_region(region, RegionInfo { origin: Some(DVec3::ZERO), ..RegionInfo::default() }).unwrap().outcome,
            crate::RegionOutcome::Patched { .. }
        ));
        let seat = observation(&mut world, region, 1, 1, Object, Root);
        let avatar = observation(&mut world, region, 2, 2, Avatar, Local(NonZeroU32::new(1).unwrap()));
        {
            let data = world.state_mut(avatar);
            data.frame_generation = u64::MAX;
            data.position_frame_generation = Some(u64::MAX);
            data.rotation_frame_generation = Some(u64::MAX);
        }
        let removed = world.remove_object(source(region, 3), NonZeroU32::new(1).unwrap()).unwrap();

        assert_eq!(removed.outcome, Removed(Uuid::from_u128(1)));
        assert!(world.object(seat).is_none());
        assert_eq!(world.object_count(), 1);
        while world.has_pending_removals() {
            let _ = world.reclaim_removals(crate::MAX_REMOVAL_WORK_PER_TURN);
        }
        assert_eq!(world.object(avatar).unwrap().parent(), Parent::Root);
        assert_eq!(
            world.object(avatar).unwrap().world_pose().unwrap(),
            crate::PoseState::Unresolved(crate::UnresolvedReason::PoseNotValidForCurrentFrame)
        );
        assert!(removed.recovery.requests.is_empty());
        assert_eq!(world.check_invariants(), Ok(()));
    }

    #[test]
    fn newer_child_keeps_avatar_pending() {
        let mut world = World::new(test_limits());
        let region = world.register_region().unwrap();
        let seat = observation(&mut world, region, 1, 1, Object, Root);
        let avatar = observation(&mut world, region, 3, 2, Avatar, Local(NonZeroU32::new(1).unwrap()));
        let removed = world.remove_object(source(region, 2), NonZeroU32::new(1).unwrap()).unwrap();

        assert_eq!(removed.outcome, Removed(Uuid::from_u128(1)));
        assert!(world.object(seat).is_none());
        assert_eq!(
            world.object(avatar).unwrap().parent(),
            Pending { address: LocalAddress::new(region, NonZeroU32::new(1).unwrap()), expected_uuid: Some(Uuid::from_u128(1)) }
        );
        assert!(removed.recovery.requests.is_empty());
        assert_eq!(drain_removals(&mut world).0, [LocalAddress::new(region, NonZeroU32::new(2).unwrap())]);
    }

    #[test]
    fn revision_exhaustion_keeps_relationships() {
        let mut world = World::new(test_limits());
        let region = world.register_region().unwrap();
        let parent = observation(&mut world, region, 1, 1, Object, Root);
        let avatar = observation(&mut world, region, 2, 2, Avatar, Local(NonZeroU32::new(1).unwrap()));
        let parent_address = LocalAddress::new(region, NonZeroU32::new(1).unwrap());

        world.journal.revision = WorldRevision(u64::MAX);
        assert_eq!(world.remove_object(source(region, 3), parent_address.local_id()), Err(ApplyError::RevisionExhausted));

        assert_eq!(world.revision(), WorldRevision(u64::MAX));
        assert_eq!(world.object_count(), 2);
        assert_eq!(world.entity(Uuid::from_u128(1)), Some(parent));
        assert_eq!(world.entity_at(parent_address), Some(parent));
        assert_eq!(world.entity(Uuid::from_u128(2)), Some(avatar));
        assert_eq!(world.object(avatar).unwrap().parent(), Parent::Resolved(parent));
        assert_eq!(world.children(parent).map(|object| object.entity()).collect::<Vec<_>>(), [avatar]);
    }

    #[test]
    fn parent_retirement_removes_avatar() {
        let mut world = World::new(test_limits());
        let region = world.register_region().unwrap();
        let parent = observation(&mut world, region, 1, 1, Object, Root);
        let child = observation(&mut world, region, 2, 2, Avatar, Local(NonZeroU32::new(1).unwrap()));
        let retired = world.retire_region(region).unwrap();
        assert!(matches!(retired.outcome, crate::RegionOutcome::Retired { entities_removed: 2, .. }));
        assert!(world.object(parent).is_none());
        assert!(world.object(child).is_none());
        assert!(retired.recovery_cancellations.is_empty());
        assert_eq!(drain_removals(&mut world).1.len(), 2);
    }

    fn observation(world: &mut World, region: Region, order: u64, local_id: u32, kind: Kind, parent: ParentUpdate) -> Entity {
        let object = ObjectObservation {
            pose: Some(Pose { position: Some(Vec3::X), rotation: Some(Quat::IDENTITY) }),
            ..ObjectObservation::new(
                source(region, order),
                Uuid::from_u128(u128::from(local_id)),
                NonZeroU32::new(local_id).unwrap(),
                kind,
                parent,
            )
        };
        let Introduced(entity) = world.observe_object(object).unwrap().outcome else { panic!("expected object introduction") };
        entity
    }

    fn source(region: Region, order: u64) -> UpdateSource {
        UpdateSource::new(region, order as u32, Ordered(order), 0)
    }
}
