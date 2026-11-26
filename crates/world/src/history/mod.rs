pub(crate) mod address;
pub(crate) mod retained;

use std::{
    collections::VecDeque,
    ops::{BitOr, BitOrAssign},
};

use uuid::Uuid;

use crate::{Entity, LocalAddress, Region, TerrainPatchId, WorldRevision};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorldEntityRemovalReason {
    Killed,
    AddressReplaced,
    ParentCascade,
    RegionRetired,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WorldChangeClasses(u8);

impl WorldChangeClasses {
    pub const POSE: Self = Self(1 << 0);
    pub const MOTION: Self = Self(1 << 1);
    pub const SCALE: Self = Self(1 << 2);
    pub const RELATIONSHIP: Self = Self(1 << 3);
    pub const SHAPE: Self = Self(1 << 4);
    pub const APPEARANCE: Self = Self(1 << 5);
    pub const MEDIA: Self = Self(1 << 6);
    pub const PBR_MATERIAL_OVERRIDES: Self = Self(1 << 7);

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, classes: Self) -> bool {
        self.0 & classes.0 == classes.0
    }

    pub(crate) const fn union(self, classes: Self) -> Self {
        Self(self.0 | classes.0)
    }
}

impl BitOr for WorldChangeClasses {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for WorldChangeClasses {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorldChange {
    EntityAdded { id: Uuid, entity: Entity, address: LocalAddress },
    EntityUpdated { id: Uuid, entity: Entity, classes: WorldChangeClasses },
    SubtreeInvalidated { id: Uuid, entity: Entity },
    SubtreeRemovalPending { id: Uuid, entity: Entity, region: Region, order: u64, block_ordinal: u32 },
    EntityRemoved { id: Uuid, entity: Entity, address: LocalAddress, reason: WorldEntityRemovalReason },
    EntityRebound { id: Uuid, entity: Entity, from: LocalAddress, to: LocalAddress },
    EntityTransferred { id: Uuid, entity: Entity, from: LocalAddress, to: LocalAddress },
    RegionRegistered(Region),
    RegionUpdated(Region),
    EnvironmentUpdated(Region),
    ParcelUpdated(Region),
    TerrainPatchUpdated { region: Region, patch: TerrainPatchId },
    WaterHeightUpdated(Region),
    RegionRetired { region: Region, entities_removed: usize },
}

#[derive(Debug)]
pub(crate) struct JournalEntry {
    pub(crate) revision: WorldRevision,
    pub(crate) changes: Box<[WorldChange]>,
}

#[derive(Debug)]
pub(crate) struct Journal {
    pub(crate) revision: WorldRevision,
    pub(crate) entries: VecDeque<JournalEntry>,
    retained_changes: usize,
    pub(crate) lost_through: Option<WorldRevision>,
}

impl Journal {
    pub(crate) fn new() -> Self {
        Self { revision: WorldRevision(0), entries: VecDeque::new(), retained_changes: 0, lost_through: None }
    }

    pub(crate) fn append(&mut self, changes: Vec<WorldChange>, max_entries: usize, max_changes: usize) -> Option<WorldRevision> {
        if changes.is_empty() {
            return None;
        }

        let value = self.revision.0.checked_add(1).expect("observable mutations preflight journal revision");
        let next = WorldRevision(value);
        self.revision = next;
        let change_count = changes.len();
        if max_entries == 0 || change_count > max_changes {
            self.entries.clear();
            self.retained_changes = 0;
            self.note_loss_through(next);
        } else {
            while self.entries.len() >= max_entries || self.retained_changes.saturating_add(change_count) > max_changes {
                let old = self.entries.pop_front().expect("an over-budget journal retains an entry");
                self.retained_changes -= old.changes.len();
                self.note_loss_through(old.revision);
            }
            self.retained_changes += change_count;
            self.entries.push_back(JournalEntry { revision: next, changes: changes.into_boxed_slice() });
        }
        Some(next)
    }

    pub(crate) fn entry_index(&self, revision: WorldRevision) -> usize {
        self.entries.partition_point(|entry| entry.revision < revision)
    }

