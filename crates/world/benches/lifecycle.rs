use std::{assert_matches, fmt, hint::black_box};

use divan::counter::ItemsCount;
use math::{Quat, Vec3};
use uuid::Uuid;
use world::{
    ApplyOutcome::{AgentHandedOff, Introduced, Removed},
    Kind, MAX_REMOVAL_WORK_PER_TURN, ObjectObservation, ObjectPatch,
    ParentUpdate::{Local, Root},
    Pose, World,
};

use super::support::{
    INITIAL_ORDER, PER_REGION, Row, Scene, empty_scene, limits, observation, populated_scene, rows_for_regions, size_name, source,
};

#[derive(Clone, Copy)]
struct SceneSize(usize);

impl fmt::Debug for SceneSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(size_name(self.0))
    }
}

const SCENE_CASES: &[SceneSize] = &[SceneSize(1_000), SceneSize(10_000), SceneSize(65_536)];

fn linked_scene(count: usize, linkset_size: usize) -> Scene {
    let mut world_limits = limits();
    world_limits.max_entities_per_region = count;
    let mut world = World::new(world_limits);
    let region = world.register_region().expect("register linked region");
    let regions = vec![region];
    let rows = rows_for_regions(count, 0, &regions, count);
    for row in &rows {
        let applied = world.observe_object(observation(*row)).expect("introduce linked object");
        assert_matches!(applied.outcome, Introduced(_));
    }
    let mut scene = Scene { world, rows, regions };
    for (index, row) in scene.rows.iter().enumerate() {
        if index % linkset_size == 0 {
            continue;
        }
        let root = scene.rows[index / linkset_size * linkset_size];
        let mut patch = ObjectPatch::new(source(row.region, INITIAL_ORDER + count as u64 + u64::from(row.local_id.get())), row.local_id);
        patch.parent = Some(Local(root.local_id));
        patch.pose = Some(Pose { position: Some(Vec3::ZERO), rotation: Some(Quat::IDENTITY) });
        let _ = scene.world.patch_object(patch).expect("link child");
    }
    scene
}

fn deep_linked_scene(count: usize, depth: usize) -> Scene {
    assert!(depth > 1 && depth <= count);
    let mut scene = populated_scene(count, false, false);
    for index in 1..depth {
        let row = scene.rows[index];
        let parent = scene.rows[index - 1];
        let patch = ObjectPatch {
            parent: Some(Local(parent.local_id)),
            ..ObjectPatch::new(source(row.region, INITIAL_ORDER + count as u64 + row.order), row.local_id)
        };
        let _ = scene.world.patch_object(patch).expect("link deep child");
    }
    scene
}

fn drain_removals(world: &mut World) -> (usize, usize) {
    let mut total_work = 0;
    let mut maximum_work = 0;
    while world.has_pending_removals() {
        let progress = world.reclaim_removals(MAX_REMOVAL_WORK_PER_TURN);
        assert!(progress.work_units <= MAX_REMOVAL_WORK_PER_TURN);
        assert!(progress.work_units > 0 || !progress.pending);
        total_work += progress.work_units;
        maximum_work = maximum_work.max(progress.work_units);
        drop(progress);
    }
    (total_work, maximum_work)
}

#[divan::bench(args = SCENE_CASES)]
fn lifecycle_spawn(bencher: divan::Bencher, size: SceneSize) {
    let count = size.0;
    let mut verification = empty_scene(count, false);
    for row in &verification.rows {
        assert_matches!(verification.world.observe_object(observation(*row)).expect("object introduction").outcome, Introduced(_));
    }
    assert_eq!(verification.world.object_count(), count);

    bencher.counter(ItemsCount::new(count as u64)).with_inputs(|| empty_scene(count, false)).bench_local_values(|mut scene| {
        for row in &scene.rows {
            let _ = black_box(scene.world.observe_object(observation(*row)).expect("object introduction"));
        }
        black_box(scene.world.object_count());
    });
}

#[divan::bench(args = SCENE_CASES)]
fn lifecycle_delete(bencher: divan::Bencher, size: SceneSize) {
    let count = size.0;
    let verification = populated_scene(count, false, false);
    let mut verification_world = verification.world;
    for row in &verification.rows {
        assert_eq!(
            verification_world
                .remove_object(source(row.region, INITIAL_ORDER + PER_REGION as u64 + u64::from(row.local_id.get())), row.local_id)
                .expect("object removal")
                .outcome,
            Removed(row.id)
        );
    }
    let _ = drain_removals(&mut verification_world);
    assert_eq!(verification_world.object_count(), 0);

    bencher.counter(ItemsCount::new(count as u64)).with_inputs(|| populated_scene(count, false, false)).bench_local_values(|mut scene| {
        for row in &scene.rows {
            let _ = black_box(
                scene
                    .world
                    .remove_object(source(row.region, INITIAL_ORDER + PER_REGION as u64 + u64::from(row.local_id.get())), row.local_id)
                    .expect("object removal"),
            );
        }
        let _ = black_box(drain_removals(&mut scene.world));
        black_box(scene.world.object_count());
    });
}

