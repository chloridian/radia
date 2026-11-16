use std::num::{NonZeroU8, NonZeroU32};

use math::{Quat, Vec3};
use uuid::Uuid;

use crate::ApplyError::InvalidValue;
use crate::ApplyOutcome::{Introduced, Unchanged, Updated};
use crate::FieldRejectionReason::ParentCycle;
use crate::FieldUpdate;
use crate::Kind::{Avatar, Object};
use crate::OrderEvidence::Ordered;
use crate::ParentUpdate::{Local, Root};
use crate::{
    Appearance, AttachmentPoint, FieldGroup, FieldRejection, ObjectObservation, ObjectPatch, Parent, ParentUpdate, PbrMaterialOverride,
    PbrMaterialOverrides, Pose, Region, RegionInfo, UpdateSource, World, WorldLimits,
};

fn limits() -> WorldLimits {
    WorldLimits {
        max_regions: 2,
        max_entities: 2,
        max_entities_per_region: 2,
        max_tombstones_per_region: 4,
        max_tombstones: 8,
        max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(4),
        max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(8),
        max_journal_entries: 3,
        max_journal_changes: 32,
        max_region_name_bytes: 8,
        max_recovery_targets: 4,
    }
}

fn uuid(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

fn local_id(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).expect("test local IDs are nonzero")
}

fn source(region: Region, order: u64) -> UpdateSource {
    let sequence = u32::try_from(order).expect("test order fits in a sequence");
    UpdateSource::new(region, sequence, Ordered(order), 0)
}

