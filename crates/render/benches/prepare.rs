#[cfg(windows)]
use core::ffi;
use std::{
    env,
    f64::consts::PI,
    hint::black_box,
    time::{Duration, Instant},
};

use criterion::{Criterion, measurement::WallTime};
use futures::executor::block_on;
use math::{DVec3, Vec3};
use render::{
    GeometryPrepare,
    scene::{Camera, DrawnMeshKey, Reconciler, SceneView},
};
use wgpu::{DeviceDescriptor, PowerPreference, RequestAdapterOptions, TextureFormat};
use world::World;

use crate::support;

const OBJECTS: usize = 3_000;
const TEXTURES: usize = 512;
const ANIMATED_OBJECTS: usize = 500;
const OBJECTS_87K_FACES: usize = 14_500;
const SETTLE_FRAMES: usize = 256;
const SIZE: [u32; 2] = [1024, 1024];
const DURATION_BUCKETS: usize = 10_001;
const DURATION_BUCKET_US: u64 = 10;
const CYCLE_BUCKETS: usize = 10_001;
const CYCLE_BUCKET_WIDTH: u64 = 100_000;

struct DurationDistribution {
    buckets: Vec<u64>,
    count: u64,
    total: Duration,
    max: Duration,
}

impl Default for DurationDistribution {
    fn default() -> Self {
        Self { buckets: vec![0; DURATION_BUCKETS], count: 0, total: Duration::ZERO, max: Duration::ZERO }
    }
}

impl DurationDistribution {
    fn record(&mut self, duration: Duration) {
        let bucket = (duration.as_micros() / u128::from(DURATION_BUCKET_US)).min((DURATION_BUCKETS - 1) as u128) as usize;
        self.buckets[bucket] += 1;
        self.count += 1;
        self.total += duration;
        self.max = self.max.max(duration);
    }

    fn percentile_bucket(&self, percentile: u64) -> Duration {
        if self.count == 0 {
            return Duration::ZERO;
        }
        let target = self.count.saturating_mul(percentile).div_ceil(100);
        let mut seen = 0;
        for (bucket, count) in self.buckets.iter().copied().enumerate() {
            seen += count;
            if seen >= target {
                return Duration::from_micros(bucket as u64 * DURATION_BUCKET_US);
            }
        }
        self.max
    }

    fn report(&self) -> String {
        format!(
            "n={}, total_ms={:.3}, mean_ms={:.4}, p50_bucket_ms={:.3}, p95_bucket_ms={:.3}, p99_bucket_ms={:.3}, max_ms={:.3}",
            self.count,
            self.total.as_secs_f64() * 1000.0,
            self.total.as_secs_f64() * 1000.0 / self.count.max(1) as f64,
            self.percentile_bucket(50).as_secs_f64() * 1000.0,
            self.percentile_bucket(95).as_secs_f64() * 1000.0,
            self.percentile_bucket(99).as_secs_f64() * 1000.0,
            self.max.as_secs_f64() * 1000.0
        )
    }
}

struct CycleDistribution {
    buckets: Vec<u64>,
    count: u64,
    total: u128,
    max: u64,
}

impl Default for CycleDistribution {
    fn default() -> Self {
        Self { buckets: vec![0; CYCLE_BUCKETS], count: 0, total: 0, max: 0 }
    }
}

impl CycleDistribution {
    fn record(&mut self, cycles: u64) {
        let bucket = (cycles / CYCLE_BUCKET_WIDTH).min((CYCLE_BUCKETS - 1) as u64) as usize;
        self.buckets[bucket] += 1;
        self.count += 1;
        self.total += u128::from(cycles);
        self.max = self.max.max(cycles);
    }

    fn percentile_bucket(&self, percentile: u64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let target = self.count.saturating_mul(percentile).div_ceil(100);
        let mut seen = 0;
        for (bucket, count) in self.buckets.iter().copied().enumerate() {
            seen += count;
            if seen >= target {
                return bucket as u64 * CYCLE_BUCKET_WIDTH;
            }
        }
        self.max
    }

    fn report(&self) -> String {
        format!(
            "n={}, total_mcycles={:.3}, mean_mcycles={:.4}, p50_bucket_mcycles={:.3}, p95_bucket_mcycles={:.3}, p99_bucket_mcycles={:.3}, max_mcycles={:.3}",
            self.count,
            self.total as f64 / 1_000_000.0,
            self.total as f64 / self.count.max(1) as f64 / 1_000_000.0,
            self.percentile_bucket(50) as f64 / 1_000_000.0,
            self.percentile_bucket(95) as f64 / 1_000_000.0,
            self.percentile_bucket(99) as f64 / 1_000_000.0,
            self.max as f64 / 1_000_000.0
        )
    }
}

#[derive(Clone, Copy, Default)]
struct PrepareSample {
    wall: Duration,
    cpu_cycles: Option<u64>,
    submit: Duration,
    poll: Duration,
    stats: render::Stats,
}

#[derive(Clone, Copy, Default)]
struct ProcessMemory {
    private_bytes: usize,
    working_set_bytes: usize,
    peak_working_set_bytes: usize,
    peak_commit_bytes: usize,
}

impl ProcessMemory {
    fn max(self, other: Self) -> Self {
        Self {
            private_bytes: self.private_bytes.max(other.private_bytes),
            working_set_bytes: self.working_set_bytes.max(other.working_set_bytes),
            peak_working_set_bytes: self.peak_working_set_bytes.max(other.peak_working_set_bytes),
            peak_commit_bytes: self.peak_commit_bytes.max(other.peak_commit_bytes),
        }
    }
}

#[derive(Default)]
struct RebuildWork {
    objects: usize,
    surfaces: usize,
    sweep_slots: usize,
    cleanup_entries: usize,
    mesh_info_bytes: usize,
    mesh_info_entries: usize,
    mesh_info_centers_dropped: usize,
    candidate_items: usize,
    mesh_demand_items: usize,
    mesh_demand_order_updates: usize,
    motion_cache_items: usize,
    motion_entities: usize,
    draw_instance_bytes: usize,
    draw_instance_write_calls: usize,
    draw_command_bytes: usize,
    draw_batch_commands: usize,
    layouts: usize,
    mesh_upload_attempts: usize,
    mesh_upload_attempt_bytes: usize,
    appearance_candidates: usize,
    visible_candidates: usize,
}

impl RebuildWork {
    fn add(&mut self, stats: render::Stats) {
        self.objects += stats.rebuild_objects;
        self.surfaces += stats.rebuild_surfaces;
        self.sweep_slots += stats.rebuild_sweep_slots;
        self.cleanup_entries += stats.rebuild_cleanup_entries;
        self.mesh_info_bytes += stats.rebuild_mesh_info_bytes;
        self.mesh_info_entries += stats.rebuild_mesh_info_entries;
        self.mesh_info_centers_dropped += stats.rebuild_mesh_info_centers_dropped;
        self.candidate_items += stats.candidate_items_visited;
        self.mesh_demand_items += stats.mesh_demand_items_visited;
        self.mesh_demand_order_updates += stats.mesh_demand_order_updates;
        self.motion_cache_items += stats.motion_cache_items_scanned;
        self.motion_entities += stats.motion_entities_evaluated;
        self.draw_instance_bytes += stats.draw_instance_bytes_uploaded;
        self.draw_instance_write_calls += stats.draw_instance_write_calls;
        self.draw_command_bytes += stats.draw_command_bytes_uploaded;
        self.draw_batch_commands += stats.draw_batch_commands_built;
        self.layouts += stats.layouts;
        self.mesh_upload_attempts += stats.mesh_upload_attempts;
        self.mesh_upload_attempt_bytes += stats.mesh_upload_attempt_bytes;
        self.appearance_candidates += stats.appearance_candidates_checked;
        self.visible_candidates += stats.visible_candidates_checked;
    }
}

struct Fixture {
    world: World,
    rows: Vec<support::Row>,
    textures: support::BenchmarkTextures,
    reconciler: Reconciler,
    view: SceneView,
    now: Instant,
    prepare: GeometryPrepare,
}

