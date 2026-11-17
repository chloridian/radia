#![allow(unexpected_cfgs)]

use std::{hint::black_box, time::Instant};

use criterion::{BatchSize, BenchmarkGroup, BenchmarkId, Criterion, Throughput, measurement::WallTime};
use math::{Quat, Vec3};
use uuid::Uuid;
use world::{
    ApplyOutcome::{AgentHandedOff, Introduced, Removed},
    Kind, MAX_OBJECT_DETAIL_BATCH_ITEMS, MAX_OBJECT_PAGE_ITEMS, MAX_REMOVAL_WORK_PER_TURN, MAX_WORLD_CHANGE_PAGE_ITEMS,
    ObjectLookupProgress, ObjectObservation, ObjectPatch,
    ParentUpdate::{Local, Root},
    Pose, World, WorldChangeCursor,
};

#[cfg(not(world_bench_uninstrumented))]
#[global_allocator]
static ALLOCATOR: test_support::allocation::CountingAllocator = test_support::allocation::CountingAllocator;

use super::support::{
    INITIAL_ORDER, PER_REGION, Row, SCENE_SIZES, Scene, empty_scene, limits, observation, populated_scene, rows_for_regions, size_name,
    source,
};

pub(super) fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("world/lifecycle");
    bench_spawn(&mut group);
    bench_delete(&mut group);
    bench_local_id_reuse(&mut group);
    bench_region_reset(&mut group);
    bench_linked_removal(&mut group);
    bench_agent_handoff(&mut group);
    group.finish();
}

fn bench_spawn(group: &mut BenchmarkGroup<'_, WallTime>) {
    for &count in SCENE_SIZES {
        group.throughput(Throughput::Elements(count as u64));
        let mut verification = empty_scene(count, false);
        for row in &verification.rows {
            std::assert_matches!(verification.world.observe_object(observation(*row)).expect("object introduction").outcome, Introduced(_));
        }
        assert_eq!(verification.world.object_count(), count);

        group.bench_with_input(BenchmarkId::new("spawn", size_name(count)), &count, |b, &count| {
            b.iter_batched(
                || empty_scene(count, false),
                |mut scene| {
                    for row in &scene.rows {
                        let _ = black_box(scene.world.observe_object(observation(*row)).expect("object introduction"));
                    }
                    black_box(scene.world.object_count());
                },
                BatchSize::LargeInput,
            );
        });
    }
}

fn bench_delete(group: &mut BenchmarkGroup<'_, WallTime>) {
    for &count in SCENE_SIZES {
        group.throughput(Throughput::Elements(count as u64));
        let verification = populated_scene(count, false, false);
        let mut verification_world = verification.world;
        for row in &verification.rows {
            assert_eq!(
                verification_world
                    .remove_object(source(row.region, INITIAL_ORDER + PER_REGION as u64 + u64::from(row.local_id.get())), row.local_id,)
                    .expect("object removal")
                    .outcome,
                Removed(row.id)
            );
        }
        let _ = drain_removals(&mut verification_world);
        assert_eq!(verification_world.object_count(), 0);

        group.bench_with_input(BenchmarkId::new("delete", size_name(count)), &count, |b, &count| {
            b.iter_batched(
                || populated_scene(count, false, false),
                |mut scene| {
                    for row in &scene.rows {
                        let _ = black_box(
                            scene
                                .world
                                .remove_object(
                                    source(row.region, INITIAL_ORDER + PER_REGION as u64 + u64::from(row.local_id.get())),
                                    row.local_id,
                                )
                                .expect("object removal"),
                        );
                    }
                    let _ = black_box(drain_removals(&mut scene.world));
                    black_box(scene.world.object_count());
                },
                BatchSize::LargeInput,
            );
        });
    }
}

fn bench_local_id_reuse(group: &mut BenchmarkGroup<'_, WallTime>) {
    for &count in SCENE_SIZES {
        group.throughput(Throughput::Elements(2));
        let mut verification = populated_scene(count, false, false);
        let target = verification.rows[count / 2];
        let mut order = INITIAL_ORDER + 2 * PER_REGION as u64;
        let removed = verification.world.remove_object(source(target.region, order), target.local_id).expect("object removal");
        assert_eq!(removed.outcome, Removed(target.id));
        order += 1;
        let new_id = Uuid::from_u128(count as u128 + 1);
        let introduced =
            verification.world.observe_object(observation(Row { id: new_id, order, ..target })).expect("local ID reintroduction");
        let Introduced(entity) = introduced.outcome else { panic!("expected local ID reintroduction") };
        assert_eq!(verification.world.entity(target.id), None);
        assert_eq!(verification.world.entity(new_id), Some(entity));
        assert_eq!(verification.world.object(entity).unwrap().address().local_id(), target.local_id);
        assert_eq!(verification.world.object_count(), count);
        let _ = drain_removals(&mut verification.world);

        group.bench_with_input(BenchmarkId::new("local_id_reuse", size_name(count)), &count, |b, &count| {
            let mut scene = populated_scene(count, false, false);
            let target = scene.rows[count / 2];
            let mut order = INITIAL_ORDER + 2 * PER_REGION as u64;
            let mut next_id = count as u128 + 1;
            b.iter(|| {
                let _ = black_box(scene.world.remove_object(source(target.region, order), target.local_id).expect("object removal"));
                order += 1;
                let id = Uuid::from_u128(next_id);
                next_id += 1;
                let row = Row { id, order, ..target };
                let _ = black_box(scene.world.observe_object(observation(row)).expect("local ID reintroduction"));
                order += 1;
                let _ = black_box(drain_removals(&mut scene.world));
            });
        });
    }
}