    fn note_loss_through(&mut self, revision: WorldRevision) {
        self.lost_through = Some(self.lost_through.map_or(revision, |lost| lost.max(revision)));
    }
}

#[cfg(test)]
pub(crate) fn journal_invariants_hold(journal: &Journal) -> bool {
    let mut retained_changes = 0_usize;
    let mut previous_revision = None;
    for entry in &journal.entries {
        if entry.changes.is_empty() || entry.revision > journal.revision {
            return false;
        }
        if previous_revision.is_some_and(|previous| entry.revision <= previous) {
            return false;
        }
        if journal.lost_through.is_some_and(|lost| entry.revision <= lost) {
            return false;
        }
        retained_changes = retained_changes.saturating_add(entry.changes.len());
        previous_revision = Some(entry.revision);
    }
    retained_changes == journal.retained_changes && journal.lost_through.is_none_or(|lost| lost <= journal.revision)
}

#[cfg(test)]
mod tests {
    use std::{assert_matches, num::NonZeroU32};

    use math::Vec3;
    use utils::AssetId;
    use uuid::Uuid;

    use super::{
        WorldChange::{
            EntityAdded, EntityRebound, EntityRemoved, EntityTransferred, EntityUpdated, RegionRegistered, RegionRetired, RegionUpdated,
            SubtreeInvalidated, SubtreeRemovalPending,
        },
        *,
    };
    use crate::{
        Appearance, Applied, ApplyError, ApplyOutcome, FaceAppearance, FieldUpdate, Kind, LocalAddress, ObjectObservation, ObjectPatch,
        OrderEvidence::Ordered,
        ParentUpdate::{Local, Root},
        PbrMaterialAssignments, Pose, RegionInfo, RegionOutcome, SculptFlags, Shape, SurfaceAppearance, UpdateSource, World, WorldLimits,
    };

    fn limits(max_journal_entries: usize) -> WorldLimits {
        WorldLimits {
            max_regions: 4,
            max_entities: 8,
            max_entities_per_region: 8,
            max_tombstones_per_region: 8,
            max_tombstones: 8,
            max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(8),
            max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(8),
            max_journal_entries,
            max_journal_changes: 8,
            max_region_name_bytes: 8,
            max_recovery_targets: 1,
        }
    }

    fn observation(region: Region, order: u64, id: u128, local_id: u32) -> ObjectObservation {
        ObjectObservation::new(
            UpdateSource::new(region, order as u32, Ordered(order), 0),
            Uuid::from_u128(id),
            NonZeroU32::new(local_id).unwrap(),
            Kind::Object,
            Root,
        )
    }

    fn last_changes(world: &World) -> &[WorldChange] {
        &world.journal.entries.back().expect("observable changes are journaled").changes
    }

    fn assert_revision(applied: Applied) {
        assert!(applied.revision.is_some());
    }

    fn assert_region_revision(outcome: RegionOutcome) {
        match outcome {
            RegionOutcome::Patched { revision } | RegionOutcome::Retired { revision, .. } => {
                assert!(revision.get() > 0);
            }
            RegionOutcome::Unchanged => panic!("changed region operation has a revision"),
        }
    }

    fn drain_removals(world: &mut World) {
        while world.has_pending_removals() {
            let progress = world.reclaim_removals(crate::MAX_REMOVAL_WORK_PER_TURN);
            assert!(progress.work_units <= crate::MAX_REMOVAL_WORK_PER_TURN);
        }
    }

    #[test]
    fn revision_exhaustion_rejects_update_atomically() {
        let mut world = World::new(limits(4));
        let region = world.register_region().unwrap();
        world.journal.revision = WorldRevision(u64::MAX);

        let result = world
            .observe_object(ObjectObservation::new(
                UpdateSource::new(region, 1, Ordered(1), 0),
                Uuid::from_u128(21),
                NonZeroU32::new(1).unwrap(),
                Kind::Object,
                Root,
            ))
            .unwrap_err();

        assert_eq!(result, ApplyError::RevisionExhausted);
        assert_eq!(world.entity(Uuid::from_u128(21)), None);
        assert_eq!(world.entity_at(LocalAddress::new(region, NonZeroU32::new(1).unwrap())), None);
        assert!(world.objects().iter().next().is_none());
        assert_eq!(world.revision(), WorldRevision(u64::MAX));
        assert_eq!(world.journal.entries.len(), 1);
        assert_eq!(world.journal.entries.front().unwrap().revision, WorldRevision(1));
        assert_eq!(world.journal.lost_through, None);
    }