impl Fixture {
    fn settled(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        regions: usize,
        meshes: usize,
        animated: usize,
        start_x: f64,
        draw_distance: f64,
    ) -> Self {
        Self::settled_with_objects(device, queue, OBJECTS, regions, meshes, animated, start_x, draw_distance)
    }

    fn settled_with_objects(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        objects: usize,
        regions: usize,
        meshes: usize,
        animated: usize,
        start_x: f64,
        draw_distance: f64,
    ) -> Self {
        let (mut world, rows) = support::live_prepare_world_with_regions(objects, meshes, TEXTURES, regions);
        if animated > 0 {
            support::animate_objects(&mut world, &rows, animated, 4.0, true);
        }
        Self::settle(device, queue, world, rows, regions, camera_at(start_x, 0.0, draw_distance))
    }

    fn settle(device: &wgpu::Device, queue: &wgpu::Queue, world: World, rows: Vec<support::Row>, regions: usize, camera: Camera) -> Self {
        let objects = rows.len();
        let textures = support::benchmark_textures(TEXTURES);
        let (reconciler, view, now) = support::settle_with_textures(&world, objects, camera, &textures.textures);
        let prepare = GeometryPrepare::new(device, queue, TextureFormat::Rgba8UnormSrgb);
        let mut fixture = Self { world, rows, textures, reconciler, view, now, prepare };
        let mut setup_cameras = Vec::with_capacity(regions * 2 + 1);
        for region in 0..regions {
            setup_cameras.push(camera_at(region as f64 * 256.0, 0.0, 1024.0));
            setup_cameras.push(camera_at(region as f64 * 256.0 + 128.0, 0.0, 1024.0));
        }
        setup_cameras.push(camera);
        for camera in setup_cameras {
            for _ in 0..TEXTURES + 32 {
                fixture.sample(device, queue, camera);
                complete_texture_work(&mut fixture.prepare, TEXTURES);
                let stats = fixture.prepare.stats();
                if !stats.rebuild_pending && stats.pending_meshes == 0 && stats.pending_textures == 0 {
                    break;
                }
            }
        }
        fixture.sample(device, queue, camera);
        assert!(!fixture.prepare.stats().rebuild_pending, "fixture rebuild must finish before timing");
        assert_eq!(fixture.prepare.stats().pending_meshes, 0, "fixture meshes must be resident before timing");
        assert_eq!(fixture.prepare.stats().pending_textures, 0, "fixture textures must be resident before timing");
        assert!(fixture.prepare.stats().instances > 0);
        assert_eq!(fixture.prepare.stats().omitted_faces, 0);
        fixture
    }

    fn sample(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, camera: Camera) -> Duration {
        prepare_sample(&mut self.prepare, device, queue, &self.view, camera).wall
    }
}

pub(crate) fn bench(c: &mut Criterion) {
    let instance = wgpu::Instance::default();
    let adapter = block_on(instance.request_adapter(&RequestAdapterOptions {
        power_preference: PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        ..Default::default()
    }))
    .expect("benchmark GPU adapter");
    eprintln!("render prepare benchmark GPU: {:?}", adapter.get_info());
    let (device, queue) = block_on(adapter.request_device(&DeviceDescriptor {
        required_features: adapter.features() & (wgpu::Features::TEXTURE_COMPRESSION_BC | wgpu::Features::INDIRECT_FIRST_INSTANCE),
        ..Default::default()
    }))
    .expect("benchmark GPU device");
    let mut group = c.benchmark_group("render/geometry_prepare");
    if env::args().any(|argument| argument.ends_with("live_walk_reprepare_pose4_texture_arrivals")) {
        bench_live_mix(&mut group, &device, &queue);
        group.finish();
        return;
    }
    if env::args().any(|argument| argument.ends_with("region_load_progress_5000") || argument.ends_with("region_load_progress_65000")) {
        bench_region_load_progress(&mut group, &device, &queue);
        group.finish();
        return;
    }
    if env::args().any(|argument| {
        [
            "history_loss_rebuild_1000",
            "history_loss_rebuild_16000",
            "history_loss_rebuild_65000",
            "history_loss_layout_rebuild_1000",
            "history_loss_layout_rebuild_16000",
            "history_loss_layout_rebuild_65000",
            "history_loss_layout_memory_1000",
            "history_loss_layout_memory_16000",
            "history_loss_layout_memory_65000",
        ]
        .contains(&argument.as_str())
    }) {
        bench_history_loss(&mut group, &device, &queue);
        group.finish();
        return;
    }
    bench_unchanged(&mut group, &device, &queue);
    bench_camera_walk(&mut group, &device, &queue);
    for (objects, name) in [(OBJECTS, "camera_culling_rotation_3000_stable"), (10_000, "camera_culling_rotation_10000_stable")] {
        let fixture = Fixture::settled_with_objects(&device, &queue, objects, 1, 50, 0, 128.0, 128.0);
        let cameras = [camera_yaw_at(128.0, -0.001, 128.0), camera_yaw_at(128.0, 0.001, 128.0)];
        bench_camera_culling_rotation(&mut group, &device, &queue, fixture, cameras, name);
    }
    let (mut world, rows) = support::live_prepare_world_with_regions(OBJECTS_87K_FACES, 50, TEXTURES, 1);
    support::blend_alternate_objects(&mut world, &rows);
    let overhead = DVec3::new(128.0, 72.0, 160.0);
    let fixture = Fixture::settle(&device, &queue, world, rows, 1, camera_yaw_from(overhead, 0.0, 512.0));
    let cameras = [camera_yaw_from(overhead, -0.001, 512.0), camera_yaw_from(overhead, 0.001, 512.0)];
    bench_camera_culling_rotation(&mut group, &device, &queue, fixture, cameras, "camera_culling_rotation_87k_faces_half_blended");
    bench_camera_culling_edge_sweep(&mut group, &device, &queue, OBJECTS, "camera_culling_edge_sweep_3000");
    bench_camera_culling_edge_sweep(&mut group, &device, &queue, 10_000, "camera_culling_edge_sweep_10000");
    bench_pose(&mut group, &device, &queue);
    bench_reprepare(&mut group, &device, &queue, 50, "stationary_one_reprepare_50_meshes");
    bench_mesh_objects(&mut group, &device, &queue);
    bench_history_loss(&mut group, &device, &queue);
    bench_live_mix(&mut group, &device, &queue);
    bench_animated(&mut group, &device, &queue);
    bench_reprepare(&mut group, &device, &queue, OBJECTS, "one_reprepare_3000_unique_meshes");
    group.finish();
}