fn bench_region_reset(group: &mut BenchmarkGroup<'_, criterion::measurement::WallTime>) {
    for &count in SCENE_SIZES {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::new("region_reset", size_name(count)), &count, |b, &count| {
            b.iter_batched(
                || populated_scene(count, false, false),
                |mut scene| {
                    let mut removed = 0;
                    for region in &scene.regions {
                        let applied = scene.world.retire_region(*region).expect("region retirement");
                        let world::RegionOutcome::Retired { entities_removed, .. } = &applied.outcome else {
                            panic!("expected region retirement")
                        };
                        removed += *entities_removed;
                        let _ = black_box(applied);
                    }
                    assert_eq!(removed, count);
                    let _ = black_box(drain_removals(&mut scene.world));
                    black_box(scene.world.object_count());
                },
                BatchSize::LargeInput,
            );
        });
    }
}

fn bench_agent_handoff(group: &mut BenchmarkGroup<'_, WallTime>) {
    group.throughput(Throughput::Elements(1));
    for &count in SCENE_SIZES {
        group.bench_with_input(BenchmarkId::new("agent_handoff", size_name(count)), &count, |b, &count| {
            let mut scene = populated_scene(count, true, true);
            let agent = scene.rows[0];
            let destination = *scene.regions.last().expect("destination region");
            let mut in_source = true;
            let mut source_order = INITIAL_ORDER + PER_REGION as u64 + 1;
            b.iter(|| {
                let (from, to) = if in_source { (agent.region, destination) } else { (destination, agent.region) };
                let applied = scene
                    .world
                    .handoff_agent(from, ObjectObservation::new(source(to, source_order), agent.id, agent.local_id, Kind::Avatar, Root))
                    .expect("agent handoff");
                std::assert_matches!(&applied.outcome, AgentHandedOff(_));
                in_source = !in_source;
                source_order += 1;
                let _ = black_box(applied);
                let _ = black_box(drain_removals(&mut scene.world));
            });
        });
    }
}