fn object(source: UpdateSource, id: u128, local_id_value: u32, parent: ParentUpdate) -> ObjectObservation {
    ObjectObservation::new(source, uuid(id), local_id(local_id_value), Object, parent)
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config { cases: 48, max_shrink_iters: 128, ..Default::default() })]

    #[test]
    fn queued_relationships_preserve_indexes(
        operations in proptest::collection::vec((0_u8..6, 2_u32..130, 0_usize..9), 1..80)
    ) {

        let mut bounds = limits();
        bounds.max_entities = 256;
        bounds.max_entities_per_region = 256;
        let mut world = World::new(bounds);
        let region = world.register_region().unwrap();
        for child in 2..130 {
            let _ = world.observe_object(object(source(region, u64::from(child)), u128::from(child), child, Local(local_id(1)))).unwrap();
        }
        let _ = world.observe_object(object(source(region, 200), 1, 1, Root)).unwrap();
        for (index, (operation, child, batch)) in operations.into_iter().enumerate() {
            let update = source(region, 1000 + index as u64);
            match operation {
                0 => { let _ = world.resolve_relationships(batch).unwrap(); }
                1 | 2 => {
                    let mut patch = ObjectPatch::new(update, local_id(child));
                    patch.parent = Some(if operation == 1 { Root } else { Local(local_id(1)) });
                    let _ = world.patch_object(patch).unwrap();
                }
                3 => {
                    let destination = if batch.is_multiple_of(2) { 1 } else { 250 };
                    let _ = world.observe_object(object(update, 1, destination, Root)).unwrap();
                }
                4 => { let _ = world.remove_object(update, local_id(child)).unwrap(); }
                5 => {
                    let _ = world.observe_object(object(update, u128::from(child), child, Local(local_id(1)))).unwrap();
                }
                _ => unreachable!(),
            }
            proptest::prop_assert_eq!(world.check_invariants(), Ok(()));
        }
        while world.has_pending_relationships() {
            let _ = world.resolve_relationships(7).unwrap();
            proptest::prop_assert_eq!(world.check_invariants(), Ok(()));
        }
    }

    #[test]
    fn mutation_sequences_preserve_world_indexes(
        operations in proptest::collection::vec((0_u8..5, 0_u8..3, 0_u8..16, 0_u8..8, 1_u8..32, 0_u8..9, 0_u8..32), 1..128)
    ) {
        let mut world_limits = limits();
        world_limits.max_regions = 3;
        world_limits.max_entities = 16;
        world_limits.max_entities_per_region = 16;
        world_limits.max_tombstones_per_region = 64;
        world_limits.max_tombstones = 96;
        world_limits.max_tombstone_bytes_per_region = WorldLimits::tombstone_bytes_for_count(64);
        world_limits.max_tombstone_bytes = WorldLimits::tombstone_bytes_for_count(96);
        world_limits.max_journal_entries = 32;
        world_limits.max_journal_changes = 512;
        world_limits.max_recovery_targets = 16;
        let mut world = World::new(world_limits);
        let regions = [world.register_region().unwrap(), world.register_region().unwrap(), world.register_region().unwrap()];

        proptest::prop_assert_eq!(world.check_invariants(), Ok(()));

        for (ordinal, (operation, region_index, identity, slot, source_order, parent_slot, value)) in operations.into_iter().enumerate() {
            let region = regions[usize::from(region_index)];
            let local_id = NonZeroU32::new(u32::from(slot) + 1).expect("generated local ID is nonzero");
            let id = uuid(800 + u128::from(identity));
            let parent = match u32::from(parent_slot) {
                0 => Root,
                parent_local if parent_local == local_id.get() => Root,
                parent_local => Local(NonZeroU32::new(parent_local).expect("generated parent local ID is nonzero")),
            };
            let order = u64::from(source_order);
            let source = UpdateSource::new(region, u32::from(source_order), Ordered(order), ordinal as u32);
            let value = f32::from(value);

            match operation {
                0 => {
                    let kind = if identity.is_multiple_of(4) { Avatar } else { Object };
                    let mut observation = ObjectObservation::new(source, id, local_id, kind, parent);
                    observation.pose = Some(Pose { position: Some(Vec3::new(value, value * 0.5, 0.0)), rotation: Some(Quat::IDENTITY) });
                    observation.scale = Some(Vec3::splat(1.0 + value / 32.0));
                    let result = world.observe_object(observation);
                    proptest::prop_assert!(result.is_ok() || matches!(result, Err(InvalidValue { field: "parent_cycle" })), "unexpected observation result: {result:?}");
                }
                1 => {
                    let mut patch = ObjectPatch::new(source, local_id);
                    patch.parent = Some(parent);
                    patch.pose = Some(Pose { position: Some(Vec3::new(value * 0.25, 0.0, value)), rotation: Some(Quat::IDENTITY) });
                    patch.shape = if (value as u8).is_multiple_of(2) {
                        FieldUpdate::Set(crate::Shape::Prim(Default::default()))
                    } else {
                        FieldUpdate::Clear
                    };
                    patch.appearance = if (value as u8).is_multiple_of(3) {
                        FieldUpdate::Set(Appearance::Surface)
                    } else {
                        FieldUpdate::Keep
                    };
                    let _ = world.patch_object(patch).expect("apply valid patch");
                }
                2 => {
                    let _ = world.remove_object(source, local_id).expect("apply valid removal");
                }
                3 => {
                    let _ = world
                        .patch_region(region, RegionInfo { name: Some(format!("Region {region_index}")), ..RegionInfo::default() })
                        .expect("apply valid region metadata");
                }
                4 => {
                    let mut patch = ObjectPatch::new(source, local_id);
                    patch.attachment_point = if parent_slot == 0 {
                        FieldUpdate::Clear
                    } else {
                        FieldUpdate::Set(AttachmentPoint::new(NonZeroU8::new(parent_slot).expect("nonzero attachment point")))
                    };
                    let _ = world.patch_object(patch).expect("apply valid attachment update");
                }
                5.. => panic!("operation generator is bounded to 0..5"),
            }

            proptest::prop_assert_eq!(world.check_invariants(), Ok(()));
        }
    }
}