fn bench_history_loss(group: &mut criterion::BenchmarkGroup<'_, WallTime>, device: &wgpu::Device, queue: &wgpu::Queue) {
    let arguments = env::args().collect::<Vec<_>>();
    let memory_probe = arguments.iter().any(|argument| argument.starts_with("history_loss_layout_memory_"));
    let layout_change = memory_probe || arguments.iter().any(|argument| argument.starts_with("history_loss_layout_rebuild_"));
    let benchmark_name = |size: usize| {
        if memory_probe {
            format!("history_loss_layout_memory_{size}")
        } else if layout_change {
            format!("history_loss_layout_rebuild_{size}")
        } else {
            format!("history_loss_rebuild_{size}")
        }
    };
    let selected_size = [1_000, 16_000, 65_000].into_iter().find(|size| {
        let name = benchmark_name(*size);
        arguments.iter().any(|argument| argument == &name)
    });
    for objects in [1_000, 16_000, 65_000] {
        if selected_size.is_some_and(|selected| selected != objects) {
            continue;
        }
        let mut process_memory_sample = None;
        let mut peak_process_memory = None;
        let (mut world, rows) = support::scene_world(objects, 1);
        let textures = support::benchmark_textures(1);
        let camera = camera_at(128.0, 0.0, 1024.0);
        let (mut reconciler, mut view, mut now) = support::settle_with_textures(&world, objects, camera, &textures.textures);
        let mut prepare = GeometryPrepare::new(device, queue, TextureFormat::Rgba8UnormSrgb);
        let setup_turns = objects.div_ceil(128) * 2 + 24;
        for _ in 0..setup_turns {
            prepare_sample(&mut prepare, device, queue, &view, camera);
            complete_texture_work(&mut prepare, 8);
            let stats = prepare.stats();
            if !stats.rebuild_pending && stats.pending_meshes == 0 && stats.pending_textures == 0 {
                break;
            }
        }
        assert!(prepare.stats().instances > 0, "history-loss fixture draws objects before timing");
        assert_eq!(prepare.stats().pending_meshes, 0, "history-loss fixture meshes settle before timing");
        assert_eq!(prepare.stats().pending_textures, 0, "history-loss fixture textures settle before timing");
        if memory_probe {
            process_memory_sample = process_memory();
            peak_process_memory = process_memory_sample;
        }

        let mut order = 20_000_000;
        let mut generation = 0u64;
        let mut latest = prepare.stats();
        let mut worst_stats = latest;
        let mut worst_ms: f64 = 0.0;
        let mut worst_cpu_cycles = 0;
        let mut worst_layouts = 0usize;
        let mut worst_items_updated = 0usize;
        let mut worst_faces_patched = 0usize;
        let mut prepare_wall = DurationDistribution::default();
        let mut layout_stage_wall = DurationDistribution::default();
        let mut layout_commit_wall = DurationDistribution::default();
        let mut draw_upload_stage_wall = DurationDistribution::default();
        let mut draw_batch_stage_wall = DurationDistribution::default();
        let mut texture_binding_stage_wall = DurationDistribution::default();
        let mut mesh_stage_wall = DurationDistribution::default();
        let mut mesh_upload_stage_wall = DurationDistribution::default();
        let mut prepare_cpu_cycles = CycleDistribution::default();
        let mut submit_stage_wall = DurationDistribution::default();
        let mut poll_wait_wall = DurationDistribution::default();
        let mut submit_wall = Duration::ZERO;
        let mut poll_wall = Duration::ZERO;
        let mut work = RebuildWork::default();
        let mut item_order_items = 0usize;
        let mut layout_items = 0usize;
        let mut layout_faces = 0usize;
        let mut texture_binding_entries = 0usize;
        let mut gpu_mesh_retirements = 0usize;
        let mut gpu_mesh_retired_bytes = 0usize;
        let mut total_turns = 0usize;
        let mut max_turns = 0usize;
        let mut rebuilds = 0usize;
        let mut turn_work_samples = Vec::new();
        group.bench_function(benchmark_name(objects), |b| {
            b.iter_custom(|iters| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iters {
                    if layout_change {
                        generation = generation.wrapping_add(1);
                        support::update_geometry(&mut world, &rows, objects.div_ceil(100), &mut order, generation);
                    }
                    for _ in 0..65 {
                        generation = generation.wrapping_add(1);
                        support::update_one(&mut world, &rows[0], &mut order, generation);
                        support::first_publish(&mut reconciler, &mut view, &world, &mut now);
                    }
                    let mut turns = 0;
                    for turn in 0..objects.div_ceil(128) * 2 + 24 {
                        let sample = prepare_sample(&mut prepare, device, queue, &view, camera);
                        if memory_probe {
                            record_process_memory(&mut process_memory_sample, &mut peak_process_memory);
                        }
                        elapsed += sample.wall;
                        prepare_wall.record(sample.wall);
                        layout_stage_wall.record(duration_ms(sample.stats.prepare_layout_ms));
                        layout_commit_wall.record(duration_ms(sample.stats.prepare_layout_commit_ms));
                        draw_upload_stage_wall.record(duration_ms(sample.stats.prepare_draw_upload_ms));
                        draw_batch_stage_wall.record(duration_ms(sample.stats.prepare_draw_batches_ms));
                        texture_binding_stage_wall.record(duration_ms(sample.stats.prepare_texture_bindings_ms));
                        mesh_stage_wall.record(duration_ms(sample.stats.prepare_meshes_ms));
                        mesh_upload_stage_wall.record(duration_ms(sample.stats.prepare_mesh_uploads_ms));
                        if let Some(cpu_cycles) = sample.cpu_cycles {
                            prepare_cpu_cycles.record(cpu_cycles);
                        }
                        submit_wall += sample.submit;
                        poll_wall += sample.poll;
                        submit_stage_wall.record(sample.submit);
                        poll_wait_wall.record(sample.poll);
                        turns += 1;
                        latest = sample.stats;
                        work.add(latest);
                        turn_work_samples.push(latest);
                        item_order_items += latest.item_order_items;
                        layout_items += latest.layout_items_visited;
                        layout_faces += latest.layout_faces_visited;
                        texture_binding_entries += latest.texture_binding_entries;
                        gpu_mesh_retirements += latest.gpu_mesh_retirements;
                        gpu_mesh_retired_bytes += latest.gpu_mesh_retired_bytes;
                        let prepare_ms = sample.wall.as_secs_f64() * 1000.0;
                        if prepare_ms > worst_ms {
                            worst_ms = prepare_ms;
                            worst_stats = latest;
                            worst_layouts = latest.layouts;
                            worst_items_updated = latest.items_updated;
                            worst_faces_patched = latest.faces_patched;
                        }
                        if let Some(cpu_cycles) = sample.cpu_cycles {
                            worst_cpu_cycles = worst_cpu_cycles.max(cpu_cycles);
                        }
                        if !latest.rebuild_pending && latest.pending_meshes == 0 && latest.pending_textures == 0 {
                            break;
                        }
                        assert!(
                            turn + 1 < objects.div_ceil(128) * 2 + 24,
                            "history-loss rebuild finishes within bounded pages: rebuild={}, meshes={}, textures={}, layouts={}",
                            latest.rebuild_pending,
                            latest.pending_meshes,
                            latest.pending_textures,
                            latest.layouts
                        );
                    }
                    assert!(!latest.rebuild_pending, "history-loss rebuild finishes within bounded pages");
                    if layout_change {
                        assert!(layout_items > 0, "layout-change catch-up visits the final visible list");
                        assert!(layout_faces > 0, "layout-change catch-up visits final draw faces");
                    }
                    total_turns += turns;
                    max_turns = max_turns.max(turns);
                    rebuilds += 1;
                }
                elapsed
            });
        });
        let turn_work = format!(
            "objects=[{}], cleanup=[{}], mesh_info_bytes=[{}], item_order=[{}], candidates=[{}], mesh_demands=[{}], mesh_order_updates=[{}], mesh_uploads=[{}], motion_redraw_items=[{}], layouts=[{}], layout_items=[{}], layout_faces=[{}], instance_bytes=[{}], instance_write_calls=[{}], command_bytes=[{}], texture_bindings=[{}], gpu_retirements=[{}], retired_bytes=[{}]",
            work_distribution(turn_work_samples.iter().map(|stats| stats.rebuild_objects)),
            work_distribution(turn_work_samples.iter().map(|stats| stats.rebuild_cleanup_entries)),
            work_distribution(turn_work_samples.iter().map(|stats| stats.rebuild_mesh_info_bytes)),
            work_distribution(turn_work_samples.iter().map(|stats| stats.item_order_items)),
            work_distribution(turn_work_samples.iter().map(|stats| stats.candidate_items_visited)),
            work_distribution(turn_work_samples.iter().map(|stats| stats.mesh_demand_items_visited)),
            work_distribution(turn_work_samples.iter().map(|stats| stats.mesh_demand_order_updates)),
            work_distribution(turn_work_samples.iter().map(|stats| stats.mesh_upload_attempts)),
            work_distribution(turn_work_samples.iter().map(|stats| stats.motion_redraw_items_checked)),
            work_distribution(turn_work_samples.iter().map(|stats| stats.layouts)),
            work_distribution(turn_work_samples.iter().map(|stats| stats.layout_items_visited)),
            work_distribution(turn_work_samples.iter().map(|stats| stats.layout_faces_visited)),
            work_distribution(turn_work_samples.iter().map(|stats| stats.draw_instance_bytes_uploaded)),
            work_distribution(turn_work_samples.iter().map(|stats| stats.draw_instance_write_calls)),
            work_distribution(turn_work_samples.iter().map(|stats| stats.draw_command_bytes_uploaded)),
            work_distribution(turn_work_samples.iter().map(|stats| stats.texture_binding_entries)),
            work_distribution(turn_work_samples.iter().map(|stats| stats.gpu_mesh_retirements)),
            work_distribution(turn_work_samples.iter().map(|stats| stats.gpu_mesh_retired_bytes)),
        );
        eprintln!(
            "{}: prepare_wall=[{}], prepare_thread_cpu_cycles=[{}], submit_wall_ms={:.3}, submit_poll_wait_wall_ms={:.3}, worst_prepare_wall_ms={worst_ms:.3}, worst_prepare_thread_cpu_cycles={worst_cpu_cycles}, worst_stage_ms=[cache={:.3}, visibility={:.3}, motion={:.3}, candidates={:.3}, requests={:.3}, textures={:.3}, texture_bindings={:.3}, meshes={:.3}, mesh_demands={:.3}, mesh_uploads={:.3}, appearances={:.3}, layout={:.3}, draw_upload={:.3}, draw_batches={:.3}, draw_list={:.3}, finalize={:.3}], worst_work=[objects={}, surfaces={}, sweep_slots={}, cleanup_entries={}, mesh_info_bytes={}, mesh_info_entries={}, mesh_info_centers_dropped={}, item_order_items={}, candidate_items={}, mesh_demand_items={}, mesh_demand_order_updates={}, mesh_upload_attempts={}, mesh_upload_attempt_bytes={}, appearance_candidates={}, visible_candidates={}, motion_cache_items={}, motion_entities={}, layout_items={}, layout_faces={}, draw_instance_bytes_uploaded={}, draw_instance_write_calls={}, draw_command_bytes_uploaded={}, draw_batch_commands_built={}, texture_binding_entries={}, gpu_mesh_retirements={}, gpu_mesh_retired_bytes={}], total_work_per_rebuild=[objects={:.1}, surfaces={:.1}, sweep_slots={:.1}, cleanup_entries={:.1}, mesh_info_bytes={:.1}, mesh_info_entries={:.1}, mesh_info_centers_dropped={:.1}, item_order_items={:.1}, candidate_items={:.1}, mesh_demand_items={:.1}, mesh_demand_order_updates={:.1}, mesh_upload_attempts={:.1}, mesh_upload_attempt_bytes={:.1}, appearance_candidates={:.1}, visible_candidates={:.1}, motion_cache_items={:.1}, motion_entities={:.1}, layout_items={:.1}, layout_faces={:.1}, draw_instance_bytes_uploaded={:.1}, draw_instance_write_calls={:.1}, draw_command_bytes_uploaded={:.1}, draw_batch_commands_built={:.1}, texture_binding_entries={:.1}, gpu_mesh_retirements={:.1}, gpu_mesh_retired_bytes={:.1}], worst_layouts={worst_layouts}, worst_items_updated={worst_items_updated}, worst_faces_patched={worst_faces_patched}, total_turns={total_turns}, max_turns={max_turns}, rebuilds={rebuilds}, average_turns_per_rebuild={:.1}, gpu_mesh_retained_bytes={}, gpu_mesh_peak_bytes={}, process_private_bytes_retained={}, process_private_bytes_peak_sampled={}, process_working_set_retained={}, process_working_set_peak={}, visible_instances={}",
            benchmark_name(objects),
            prepare_wall.report(),
            prepare_cpu_cycles.report(),
            submit_wall.as_secs_f64() * 1000.0,
            poll_wall.as_secs_f64() * 1000.0,
            worst_stats.prepare_cache_ms,
            worst_stats.prepare_visibility_ms,
            worst_stats.prepare_motion_ms,
            worst_stats.prepare_candidates_ms,
            worst_stats.prepare_requests_ms,
            worst_stats.prepare_textures_ms,
            worst_stats.prepare_texture_bindings_ms,
            worst_stats.prepare_meshes_ms,
            worst_stats.prepare_mesh_demands_ms,
            worst_stats.prepare_mesh_uploads_ms,
            worst_stats.prepare_appearances_ms,
            worst_stats.prepare_layout_ms,
            worst_stats.prepare_draw_upload_ms,
            worst_stats.prepare_draw_batches_ms,
            worst_stats.prepare_draw_list_ms,
            worst_stats.prepare_finalize_ms,
            worst_stats.rebuild_objects,
            worst_stats.rebuild_surfaces,
            worst_stats.rebuild_sweep_slots,
            worst_stats.rebuild_cleanup_entries,
            worst_stats.rebuild_mesh_info_bytes,
            worst_stats.rebuild_mesh_info_entries,
            worst_stats.rebuild_mesh_info_centers_dropped,
            worst_stats.item_order_items,
            worst_stats.candidate_items_visited,
            worst_stats.mesh_demand_items_visited,
            worst_stats.mesh_demand_order_updates,
            worst_stats.mesh_upload_attempts,
            worst_stats.mesh_upload_attempt_bytes,
            worst_stats.appearance_candidates_checked,
            worst_stats.visible_candidates_checked,
            worst_stats.motion_cache_items_scanned,
            worst_stats.motion_entities_evaluated,
            worst_stats.layout_items_visited,
            worst_stats.layout_faces_visited,
            worst_stats.draw_instance_bytes_uploaded,
            worst_stats.draw_instance_write_calls,
            worst_stats.draw_command_bytes_uploaded,
            worst_stats.draw_batch_commands_built,
            worst_stats.texture_binding_entries,
            worst_stats.gpu_mesh_retirements,
            worst_stats.gpu_mesh_retired_bytes,
            work.objects as f64 / rebuilds.max(1) as f64,
            work.surfaces as f64 / rebuilds.max(1) as f64,
            work.sweep_slots as f64 / rebuilds.max(1) as f64,
            work.cleanup_entries as f64 / rebuilds.max(1) as f64,
            work.mesh_info_bytes as f64 / rebuilds.max(1) as f64,
            work.mesh_info_entries as f64 / rebuilds.max(1) as f64,
            work.mesh_info_centers_dropped as f64 / rebuilds.max(1) as f64,
            item_order_items as f64 / rebuilds.max(1) as f64,
            work.candidate_items as f64 / rebuilds.max(1) as f64,
            work.mesh_demand_items as f64 / rebuilds.max(1) as f64,
            work.mesh_demand_order_updates as f64 / rebuilds.max(1) as f64,
            work.mesh_upload_attempts as f64 / rebuilds.max(1) as f64,
            work.mesh_upload_attempt_bytes as f64 / rebuilds.max(1) as f64,
            work.appearance_candidates as f64 / rebuilds.max(1) as f64,
            work.visible_candidates as f64 / rebuilds.max(1) as f64,
            work.motion_cache_items as f64 / rebuilds.max(1) as f64,
            work.motion_entities as f64 / rebuilds.max(1) as f64,
            layout_items as f64 / rebuilds.max(1) as f64,
            layout_faces as f64 / rebuilds.max(1) as f64,
            work.draw_instance_bytes as f64 / rebuilds.max(1) as f64,
            work.draw_instance_write_calls as f64 / rebuilds.max(1) as f64,
            work.draw_command_bytes as f64 / rebuilds.max(1) as f64,
            work.draw_batch_commands as f64 / rebuilds.max(1) as f64,
            texture_binding_entries as f64 / rebuilds.max(1) as f64,
            gpu_mesh_retirements as f64 / rebuilds.max(1) as f64,
            gpu_mesh_retired_bytes as f64 / rebuilds.max(1) as f64,
            total_turns as f64 / rebuilds.max(1) as f64,
            latest.mesh_bytes,
            latest.mesh_peak,
            process_memory_sample.map_or(0, |memory| memory.private_bytes),
            peak_process_memory.map_or(0, |memory| memory.private_bytes),
            process_memory_sample.map_or(0, |memory| memory.working_set_bytes),
            peak_process_memory.map_or(0, |memory| memory.peak_working_set_bytes),
            latest.instances
        );
        eprintln!("{}: worst_layout_commit_ms={:.3}", benchmark_name(objects), worst_stats.prepare_layout_commit_ms);
        eprintln!(
            "{}: stage_wall=[layout={}, layout_commit={}, draw_upload={}, draw_batches={}, texture_bindings={}, meshes={}, mesh_uploads={}, submit={}, gpu_poll_wait={}], process_memory_sampled={memory_probe}",
            benchmark_name(objects),
            layout_stage_wall.report(),
            layout_commit_wall.report(),
            draw_upload_stage_wall.report(),
            draw_batch_stage_wall.report(),
            texture_binding_stage_wall.report(),
            mesh_stage_wall.report(),
            mesh_upload_stage_wall.report(),
            submit_stage_wall.report(),
            poll_wait_wall.report(),
        );
        eprintln!("{}: work_per_turn=[{turn_work}]", benchmark_name(objects));
        eprintln!("{}: layouts_per_rebuild={:.2}", benchmark_name(objects), work.layouts as f64 / rebuilds.max(1) as f64);
    }
}