    #[test]
    fn patch_revision_exhaustion_preserves_state() {
        let mut world = World::new(limits(4));
        let region = world.register_region().unwrap();
        let created = world.observe_object(observation(region, 1, 270, 1)).unwrap();
        let ApplyOutcome::Introduced(entity) = created.outcome else { panic!("expected introduction") };
        let revision = world.revision();
        world.journal.revision = WorldRevision(u64::MAX);
        let before = world.object_detail(entity).unwrap();
        let patch = ObjectPatch {
            pose: Some(Pose { position: Some(Vec3::Y), rotation: None }),
            shape: FieldUpdate::Set(Shape::Prim(Default::default())),
            ..ObjectPatch::new(UpdateSource::new(region, 100, Ordered(100), 0), NonZeroU32::new(1).unwrap())
        };

        assert_eq!(world.patch_object(patch), Err(ApplyError::RevisionExhausted));
        assert_eq!(world.object_detail(entity).unwrap(), before);

        world.journal.revision = revision;
        let removed = world.remove_object(UpdateSource::new(region, 2, Ordered(2), 0), NonZeroU32::new(1).unwrap()).unwrap();
        assert_eq!(removed.outcome, ApplyOutcome::Removed(Uuid::from_u128(270)));
    }

    #[test]
    fn noop_updates_need_no_revision() {
        let mut world = World::new(limits(4));
        let region = world.register_region().unwrap();
        let mut initial = observation(region, 1, 270, 1);
        initial.pose = Some(Pose { position: Some(Vec3::X), rotation: None });
        let introduced = world.observe_object(initial).unwrap();
        let ApplyOutcome::Introduced(entity) = introduced.outcome else { panic!("expected object introduction") };
        assert!(introduced.revision.is_some());
        let initial_revision = world.revision();
        world.journal.revision = WorldRevision(u64::MAX);
        let stale = ObjectPatch {
            pose: Some(Pose { position: Some(Vec3::Y), rotation: None }),
            ..ObjectPatch::new(UpdateSource::new(region, 1, Ordered(1), 0), NonZeroU32::new(1).unwrap())
        };
        assert_eq!(world.patch_object(stale).unwrap().outcome, ApplyOutcome::Unchanged);

        world.journal.revision = initial_revision;
        let same_pose = ObjectPatch {
            pose: Some(Pose { position: Some(Vec3::X), rotation: None }),
            ..ObjectPatch::new(UpdateSource::new(region, 2, Ordered(2), 0), NonZeroU32::new(1).unwrap())
        };
        assert_eq!(world.patch_object(same_pose).unwrap().outcome, ApplyOutcome::Updated(entity));
        world.journal.revision = WorldRevision(u64::MAX);
        assert_eq!(world.observe_object(observation(region, 3, 270, 1)).unwrap().outcome, ApplyOutcome::Unchanged);
        assert_eq!(world.revision(), WorldRevision(u64::MAX));
    }

    #[test]
    fn appearance_patches_emit_appearance() {
        let mut world = World::new(limits(4));
        let region = world.register_region().unwrap();
        let patch = |local_id| ObjectPatch::new(UpdateSource::new(region, 2, Ordered(2), 0), NonZeroU32::new(local_id).unwrap());
        let surface = SurfaceAppearance::new(
            FaceAppearance { texture: Some(AssetId::new(Uuid::from_u128(281))), ..FaceAppearance::default() },
            Vec::new(),
        )
        .unwrap();
        let materials = PbrMaterialAssignments::new(vec![(3, AssetId::new(Uuid::from_u128(283)))]).unwrap();
        let cases = [
            ("surface_appearance", 280, ObjectPatch { surface_appearance: Some(surface), ..patch(1) }),
            ("pbr_materials", 282, ObjectPatch { pbr_materials: Some(materials), ..patch(2) }),
        ];

        for (case, id, patch) in cases {
            let added = world.observe_object(observation(region, 1, id, patch.local_id.get())).unwrap();
            let ApplyOutcome::Introduced(entity) = added.outcome else { panic!("{case}: expected object introduction") };

            let applied = world.patch_object(patch).unwrap();

            assert_eq!(applied.outcome, ApplyOutcome::Updated(entity), "{case}");
            assert_matches!(
                last_changes(&world),
                [EntityUpdated { id: changed, classes, .. }]
                    if *changed == Uuid::from_u128(id)
                        && classes.contains(WorldChangeClasses::APPEARANCE)
                        && !classes.contains(WorldChangeClasses::SHAPE),
                "{case}"
            );
        }
    }