fn linked_scene(count: usize, linkset_size: usize) -> Scene {
    let mut world_limits = limits();
    world_limits.max_entities_per_region = count;
    let mut world = World::new(world_limits);
    let region = world.register_region().expect("register linked region");
    let regions = vec![region];
    let rows = rows_for_regions(count, 0, &regions, count);
    for row in &rows {
        let applied = world.observe_object(observation(*row)).expect("introduce linked object");
        std::assert_matches!(applied.outcome, Introduced(_));
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

#[cfg(not(world_bench_uninstrumented))]
fn drain_changes(world: &World, cursor: &mut WorldChangeCursor) -> (usize, std::time::Duration) {
    let started = Instant::now();
    let mut count = 0;
    loop {
        let page = world.change_page(*cursor, MAX_WORLD_CHANGE_PAGE_ITEMS).expect("profile change page");
        count += page.records.len();
        *cursor = page.cursor;
        let has_more = page.has_more;
        drop(page);
        if !has_more {
            break;
        }
    }
    (count, started.elapsed())
}

#[cfg(not(world_bench_uninstrumented))]
struct RemovalProfile {
    trigger: std::time::Duration,
    owner_point_lookup: std::time::Duration,
    owner_point_lookup_turns: usize,
    owner_point_lookup_work_units: usize,
    owner_point_lookup_max_turn: std::time::Duration,
    trigger_live_delta: i128,
    post_trigger_read: std::time::Duration,
    post_trigger_read_live_delta: i128,
    initial_publication_read: std::time::Duration,
    turn_publication_read: std::time::Duration,
    cleanup: std::time::Duration,
    total: std::time::Duration,
    turns: usize,
    maximum_turn: std::time::Duration,
    maximum_turn_index: usize,
    final_turn: std::time::Duration,
    removal_work: usize,
    maximum_removal_work: usize,
    object_pages: usize,
    empty_object_pages: usize,
    visible_objects: usize,
    object_read_work: usize,
    maximum_object_page_work: usize,
    maximum_object_page: std::time::Duration,
    detail_batches: usize,
    detail_read_work: usize,
    maximum_detail_page_work: usize,
    maximum_detail_page: std::time::Duration,
    published_changes: usize,
}

#[cfg(not(world_bench_uninstrumented))]
fn report_removal_profile(name: &str, mut scene: Scene, retire: bool, expected_remaining: usize, lookup_id: Uuid) {
    let mut turn_times = Vec::with_capacity(16_384);
    let live_before = test_support::allocation::live_bytes();
    let (profile, allocations) = test_support::allocation::measure(|| {
        let mut change_cursor = scene.world.read_barrier().changes;
        let total_started = Instant::now();
        let trigger_started = Instant::now();
        if retire {
            let _ = scene.world.retire_region(scene.regions[0]).expect("retire profile region");
        } else {
            let row = scene.rows[0];
            let _ = scene
                .world
                .remove_object(source(row.region, INITIAL_ORDER + 3 * scene.rows.len() as u64), row.local_id)
                .expect("remove profile root");
        }
        let trigger = trigger_started.elapsed();
        let trigger_live_delta = test_support::allocation::live_bytes() - live_before;
        let owner_lookup_started = Instant::now();
        let mut lookup_cursor = None;
        let mut owner_point_lookup_turns = 0;
        let mut owner_point_lookup_work_units = 0;
        let mut owner_point_lookup_max_turn = std::time::Duration::ZERO;
        loop {
            owner_point_lookup_turns += 1;
            let turn_started = Instant::now();
            let progress = scene.world.lookup_entity(lookup_id, lookup_cursor);
            let turn = turn_started.elapsed();
            owner_point_lookup_max_turn = owner_point_lookup_max_turn.max(turn);
            match progress {
                ObjectLookupProgress::Visible { work_units, .. } | ObjectLookupProgress::Hidden { work_units } => {
                    owner_point_lookup_work_units += work_units;
                    break;
                }
                ObjectLookupProgress::Pending { cursor, work_units } => {
                    owner_point_lookup_work_units += work_units;
                    lookup_cursor = Some(cursor);
                }
            }
        }
        let owner_point_lookup = owner_lookup_started.elapsed();
        let read_started = Instant::now();
        let barrier = scene.world.read_barrier();
        let mut object_cursor = None;
        let mut object_pages = 0;
        let mut empty_object_pages = 0;
        let mut visible_objects = 0;
        let mut object_read_work = 0;
        let mut maximum_object_page_work = 0;
        let mut maximum_read_page = std::time::Duration::ZERO;
        let mut object_entities = Vec::new();
        loop {
            let page_started = Instant::now();
            let page = scene.world.object_list_page_with_cache(object_cursor, MAX_OBJECT_PAGE_ITEMS).expect("profile object page");
            object_pages += 1;
            visible_objects += page.objects.len();
            object_read_work += page.work_units;
            maximum_object_page_work = maximum_object_page_work.max(page.work_units);
            object_entities.extend(page.objects.iter().map(|object| object.entity));
            if page.objects.is_empty() {
                empty_object_pages += 1;
            }
            object_cursor = page.next;
            let has_more = object_cursor.is_some();
            drop(page);
            maximum_read_page = maximum_read_page.max(page_started.elapsed());
            if !has_more {
                break;
            }
        }
        let mut detail_batches = 0;
        let mut detail_read_work = 0;
        let mut maximum_detail_page_work = 0;
        let mut maximum_detail_page = std::time::Duration::ZERO;
        for entities in object_entities.chunks(MAX_OBJECT_DETAIL_BATCH_ITEMS) {
            let mut detail_cursor = None;
            loop {
                let page_started = Instant::now();
                let page = scene.world.object_detail_batch_page(entities, detail_cursor).expect("profile detail page");
                detail_batches += 1;
                detail_read_work += page.work_units;
                maximum_detail_page_work = maximum_detail_page_work.max(page.work_units);
                detail_cursor = page.next;
                let has_more = detail_cursor.is_some();
                drop(page);
                maximum_detail_page = maximum_detail_page.max(page_started.elapsed());
                if !has_more {
                    break;
                }
            }
        }
        assert!(barrier.object_count_upper_bound >= visible_objects);
        let post_trigger_read = read_started.elapsed();
        let post_trigger_read_live_delta = test_support::allocation::live_bytes() - live_before;
        drop(object_entities);
        let (mut published_changes, initial_publication_read) = drain_changes(&scene.world, &mut change_cursor);
        let cleanup_started = Instant::now();
        let mut maximum_turn = std::time::Duration::ZERO;
        let mut maximum_turn_index = 0;
        let mut turn_index = 0;
        let mut final_turn = std::time::Duration::ZERO;
        let mut removal_work = 0;
        let mut maximum_removal_work = 0;
        let mut turn_publication_read = std::time::Duration::ZERO;
        while scene.world.has_pending_removals() {
            turn_index += 1;
            let started = Instant::now();
            let progress = scene.world.reclaim_removals(MAX_REMOVAL_WORK_PER_TURN);
            let pending = progress.pending;
            assert!(progress.work_units <= MAX_REMOVAL_WORK_PER_TURN);
            assert!(progress.work_units > 0 || !pending);
            removal_work += progress.work_units;
            maximum_removal_work = maximum_removal_work.max(progress.work_units);
            drop(progress);
            let _barrier = scene.world.read_barrier();
            let (new_changes, publication_read) = drain_changes(&scene.world, &mut change_cursor);
            turn_publication_read += publication_read;
            published_changes += new_changes;
            let turn = started.elapsed();
            if turn > maximum_turn {
                maximum_turn = turn;
                maximum_turn_index = turn_index;
            }
            turn_times.push(turn);
            if !pending {
                final_turn = turn;
            }
        }
        let cleanup = cleanup_started.elapsed();
        let total = total_started.elapsed();
        RemovalProfile {
            trigger,
            owner_point_lookup,
            owner_point_lookup_turns,
            owner_point_lookup_work_units,
            owner_point_lookup_max_turn,
            trigger_live_delta,
            post_trigger_read,
            post_trigger_read_live_delta,
            initial_publication_read,
            turn_publication_read,
            cleanup,
            total,
            turns: turn_index,
            maximum_turn,
            maximum_turn_index,
            final_turn,
            removal_work,
            maximum_removal_work,
            object_pages,
            empty_object_pages,
            visible_objects,
            object_read_work,
            maximum_object_page_work,
            maximum_object_page: maximum_read_page,
            detail_batches,
            detail_read_work,
            maximum_detail_page_work,
            maximum_detail_page,
            published_changes,
        }
    });
    assert_eq!(scene.world.object_count(), expected_remaining);
    let retained_delta = test_support::allocation::live_bytes() - live_before;
    turn_times.sort_unstable();
    let p99_turn = turn_times[(turn_times.len() - 1) * 99 / 100];
    let p999_turn = turn_times[(turn_times.len() - 1) * 999 / 1000];
    eprintln!(
        "removal-profile={name} entities={} trigger_us={} trigger_live_delta_bytes={} owner_point_lookup_us={} owner_point_lookup_turns={} owner_point_lookup_work_units={} owner_point_lookup_max_turn_us={} post_trigger_read_us={} post_trigger_read_live_delta_bytes={} initial_publication_read_us={} turn_publication_read_us={} cleanup_us={} total_us={} object_pages={} empty_object_pages={} visible_objects={} object_read_work_units={} max_object_page_work_units={} max_object_page_us={} detail_batches={} detail_read_work_units={} max_detail_page_work_units={} max_detail_page_us={} turns={} max_turn_us={} max_turn_index={} p99_turn_us={} p999_turn_us={} final_turn_us={} removal_work_units={} max_removal_work_units={} counted_total_work_units={} published_changes={} peak_thread_live_bytes={} retained_thread_live_delta_bytes={} alloc_calls={} realloc_calls={} final_buffer_release_in_final_turn=true",
        scene.rows.len(),
        profile.trigger.as_micros(),
        profile.trigger_live_delta,
        profile.owner_point_lookup.as_micros(),
        profile.owner_point_lookup_turns,
        profile.owner_point_lookup_work_units,
        profile.owner_point_lookup_max_turn.as_micros(),
        profile.post_trigger_read.as_micros(),
        profile.post_trigger_read_live_delta,
        profile.initial_publication_read.as_micros(),
        profile.turn_publication_read.as_micros(),
        profile.cleanup.as_micros(),
        profile.total.as_micros(),
        profile.object_pages,
        profile.empty_object_pages,
        profile.visible_objects,
        profile.object_read_work,
        profile.maximum_object_page_work,
        profile.maximum_object_page.as_micros(),
        profile.detail_batches,
        profile.detail_read_work,
        profile.maximum_detail_page_work,
        profile.maximum_detail_page.as_micros(),
        profile.turns,
        profile.maximum_turn.as_micros(),
        profile.maximum_turn_index,
        p99_turn.as_micros(),
        p999_turn.as_micros(),
        profile.final_turn.as_micros(),
        profile.removal_work,
        profile.maximum_removal_work,
        profile.removal_work + profile.object_read_work + profile.detail_read_work,
        profile.published_changes,
        live_before + allocations.peak_bytes,
        retained_delta,
        allocations.allocation_calls,
        allocations.reallocation_calls,
    );
}

#[cfg(world_bench_uninstrumented)]
#[derive(Clone, Copy, Default)]
struct PhaseTime {
    wall: std::time::Duration,
    cycles: Option<u64>,
}

#[cfg(all(world_bench_uninstrumented, world_bench_detailed))]
const LATENCY_PROFILE_MODE: &str = "allocator-free-phase-instrumented";
#[cfg(all(world_bench_uninstrumented, not(world_bench_detailed)))]
const LATENCY_PROFILE_MODE: &str = "allocator-free";

#[cfg(world_bench_uninstrumented)]
#[derive(Clone, Copy, Default)]
struct LatencySample {
    trigger: PhaseTime,
    lookup: PhaseTime,
    lookup_turns: usize,
    lookup_max_turn: PhaseTime,
    barrier: PhaseTime,
    list_read: PhaseTime,
    detail_read: PhaseTime,
    initial_journal: PhaseTime,
    cleanup: PhaseTime,
    reclaim_calls: PhaseTime,
    turn_barriers: PhaseTime,
    turn_journal_reads: PhaseTime,
    recovery_effects: PhaseTime,
    max_reclaim_turn: PhaseTime,
    final_reclaim_turn: PhaseTime,
    total: PhaseTime,
    turns: usize,
    removal_work: usize,
    removed_entities: usize,
    maximum_removal_work: usize,
    object_pages: usize,
    object_read_work: usize,
    detail_batches: usize,
    detail_read_work: usize,
    journal_pages: usize,
    recovery_requests: usize,
    recovery_cancellations: usize,
    published_changes: usize,
    #[cfg(world_bench_detailed)]
    reclamation_work_time: std::time::Duration,
    #[cfg(world_bench_detailed)]
    finalization_time: std::time::Duration,
    #[cfg(world_bench_detailed)]
    entity_children_time: std::time::Duration,
    #[cfg(world_bench_detailed)]
    entity_relationship_time: std::time::Duration,
    #[cfg(world_bench_detailed)]
    region_objects_time: std::time::Duration,
    #[cfg(world_bench_detailed)]
    region_state_time: std::time::Duration,
    #[cfg(world_bench_detailed)]
    region_tombstones_time: std::time::Duration,
    #[cfg(world_bench_detailed)]
    transfer_time: std::time::Duration,
    #[cfg(world_bench_detailed)]
    index_removal_time: std::time::Duration,
    #[cfg(world_bench_detailed)]
    component_destruction_time: std::time::Duration,
    #[cfg(world_bench_detailed)]
    tombstone_insertion_time: std::time::Duration,
    #[cfg(world_bench_detailed)]
    journal_publication_time: std::time::Duration,
    #[cfg(world_bench_detailed)]
    recovery_compaction_time: std::time::Duration,
    #[cfg(world_bench_detailed)]
    final_buffer_release_time: std::time::Duration,
}

#[cfg(world_bench_uninstrumented)]
fn report_removal_latency(
    name: &str,
    repetition: usize,
    mut scene: Scene,
    retire: bool,
    expected_remaining: usize,
    lookup_id: Uuid,
) -> LatencySample {
    let mut sample = LatencySample::default();
    let total_started = Instant::now();
    let total_cycles = thread_cycles();
    let (initial_barrier, initial_barrier_time) = timed(|| scene.world.read_barrier());
    sample.barrier = initial_barrier_time;
    let mut change_cursor = initial_barrier.changes;
    let ((), trigger) = timed(|| {
        if retire {
            let _ = scene.world.retire_region(scene.regions[0]).expect("retire latency profile region");
        } else {
            let row = scene.rows[0];
            let _ = scene
                .world
                .remove_object(source(row.region, INITIAL_ORDER + 3 * scene.rows.len() as u64), row.local_id)
                .expect("remove latency profile root");
        }
    });
    sample.trigger = trigger;

    let lookup_started = Instant::now();
    let lookup_cycles = thread_cycles();
    let mut lookup_cursor = None;
    loop {
        sample.lookup_turns += 1;
        let (progress, turn) = timed(|| scene.world.lookup_entity(lookup_id, lookup_cursor));
        sample.lookup_max_turn = max_time(sample.lookup_max_turn, turn);
        match progress {
            ObjectLookupProgress::Visible { work_units: _, .. } | ObjectLookupProgress::Hidden { work_units: _ } => break,
            ObjectLookupProgress::Pending { cursor, work_units: _ } => lookup_cursor = Some(cursor),
        }
    }
    sample.lookup = PhaseTime { wall: lookup_started.elapsed(), cycles: cycle_delta(lookup_cycles, thread_cycles()) };

    let (barrier, barrier_time) = timed(|| scene.world.read_barrier());
    sample.barrier.wall += barrier_time.wall;
    sample.barrier.cycles = sum_cycles(sample.barrier.cycles, barrier_time.cycles);
    let list_started = Instant::now();
    let list_cycles = thread_cycles();
    let mut object_cursor = None;
    let mut object_entities = Vec::new();
    loop {
        let page = scene.world.object_list_page_with_cache(object_cursor, MAX_OBJECT_PAGE_ITEMS).expect("latency profile object page");
        sample.object_pages += 1;
        sample.object_read_work += page.work_units;
        object_entities.extend(page.objects.iter().map(|object| object.entity));
        object_cursor = page.next;
        if object_cursor.is_none() {
            break;
        }
    }
    sample.list_read = PhaseTime { wall: list_started.elapsed(), cycles: cycle_delta(list_cycles, thread_cycles()) };

    let detail_started = Instant::now();
    let detail_cycles = thread_cycles();
    for entities in object_entities.chunks(MAX_OBJECT_DETAIL_BATCH_ITEMS) {
        sample.detail_batches += 1;
        let mut detail_cursor = None;
        loop {
            let page = scene.world.object_detail_batch_page(entities, detail_cursor).expect("latency profile detail page");
            black_box(page.batch.details.len());
            sample.detail_read_work += page.work_units;
            detail_cursor = page.next;
            if detail_cursor.is_none() {
                break;
            }
        }
    }
    sample.detail_read = PhaseTime { wall: detail_started.elapsed(), cycles: cycle_delta(detail_cycles, thread_cycles()) };
    drop(object_entities);

    let (published_changes, initial_journal, journal_pages) = drain_changes_profile(&scene.world, &mut change_cursor);
    sample.initial_journal = initial_journal;
    sample.journal_pages += journal_pages;
    sample.published_changes += published_changes;

    let cleanup_started = Instant::now();
    let cleanup_cycles = thread_cycles();
    while scene.world.has_pending_removals() {
        sample.turns += 1;
        let (progress, reclaim_time) = timed(|| scene.world.reclaim_removals(MAX_REMOVAL_WORK_PER_TURN));
        sample.reclaim_calls.wall += reclaim_time.wall;
        sample.reclaim_calls.cycles = sum_cycles(sample.reclaim_calls.cycles, reclaim_time.cycles);
        sample.max_reclaim_turn = max_time(sample.max_reclaim_turn, reclaim_time);
        if !progress.pending {
            sample.final_reclaim_turn = reclaim_time;
        }
        sample.removal_work += progress.work_units;
        sample.maximum_removal_work = sample.maximum_removal_work.max(progress.work_units);
        sample.removed_entities += progress.removed_entities;
        #[cfg(world_bench_detailed)]
        {
            sample.reclamation_work_time += progress.reclamation_work_time;
            sample.finalization_time += progress.finalization_time;
            sample.entity_children_time += progress.entity_children_time;
            sample.entity_relationship_time += progress.entity_relationship_time;
            sample.region_objects_time += progress.region_objects_time;
            sample.region_state_time += progress.region_state_time;
            sample.region_tombstones_time += progress.region_tombstones_time;
            sample.transfer_time += progress.transfer_time;
            sample.index_removal_time += progress.index_removal_time;
            sample.component_destruction_time += progress.component_destruction_time;
            sample.tombstone_insertion_time += progress.tombstone_insertion_time;
            sample.journal_publication_time += progress.journal_publication_time;
            sample.recovery_compaction_time += progress.recovery_compaction_time;
            sample.final_buffer_release_time += progress.final_buffer_release_time;
        }
        let ((requests, cancellations), recovery_time) =
            timed(|| (progress.recovery.requests.len(), progress.recovery_cancellations.len()));
        sample.recovery_requests += requests;
        sample.recovery_cancellations += cancellations;
        sample.recovery_effects.wall += recovery_time.wall;
        sample.recovery_effects.cycles = sum_cycles(sample.recovery_effects.cycles, recovery_time.cycles);
        let ((), recovery_time) = timed(|| drop(progress));
        sample.recovery_effects.wall += recovery_time.wall;
        sample.recovery_effects.cycles = sum_cycles(sample.recovery_effects.cycles, recovery_time.cycles);

        let (_, turn_barrier) = timed(|| scene.world.read_barrier());
        sample.turn_barriers.wall += turn_barrier.wall;
        sample.turn_barriers.cycles = sum_cycles(sample.turn_barriers.cycles, turn_barrier.cycles);
        let (changes, journal_read, pages) = drain_changes_profile(&scene.world, &mut change_cursor);
        sample.turn_journal_reads.wall += journal_read.wall;
        sample.turn_journal_reads.cycles = sum_cycles(sample.turn_journal_reads.cycles, journal_read.cycles);
        sample.journal_pages += pages;
        sample.published_changes += changes;
    }
    sample.cleanup = PhaseTime { wall: cleanup_started.elapsed(), cycles: cycle_delta(cleanup_cycles, thread_cycles()) };
    sample.total = PhaseTime { wall: total_started.elapsed(), cycles: cycle_delta(total_cycles, thread_cycles()) };
    assert_eq!(scene.world.object_count(), expected_remaining);
    assert!(barrier.object_count_upper_bound >= expected_remaining);
    eprintln!(
        "latency-profile={name} repetition={repetition} mode={LATENCY_PROFILE_MODE} entities={} trigger_us={} trigger_cycles={} lookup_us={} lookup_cycles={} lookup_turns={} lookup_max_turn_us={} barrier_us={} barrier_cycles={} list_read_us={} list_read_cycles={} object_pages={} object_read_work_units={} detail_read_us={} detail_read_cycles={} detail_batches={} detail_read_work_units={} initial_journal_us={} initial_journal_cycles={} cleanup_us={} cleanup_cycles={} reclaim_calls_us={} reclaim_cycles={} turn_barriers_us={} turn_journal_reads_us={} recovery_effects_us={} recovery_requests={} recovery_cancellations={} turns={} max_reclaim_turn_us={} max_reclaim_turn_cycles={} final_reclaim_turn_us={} removal_work_units={} maximum_removal_work_units={} removed_entities={} published_changes={} journal_pages={} total_us={} total_cycles={}",
        scene.rows.len(),
        sample.trigger.wall.as_micros(),
        sample.trigger.cycles.unwrap_or_default(),
        sample.lookup.wall.as_micros(),
        sample.lookup.cycles.unwrap_or_default(),
        sample.lookup_turns,
        sample.lookup_max_turn.wall.as_micros(),
        sample.barrier.wall.as_micros(),
        sample.barrier.cycles.unwrap_or_default(),
        sample.list_read.wall.as_micros(),
        sample.list_read.cycles.unwrap_or_default(),
        sample.object_pages,
        sample.object_read_work,
        sample.detail_read.wall.as_micros(),
        sample.detail_read.cycles.unwrap_or_default(),
        sample.detail_batches,
        sample.detail_read_work,
        sample.initial_journal.wall.as_micros(),
        sample.initial_journal.cycles.unwrap_or_default(),
        sample.cleanup.wall.as_micros(),
        sample.cleanup.cycles.unwrap_or_default(),
        sample.reclaim_calls.wall.as_micros(),
        sample.reclaim_calls.cycles.unwrap_or_default(),
        sample.turn_barriers.wall.as_micros(),
        sample.turn_journal_reads.wall.as_micros(),
        sample.recovery_effects.wall.as_micros(),
        sample.recovery_requests,
        sample.recovery_cancellations,
        sample.turns,
        sample.max_reclaim_turn.wall.as_micros(),
        sample.max_reclaim_turn.cycles.unwrap_or_default(),
        sample.final_reclaim_turn.wall.as_micros(),
        sample.removal_work,
        sample.maximum_removal_work,
        sample.removed_entities,
        sample.published_changes,
        sample.journal_pages,
        sample.total.wall.as_micros(),
        sample.total.cycles.unwrap_or_default(),
    );
    #[cfg(world_bench_detailed)]
    eprintln!(
        "latency-detail={name} repetition={repetition} reclamation_work_us={} entity_children_us={} entity_relationship_work_us={} entity_finalize_us={} region_objects_us={} region_state_us={} region_tombstones_us={} transfer_work_us={} component_destruction_us={} index_removal_us={} tombstone_insertion_us={} journal_publication_us={} recovery_compaction_us={} final_buffer_release_us={}",
        sample.reclamation_work_time.as_micros(),
        sample.entity_children_time.as_micros(),
        sample.entity_relationship_time.as_micros(),
        sample.finalization_time.as_micros(),
        sample.region_objects_time.as_micros(),
        sample.region_state_time.as_micros(),
        sample.region_tombstones_time.as_micros(),
        sample.transfer_time.as_micros(),
        sample.component_destruction_time.as_micros(),
        sample.index_removal_time.as_micros(),
        sample.tombstone_insertion_time.as_micros(),
        sample.journal_publication_time.as_micros(),
        sample.recovery_compaction_time.as_micros(),
        sample.final_buffer_release_time.as_micros(),
    );
    sample
}

#[cfg(world_bench_uninstrumented)]
fn report_latency_distribution(name: &str, samples: &[LatencySample]) {
    let durations: [(&str, fn(&LatencySample) -> std::time::Duration); 15] = [
        ("trigger", |sample| sample.trigger.wall),
        ("lookup", |sample| sample.lookup.wall),
        ("lookup_max_turn", |sample| sample.lookup_max_turn.wall),
        ("barrier", |sample| sample.barrier.wall),
        ("list_read", |sample| sample.list_read.wall),
        ("detail_read", |sample| sample.detail_read.wall),
        ("initial_journal", |sample| sample.initial_journal.wall),
        ("cleanup", |sample| sample.cleanup.wall),
        ("reclaim_calls", |sample| sample.reclaim_calls.wall),
        ("turn_barriers", |sample| sample.turn_barriers.wall),
        ("turn_journal_reads", |sample| sample.turn_journal_reads.wall),
        ("recovery_effects", |sample| sample.recovery_effects.wall),
        ("max_reclaim_turn", |sample| sample.max_reclaim_turn.wall),
        ("final_reclaim_turn", |sample| sample.final_reclaim_turn.wall),
        ("total", |sample| sample.total.wall),
    ];
    for (phase, duration) in durations {
        let mut values = samples.iter().map(duration).collect::<Vec<_>>();
        values.sort_unstable();
        eprintln!(
            "latency-distribution={name} phase={phase} min_us={} median_us={} max_us={}",
            values[0].as_micros(),
            values[values.len() / 2].as_micros(),
            values[values.len() - 1].as_micros(),
        );
    }
    let cycles: [(&str, fn(&LatencySample) -> Option<u64>); 12] = [
        ("total", |sample| sample.total.cycles),
        ("lookup_max_turn", |sample| sample.lookup_max_turn.cycles),
        ("barrier", |sample| sample.barrier.cycles),
        ("list_read", |sample| sample.list_read.cycles),
        ("detail_read", |sample| sample.detail_read.cycles),
        ("initial_journal", |sample| sample.initial_journal.cycles),
        ("cleanup", |sample| sample.cleanup.cycles),
        ("reclaim_calls", |sample| sample.reclaim_calls.cycles),
        ("turn_barriers", |sample| sample.turn_barriers.cycles),
        ("turn_journal_reads", |sample| sample.turn_journal_reads.cycles),
        ("recovery_effects", |sample| sample.recovery_effects.cycles),
        ("max_reclaim_turn", |sample| sample.max_reclaim_turn.cycles),
    ];
    for (phase, cycle_count) in cycles {
        let mut values = samples.iter().filter_map(cycle_count).collect::<Vec<_>>();
        values.sort_unstable();
        if let (Some(min), Some(median), Some(max)) = (values.first(), values.get(values.len() / 2), values.last()) {
            eprintln!("latency-cycle-distribution={name} phase={phase} min={min} median={median} max={max}");
        } else {
            eprintln!("latency-cycle-distribution={name} phase={phase} unavailable");
        }
    }
}

#[cfg(world_bench_uninstrumented)]
#[derive(Clone, Copy)]
struct SystemCpuTimes {
    idle: u64,
    kernel: u64,
    user: u64,
}

#[cfg(all(world_bench_uninstrumented, windows))]
#[repr(C)]
struct SystemFileTime {
    low: u32,
    high: u32,
}

#[cfg(all(world_bench_uninstrumented, windows))]
fn system_cpu_times() -> Option<SystemCpuTimes> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetSystemTimes(idle: *mut SystemFileTime, kernel: *mut SystemFileTime, user: *mut SystemFileTime) -> i32;
    }

    let mut idle = SystemFileTime { low: 0, high: 0 };
    let mut kernel = SystemFileTime { low: 0, high: 0 };
    let mut user = SystemFileTime { low: 0, high: 0 };
    let read = unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) };
    (read != 0).then(|| SystemCpuTimes {
        idle: system_file_time_value(idle),
        kernel: system_file_time_value(kernel),
        user: system_file_time_value(user),
    })
}