#[divan::bench(args = SCENE_CASES)]
fn lifecycle_local_id_reuse(bencher: divan::Bencher, size: SceneSize) {
    let count = size.0;
    bencher
        .counter(ItemsCount::new(2_u64))
        .with_inputs(|| {
            let scene = populated_scene(count, false, false);
            let target = scene.rows[count / 2];
            let order = INITIAL_ORDER + 2 * PER_REGION as u64;
            (scene, target, order, count as u128 + 1)
        })
        .bench_local_refs(|(scene, target, order, next_id)| {
            let removed = scene.world.remove_object(source(target.region, *order), target.local_id).expect("object removal");
            assert_eq!(removed.outcome, Removed(target.id));
            *order += 1;
            let id = Uuid::from_u128(*next_id);
            *next_id += 1;
            let row = Row { id, order: *order, ..*target };
            let introduced = scene.world.observe_object(observation(row)).expect("local ID reintroduction");
            let Introduced(entity) = introduced.outcome else { panic!("expected local ID reintroduction") };
            assert_eq!(scene.world.entity(target.id), None);
            assert_eq!(scene.world.entity(id), Some(entity));
            assert_eq!(scene.world.object(entity).unwrap().address().local_id(), target.local_id);
            *order += 1;
            let _ = black_box(drain_removals(&mut scene.world));
        });
}

#[divan::bench(args = SCENE_CASES)]
fn lifecycle_region_reset(bencher: divan::Bencher, size: SceneSize) {
    let count = size.0;
    bencher.counter(ItemsCount::new(count as u64)).with_inputs(|| populated_scene(count, false, false)).bench_local_values(|mut scene| {
        let mut removed = 0;
        for region in &scene.regions {
            let applied = scene.world.retire_region(*region).expect("region retirement");
            let world::RegionOutcome::Retired { entities_removed, .. } = &applied.outcome else { panic!("expected region retirement") };
            removed += *entities_removed;
            let _ = black_box(applied);
        }
        assert_eq!(removed, count);
        let _ = black_box(drain_removals(&mut scene.world));
        black_box(scene.world.object_count());
    });
}

#[divan::bench(args = SCENE_CASES)]
fn lifecycle_agent_handoff(bencher: divan::Bencher, size: SceneSize) {
    let count = size.0;
    bencher
        .counter(ItemsCount::new(1_u64))
        .with_inputs(|| {
            let scene = populated_scene(count, true, true);
            let agent = scene.rows[0];
            let destination = *scene.regions.last().expect("destination region");
            (scene, agent, destination, true, INITIAL_ORDER + PER_REGION as u64 + 1)
        })
        .bench_local_refs(|(scene, agent, destination, in_source, source_order)| {
            let (from, to) = if *in_source { (agent.region, *destination) } else { (*destination, agent.region) };
            let applied = scene
                .world
                .handoff_agent(from, ObjectObservation::new(source(to, *source_order), agent.id, agent.local_id, Kind::Avatar, Root))
                .expect("agent handoff");
            assert_matches!(&applied.outcome, AgentHandedOff(_));
            *in_source = !*in_source;
            *source_order += 1;
            let _ = black_box(applied);
            let _ = black_box(drain_removals(&mut scene.world));
        });
}

#[divan::bench]
fn lifecycle_delete_linkset_10k(bencher: divan::Bencher) {
    run_linked_removal(bencher, 10_000, 10_000, false);
}

#[divan::bench]
fn lifecycle_retire_linkset_10k(bencher: divan::Bencher) {
    run_linked_removal(bencher, 10_000, 10_000, true);
}

#[divan::bench]
fn lifecycle_delete_linkset_65k(bencher: divan::Bencher) {
    run_linked_removal(bencher, 65_536, 65_536, false);
}

#[divan::bench]
fn lifecycle_retire_linkset_65k(bencher: divan::Bencher) {
    run_linked_removal(bencher, 65_536, 65_536, true);
}

#[divan::bench]
fn lifecycle_retire_linksets_65k(bencher: divan::Bencher) {
    run_linked_removal(bencher, 65_536, 128, true);
}

#[divan::bench]
fn lifecycle_delete_deep_chain_65k_depth256(bencher: divan::Bencher) {
    const PROFILE_COUNT: usize = 65_536;
    const DEEP_CHAIN: usize = 4_096;
    bencher.counter(ItemsCount::new(DEEP_CHAIN as u64)).with_inputs(|| deep_linked_scene(PROFILE_COUNT, DEEP_CHAIN)).bench_local_values(
        |mut scene| {
            let root = scene.rows[0];
            let _ = black_box(
                scene
                    .world
                    .remove_object(source(root.region, INITIAL_ORDER + 3 * scene.rows.len() as u64), root.local_id)
                    .expect("remove deep chain root"),
            );
            let _ = black_box(drain_removals(&mut scene.world));
            assert_eq!(scene.world.object_count(), PROFILE_COUNT - DEEP_CHAIN);
        },
    );
}

fn run_linked_removal(bencher: divan::Bencher, count: usize, linkset_size: usize, retire: bool) {
    bencher.counter(ItemsCount::new(count as u64)).with_inputs(|| linked_scene(count, linkset_size)).bench_local_values(|mut scene| {
        let root = scene.rows[0];
        if retire {
            let _ = black_box(scene.world.retire_region(root.region).expect("retire linked region"));
        } else {
            let _ = black_box(
                scene
                    .world
                    .remove_object(source(root.region, INITIAL_ORDER + 3 * count as u64), root.local_id)
                    .expect("remove linked root"),
            );
        }
        let _ = black_box(drain_removals(&mut scene.world));
        assert_eq!(scene.world.object_count(), 0);
    });
}