    #[test]
    fn change_budget_evicts_oldest() {
        let mut limits = limits(4);
        limits.max_journal_changes = 1;
        let mut world = World::new(limits);
        let region = world.register_region().unwrap();

        let first_revision = world.revision();
        assert_region_revision(
            world.patch_region(region, RegionInfo { name: Some("region".to_owned()), ..RegionInfo::default() }).unwrap().outcome,
        );

        assert_eq!(world.journal.entries.len(), 1);
        assert_eq!(world.journal.retained_changes, 1);
        assert_eq!(world.journal.entries.front().unwrap().revision, world.revision());
        assert_eq!(world.journal.lost_through, Some(first_revision));
    }

    #[test]
    fn region_retirement_journals_removals_incrementally() {
        let mut limits = limits(8);
        limits.max_journal_changes = 2;
        let mut world = World::new(limits);
        let region = world.register_region().unwrap();
        assert_revision(world.observe_object(observation(region, 1, 290, 1)).unwrap());
        assert_revision(world.observe_object(observation(region, 2, 291, 2)).unwrap());
        let before_retirement = world.revision();
        let before_retirement_cursor = world.read_barrier().changes;

        let retired = world.retire_region(region).unwrap();
        let revision = match retired.outcome {
            RegionOutcome::Retired { revision, entities_removed } => {
                assert_eq!(entities_removed, 2);
                revision
            }
            outcome => panic!("expected region retirement, got {outcome:?}"),
        };
        assert_eq!(revision, WorldRevision(before_retirement.get() + 1));
        assert_eq!(world.region(region), None);
        assert!(world.objects().iter().next().is_none());
        let retirement_page = world.change_page(before_retirement_cursor, 8).unwrap();
        assert_matches!(retirement_page.records.as_slice(), [record]
            if record.revision == revision && matches!(record.change, RegionRetired { region: retired, entities_removed: 2 } if retired == region));
        assert!(!retirement_page.has_more);
        assert_eq!(world.check_invariants(), Ok(()));

        drain_removals(&mut world);
        let removal_page = world.change_page(retirement_page.cursor, 8).unwrap();
        assert_eq!(removal_page.records.len(), 2);
        assert!(removal_page.records.iter().all(|record| {
            record.revision > revision && matches!(record.change, EntityRemoved { reason: WorldEntityRemovalReason::RegionRetired, .. })
        }));
        assert!(!removal_page.has_more);

        let after_retirement_cursor = world.read_barrier().changes;
        assert!(world.change_page(after_retirement_cursor, 8).unwrap().records.is_empty());
        let registered_region = world.register_region().unwrap();
        let changes = world.change_page(after_retirement_cursor, 8).unwrap();
        assert_matches!(
            changes.records.as_slice(),
            [record] if record.revision == world.revision()
                && matches!(record.change, RegionRegistered(region) if region == registered_region)
        );
        assert!(!changes.has_more);
        assert_eq!(world.check_invariants(), Ok(()));
    }

    #[test]
    fn zero_journal_discards_history() {
        let mut limits = limits(0);
        limits.max_journal_changes = 4;
        let mut world = World::new(limits);
        let before_registration = world.read_barrier().changes;
        let _region = world.register_region().unwrap();

        assert!(world.journal.entries.is_empty());
        assert_eq!(world.journal.retained_changes, 0);
        assert_eq!(world.journal.lost_through, Some(world.revision()));
        assert_eq!(world.change_page(before_registration, 8), Err(crate::WorldReadError::ResyncRequired));
        assert_eq!(world.check_invariants(), Ok(()));
    }