fn bench_region_load_progress(group: &mut criterion::BenchmarkGroup<'_, WallTime>, device: &wgpu::Device, queue: &wgpu::Queue) {
    let arguments = env::args().collect::<Vec<_>>();
    let selected_size = [5_000, 65_000]
        .into_iter()
        .find(|size| arguments.iter().any(|argument| argument.ends_with(&format!("region_load_progress_{size}"))));
    for objects in [5_000, 65_000] {
        if selected_size.is_some_and(|selected| selected != objects) {
            continue;
        }
        let name = format!("region_load_progress_{objects}");
        let (mut world, rows) = support::live_prepare_world_with_regions(objects, 128, 8, 2);
        let mut order = 20_000_000;
        let meshes = support::add_meshes(&mut world, &rows, 128, &mut order);
        let textures = support::benchmark_textures(8);
        let camera = camera_at(256.5, 0.0, 768.0);
        let (mut reconciler, mut view, mut now) = support::settle_with_assets(&world, objects, camera, &textures.textures, &meshes);
        assert_eq!(meshes.len(), 128);
        assert_eq!(
            view.objects().filter(|object| matches!(object.mesh().map(|(key, _)| key), Some(DrawnMeshKey::Asset { .. }))).count(),
            128,
            "all distinct mesh assets are ready in the scene"
        );
        let mut generation = 0u64;
        let mut first_useful_latency = DurationDistribution::default();
        let mut preparation_turns = DurationDistribution::default();
        let mut owner_turns = DurationDistribution::default();
        let mut scene_updates = DurationDistribution::default();
        let mut thread_cycles = CycleDistribution::default();
        let mut work = RebuildWork::default();
        let mut turn_samples = Vec::new();
        let mut peak_memory = None;
        let mut current_memory = None;
        let mut total_loads = 0usize;
        let mut useful_loads = 0usize;
        let mut converged_loads = 0usize;
        let mut quiet_turns_total = 0usize;
        let mut quiet_turn_samples = Vec::new();
        let mut first_useful_turn_total = 0usize;
        let mut visible_instances_total = 0usize;
        let mut visible_instance_samples = 0usize;
        let mut peak_instances = 0usize;
        let mut final_stats = render::Stats::default();
        let mut recovery_periods = 0usize;
        let mut published_count_drops = 0usize;
        group.bench_function(name.clone(), |b| {
            b.iter_custom(|iters| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iters {
                    let memory_before = process_memory();
                    let mut prepare = GeometryPrepare::new(device, queue, TextureFormat::Rgba8UnormSrgb);
                    let mut load_elapsed = Duration::ZERO;
                    let mut first_useful = None;
                    let mut latest = prepare.stats();
                    let mut rebuild_was_pending = false;
                    let mut previous_instances = 0usize;
                    for turn in 0..60usize {
                        let owner_started = Instant::now();
                        for (index, row) in rows.iter().take(128).enumerate() {
                            generation = generation.wrapping_add(1);
                            support::update_mesh_one(&mut world, row, index, &mut order, generation);
                        }
                        support::first_publish(&mut reconciler, &mut view, &world, &mut now);
                        let scene_update = owner_started.elapsed();
                        scene_updates.record(scene_update);
                        let fast_steps = (turn + 1).min(2);
                        let slow_steps = (turn + 1).saturating_sub(2).min(28);
                        let walk = fast_steps as f64 * 0.3 + slow_steps as f64 * (0.7 / 28.0);
                        let camera = camera_at(256.5 - walk, turn as f64, 768.0);
                        let sample = prepare_sample(&mut prepare, device, queue, &view, camera);
                        let owner_turn = scene_update + sample.wall + sample.submit + sample.poll;
                        elapsed += owner_turn;
                        load_elapsed += owner_turn;
                        preparation_turns.record(sample.wall);
                        owner_turns.record(owner_turn);
                        if let Some(cpu_cycles) = sample.cpu_cycles {
                            thread_cycles.record(cpu_cycles);
                        }
                        latest = sample.stats;
                        work.add(latest);
                        turn_samples.push(latest);
                        visible_instances_total = visible_instances_total.saturating_add(latest.instances);
                        visible_instance_samples += 1;
                        peak_instances = peak_instances.max(latest.instances);
                        published_count_drops += usize::from(latest.instances < previous_instances);
                        previous_instances = latest.instances;
                        if latest.rebuild_pending && !rebuild_was_pending {
                            recovery_periods += 1;
                        }
                        rebuild_was_pending = latest.rebuild_pending;
                        if first_useful.is_none() && latest.instances > 0 && latest.draw_calls > 0 {
                            first_useful = Some((turn + 1, load_elapsed));
                        }
                        complete_texture_work(&mut prepare, 8);
                    }
                    let quiet_camera = camera_at(255.5, 3.0, 768.0);
                    let mut quiet_turns = 0usize;
                    let mut quiet_trace = Vec::new();
                    while prepare.needs_frame(Instant::now()) && quiet_turns < 1_500 {
                        let sample = prepare_sample(&mut prepare, device, queue, &view, quiet_camera);
                        elapsed += sample.wall + sample.submit + sample.poll;
                        load_elapsed += sample.wall + sample.submit + sample.poll;
                        preparation_turns.record(sample.wall);
                        owner_turns.record(sample.wall + sample.submit + sample.poll);
                        if let Some(cpu_cycles) = sample.cpu_cycles {
                            thread_cycles.record(cpu_cycles);
                        }
                        latest = sample.stats;
                        work.add(latest);
                        turn_samples.push(latest);
                        visible_instances_total = visible_instances_total.saturating_add(latest.instances);
                        visible_instance_samples += 1;
                        peak_instances = peak_instances.max(latest.instances);
                        published_count_drops += usize::from(latest.instances < previous_instances);
                        previous_instances = latest.instances;
                        complete_texture_work(&mut prepare, 8);
                        quiet_turns += 1;
                        if quiet_turns == 1 || quiet_turns.is_multiple_of(15) {
                            quiet_trace.push(format!(
                                "turn={quiet_turns}:rebuild_pending={},objects={},surfaces={},sweep={},cleanup={},candidates={},updated={},layouts={},layout_items={},layout_faces={},instances={},pending_meshes={},pending_textures={}",
                                latest.rebuild_pending,
                                latest.rebuild_objects,
                                latest.rebuild_surfaces,
                                latest.rebuild_sweep_slots,
                                latest.rebuild_cleanup_entries,
                                latest.candidate_items_visited,
                                latest.items_updated,
                                latest.layouts,
                                latest.layout_items_visited,
                                latest.layout_faces_visited,
                                latest.instances,
                                latest.pending_meshes,
                                latest.pending_textures,
                            ));
                        }
                    }
                    assert!(
                        !prepare.needs_frame(Instant::now()),
                        "{name} continuation did not settle within 1,500 quiet turns: rebuild_pending={}, pending_meshes={}, pending_textures={}, quiet_trace={quiet_trace:?}",
                        latest.rebuild_pending,
                        latest.pending_meshes,
                        latest.pending_textures,
                    );
                    assert!(
                        !latest.rebuild_pending && latest.pending_meshes == 0 && latest.pending_textures == 0,
                        "{name} settled without converged content: rebuild_pending={}, pending_meshes={}, pending_textures={}",
                        latest.rebuild_pending,
                        latest.pending_meshes,
                        latest.pending_textures,
                    );
                    if let Some((turn, latency)) = first_useful {
                        first_useful_turn_total += turn;
                        first_useful_latency.record(latency);
                        useful_loads += 1;
                    }
                    if !latest.rebuild_pending && latest.pending_meshes == 0 && latest.pending_textures == 0 {
                        converged_loads += 1;
                    }
                    quiet_turns_total += quiet_turns;
                    quiet_turn_samples.push(quiet_turns);
                    total_loads += 1;
                    final_stats = latest;
                    if let Some(memory) = memory_before {
                        current_memory = Some(memory);
                        peak_memory = Some(peak_memory.map_or(memory, |previous: ProcessMemory| previous.max(memory)));
                    }
                    record_process_memory(&mut current_memory, &mut peak_memory);
                }
                elapsed
            });
        });
        eprintln!(
            "{name}: loads={total_loads}, useful_loads={useful_loads}, converged={converged_loads}, first_useful_turn_mean={:.1}, first_useful_latency=[{}], quiet_turns=[{}], recovery_periods={}, published_count_drops={}, prep_turns=[{}], complete_owner_turns=[{}], scene_updates=[{}], visible_instances_mean={:.1}, peak_instances={}, final_instances={}, cpu_cycles=[{}], work_per_load=[rebuild_objects={:.1}, rebuild_surfaces={:.1}, mesh_info_bytes={:.1}, candidates={:.1}, mesh_demands={:.1}, mesh_upload_attempts={:.1}, uploaded_bytes={:.1}, layouts={:.1}, layout_instance_bytes={:.1}, layout_command_bytes={:.1}], gpu_mesh_retained_bytes={}, gpu_mesh_peak_bytes={}, process_private_bytes_sampled={}, process_private_bytes_peak={}, process_working_set_sampled={}, process_working_set_peak={}",
            first_useful_turn_total as f64 / useful_loads.max(1) as f64,
            first_useful_latency.report(),
            work_distribution(quiet_turn_samples.iter().copied()),
            recovery_periods,
            published_count_drops,
            preparation_turns.report(),
            owner_turns.report(),
            scene_updates.report(),
            visible_instances_total as f64 / visible_instance_samples.max(1) as f64,
            peak_instances,
            final_stats.instances,
            thread_cycles.report(),
            work.objects as f64 / total_loads.max(1) as f64,
            work.surfaces as f64 / total_loads.max(1) as f64,
            work.mesh_info_bytes as f64 / total_loads.max(1) as f64,
            work.candidate_items as f64 / total_loads.max(1) as f64,
            work.mesh_demand_items as f64 / total_loads.max(1) as f64,
            work.mesh_upload_attempts as f64 / total_loads.max(1) as f64,
            work.mesh_upload_attempt_bytes as f64 / total_loads.max(1) as f64,
            work.layouts as f64 / total_loads.max(1) as f64,
            work.draw_instance_bytes as f64 / total_loads.max(1) as f64,
            work.draw_command_bytes as f64 / total_loads.max(1) as f64,
            final_stats.mesh_bytes,
            final_stats.mesh_peak,
            current_memory.map_or(0, |memory| memory.private_bytes),
            peak_memory.map_or(0, |memory| memory.peak_commit_bytes),
            current_memory.map_or(0, |memory| memory.working_set_bytes),
            peak_memory.map_or(0, |memory| memory.peak_working_set_bytes),
        );
        eprintln!(
            "{name}: turn_work=[rebuild_objects={}, cleanup_entries={}, item_order={}, candidates={}, mesh_demands={}, mesh_order_updates={}, mesh_upload_attempts={}, mesh_upload_attempt_bytes={}, layouts={}, layout_items={}, layout_faces={}]",
            work_distribution(turn_samples.iter().map(|stats| stats.rebuild_objects)),
            work_distribution(turn_samples.iter().map(|stats| stats.rebuild_cleanup_entries)),
            work_distribution(turn_samples.iter().map(|stats| stats.item_order_items)),
            work_distribution(turn_samples.iter().map(|stats| stats.candidate_items_visited)),
            work_distribution(turn_samples.iter().map(|stats| stats.mesh_demand_items_visited)),
            work_distribution(turn_samples.iter().map(|stats| stats.mesh_demand_order_updates)),
            work_distribution(turn_samples.iter().map(|stats| stats.mesh_upload_attempts)),
            work_distribution(turn_samples.iter().map(|stats| stats.mesh_upload_attempt_bytes)),
            work_distribution(turn_samples.iter().map(|stats| stats.layouts)),
            work_distribution(turn_samples.iter().map(|stats| stats.layout_items_visited)),
            work_distribution(turn_samples.iter().map(|stats| stats.layout_faces_visited)),
        );
    }
}

