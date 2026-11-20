use std::{assert_matches, hint::black_box, num::NonZeroU32};

use math::{DVec3, Quat, Vec3};
use uuid::Uuid;
use world::{
    ApplyOutcome::Introduced,
    Entity, FieldGroup, FieldRejection,
    FieldRejectionReason::ParentCycle,
    Kind::Object,
    ObjectObservation, ObjectPatch,
    OrderEvidence::Ordered,
    Parent,
    ParentUpdate::{Local, Root},
    Pose, PoseError, PoseState, Region, RegionInfo, UpdateSource, World, WorldLimits,
};

#[derive(Clone, Copy, Debug)]
struct DepthCase {
    name: &'static str,
    edge_depth: usize,
}

const DEPTHS: &[DepthCase] = &[
    DepthCase { name: "root", edge_depth: 0 },
    DepthCase { name: "typical", edge_depth: 8 },
    DepthCase { name: "limit", edge_depth: 16 },
    DepthCase { name: "over_limit", edge_depth: 17 },
];

impl std::fmt::Display for DepthCase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name)
    }
}

fn world_with_chain(edge_depth: usize) -> (World, Region, Entity) {
    let entity_count = edge_depth + 1;
    let mut world = World::new(WorldLimits {
        max_regions: 1,
        max_entities: entity_count,
        max_entities_per_region: entity_count,
        max_tombstones_per_region: 1,
        max_tombstones: 1,
        max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(1),
        max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(1),
        max_journal_entries: entity_count,
        max_journal_changes: entity_count.saturating_mul(4),
        max_region_name_bytes: 0,
        max_recovery_targets: 0,
    });
    let region = world.register_region().unwrap();

    assert_matches!(
        world.patch_region(region, RegionInfo { origin: Some(DVec3::ZERO), ..RegionInfo::default() }).expect("region origin").outcome,
        world::RegionOutcome::Patched { .. }
    );

    let mut target = None;
    for index in 0..entity_count {
        let index = u32::try_from(index).expect("benchmark chain index fits u32");
        let local_id = NonZeroU32::new(index + 1).expect("local ID is nonzero");
        let parent = if index == 0 { Root } else { Local(NonZeroU32::new(index).expect("parent local ID is nonzero")) };
        let source = UpdateSource::new(region, index + 1, Ordered(u64::from(index) + 1), 0);
        let observation = ObjectObservation {
            pose: Some(Pose { position: Some(Vec3::X), rotation: Some(Quat::IDENTITY) }),
            ..ObjectObservation::new(source, Uuid::from_u128(u128::from(index) + 1), local_id, Object, parent)
        };
        let applied = world.observe_object(observation).expect("object observation");
        let Introduced(entity) = applied.outcome else { panic!("expected object introduction") };
        target = Some(entity);
    }

    (world, region, target.expect("chain contains a root"))
}

fn cycle_patch(region: Region, order: u64, root_local_id: NonZeroU32, tail_local_id: NonZeroU32) -> ObjectPatch {
    let mut patch = ObjectPatch::new(
        UpdateSource::new(region, u32::try_from(order).expect("benchmark order fits u32"), Ordered(order), 0),
        root_local_id,
    );
    patch.parent = Some(Local(tail_local_id));
    patch
}

fn assert_chain_depth(world: &World, target: Entity, edge_depth: usize) {
    let mut current = target;
    for _ in 0..edge_depth {
        let Parent::Resolved(parent) = world.object(current).expect("chain object").parent() else {
            panic!("parent chain ended before its measured depth");
        };
        current = parent;
    }
    assert_matches!(world.object(current).map(|object| object.parent()), Some(Parent::Root));
}

fn assert_pose_contract(world: &World, target: Entity, edge_depth: usize) {
    let object = world.object(target).expect("target object");
    match (object.world_pose(), object.coordinate_frame()) {
        (Ok(PoseState::Resolved(_)), Ok(_)) if edge_depth <= 16 => {}
        (Err(PoseError::TraversalLimitExceeded { max_edges: 16 }), Err(PoseError::TraversalLimitExceeded { max_edges: 16 }))
            if edge_depth > 16 => {}
        _ => panic!("unexpected pose result at {edge_depth} edges"),
    }
}

mod benches {
    use super::*;

    #[divan::bench(args = DEPTHS)]
    fn pose_resolve(bencher: divan::Bencher, case: DepthCase) {
        let (world, _region, target) = world_with_chain(case.edge_depth);
        assert_chain_depth(&world, target, case.edge_depth);
        assert_pose_contract(&world, target, case.edge_depth);
        let object = world.object(target).expect("target object");
        bencher.counter(divan::counter::ItemsCount::new(1)).bench_local(|| black_box(object.world_pose()));
    }

    #[divan::bench(args = DEPTHS)]
    fn pose_coordinate_frame(bencher: divan::Bencher, case: DepthCase) {
        let (world, _region, target) = world_with_chain(case.edge_depth);
        assert_chain_depth(&world, target, case.edge_depth);
        assert_pose_contract(&world, target, case.edge_depth);
        let object = world.object(target).expect("target object");
        bencher.counter(divan::counter::ItemsCount::new(1)).bench_local(|| black_box(object.coordinate_frame()));
    }

    #[divan::bench(args = DEPTHS)]
    fn pose_cycle_proposal(bencher: divan::Bencher, case: DepthCase) {
        let (mut world, region, _target) = world_with_chain(case.edge_depth);
        let root_local_id = NonZeroU32::new(1).expect("root local ID is nonzero");
        let tail_local_id =
            NonZeroU32::new(u32::try_from(case.edge_depth + 1).expect("tail local ID fits u32")).expect("tail local ID is nonzero");
        let mut order = u64::try_from(case.edge_depth + 2).expect("cycle order fits u64");
        let probe =
            world.patch_object(cycle_patch(region, order, root_local_id, tail_local_id)).expect("cycle proposal is a field rejection");
        assert_eq!(probe.rejections.len(), 1);
        assert_eq!(probe.rejections.iter().next().copied(), Some(FieldRejection { group: FieldGroup::Parent, reason: ParentCycle }));
        order += 1;

        bencher.counter(divan::counter::ItemsCount::new(1)).bench_local(|| {
            let current_order = order;
            order += 1;
            let _ = black_box(
                world.patch_object(cycle_patch(region, current_order, root_local_id, tail_local_id)).expect("cycle proposal is rejected"),
            );
        });
    }
}