#[test]
fn cycle_check_exceeds_pose_limit() {
    for use_observation in [false, true] {
        let mut world_limits = limits();
        world_limits.max_entities = 34;
        world_limits.max_entities_per_region = 34;
        let mut world = World::new(world_limits);
        let region = world.register_region().unwrap();
        let mut entities = Vec::new();
        for local in 1..=34 {
            let parent = if local == 1 { Root } else { Local(local_id(local - 1)) };
            let applied = world.observe_object(object(source(region, u64::from(local)), u128::from(local), local, parent)).unwrap();
            let Introduced(entity) = applied.outcome else { panic!("expected chain introduction") };
            entities.push(entity);
        }
        let valid = ObjectPatch { parent: Some(Local(local_id(32))), ..ObjectPatch::new(source(region, 35), local_id(34)) };
        assert_eq!(world.patch_object(valid).unwrap().outcome, Updated(entities[33]));
        assert_eq!(world.object(entities[33]).unwrap().parent(), Parent::Resolved(entities[31]));
        assert_eq!(world.children(entities[32]).count(), 0);
        let revision = world.revision();
        let applied = if use_observation {
            world.observe_object(object(source(region, 36), 1, 1, Local(local_id(34)))).unwrap()
        } else {
            world
                .patch_object(ObjectPatch { parent: Some(Local(local_id(34))), ..ObjectPatch::new(source(region, 36), local_id(1)) })
                .unwrap()
        };
        assert_eq!(applied.outcome, Unchanged);
        assert_eq!(
            applied.rejections.iter().copied().collect::<Vec<_>>(),
            [FieldRejection { group: FieldGroup::Parent, reason: ParentCycle }]
        );
        assert_eq!(world.revision(), revision);
        assert_eq!(world.object(entities[0]).unwrap().parent(), Parent::Root);
        for (index, entity) in entities.iter().enumerate().skip(1) {
            let parent = if index == 33 { entities[31] } else { entities[index - 1] };
            assert_eq!(world.object(*entity).unwrap().parent(), Parent::Resolved(parent));
        }
        assert_eq!(world.check_invariants(), Ok(()));
    }
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config { cases: 48, max_shrink_iters: 128, ..Default::default() })]

    #[test]
    fn override_retention_preserves_world_indexes(
        operations in proptest::collection::vec((0_u8..6, 0_u8..2, 0_u8..8, 0_u8..8, 1_u8..32, 0_u8..4), 1..128)
    ) {
        let mut world_limits = limits();
        world_limits.max_entities = 16;
        world_limits.max_entities_per_region = 16;
        world_limits.max_tombstones_per_region = 4;
        world_limits.max_tombstones = 6;
        world_limits.max_tombstone_bytes_per_region = WorldLimits::tombstone_bytes_for_count(4);
        world_limits.max_tombstone_bytes = WorldLimits::tombstone_bytes_for_count(6);
        world_limits.max_journal_entries = 32;
        world_limits.max_journal_changes = 512;
        world_limits.max_recovery_targets = 16;
        let mut world = World::new(world_limits);
        let mut regions = [world.register_region().expect("register generated region"), world.register_region().expect("register generated region")];

        for (ordinal, (operation, region_index, slot, identity, source_order, value)) in operations.into_iter().enumerate() {
            let region = regions[usize::from(region_index)];
            let local_id = local_id(u32::from(slot) + 1);
            let source = UpdateSource::new(region, u32::from(source_order), Ordered(u64::from(source_order)), ordinal as u32);
            let faces = if value == 0 {
                Vec::new()
            } else {
                vec![(value, PbrMaterialOverride { roughness: Some(f32::from(value) / 4.0), ..PbrMaterialOverride::default() })]
            };
            let overrides = PbrMaterialOverrides::new(faces).expect("valid generated overrides");
            match operation {
                0 => {
                    let patch = ObjectPatch { pbr_material_overrides: Some(overrides), ..ObjectPatch::new(source, local_id) };
                    let _ = world.patch_object(patch).expect("apply valid override");
                }
                1 => {
                    let observation = ObjectObservation {
                        appearance: FieldUpdate::Set(Appearance::Surface),
                        ..ObjectObservation::new(source, uuid(1_000 + u128::from(identity)), local_id, Object, Root)
                    };
                    if let Err(error) = world.observe_object(observation) {
                        proptest::prop_assert_eq!(error, crate::ApplyError::RemovalCapacityExhausted);
                    }
                }
                2 => {
                    let _ = world.remove_object(source, local_id).expect("apply valid removal");
                }
                3 => match world.retire_region(region) {
                    Ok(_) => regions[usize::from(region_index)] = world.register_region().expect("register replacement region"),
                    Err(error) => proptest::prop_assert_eq!(error, crate::ApplyError::RemovalCapacityExhausted),
                },
                4 => {
                    let expected_id = uuid(1_000 + u128::from(identity));
                    let _ = world.retain_pbr_material_overrides(source, local_id, expected_id, overrides).expect("retain valid override");
                }
                5 => {
                    let progress = world.reclaim_removals(usize::from(value) + 1);
                    proptest::prop_assert!(progress.work_units <= usize::from(value) + 1);
                }
                6.. => panic!("operation generator is bounded to 0..6"),
            }

            proptest::prop_assert_eq!(world.check_invariants(), Ok(()));
        }
    }
}