fn bench_mesh_objects(group: &mut criterion::BenchmarkGroup<'_, WallTime>, device: &wgpu::Device, queue: &wgpu::Queue) {
    let (_reconciler, view, _world, _now) = support::ready_distinct_mesh_scene(128);
    let camera = Camera::new(DVec3::ZERO, Vec3::X, PI / 3.0, SIZE).expect("mesh preparation camera");
    let mut prepare = GeometryPrepare::new(device, queue, TextureFormat::Rgba8UnormSrgb);
    for _ in 0..16 {
        prepare_sample(&mut prepare, device, queue, &view, camera);
        if !prepare.stats().rebuild_pending && prepare.stats().pending_meshes == 0 {
            break;
        }
    }
    assert_eq!(prepare.stats().pending_meshes, 0, "mesh benchmark residents settle before timing");
    assert!(!prepare.stats().rebuild_pending, "mesh benchmark rebuild settles before timing");
    let mut generation = 0usize;
    let mut latest = prepare.stats();
    group.bench_function("camera_change_128_distinct_mesh_assets", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                generation = generation.wrapping_add(1);
                let camera = Camera::new(DVec3::new((generation % 2) as f64 * 0.01, 0.0, 0.0), Vec3::X, PI / 3.0, SIZE)
                    .expect("mesh preparation camera");
                elapsed += prepare_sample(&mut prepare, device, queue, &view, camera).wall;
                latest = prepare.stats();
                assert_eq!(latest.pending_meshes, 0, "camera changes use resident mesh allocations");
                assert_eq!(latest.omitted_faces, 0);
            }
            elapsed
        });
    });
    report("camera_change_128_distinct_mesh_assets", latest);
}

