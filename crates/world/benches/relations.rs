use std::{fmt, hint::black_box, num::NonZeroU32};

use divan::counter::ItemsCount;
use uuid::Uuid;
use world::{
    ApplyOutcome::Introduced,
    Kind::Object,
    ObjectObservation, ObjectPatch,
    ParentUpdate::{Local, Root},
    Region, World,
};

use super::support::{PER_REGION, limits, size_name, source};

#[derive(Clone, Copy)]
struct SceneSize(usize);

impl fmt::Debug for SceneSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(size_name(self.0))
    }
}

const SCENE_CASES: &[SceneSize] = &[SceneSize(1_000), SceneSize(10_000), SceneSize(65_536)];

#[derive(Clone, Copy)]
struct RootCase {
    region: Region,
    id: Uuid,
    child_count: usize,
}

fn child_first_scene(count: usize, per_region: usize) -> (World, Vec<RootCase>) {
    let mut roots = Vec::with_capacity(count.div_ceil(per_region));
    let mut bounds = limits();
    bounds.max_entities_per_region = per_region;
    let mut world = World::new(bounds);
    let mut first_child_id = 1_u128;
    let mut remaining = count;

    for region_index in 0..count.div_ceil(per_region) {
        let region = world.register_region().expect("register relationship region");
        let entity_count = remaining.min(per_region);
        let child_count = entity_count - 1;
        for local_index in 0..child_count {
            let local_id = NonZeroU32::new(local_index as u32 + 2).expect("child local ID is nonzero");
            let child = ObjectObservation::new(
                source(region, first_child_id as u64),
                Uuid::from_u128(first_child_id),
                local_id,
                Object,
                Local(NonZeroU32::new(1).expect("root local ID is nonzero")),
            );
            first_child_id += 1;
            let _ = black_box(world.observe_object(child).expect("pending child introduction"));
        }

        roots.push(RootCase { region, id: Uuid::from_u128(u128::MAX - region_index as u128), child_count });
        remaining -= entity_count;
    }

    (world, roots)
}

fn root_observation(root: RootCase, order: u64) -> ObjectObservation {
    ObjectObservation::new(source(root.region, order), root.id, NonZeroU32::new(1).expect("root local ID is nonzero"), Object, Root)
}

fn deep_parent_fanout_scene() -> (World, Region) {
    const MAX_ENTITIES: usize = 65_536;

    let mut bounds = limits();
    bounds.max_entities_per_region = MAX_ENTITIES;
    let mut world = World::new(bounds);
    let region = world.register_region().expect("register fanout region");

    for local in 2_u32..=32_768 {
        let child = ObjectObservation::new(
            source(region, u64::from(local)),
            Uuid::from_u128(u128::from(local)),
            NonZeroU32::new(local).expect("child local ID is nonzero"),
            Object,
            Local(NonZeroU32::new(1).expect("late parent local ID is nonzero")),
        );
        let _ = world.observe_object(child).expect("pending fanout child introduction");
    }

    for local in 32_769_u32..=65_536 {
        let parent = if local == 32_769 { Root } else { Local(NonZeroU32::new(local - 1).expect("chain parent local ID is nonzero")) };
        let chain_object = ObjectObservation::new(
            source(region, u64::from(local)),
            Uuid::from_u128(u128::from(local)),
            NonZeroU32::new(local).expect("chain local ID is nonzero"),
            Object,
            parent,
        );
        let _ = world.observe_object(chain_object).expect("chain object introduction");
    }

    (world, region)
}

fn late_fanout_parent(region: Region) -> ObjectObservation {
    ObjectObservation::new(
        source(region, 65_537),
        Uuid::from_u128(1),
        NonZeroU32::new(1).expect("late parent local ID is nonzero"),
        Object,
        Local(NonZeroU32::new(65_536).expect("chain tail local ID is nonzero")),
    )
}

#[divan::bench(args = SCENE_CASES)]
fn relations_parent_resolution(bencher: divan::Bencher, size: SceneSize) {
    let count = size.0;
    let region_count = count.div_ceil(PER_REGION);
    let (mut verification, roots) = child_first_scene(count, PER_REGION);
    for (index, root) in roots.iter().copied().enumerate() {
        let order = count as u64 + index as u64 + 1;
        let verified = verification.observe_object(root_observation(root, order)).expect("root arrival");
        let Introduced(entity) = verified.outcome else { panic!("expected root introduction") };
        while verification.has_pending_relationships() {
            let _ = verification.resolve_relationships(64).expect("resolve waiting children");
        }
        assert_eq!(verification.children(entity).count(), root.child_count);
    }

    bencher.counter(ItemsCount::new((count - region_count) as u64)).with_inputs(|| child_first_scene(count, PER_REGION)).bench_local_refs(
        |(world, roots)| {
            for (index, root) in roots.iter().copied().enumerate() {
                let order = count as u64 + index as u64 + 1;
                let applied = world.observe_object(root_observation(root, order)).expect("root arrival");
                let Introduced(_) = applied.outcome else { panic!("expected root introduction") };
                let _ = black_box(applied);
                while world.has_pending_relationships() {
                    let _ = black_box(world.resolve_relationships(64).expect("resolve waiting children"));
                }
            }
        },
    );
}

