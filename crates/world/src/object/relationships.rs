use std::{
    collections::BTreeMap,
    num::NonZeroU32,
    ops::Bound::{Excluded, Unbounded},
};

use uuid::Uuid;

use crate::history::{
    WorldChange::{EntityUpdated, SubtreeInvalidated},
    WorldChangeClasses,
};
use crate::mutation::Effects;
use crate::update::{FieldStamp, Stamp};
use crate::{ApplyError, Recovery, WorldRevision};
use crate::{AttachmentPoint, Entity, Kind, LocalAddress, ObjectRecord, Parent, ParentUpdate, Region};
use crate::{Binding, World};
use ParentResolution::{Accepted, Cycle};

const IMMEDIATE_RELATIONSHIPS: usize = 64;

#[derive(Clone, Copy)]
pub(crate) struct PendingResolution {
    pub(crate) parent: Entity,
    pub(crate) binding: Binding,
    after: Option<Uuid>,
}

#[derive(Debug, Default)]
pub struct RelationshipResolution {
    pub revision: Option<WorldRevision>,
    pub recovery: Recovery,
    pub recovery_cancellations: Vec<LocalAddress>,
}

pub(crate) enum ParentResolution {
    Accepted(Parent),
    Cycle,
}

impl World {
    pub(crate) fn ensure_relationship_capacity(&self, address: LocalAddress, parent_uuid: Uuid) -> Result<(), ApplyError> {
        if self.relationship_work.len() >= self.limits.max_entities
            && !self.relationship_work.contains_key(&(parent_uuid, address))
            && self.pending_children.get(&address).is_some_and(|children| children.len() > IMMEDIATE_RELATIONSHIPS)
        {
            return Err(ApplyError::RelationshipBudgetExhausted);
        }
        Ok(())
    }

    pub(crate) fn waiting_children_will_change(
        &self,
        address: LocalAddress,
        entity: Entity,
        parent_uuid: Uuid,
        parent_binding: Binding,
    ) -> bool {
        let Some(waiting) = self.pending_children.get(&address) else { return false };
        let original_binding = self.relationship_work.get(&(parent_uuid, address)).map_or(parent_binding, |work| work.binding);
        let records = self.store.ordered_records();
        waiting.values().take(IMMEDIATE_RELATIONSHIPS).any(|child| {
            let Some((record, ordering)) = records.get(child.0) else { return false };
            match record.state.parent {
                Parent::Pending { expected_uuid, .. } => {
                    Self::pending_link_admitted(parent_binding, original_binding, expected_uuid, ordering.fences.parent, parent_uuid)
                        && (!self.would_cycle(*child, entity) || expected_uuid.is_none())
                }
                _ => false,
            }
        })
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
                let expected_uuid =
                    child.and_then(|entity| self.store.object_record(entity)).and_then(|object| match object.state.parent {
                        Parent::Pending { address: pending_address, expected_uuid } if pending_address == address => expected_uuid,
                        _ => None,
                    });
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
        let child_uuid = {
            let record = self.store.object_record(child).expect("a parented child is live");
            if record.state.parent == parent {
                return;
            }
            record.id
        };
        self.detach_parent(child);
        self.store.set_parent(child, parent);
        match parent {
            Parent::Resolved(parent) => {
                self.children.entry(parent).or_default().insert(child_uuid, child);
            }
            Parent::Pending { address, .. } => {
                self.pending_children.entry(address).or_default().insert(child_uuid, child);
            }
            Parent::Unknown | Parent::Root => {}
        }
    }