fn bench_unchanged(group: &mut criterion::BenchmarkGroup<'_, WallTime>, device: &wgpu::Device, queue: &wgpu::Queue) {
    let mut fixture = Fixture::settled(device, queue, 1, 50, 0, 50.0, 512.0);
    let camera = camera_at(50.0, 0.0, 512.0);
    fixture.sample(device, queue, camera);
    let mut latest = fixture.prepare.stats();
    group.bench_function("unchanged_static_camera", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                elapsed += fixture.sample(device, queue, camera);
                latest = fixture.prepare.stats();
                assert_eq!(latest.pending_meshes, 0);
                assert_eq!(latest.pending_textures, 0);
                assert_eq!(latest.omitted_faces, 0);
                assert_eq!(latest.layouts, 0, "unchanged frames reuse the prepared draw list");
                assert_eq!(latest.items_updated, 0);
                assert_eq!(latest.faces_patched, 0);
            }
            elapsed
        });
    });
    report("unchanged_static_camera", latest);
}

fn bench_camera_walk(group: &mut criterion::BenchmarkGroup<'_, WallTime>, device: &wgpu::Device, queue: &wgpu::Queue) {
    let mut fixture = Fixture::settled(device, queue, 7, 50, 0, 896.0, 1024.0);
    let mut generation = 0usize;
    let mut layouts = 0usize;
    let mut latest = fixture.prepare.stats();
    group.bench_function("camera_walk_025_multi_region", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                generation = generation.wrapping_add(1);
                let step = (generation % 2048) as f64 * 0.25;
                elapsed += fixture.sample(device, queue, camera_at(768.0 + step, step, 1024.0));
                latest = fixture.prepare.stats();
                layouts += latest.layouts;
                assert_eq!(latest.items_updated, 0, "camera movement does not update cached items");
                assert_eq!(latest.pending_meshes, 0, "camera walk must use resident meshes");
                assert_eq!(latest.pending_textures, 0, "camera walk must use resident textures");
                assert_eq!(latest.omitted_faces, 0);
            }
            elapsed
        });
    });
    assert!(generation == 0 || layouts > 0, "multi-region camera walk must relayout when visible items change");
    report("camera_walk_025_multi_region", latest);
}