#[cfg(all(world_bench_uninstrumented, windows))]
fn system_file_time_value(time: SystemFileTime) -> u64 {
    (u64::from(time.high) << 32) | u64::from(time.low)
}

#[cfg(all(world_bench_uninstrumented, not(windows)))]
fn system_cpu_times() -> Option<SystemCpuTimes> {
    None
}

#[cfg(world_bench_uninstrumented)]
fn system_busy_percentage(start: Option<SystemCpuTimes>, end: Option<SystemCpuTimes>) -> Option<f64> {
    let (start, end) = (start?, end?);
    let elapsed = end.kernel.saturating_sub(start.kernel).saturating_add(end.user.saturating_sub(start.user));
    let idle = end.idle.saturating_sub(start.idle);
    (elapsed != 0).then(|| elapsed.saturating_sub(idle) as f64 * 100.0 / elapsed as f64)
}

#[cfg(world_bench_uninstrumented)]
fn drain_changes_profile(world: &World, cursor: &mut WorldChangeCursor) -> (usize, PhaseTime, usize) {
    let started = Instant::now();
    let cycles = thread_cycles();
    let mut count = 0;
    let mut pages = 0;
    loop {
        let page = world.change_page(*cursor, MAX_WORLD_CHANGE_PAGE_ITEMS).expect("latency profile change page");
        count += page.records.len();
        *cursor = page.cursor;
        pages += 1;
        if !page.has_more {
            break;
        }
    }
    (count, PhaseTime { wall: started.elapsed(), cycles: cycle_delta(cycles, thread_cycles()) }, pages)
}