    #[test]
    fn journal_records_add_rebind_transfer() {
        let mut world = World::new(limits(8));
        let first_region = world.register_region().unwrap();
        let second_region = world.register_region().unwrap();
        assert_matches!(last_changes(&world), [RegionRegistered(region)] if *region == second_region);
        assert_region_revision(
            world.patch_region(first_region, RegionInfo { name: Some("first".to_owned()), ..RegionInfo::default() }).unwrap().outcome,
        );
        assert_matches!(last_changes(&world), [RegionUpdated(region)] if *region == first_region);

        let first_address = LocalAddress::new(first_region, NonZeroU32::new(1).unwrap());
        assert_revision(world.observe_object(observation(first_region, 1, 230, 1)).unwrap());
        assert_matches!(
            last_changes(&world),
            [EntityAdded { id, address, .. }] if *id == Uuid::from_u128(230) && *address == first_address
        );

        let rebound_address = LocalAddress::new(first_region, NonZeroU32::new(2).unwrap());
        assert_revision(world.observe_object(observation(first_region, 2, 230, 2)).unwrap());
        assert!(last_changes(&world).iter().any(|change| {
            matches!(change, EntityRebound { id, from, to, .. } if *id == Uuid::from_u128(230) && *from == first_address && *to == rebound_address)
        }));

        let transfer_address = LocalAddress::new(second_region, NonZeroU32::new(3).unwrap());
        assert_revision(world.observe_object(observation(second_region, 1, 230, 3)).unwrap());
        assert!(last_changes(&world).iter().any(|change| {
            matches!(change, EntityTransferred { id, from, to, .. } if *id == Uuid::from_u128(230) && *from == rebound_address && *to == transfer_address)
        }));
    }

    #[test]
    fn kill_and_cascade_reasons_recorded() {
        let mut world = World::new(limits(8));
        let region = world.register_region().unwrap();
        assert_revision(world.observe_object(observation(region, 1, 250, 1)).unwrap());
        let mut child = observation(region, 2, 251, 2);
        child.parent = Local(NonZeroU32::new(1).unwrap());
        assert_revision(world.observe_object(child).unwrap());
        let mut avatar = observation(region, 3, 252, 3);
        avatar.kind = Kind::Avatar;
        avatar.parent = Local(NonZeroU32::new(1).unwrap());
        assert_revision(world.observe_object(avatar).unwrap());
        let mut attachment = observation(region, 4, 253, 4);
        attachment.parent = Local(NonZeroU32::new(3).unwrap());
        assert_revision(world.observe_object(attachment).unwrap());

        assert_revision(world.remove_object(UpdateSource::new(region, 5, Ordered(5), 0), NonZeroU32::new(1).unwrap()).unwrap());
        assert_matches!(last_changes(&world), [SubtreeRemovalPending { id, .. }] if *id == Uuid::from_u128(250));
        assert_eq!(world.object_count(), 2);
        assert!(world.entity(Uuid::from_u128(250)).is_none());
        assert!(world.entity(Uuid::from_u128(251)).is_none());
        assert!(world.entity(Uuid::from_u128(252)).is_some());
        drain_removals(&mut world);

        let changes = world.journal.entries.iter().flat_map(|entry| &entry.changes).collect::<Vec<_>>();
        assert!(changes.iter().any(|change| {
            matches!(change, EntityRemoved { id, address, reason: WorldEntityRemovalReason::Killed, .. }
                if *id == Uuid::from_u128(250) && address.local_id().get() == 1)
        }));
        assert!(changes.iter().any(|change| {
            matches!(change, EntityRemoved { id, address, reason: WorldEntityRemovalReason::ParentCascade, .. }
                if *id == Uuid::from_u128(251) && address.local_id().get() == 2)
        }));
        assert!(changes.iter().any(|change| {
            matches!(change, EntityUpdated { id, classes, .. }
                if *id == Uuid::from_u128(252)
                    && classes.contains(WorldChangeClasses::RELATIONSHIP | WorldChangeClasses::POSE))
        }));
        assert!(changes.iter().any(|change| matches!(change, SubtreeInvalidated { id, .. } if *id == Uuid::from_u128(252))));
    }