fn bench_camera_culling_rotation(
    group: &mut criterion::BenchmarkGroup<'_, WallTime>,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    mut fixture: Fixture,
    cameras: [Camera; 2],
    name: &'static str,
) {
    let initial = fixture.prepare.stats();
    let mut rotated = initial;
    for camera in cameras {
        fixture.sample(device, queue, camera);
        let stats = fixture.prepare.stats();
        assert_eq!(stats.pending_meshes, 0, "warm rotation must use resident meshes");
        assert_eq!(stats.pending_textures, 0, "warm rotation must use resident textures");
        assert_eq!(stats.omitted_faces, 0);
        assert_eq!(stats.layouts, 0, "small in-place rotation must preserve range membership");
        assert_eq!(stats.instances, initial.instances, "small in-place rotation must preserve drawn items");
        assert_eq!(stats.draw_calls, initial.draw_calls);
        assert_eq!(stats.draw_commands, initial.draw_commands);
        rotated = stats;
    }
    eprintln!(
        "{name}: stable membership instances={}->{} layouts={}->{}",
        initial.instances, rotated.instances, initial.layouts, rotated.layouts
    );

    let mut generation = 0usize;
    let mut latest = fixture.prepare.stats();
    let mut cull = DurationDistribution::default();
    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                generation = generation.wrapping_add(1);
                elapsed += fixture.sample(device, queue, cameras[generation % cameras.len()]);
                latest = fixture.prepare.stats();
                cull.record(duration_ms(latest.prepare_draw_batches_ms));
                assert_eq!(latest.pending_meshes, 0, "warm rotation must use resident meshes");
                assert_eq!(latest.pending_textures, 0, "warm rotation must use resident textures");
                assert_eq!(latest.omitted_faces, 0);
                assert_eq!(latest.layouts, 0, "small in-place rotation must preserve range membership");
                assert_eq!(latest.instances, initial.instances, "small in-place rotation must preserve drawn items");
                assert_eq!(latest.draw_calls, initial.draw_calls);
                assert_eq!(latest.draw_commands, initial.draw_commands);
            }
            elapsed
        });
    });
    report(name, latest);
    eprintln!("{name}: cull {}", cull.report());
}

fn bench_camera_culling_edge_sweep(
    group: &mut criterion::BenchmarkGroup<'_, WallTime>,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    objects: usize,
    name: &'static str,
) {
    let mut fixture = Fixture::settled_with_objects(device, queue, objects, 7, 50, 0, 896.0, 1024.0);
    let cameras = [camera_yaw_at(896.0, 0.0, 1024.0), camera_yaw_at(896.0, 0.35, 1024.0)];
    fixture.sample(device, queue, cameras[0]);
    let first = fixture.prepare.stats();
    let mut second = first;
    let mut settle_layouts = 0;
    let mut settle_frames = 0;
    for frame in 1..=SETTLE_FRAMES {
        fixture.sample(device, queue, cameras[1]);
        second = fixture.prepare.stats();
        settle_layouts += second.layouts;
        settle_frames = frame;
        if !second.rebuild_pending {
            break;
        }
    }
    for stats in [first, second] {
        assert_eq!(stats.pending_meshes, 0, "warm edge sweep must use resident meshes");
        assert_eq!(stats.pending_textures, 0, "warm edge sweep must use resident textures");
        assert_eq!(stats.omitted_faces, 0);
    }
    assert!(!second.rebuild_pending, "edge sweep layout must settle within {SETTLE_FRAMES} frames");
    assert_ne!(first.draw_commands, second.draw_commands, "edge sweep must change the emitted commands");
    assert_eq!(settle_layouts, 0, "turning in place must keep range membership");
    eprintln!(
        "{name}: edge sweep commands={}->{} instances={} settled after {settle_frames} frames",
        first.draw_commands, second.draw_commands, second.instances
    );

    let mut generation = 0usize;
    let mut latest = second;
    let mut cull = DurationDistribution::default();
    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                let camera_index = generation % cameras.len();
                generation = generation.wrapping_add(1);
                elapsed += fixture.sample(device, queue, cameras[camera_index]);
                latest = fixture.prepare.stats();
                cull.record(duration_ms(latest.prepare_draw_batches_ms));
                assert_eq!(latest.pending_meshes, 0, "warm edge sweep must use resident meshes");
                assert_eq!(latest.pending_textures, 0, "warm edge sweep must use resident textures");
                assert_eq!(latest.omitted_faces, 0);
                assert_eq!(latest.layouts, 0, "the alternating sweep must keep range membership");
            }
            elapsed
        });
    });
    report(name, latest);
    eprintln!("{name}: cull {}", cull.report());
}

fn bench_pose(group: &mut criterion::BenchmarkGroup<'_, WallTime>, device: &wgpu::Device, queue: &wgpu::Queue) {
    let mut fixture = Fixture::settled(device, queue, 1, 50, 0, 50.0, 512.0);
    let camera = camera_at(50.0, 0.0, 512.0);
    let mut generation = 0;
    let mut order = 20_000_000;
    let mut latest = fixture.prepare.stats();
    group.bench_function("one_pose_change", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                generation += 1;
                support::update_one(&mut fixture.world, &fixture.rows[0], &mut order, generation);
                support::first_publish(&mut fixture.reconciler, &mut fixture.view, &fixture.world, &mut fixture.now);
                elapsed += fixture.sample(device, queue, camera);
                latest = fixture.prepare.stats();
                assert!(latest.items_updated > 0, "pose update must reach the cache");
                assert!(latest.faces_patched > 0, "pose update must patch packed instance bytes");
                assert_eq!(latest.omitted_faces, 0);
            }
            elapsed
        });
    });
    report("one_pose_change", latest);
}

fn bench_reprepare(
    group: &mut criterion::BenchmarkGroup<'_, WallTime>,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    mesh_count: usize,
    name: &'static str,
) {
    let mut fixture = Fixture::settled(device, queue, 1, mesh_count, 0, 50.0, 512.0);
    let camera = camera_at(50.0, 0.0, 512.0);
    let mut generation = 0;
    let mut order = 20_000_000;
    let mut latest = fixture.prepare.stats();
    let mut frames = 0usize;
    let mut max_frames = 0usize;
    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                generation += 1;
                let texture = support::update_texture(&mut fixture.world, &fixture.rows[0], &mut order, generation);
                support::publish_texture_update(
                    &mut fixture.reconciler,
                    &mut fixture.view,
                    &fixture.world,
                    &mut fixture.now,
                    &fixture.rows[0],
                    texture,
                );
                elapsed += fixture.sample(device, queue, camera);
                latest = fixture.prepare.stats();
                assert!(latest.items_updated > 0, "appearance update must refresh an object");
                let mut layouts = latest.layouts;
                let mut settle_frames = 1;
                while latest.rebuild_pending && settle_frames < SETTLE_FRAMES {
                    elapsed += fixture.sample(device, queue, camera);
                    latest = fixture.prepare.stats();
                    layouts += latest.layouts;
                    settle_frames += 1;
                }
                assert!(!latest.rebuild_pending, "appearance change layout must settle within {SETTLE_FRAMES} frames");
                assert_eq!(layouts, 1, "appearance change must rebuild the draw list once");
                assert_eq!(latest.omitted_faces, 0);
                frames += settle_frames;
                max_frames = max_frames.max(settle_frames);
            }
            elapsed
        });
    });
    eprintln!("{name}: frames_per_change mean={:.1} max={max_frames}", frames as f64 / generation.max(1) as f64);
    report(name, latest);
}