#[cfg(world_bench_uninstrumented)]
fn timed<T>(operation: impl FnOnce() -> T) -> (T, PhaseTime) {
    let cycles = thread_cycles();
    let started = Instant::now();
    let value = operation();
    (value, PhaseTime { wall: started.elapsed(), cycles: cycle_delta(cycles, thread_cycles()) })
}

#[cfg(world_bench_uninstrumented)]
fn max_time(previous: PhaseTime, next: PhaseTime) -> PhaseTime {
    if next.wall > previous.wall { next } else { previous }
}

#[cfg(world_bench_uninstrumented)]
fn sum_cycles(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        (Some(cycles), None) | (None, Some(cycles)) => Some(cycles),
        (None, None) => None,
    }
}

#[cfg(world_bench_uninstrumented)]
fn cycle_delta(start: Option<u64>, end: Option<u64>) -> Option<u64> {
    start.zip(end).map(|(start, end)| end.saturating_sub(start))
}

#[cfg(world_bench_uninstrumented)]
fn thread_cycles() -> Option<u64> {
    #[cfg(windows)]
    {
        use std::ffi::c_void;

        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThread() -> *mut c_void;
            fn QueryThreadCycleTime(thread: *mut c_void, cycles: *mut u64) -> i32;
        }

        let mut cycles = 0;
        let queried = unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut cycles) };
        (queried != 0).then_some(cycles)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