    #[test]
    fn relationships_invalidate_transform_motion() {
        let mut world = World::new(limits(8));
        let region = world.register_region().unwrap();
        assert_revision(world.observe_object(observation(region, 1, 320, 1)).unwrap());
        assert_revision(world.observe_object(observation(region, 2, 321, 2)).unwrap());

        let mut link = ObjectPatch::new(UpdateSource::new(region, 3, Ordered(3), 0), NonZeroU32::new(2).unwrap());
        link.parent = Some(Local(NonZeroU32::new(1).unwrap()));
        assert_revision(world.patch_object(link).unwrap());

        assert!(last_changes(&world).iter().any(|change| {
            matches!(change, EntityUpdated { id, classes, .. }
                if *id == Uuid::from_u128(321)
                    && classes.contains(WorldChangeClasses::RELATIONSHIP | WorldChangeClasses::POSE))
        }));
    }

    #[test]
    fn shape_and_appearance_journal_separately() {
        let mut world = World::new(limits(8));
        let region = world.register_region().unwrap();
        let mut object = observation(region, 1, 340, 1);
        object.shape = FieldUpdate::Set(Shape::Prim(Default::default()));
        object.appearance = FieldUpdate::Set(Appearance::Surface);
        assert_revision(world.observe_object(object).unwrap());

        let appearance = ObjectPatch {
            appearance: FieldUpdate::Set(Appearance::Avatar),
            ..ObjectPatch::new(UpdateSource::new(region, 3, Ordered(3), 0), NonZeroU32::new(1).unwrap())
        };
        assert_revision(world.patch_object(appearance).unwrap());
        assert_matches!(
            last_changes(&world),
            [EntityUpdated { id, classes, .. }]
                if *id == Uuid::from_u128(340) && classes.contains(WorldChangeClasses::APPEARANCE)
                    && !classes.contains(WorldChangeClasses::SHAPE)
        );

        let mixed_freshness = ObjectPatch {
            shape: FieldUpdate::Set(Shape::Mesh { asset_id: AssetId::new(Uuid::from_u128(1)), flags: SculptFlags::default() }),
            appearance: FieldUpdate::Set(Appearance::Surface),
            ..ObjectPatch::new(UpdateSource::new(region, 2, Ordered(2), 0), NonZeroU32::new(1).unwrap())
        };
        assert_revision(world.patch_object(mixed_freshness).unwrap());
        assert_matches!(
            last_changes(&world),
            [EntityUpdated { id, classes, .. }]
                if *id == Uuid::from_u128(340) && classes.contains(WorldChangeClasses::SHAPE)
                    && !classes.contains(WorldChangeClasses::APPEARANCE)
        );

        let asset_change = ObjectPatch {
            shape: FieldUpdate::Set(Shape::Mesh { asset_id: AssetId::new(Uuid::from_u128(2)), flags: SculptFlags::default() }),
            ..ObjectPatch::new(UpdateSource::new(region, 4, Ordered(4), 0), NonZeroU32::new(1).unwrap())
        };
        assert_revision(world.patch_object(asset_change).unwrap());
        assert_matches!(
            last_changes(&world),
            [EntityUpdated { id, classes, .. }]
                if *id == Uuid::from_u128(340) && classes.contains(WorldChangeClasses::SHAPE)
                    && !classes.contains(WorldChangeClasses::APPEARANCE)
        );
    }

    #[test]
    fn parent_pose_invalidates_subtree() {
        let mut world = World::new(limits(8));
        let region = world.register_region().unwrap();
        assert_revision(world.observe_object(observation(region, 1, 310, 1)).unwrap());

        let mut child = observation(region, 2, 311, 2);
        child.parent = Local(NonZeroU32::new(1).unwrap());
        assert_revision(world.observe_object(child).unwrap());

        let mut grandchild = observation(region, 3, 312, 3);
        grandchild.parent = Local(NonZeroU32::new(2).unwrap());
        assert_revision(world.observe_object(grandchild).unwrap());

        let mut movement = ObjectPatch::new(UpdateSource::new(region, 4, Ordered(4), 0), NonZeroU32::new(1).unwrap());
        movement.pose = Some(Pose { position: Some(Vec3::X), rotation: None });
        assert_revision(world.patch_object(movement).unwrap());

        assert_matches!(
            last_changes(&world),
            [EntityUpdated { id, classes, .. }, SubtreeInvalidated { id: root, .. }]
                if *id == Uuid::from_u128(310)
                    && classes.contains(WorldChangeClasses::POSE)
                    && *root == Uuid::from_u128(310)
        );
    }