fn bench_live_mix(group: &mut criterion::BenchmarkGroup<'_, WallTime>, device: &wgpu::Device, queue: &wgpu::Queue) {
    let mut fixture = Fixture::settled(device, queue, 7, 50, 0, 768.0, 1024.0);
    let mut generation = 0u64;
    let mut order = 20_000_000;
    let mut latest = fixture.prepare.stats();
    let mut faces_patched = 0usize;
    let rows_per_region = OBJECTS.div_ceil(7);
    group.bench_function("live_walk_reprepare_pose4_texture_arrivals", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                generation += 1;
                let step = (generation % 2048) as f64 * 0.25;
                let camera_x = 768.0 + step;
                let local_x = (camera_x % 256.0) as usize;
                let (region, first_local_id) = if local_x >= 220 {
                    (camera_x as usize / 256 + 1, 50)
                } else if local_x >= 100 {
                    (camera_x as usize / 256, 95)
                } else {
                    (camera_x as usize / 256, local_x.clamp(1, 95))
                };
                let first_row = region * rows_per_region + first_local_id - 1;
                let rows = &fixture.rows[first_row..first_row + 5];
                let texture = support::update_texture(&mut fixture.world, &rows[0], &mut order, generation);
                support::publish_texture_update(
                    &mut fixture.reconciler,
                    &mut fixture.view,
                    &fixture.world,
                    &mut fixture.now,
                    &rows[0],
                    texture,
                );
                if generation.is_multiple_of(20) {
                    let texture_indexes = [generation as usize % 508, (generation as usize + 1) % 508];
                    let refinement_size = 128 << ((generation as usize / 20).min(4));
                    support::complete_texture_refinements(&mut fixture.textures, texture_indexes, refinement_size, Instant::now());
                }
                for row in &rows[1..] {
                    support::update_one(&mut fixture.world, row, &mut order, generation);
                    support::first_publish(&mut fixture.reconciler, &mut fixture.view, &fixture.world, &mut fixture.now);
                }
                elapsed += fixture.sample(device, queue, camera_at(768.0 + step, step, 1024.0));
                latest = fixture.prepare.stats();
                assert!(latest.items_updated > 0, "live mix must include published scene updates");
                faces_patched += latest.faces_patched;
                assert_eq!(latest.omitted_faces, 0);
                complete_texture_work(&mut fixture.prepare, 5);
            }
            assert!(faces_patched > 0, "live mix must patch visible pose data");
            elapsed
        });
    });
    report("live_walk_reprepare_pose4_texture_arrivals", latest);
}

fn bench_animated(group: &mut criterion::BenchmarkGroup<'_, WallTime>, device: &wgpu::Device, queue: &wgpu::Queue) {
    let mut fixture = Fixture::settled(device, queue, 1, 50, ANIMATED_OBJECTS, 50.0, 512.0);
    let camera = camera_at(50.0, 0.0, 512.0);
    let mut visited = 0usize;
    let mut changed = 0usize;
    let mut latest = fixture.prepare.stats();
    group.bench_function("animated_500_objects", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                elapsed += fixture.sample(device, queue, camera);
                latest = fixture.prepare.stats();
                visited += latest.animated_faces_visited;
                changed += latest.animated_faces_changed;
                assert_eq!(latest.omitted_faces, 0);
                assert_eq!(latest.blended_sorts, 0, "parameter animation must not resort blended faces");
            }
            assert!(visited > 0, "animated scene must visit due faces");
            assert!(changed > 0, "animated scene must patch changed parameters");
            elapsed
        });
    });
    report("animated_500_objects", latest);
}

fn complete_texture_work(prepare: &mut GeometryPrepare, limit: usize) -> usize {
    let mut completed = 0;
    while completed < limit {
        let Some(job) = prepare.next_texture_job() else { break };
        prepare.complete_texture(job.run(render::CompressionQuality::Fast));
        completed += 1;
    }
    completed
}

fn work_distribution(values: impl Iterator<Item = usize>) -> String {
    let mut values = values.collect::<Vec<_>>();
    values.sort_unstable();
    let count = values.len();
    let percentile = |percent: usize| {
        let index = count.saturating_mul(percent).div_ceil(100).saturating_sub(1);
        values.get(index).copied().unwrap_or_default()
    };
    let total = values.iter().map(|value| *value as u128).sum::<u128>();
    format!(
        "n={count}, mean={:.1}, p50={}, p95={}, p99={}, max={}",
        total as f64 / count.max(1) as f64,
        percentile(50),
        percentile(95),
        percentile(99),
        values.last().copied().unwrap_or_default()
    )
}

fn prepare_sample(
    prepare: &mut GeometryPrepare,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    view: &SceneView,
    camera: Camera,
) -> PrepareSample {
    let mut encoder = device.create_command_encoder(&Default::default());
    let cpu_started = thread_cpu_cycles();
    let started = Instant::now();
    prepare.prepare(device, &mut encoder, view, camera, SIZE, None);
    let wall = started.elapsed();
    let cpu_cycles = cpu_started.zip(thread_cpu_cycles()).map(|(before, after)| after.saturating_sub(before));
    let stats = prepare.stats();
    black_box((stats.instances, stats.draw_calls, stats.draw_commands, stats.omitted_faces, stats.pending_meshes, stats.pending_textures));
    let submit_started = Instant::now();
    queue.submit([encoder.finish()]);
    prepare.submitted();
    let submit = submit_started.elapsed();
    let poll_started = Instant::now();
    device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).expect("benchmark GPU completion");
    PrepareSample { wall, cpu_cycles, submit, poll: poll_started.elapsed(), stats }
}

#[cfg(windows)]
fn thread_cpu_cycles() -> Option<u64> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThread() -> *mut ffi::c_void;
        fn QueryThreadCycleTime(thread: *mut ffi::c_void, cycles: *mut u64) -> i32;
    }

    let mut cycles = 0;
    let succeeded = unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut cycles) };
    (succeeded != 0).then_some(cycles)
}

#[cfg(not(windows))]
fn thread_cpu_cycles() -> Option<u64> {
    None
}

#[cfg(windows)]
fn process_memory() -> Option<ProcessMemory> {
    #[repr(C)]
    #[derive(Default)]
    struct ProcessMemoryCountersEx {
        cb: u32,
        _page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        _quota_peak_paged_pool_usage: usize,
        _quota_paged_pool_usage: usize,
        _quota_peak_nonpaged_pool_usage: usize,
        _quota_nonpaged_pool_usage: usize,
        _pagefile_usage: usize,
        peak_pagefile_usage: usize,
        private_usage: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut ffi::c_void;
    }

    #[link(name = "psapi")]
    unsafe extern "system" {
        fn GetProcessMemoryInfo(process: *mut ffi::c_void, counters: *mut ProcessMemoryCountersEx, size: u32) -> i32;
    }

    let mut counters = ProcessMemoryCountersEx { cb: size_of::<ProcessMemoryCountersEx>() as u32, ..Default::default() };
    let size = counters.cb;
    let succeeded = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) };
    (succeeded != 0).then_some(ProcessMemory {
        private_bytes: counters.private_usage,
        working_set_bytes: counters.working_set_size,
        peak_working_set_bytes: counters.peak_working_set_size,
        peak_commit_bytes: counters.peak_pagefile_usage,
    })
}

#[cfg(not(windows))]
fn process_memory() -> Option<ProcessMemory> {
    None
}

fn record_process_memory(current: &mut Option<ProcessMemory>, peak: &mut Option<ProcessMemory>) {
    if let Some(memory) = process_memory() {
        *current = Some(memory);
        *peak = Some((*peak).map_or(memory, |previous| previous.max(memory)));
    }
}

fn duration_ms(milliseconds: f64) -> Duration {
    Duration::from_secs_f64(milliseconds.max(0.0) / 1000.0)
}

fn report(name: &str, stats: render::Stats) {
    eprintln!(
        "{name}: draw_submissions={}, draw_commands={}, omitted_faces={}",
        stats.draw_calls, stats.draw_commands, stats.omitted_faces
    );
}

fn camera_at(x: f64, rotation: f64, draw_distance: f64) -> Camera {
    camera_yaw_at(x, rotation * 0.00005, draw_distance)
}

fn camera_yaw_at(x: f64, yaw: f64, draw_distance: f64) -> Camera {
    camera_yaw_from(DVec3::new(x, 15.0, 120.0), yaw, draw_distance)
}

fn camera_yaw_from(eye: DVec3, yaw: f64, draw_distance: f64) -> Camera {
    let direction = Vec3::new(yaw.sin() as f32, 0.0, -yaw.cos() as f32);
    Camera::new(eye, direction, PI / 2.0, SIZE)
        .expect("benchmark camera")
        .with_quality(draw_distance, 1.0)
        .expect("benchmark camera quality")
}