fn bench_linked_removal(group: &mut BenchmarkGroup<'_, WallTime>) {
    const PROFILE_COUNT: usize = 65_536;
    const DEEP_CHAIN: usize = 4_096;
    #[cfg(not(world_bench_uninstrumented))]
    {
        let wide_kill = linked_scene(PROFILE_COUNT, PROFILE_COUNT);
        report_removal_profile("wide_kill_65k", wide_kill, false, 0, Uuid::from_u128(2));
        let wide_retirement = linked_scene(PROFILE_COUNT, PROFILE_COUNT);
        report_removal_profile("wide_retire_65k", wide_retirement, true, 0, Uuid::from_u128(1));
        let deep_kill = deep_linked_scene(PROFILE_COUNT, DEEP_CHAIN);
        report_removal_profile(
            "deep_kill_65k_depth_4096",
            deep_kill,
            false,
            PROFILE_COUNT - DEEP_CHAIN,
            Uuid::from_u128(DEEP_CHAIN as u128),
        );
    }
    #[cfg(world_bench_uninstrumented)]
    {
        let machine_start = system_cpu_times();
        let mut wide_kill_samples = Vec::with_capacity(5);
        let mut wide_retire_samples = Vec::with_capacity(5);
        let mut deep_kill_samples = Vec::with_capacity(5);
        for repetition in 0..5 {
            wide_kill_samples.push(report_removal_latency(
                "wide_kill_65k",
                repetition,
                linked_scene(PROFILE_COUNT, PROFILE_COUNT),
                false,
                0,
                Uuid::from_u128(2),
            ));
        }
        for repetition in 0..5 {
            wide_retire_samples.push(report_removal_latency(
                "wide_retire_65k",
                repetition,
                linked_scene(PROFILE_COUNT, PROFILE_COUNT),
                true,
                0,
                Uuid::from_u128(1),
            ));
        }
        for repetition in 0..5 {
            deep_kill_samples.push(report_removal_latency(
                "deep_kill_65k_depth_4096",
                repetition,
                deep_linked_scene(PROFILE_COUNT, DEEP_CHAIN),
                false,
                PROFILE_COUNT - DEEP_CHAIN,
                Uuid::from_u128(DEEP_CHAIN as u128),
            ));
        }
        report_latency_distribution("wide_kill_65k", &wide_kill_samples);
        report_latency_distribution("wide_retire_65k", &wide_retire_samples);
        report_latency_distribution("deep_kill_65k_depth_4096", &deep_kill_samples);
        eprintln!(
            "latency-environment mode=allocator-free repetitions=5 processors={} system_busy_percent={}",
            std::thread::available_parallelism().map_or(0, |processors| processors.get()),
            system_busy_percentage(machine_start, system_cpu_times())
                .map_or_else(|| "unavailable".to_owned(), |percent| format!("{percent:.1}")),
        );
    }

    for (count, linkset_size) in [(10_000, 10_000), (65_536, 65_536), (65_536, 128)] {
        group.throughput(Throughput::Elements(count as u64));
        for retire in [false, true] {
            if !retire && linkset_size != count {
                continue;
            }
            let name = if !retire {
                "delete_linkset"
            } else if linkset_size == count {
                "retire_linkset"
            } else {
                "retire_linksets"
            };
            group.bench_function(BenchmarkId::new(name, size_name(count)), |b| {
                b.iter_batched_ref(
                    || linked_scene(count, linkset_size),
                    |scene| {
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
                    },
                    BatchSize::LargeInput,
                );
            });
        }
    }

    group.throughput(Throughput::Elements(DEEP_CHAIN as u64));
    group.bench_function(BenchmarkId::new("delete_deep_chain", "65k_depth256"), |b| {
        b.iter_batched_ref(
            || deep_linked_scene(PROFILE_COUNT, DEEP_CHAIN),
            |scene| {
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
            BatchSize::LargeInput,
        );
    });
}