    #[test]
    fn journal_records_replacement_and_retirement() {
        let mut world = World::new(limits(8));
        let region = world.register_region().unwrap();
        assert_revision(world.observe_object(observation(region, 1, 260, 1)).unwrap());
        assert_revision(world.observe_object(observation(region, 2, 261, 1)).unwrap());
        assert_matches!(last_changes(&world), [SubtreeRemovalPending { id, .. }, EntityAdded { .. }] if *id == Uuid::from_u128(260));
        assert!(world.entity(Uuid::from_u128(260)).is_none());
        drain_removals(&mut world);
        assert!(last_changes(&world).iter().any(|change| {
            matches!(change, EntityRemoved { id, reason: WorldEntityRemovalReason::AddressReplaced, .. } if *id == Uuid::from_u128(260))
        }));

        let entity_address = LocalAddress::new(region, NonZeroU32::new(1).unwrap());
        let mut child = observation(region, 3, 262, 2);
        child.parent = Local(NonZeroU32::new(1).unwrap());
        assert_revision(world.observe_object(child).unwrap());
        assert_region_revision(world.retire_region(region).unwrap().outcome);
        assert_matches!(last_changes(&world), [RegionRetired { region: retired, entities_removed: 2 }] if *retired == region);
        assert_eq!(world.object_count(), 0);
        drain_removals(&mut world);
        let changes = world.journal.entries.iter().flat_map(|entry| &entry.changes).collect::<Vec<_>>();
        assert!(changes.iter().any(|change| {
            matches!(change, EntityRemoved { id, address, reason: WorldEntityRemovalReason::RegionRetired, .. }
                if *id == Uuid::from_u128(261) && *address == entity_address)
        }));
        assert!(changes.iter().any(|change| {
            matches!(change, EntityRemoved { id, reason: WorldEntityRemovalReason::RegionRetired, .. } if *id == Uuid::from_u128(262))
        }));
        assert!(
            changes.iter().any(|change| matches!(change, RegionRetired { region: retired, entities_removed: 2 } if *retired == region))
        );
    }

    #[test]
    fn reclamation_revises_once_per_turn() {
        let mut world = World::new(limits(16));
        let region = world.register_region().unwrap();
        assert_revision(world.observe_object(observation(region, 1, 350, 1)).unwrap());
        let mut child = observation(region, 2, 351, 2);
        child.parent = Local(NonZeroU32::new(1).unwrap());
        assert_revision(world.observe_object(child).unwrap());
        let mut grandchild = observation(region, 3, 352, 3);
        grandchild.parent = Local(NonZeroU32::new(2).unwrap());
        assert_revision(world.observe_object(grandchild).unwrap());

        let cursor = world.read_barrier().changes;
        assert_revision(world.remove_object(UpdateSource::new(region, 4, Ordered(4), 0), NonZeroU32::new(1).unwrap()).unwrap());
        let pending = world.change_page(cursor, 8).unwrap();
        assert_matches!(pending.records.as_slice(), [record] if matches!(record.change, SubtreeRemovalPending { .. }));

        let progress = world.reclaim_removals(crate::MAX_REMOVAL_WORK_PER_TURN);
        assert_eq!(progress.removed_entities, 3);
        assert!(!progress.pending);
        let changes = world.change_page(pending.cursor, 8).unwrap();
        assert_eq!(changes.records.len(), 3);
        assert!(changes.records.iter().all(|record| {
            record.revision == progress.revision.expect("removal changes are published") && matches!(record.change, EntityRemoved { .. })
        }));
        assert!(!changes.has_more);
    }
}