#[divan::bench]
fn relations_single_region_parent_65k(bencher: divan::Bencher) {
    const COUNT: usize = 65_536;
    let (mut verification, roots) = child_first_scene(COUNT, COUNT);
    let applied = verification.observe_object(root_observation(roots[0], COUNT as u64 + 1)).expect("root arrival");
    let Introduced(entity) = applied.outcome else { panic!("expected root introduction") };
    while verification.has_pending_relationships() {
        let _ = verification.resolve_relationships(64).expect("resolve waiting children");
    }
    assert_eq!(verification.children(entity).count(), COUNT - 1);

    bencher.with_inputs(|| child_first_scene(COUNT, COUNT)).bench_local_refs(|(world, roots)| {
        let root = roots[0];
        let applied = world.observe_object(root_observation(root, COUNT as u64 + 1)).expect("root arrival");
        let Introduced(_) = applied.outcome else { panic!("expected root introduction") };
        let _ = black_box(applied);
    });
}

#[divan::bench]
fn relations_single_region_resolution_batch_65k(bencher: divan::Bencher) {
    const COUNT: usize = 65_536;
    bencher
        .with_inputs(|| {
            let (mut world, roots) = child_first_scene(COUNT, COUNT);
            let _ = world.observe_object(root_observation(roots[0], COUNT as u64 + 1)).expect("root arrival");
            world
        })
        .bench_local_refs(|world| {
            let _ = black_box(world.resolve_relationships(64).expect("resolve waiting children"));
        });
}

#[divan::bench]
fn relations_single_region_resolution_total_65k(bencher: divan::Bencher) {
    const COUNT: usize = 65_536;
    bencher.with_inputs(|| child_first_scene(COUNT, COUNT)).bench_local_refs(|(world, roots)| {
        let _ = black_box(world.observe_object(root_observation(roots[0], COUNT as u64 + 1)).expect("root arrival"));
        while world.has_pending_relationships() {
            let _ = black_box(world.resolve_relationships(64).expect("resolve waiting children"));
        }
    });
}

#[divan::bench]
fn relations_deep_parent_fanout_65k(bencher: divan::Bencher) {
    const OBJECT_COUNT: usize = 65_536;
    const CHILD_COUNT: usize = 32_767;
    let (mut verification, region) = deep_parent_fanout_scene();
    let applied = verification.observe_object(late_fanout_parent(region)).expect("late fanout parent arrival");
    let Introduced(parent) = applied.outcome else { panic!("expected late parent introduction") };
    while verification.has_pending_relationships() {
        let _ = verification.resolve_relationships(64).expect("resolve fanout children");
    }
    assert_eq!(verification.object_count(), OBJECT_COUNT);
    assert_eq!(verification.children(parent).count(), CHILD_COUNT);

    bencher.with_inputs(deep_parent_fanout_scene).bench_local_refs(|(world, region)| {
        let applied = world.observe_object(late_fanout_parent(*region)).expect("late fanout parent arrival");
        let Introduced(_) = applied.outcome else { panic!("expected late parent introduction") };
        let _ = black_box(applied);
    });
}

#[divan::bench(args = SCENE_CASES)]
fn relations_deep_reparent(bencher: divan::Bencher, size: SceneSize) {
    let count = size.0;
    let mut bounds = limits();
    bounds.max_entities_per_region = count;
    bencher
        .with_inputs(|| {
            let mut world = World::new(bounds);
            let region = world.register_region().unwrap();
            for local in 1..=count as u32 {
                let parent = if local == 1 { Root } else { Local(NonZeroU32::new(local - 1).unwrap()) };
                let _ = world
                    .observe_object(ObjectObservation::new(
                        source(region, u64::from(local)),
                        Uuid::from_u128(u128::from(local)),
                        NonZeroU32::new(local).unwrap(),
                        Object,
                        parent,
                    ))
                    .expect("deep child");
            }
            (world, region, count as u64 + 1, false)
        })
        .bench_local_refs(|(world, region, order, alternate)| {
            let parent = count as u32 - if *alternate { 1 } else { 2 };
            *alternate = !*alternate;
            let mut patch = ObjectPatch::new(source(*region, *order), NonZeroU32::new(count as u32).unwrap());
            *order += 1;
            patch.parent = Some(Local(NonZeroU32::new(parent).unwrap()));
            let _ = black_box(world.patch_object(patch).expect("deep reparent"));
        });
}