    pub(crate) fn detach_parent(&mut self, child: Entity) {
        let Some((parent, child_uuid)) = self.store.object_record(child).map(|object| (object.state.parent, object.id)) else {
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
                    children.remove(&child_uuid);
                    if children.is_empty() {
                        self.pending_children.remove(&address);
                        self.relationship_work.retain(|(_, pending_address), _| *pending_address != address);
                    }
                }
            }
            Parent::Unknown | Parent::Root => {}
        }
    }

    pub(crate) fn pend_source_children(&mut self, parent: Entity, old_address: LocalAddress, parent_uuid: Uuid, effects: &mut Effects) {
        let children = self.take_dependents(parent);
        for child in children.into_values() {
            let Some((child_address, child_uuid)) = self.store.object_record(child).map(|object| (object.state.binding.address, object.id))
            else {
                continue;
            };
            if child_address.region != old_address.region {
                continue;
            }

            self.set_parent(child, Parent::Pending { address: old_address, expected_uuid: Some(parent_uuid) });
            effects.recovery_targets.push(child_address);
            self.relink_changed(child, child_uuid, effects);
        }
    }

    pub(crate) fn take_dependents(&mut self, parent: Entity) -> BTreeMap<Uuid, Entity> {
        let mut children = self.children.remove(&parent).unwrap_or_default();
        let Some(record) = self.store.object_record(parent) else { return children };
        let parent_uuid = record.id;
        let current_binding = record.state.binding;
        let start = (parent_uuid, LocalAddress::new(Region::new(0), NonZeroU32::MIN));
        let keys: Vec<_> =
            self.relationship_work.range(start..).take_while(|((id, _), _)| *id == parent_uuid).map(|(key, _)| *key).collect();
        for key in keys {
            let address = key.1;
            let work = self.relationship_work.remove(&key).expect("a dependent resolution remains queued");
            if let Some(waiting) = self.pending_children.get(&address) {
                for (&id, &child) in waiting {
                    let mut read = self.store.read(child.0);
                    let Ok((record, ordering)) = read.get() else { continue };
                    let Parent::Pending { expected_uuid, .. } = record.state.parent else { continue };
                    if Self::pending_link_admitted(current_binding, work.binding, expected_uuid, ordering.fences.parent, parent_uuid)
                        && !self.would_cycle(child, parent)
                    {
                        children.insert(id, child);
                    }
                }
            }
        }
        children
    }

    pub(crate) fn relink_changed(&self, child: Entity, id: Uuid, effects: &mut Effects) {
        effects.changes.push(EntityUpdated { id, entity: child, classes: WorldChangeClasses::RELATIONSHIP | WorldChangeClasses::POSE });
        self.invalidate_subtree(child, effects);
    }

    pub(crate) fn resolve_waiting_children(&mut self, address: LocalAddress, parent: Entity, parent_uuid: Uuid, effects: &mut Effects) {
        let key = (parent_uuid, address);
        let previous = self.relationship_work.remove(&key);
        let binding = self.store.object_record(parent).expect("address index contains a live parent").state.binding;
        let mut resolution = PendingResolution { parent, binding: previous.map_or(binding, |work| work.binding), after: None };
        self.resolve_children(address, parent_uuid, &mut resolution, IMMEDIATE_RELATIONSHIPS, effects);
        if self.next_pending_child(address, resolution.after).is_some() {
            self.relationship_work.insert(key, resolution);
        }
    }

    pub fn has_pending_relationships(&self) -> bool {
        !self.relationship_work.is_empty()
    }

    pub fn resolve_relationships(&mut self, max_children: usize) -> Result<RelationshipResolution, ApplyError> {
        if max_children == 0 || self.relationship_work.is_empty() {
            return Ok(RelationshipResolution::default());
        }
        self.ensure_revision_available()?;
        let mut effects = Effects::default();
        let mut remaining = max_children;
        while remaining > 0 {
            let Some((key, mut resolution)) = self.relationship_work.first_key_value().map(|(key, work)| (*key, *work)) else { break };
            let address = key.1;
            let Some(record) = self.store.object_record(resolution.parent) else {
                self.relationship_work.remove(&key);
                remaining -= 1;
                continue;
            };
            if record.state.binding.address.region != address.region {
                self.relationship_work.remove(&key);
                remaining -= 1;
                continue;
            }
            let parent_uuid = record.id;
            let processed = self.resolve_children(address, parent_uuid, &mut resolution, remaining, &mut effects);
            remaining -= processed.max(1);
            if self.next_pending_child(address, resolution.after).is_some() {
                self.relationship_work.insert(key, resolution);
            } else {
                self.relationship_work.remove(&key);
            }
        }
        Ok(RelationshipResolution {
            revision: self.publish_changes(effects.changes),
            recovery: self.recovery(effects.recovery_targets),
            recovery_cancellations: effects.recovery_cancellations,
        })
    }

    fn next_pending_child(&self, address: LocalAddress, after: Option<Uuid>) -> Option<(Uuid, Entity)> {
        let children = self.pending_children.get(&address)?;
        let lower = after.map_or(Unbounded, Excluded);
        children.range((lower, Unbounded)).next().map(|(id, entity)| (*id, *entity))
    }

    fn resolve_children(
        &mut self,
        address: LocalAddress,
        parent_uuid: Uuid,
        resolution: &mut PendingResolution,
        limit: usize,
        effects: &mut Effects,
    ) -> usize {
        let parent = resolution.parent;
        let binding = self.store.object_record(parent).expect("a pending resolution parent is live").state.binding;
        let mut processed = 0;
        while processed < limit {
            let Some((id, child)) = self.next_pending_child(address, resolution.after) else { break };
            resolution.after = Some(id);
            processed += 1;
            let (parent_state, child_address, uuid, parent_stamp) = {
                let mut read = self.store.read(child.0);
                let Ok((record, ordering)) = read.get() else { continue };
                (record.state.parent, record.state.binding.address, record.id, ordering.fences.parent)
            };
            let Parent::Pending { expected_uuid, .. } = parent_state else { continue };
            if !Self::pending_link_admitted(binding, resolution.binding, expected_uuid, parent_stamp, parent_uuid) {
                effects.recovery_targets.push(child_address);
                continue;
            }
            if self.would_cycle(child, parent) {
                if expected_uuid.is_none() {
                    self.set_parent(child, Parent::Pending { address, expected_uuid: Some(parent_uuid) });
                    self.relink_changed(child, uuid, effects);
                }
                effects.recovery_targets.push(child_address);
                continue;
            }
            self.set_parent(child, Parent::Resolved(parent));
            self.relink_changed(child, uuid, effects);
            effects.recovery_cancellations.push(child_address);
        }
        processed
    }

    fn pending_link_admitted(current: Binding, original: Binding, expected: Option<Uuid>, stamp: Option<FieldStamp>, id: Uuid) -> bool {
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
        if parent == child {
            return true;
        }
        let Some(Parent::Resolved(next)) = self.store.object_record(parent).map(|object| object.state.parent) else { return false };
        parent = next;
        let records = self.store.component_view::<ObjectRecord>();
        for _ in 1..self.store.len() {
            if parent == child {
                return true;
            }
            if parent.1 != self.id {
                return false;
            }
            let Some(record) = records.get(parent.0) else { return false };
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
            max_region_registrations: 1,
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

    #[test]
    fn queued_resolution_reserves_revision() {
        let region = Region::new(1);
        let mut limits = test_limits();
        limits.max_entities = 130;
        limits.max_entities_per_region = 130;
        let mut world = World::new(limits);
        let _ = world.adopt_region(region).unwrap();
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
    fn generation_limit_keeps_avatar_pending() {
        let region = Region::new(1);
        let mut world = World::new(test_limits());
        assert!(matches!(world.adopt_region(region).unwrap().outcome, crate::RegionOutcome::Adopted { .. }));
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
        assert_eq!(
            world.object(avatar).unwrap().parent(),
            Pending { address: LocalAddress::new(region, NonZeroU32::new(1).unwrap()), expected_uuid: Some(Uuid::from_u128(1)) }
        );
        assert_eq!(removed.recovery.requests, [LocalAddress::new(region, NonZeroU32::new(2).unwrap())]);
    }

    #[test]
    fn newer_child_keeps_avatar_pending() {
        let region = Region::new(1);
        let mut world = World::new(test_limits());
        assert!(matches!(world.adopt_region(region).unwrap().outcome, crate::RegionOutcome::Adopted { .. }));
        let seat = observation(&mut world, region, 1, 1, Object, Root);
        let avatar = observation(&mut world, region, 3, 2, Avatar, Local(NonZeroU32::new(1).unwrap()));
        let removed = world.remove_object(source(region, 2), NonZeroU32::new(1).unwrap()).unwrap();

        assert_eq!(removed.outcome, Removed(Uuid::from_u128(1)));
        assert!(world.object(seat).is_none());
        assert_eq!(
            world.object(avatar).unwrap().parent(),
            Pending { address: LocalAddress::new(region, NonZeroU32::new(1).unwrap()), expected_uuid: Some(Uuid::from_u128(1)) }
        );
        assert_eq!(removed.recovery.requests, [LocalAddress::new(region, NonZeroU32::new(2).unwrap())]);
    }

    #[test]
    fn revision_exhaustion_keeps_relationships() {
        let region = Region::new(1);
        let mut world = World::new(test_limits());
        assert!(matches!(world.adopt_region(region).unwrap().outcome, crate::RegionOutcome::Adopted { .. }));
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
        let region = Region::new(1);
        let mut world = World::new(test_limits());
        assert!(matches!(world.adopt_region(region).unwrap().outcome, crate::RegionOutcome::Adopted { .. }));
        let parent = observation(&mut world, region, 1, 1, Object, Root);
        let child = observation(&mut world, region, 2, 2, Avatar, Local(NonZeroU32::new(1).unwrap()));
        let retired = world.retire_region(region).unwrap();
        assert!(matches!(retired.outcome, crate::RegionOutcome::Retired { entities_removed: 2, .. }));
        assert!(world.object(parent).is_none());
        assert!(world.object(child).is_none());
        assert_eq!(retired.recovery_cancellations.len(), 2);
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
