use std::{
    f64::consts::PI,
    hint::black_box,
    time::{Duration, Instant},
};

use futures::executor::block_on;
use math::{DVec3, Vec3};
use render::{
    GeometryPrepare,
    scene::{Camera, DrawnMeshKey, Reconciler, SceneView},
};
use wgpu::{Device, DeviceDescriptor, PowerPreference, Queue, RequestAdapterOptions, TextureFormat};
use world::World;

use crate::support;

const OBJECTS: usize = 3_000;
const TEXTURES: usize = 512;
const ANIMATED_OBJECTS: usize = 500;
const OBJECTS_87K_FACES: usize = 14_500;
const SETTLE_FRAMES: usize = 256;
const SIZE: [u32; 2] = [1024, 1024];

struct PrepareSample {
    wall: Duration,
    stats: render::Stats,
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
    fn settled(device: &Device, queue: &Queue, regions: usize, meshes: usize, animated: usize, start_x: f64, draw_distance: f64) -> Self {
        Self::settled_with_objects(device, queue, OBJECTS, regions, meshes, animated, start_x, draw_distance)
    }

    fn settled_with_objects(
        device: &Device,
        queue: &Queue,
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

    fn settle(device: &Device, queue: &Queue, world: World, rows: Vec<support::Row>, regions: usize, camera: Camera) -> Self {
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

    fn sample(&mut self, device: &Device, queue: &Queue, camera: Camera) -> Duration {
        prepare_sample(&mut self.prepare, device, queue, &self.view, camera).wall
    }
}

fn gpu() -> (Device, Queue) {
    let instance = wgpu::Instance::default();
    let adapter = block_on(instance.request_adapter(&RequestAdapterOptions {
        power_preference: PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        ..Default::default()
    }))
    .expect("benchmark GPU adapter");
    eprintln!("render prepare benchmark GPU: {:?}", adapter.get_info());
    block_on(adapter.request_device(&DeviceDescriptor {
        required_features: adapter.features() & (wgpu::Features::TEXTURE_COMPRESSION_BC | wgpu::Features::INDIRECT_FIRST_INSTANCE),
        ..Default::default()
    }))
    .expect("benchmark GPU device")
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

fn prepare_sample(prepare: &mut GeometryPrepare, device: &Device, queue: &Queue, view: &SceneView, camera: Camera) -> PrepareSample {
    let mut encoder = device.create_command_encoder(&Default::default());
    let started = Instant::now();
    prepare.prepare(device, &mut encoder, view, camera, SIZE, None);
    let wall = started.elapsed();
    let stats = prepare.stats();
    black_box((stats.instances, stats.draw_calls, stats.draw_commands, stats.omitted_faces, stats.pending_meshes, stats.pending_textures));
    queue.submit([encoder.finish()]);
    prepare.submitted();
    device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).expect("benchmark GPU completion");
    PrepareSample { wall, stats }
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

fn history_loss(bencher: divan::Bencher, device: &Device, queue: &Queue, objects: usize, layout_change: bool) {
    let (mut world, rows) = support::scene_world(objects, 1);
    let textures = support::benchmark_textures(1);
    let camera = camera_at(128.0, 0.0, 1024.0);
    let (mut reconciler, mut view, mut now) = support::settle_with_textures(&world, objects, camera, &textures.textures);
    let mut prepare = GeometryPrepare::new(device, queue, TextureFormat::Rgba8UnormSrgb);
    for _ in 0..objects.div_ceil(128) * 2 + 24 {
        prepare_sample(&mut prepare, device, queue, &view, camera);
        complete_texture_work(&mut prepare, 8);
        let stats = prepare.stats();
        if !stats.rebuild_pending && stats.pending_meshes == 0 && stats.pending_textures == 0 {
            break;
        }
    }
    assert!(prepare.stats().instances > 0, "history-loss fixture draws objects before timing");
    assert_eq!(prepare.stats().pending_meshes, 0);
    assert_eq!(prepare.stats().pending_textures, 0);

    let mut order = 20_000_000;
    let mut generation = 0u64;
    let mut latest = prepare.stats();
    bencher.bench_local(|| {
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
        loop {
            let sample = prepare_sample(&mut prepare, device, queue, &view, camera);
            latest = sample.stats;
            complete_texture_work(&mut prepare, 8);
            turns += 1;
            if !latest.rebuild_pending && latest.pending_meshes == 0 && latest.pending_textures == 0 {
                break;
            }
            assert!(turns < objects.div_ceil(128) * 2 + 24, "history-loss rebuild finishes within bounded pages");
        }
        if layout_change {
            assert!(latest.layouts > 0, "layout-change catch-up visits the final visible list");
        }
        black_box(latest);
    });
    report(if layout_change { "history_loss_layout_rebuild" } else { "history_loss_rebuild" }, latest);
}

fn region_load_progress(bencher: divan::Bencher, device: &Device, queue: &Queue, objects: usize) {
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
    let mut prepare = GeometryPrepare::new(device, queue, TextureFormat::Rgba8UnormSrgb);
    let mut latest = prepare.stats();
    bencher.bench_local(|| {
        for turn in 0..60usize {
            for (index, row) in rows.iter().take(128).enumerate() {
                generation = generation.wrapping_add(1);
                support::update_mesh_one(&mut world, row, index, &mut order, generation);
            }
            support::first_publish(&mut reconciler, &mut view, &world, &mut now);
            let fast_steps = (turn + 1).min(2);
            let slow_steps = (turn + 1).saturating_sub(2).min(28);
            let walk = fast_steps as f64 * 0.3 + slow_steps as f64 * (0.7 / 28.0);
            latest = prepare_sample(&mut prepare, device, queue, &view, camera_at(256.5 - walk, turn as f64, 768.0)).stats;
            complete_texture_work(&mut prepare, 8);
        }
        let quiet_camera = camera_at(255.5, 3.0, 768.0);
        let mut quiet_turns = 0;
        while prepare.needs_frame(Instant::now()) && quiet_turns < 1_500 {
            latest = prepare_sample(&mut prepare, device, queue, &view, quiet_camera).stats;
            complete_texture_work(&mut prepare, 8);
            quiet_turns += 1;
        }
        assert!(!prepare.needs_frame(Instant::now()), "region load progress settles within 1,500 quiet turns");
        assert!(!latest.rebuild_pending && latest.pending_meshes == 0 && latest.pending_textures == 0);
        black_box((latest, quiet_turns));
    });
    report("region_load_progress", latest);
}

fn camera_culling_rotation(
    bencher: divan::Bencher,
    mut fixture: Fixture,
    cameras: [Camera; 2],
    name: &'static str,
    device: &Device,
    queue: &Queue,
) {
    let initial = fixture.prepare.stats();
    let mut rotated = initial;
    for camera in cameras {
        fixture.sample(device, queue, camera);
        rotated = fixture.prepare.stats();
        assert_eq!(rotated.pending_meshes, 0);
        assert_eq!(rotated.pending_textures, 0);
        assert_eq!(rotated.omitted_faces, 0);
        assert_eq!(rotated.layouts, 0, "small in-place rotation must preserve range membership");
        assert_eq!(rotated.instances, initial.instances);
        assert_eq!(rotated.draw_calls, initial.draw_calls);
        assert_eq!(rotated.draw_commands, initial.draw_commands);
    }
    eprintln!(
        "{name}: stable membership instances={}->{} layouts={}->{}",
        initial.instances, rotated.instances, initial.layouts, rotated.layouts
    );
    let mut generation = 0usize;
    bencher.bench_local(|| {
        generation = generation.wrapping_add(1);
        let sample = fixture.sample(device, queue, cameras[generation % cameras.len()]);
        let stats = fixture.prepare.stats();
        assert_eq!(stats.pending_meshes, 0);
        assert_eq!(stats.pending_textures, 0);
        assert_eq!(stats.omitted_faces, 0);
        assert_eq!(stats.layouts, 0);
        black_box(sample);
    });
    report(name, fixture.prepare.stats());
}

fn camera_edge_sweep(bencher: divan::Bencher, device: &Device, queue: &Queue, objects: usize, name: &'static str) {
    let mut fixture = Fixture::settled_with_objects(device, queue, objects, 7, 50, 0, 896.0, 1024.0);
    let cameras = [camera_yaw_at(896.0, 0.0, 1024.0), camera_yaw_at(896.0, 0.35, 1024.0)];
    fixture.sample(device, queue, cameras[0]);
    let first = fixture.prepare.stats();
    let mut second = first;
    let mut settle_layouts = 0;
    let mut settled = false;
    for _ in 1..=SETTLE_FRAMES {
        fixture.sample(device, queue, cameras[1]);
        second = fixture.prepare.stats();
        settle_layouts += second.layouts;
        if !second.rebuild_pending {
            settled = true;
            break;
        }
    }
    assert!(settled, "edge sweep layout must settle within {SETTLE_FRAMES} frames");
    assert_ne!(first.draw_commands, second.draw_commands, "edge sweep must change the emitted commands");
    assert_eq!(settle_layouts, 0, "turning in place must keep range membership");
    let mut generation = 0usize;
    bencher.bench_local(|| {
        let camera_index = generation % cameras.len();
        generation = generation.wrapping_add(1);
        fixture.sample(device, queue, cameras[camera_index]);
        let stats = fixture.prepare.stats();
        assert_eq!(stats.pending_meshes, 0);
        assert_eq!(stats.pending_textures, 0);
        assert_eq!(stats.omitted_faces, 0);
        assert_eq!(stats.layouts, 0);
        black_box(stats);
    });
    report(name, fixture.prepare.stats());
}

mod benches {
    use super::*;

    #[divan::bench]
    fn static_camera(bencher: divan::Bencher) {
        let (device, queue) = gpu();
        let mut fixture = Fixture::settled(&device, &queue, 1, 50, 0, 50.0, 512.0);
        let camera = camera_at(50.0, 0.0, 512.0);
        fixture.sample(&device, &queue, camera);
        bencher.bench_local(|| {
            fixture.sample(&device, &queue, camera);
            let stats = fixture.prepare.stats();
            assert_eq!(stats.pending_meshes, 0);
            assert_eq!(stats.pending_textures, 0);
            assert_eq!(stats.omitted_faces, 0);
            assert_eq!(stats.layouts, 0);
            assert_eq!(stats.items_updated, 0);
            assert_eq!(stats.faces_patched, 0);
            black_box(stats);
        });
        report("unchanged_static_camera", fixture.prepare.stats());
    }

    #[divan::bench]
    fn camera_walk_multi_region(bencher: divan::Bencher) {
        let (device, queue) = gpu();
        let mut fixture = Fixture::settled(&device, &queue, 7, 50, 0, 896.0, 1024.0);
        let mut generation = 0usize;
        let mut layouts = 0usize;
        bencher.bench_local(|| {
            generation = generation.wrapping_add(1);
            let step = (generation % 2048) as f64 * 0.25;
            fixture.sample(&device, &queue, camera_at(768.0 + step, step, 1024.0));
            let stats = fixture.prepare.stats();
            layouts += stats.layouts;
            assert_eq!(stats.items_updated, 0);
            assert_eq!(stats.pending_meshes, 0);
            assert_eq!(stats.pending_textures, 0);
            assert_eq!(stats.omitted_faces, 0);
            black_box(stats);
        });
        assert!(layouts > 0, "multi-region camera walk must relayout when visible items change");
        report("camera_walk_025_multi_region", fixture.prepare.stats());
    }

    #[divan::bench]
    fn cull_rotation_3k(bencher: divan::Bencher) {
        let (device, queue) = gpu();
        let fixture = Fixture::settled_with_objects(&device, &queue, OBJECTS, 1, 50, 0, 128.0, 128.0);
        let cameras = [camera_yaw_at(128.0, -0.001, 128.0), camera_yaw_at(128.0, 0.001, 128.0)];
        camera_culling_rotation(bencher, fixture, cameras, "camera_culling_rotation_3000_stable", &device, &queue);
    }

    #[divan::bench]
    fn cull_rotation_10k(bencher: divan::Bencher) {
        let (device, queue) = gpu();
        let fixture = Fixture::settled_with_objects(&device, &queue, 10_000, 1, 50, 0, 128.0, 128.0);
        let cameras = [camera_yaw_at(128.0, -0.001, 128.0), camera_yaw_at(128.0, 0.001, 128.0)];
        camera_culling_rotation(bencher, fixture, cameras, "camera_culling_rotation_10000_stable", &device, &queue);
    }

    #[divan::bench]
    fn cull_rotation_87k_blended(bencher: divan::Bencher) {
        let (device, queue) = gpu();
        let (mut world, rows) = support::live_prepare_world_with_regions(OBJECTS_87K_FACES, 50, TEXTURES, 1);
        support::blend_alternate_objects(&mut world, &rows);
        let overhead = DVec3::new(128.0, 72.0, 160.0);
        let fixture = Fixture::settle(&device, &queue, world, rows, 1, camera_yaw_from(overhead, 0.0, 512.0));
        let cameras = [camera_yaw_from(overhead, -0.001, 512.0), camera_yaw_from(overhead, 0.001, 512.0)];
        camera_culling_rotation(bencher, fixture, cameras, "camera_culling_rotation_87k_faces_half_blended", &device, &queue);
    }

    #[divan::bench]
    fn cull_edge_sweep_3k(bencher: divan::Bencher) {
        let (device, queue) = gpu();
        camera_edge_sweep(bencher, &device, &queue, OBJECTS, "camera_culling_edge_sweep_3000");
    }

    #[divan::bench]
    fn cull_edge_sweep_10k(bencher: divan::Bencher) {
        let (device, queue) = gpu();
        camera_edge_sweep(bencher, &device, &queue, 10_000, "camera_culling_edge_sweep_10000");
    }

    #[divan::bench]
    fn one_pose_change(bencher: divan::Bencher) {
        let (device, queue) = gpu();
        let mut fixture = Fixture::settled(&device, &queue, 1, 50, 0, 50.0, 512.0);
        let camera = camera_at(50.0, 0.0, 512.0);
        let mut generation = 0;
        let mut order = 20_000_000;
        bencher.bench_local(|| {
            generation += 1;
            support::update_one(&mut fixture.world, &fixture.rows[0], &mut order, generation);
            support::first_publish(&mut fixture.reconciler, &mut fixture.view, &fixture.world, &mut fixture.now);
            fixture.sample(&device, &queue, camera);
            let stats = fixture.prepare.stats();
            assert!(stats.items_updated > 0);
            assert!(stats.faces_patched > 0);
            assert_eq!(stats.omitted_faces, 0);
            black_box(stats);
        });
        report("one_pose_change", fixture.prepare.stats());
    }

    #[divan::bench]
    fn stationary_50_meshes(bencher: divan::Bencher) {
        run_reprepare(bencher, 50, "stationary_one_reprepare_50_meshes");
    }

    #[divan::bench]
    fn reprepare_3k_meshes(bencher: divan::Bencher) {
        run_reprepare(bencher, OBJECTS, "one_reprepare_3000_unique_meshes");
    }

    #[divan::bench]
    fn camera_change_128_meshes(bencher: divan::Bencher) {
        let (device, queue) = gpu();
        let (_reconciler, view, _world, _now) = support::ready_distinct_mesh_scene(128);
        let camera = Camera::new(DVec3::ZERO, Vec3::X, PI / 3.0, SIZE).expect("mesh preparation camera");
        let mut prepare = GeometryPrepare::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
        for _ in 0..16 {
            prepare_sample(&mut prepare, &device, &queue, &view, camera);
            if !prepare.stats().rebuild_pending && prepare.stats().pending_meshes == 0 {
                break;
            }
        }
        assert_eq!(prepare.stats().pending_meshes, 0);
        assert!(!prepare.stats().rebuild_pending);
        let mut generation = 0usize;
        bencher.bench_local(|| {
            generation = generation.wrapping_add(1);
            let camera = Camera::new(DVec3::new((generation % 2) as f64 * 0.01, 0.0, 0.0), Vec3::X, PI / 3.0, SIZE)
                .expect("mesh preparation camera");
            let _ = prepare_sample(&mut prepare, &device, &queue, &view, camera);
            let stats = prepare.stats();
            assert_eq!(stats.pending_meshes, 0);
            assert_eq!(stats.omitted_faces, 0);
            black_box(stats);
        });
        report("camera_change_128_distinct_mesh_assets", prepare.stats());
    }

    #[divan::bench]
    fn animated_500(bencher: divan::Bencher) {
        let (device, queue) = gpu();
        let mut fixture = Fixture::settled(&device, &queue, 1, 50, ANIMATED_OBJECTS, 50.0, 512.0);
        let camera = camera_at(50.0, 0.0, 512.0);
        let mut visited = 0usize;
        let mut changed = 0usize;
        bencher.bench_local(|| {
            fixture.sample(&device, &queue, camera);
            let stats = fixture.prepare.stats();
            visited += stats.animated_faces_visited;
            changed += stats.animated_faces_changed;
            assert_eq!(stats.omitted_faces, 0);
            assert_eq!(stats.blended_sorts, 0);
            black_box(stats);
        });
        assert!(visited > 0);
        assert!(changed > 0);
        report("animated_500_objects", fixture.prepare.stats());
    }

    #[divan::bench]
    fn live_walk_textures(bencher: divan::Bencher) {
        let (device, queue) = gpu();
        run_live_mix(bencher, &device, &queue);
    }

    #[divan::bench]
    fn history_loss_1k(bencher: divan::Bencher) {
        let (device, queue) = gpu();
        history_loss(bencher, &device, &queue, 1_000, false);
    }

    #[divan::bench]
    fn history_loss_16k(bencher: divan::Bencher) {
        let (device, queue) = gpu();
        history_loss(bencher, &device, &queue, 16_000, false);
    }

    #[divan::bench]
    fn history_loss_65k(bencher: divan::Bencher) {
        let (device, queue) = gpu();
        history_loss(bencher, &device, &queue, 65_000, false);
    }

    #[divan::bench]
    fn history_loss_layout_1k(bencher: divan::Bencher) {
        let (device, queue) = gpu();
        history_loss(bencher, &device, &queue, 1_000, true);
    }

    #[divan::bench]
    fn history_loss_layout_16k(bencher: divan::Bencher) {
        let (device, queue) = gpu();
        history_loss(bencher, &device, &queue, 16_000, true);
    }

    #[divan::bench]
    fn history_loss_layout_65k(bencher: divan::Bencher) {
        let (device, queue) = gpu();
        history_loss(bencher, &device, &queue, 65_000, true);
    }

    #[divan::bench]
    fn region_load_5k(bencher: divan::Bencher) {
        let (device, queue) = gpu();
        region_load_progress(bencher, &device, &queue, 5_000);
    }

    #[divan::bench]
    fn region_load_65k(bencher: divan::Bencher) {
        let (device, queue) = gpu();
        region_load_progress(bencher, &device, &queue, 65_000);
    }
}

fn run_reprepare(bencher: divan::Bencher, mesh_count: usize, name: &'static str) {
    let (device, queue) = gpu();
    let mut fixture = Fixture::settled(&device, &queue, 1, mesh_count, 0, 50.0, 512.0);
    let camera = camera_at(50.0, 0.0, 512.0);
    let mut generation = 0;
    let mut order = 20_000_000;
    let mut frames = 0usize;
    let mut max_frames = 0usize;
    bencher.bench_local(|| {
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
        fixture.sample(&device, &queue, camera);
        let mut stats = fixture.prepare.stats();
        assert!(stats.items_updated > 0);
        let mut layouts = stats.layouts;
        let mut settle_frames = 1;
        while stats.rebuild_pending && settle_frames < SETTLE_FRAMES {
            fixture.sample(&device, &queue, camera);
            stats = fixture.prepare.stats();
            layouts += stats.layouts;
            settle_frames += 1;
        }
        assert!(!stats.rebuild_pending);
        assert_eq!(layouts, 1);
        assert_eq!(stats.omitted_faces, 0);
        frames += settle_frames;
        max_frames = max_frames.max(settle_frames);
        black_box(stats);
    });
    eprintln!("{name}: frames_per_change mean={:.1} max={max_frames}", frames as f64 / generation.max(1) as f64);
    report(name, fixture.prepare.stats());
}

fn run_live_mix(bencher: divan::Bencher, device: &Device, queue: &Queue) {
    let mut fixture = Fixture::settled(device, queue, 7, 50, 0, 768.0, 1024.0);
    let mut generation = 0u64;
    let mut order = 20_000_000;
    let mut faces_patched = 0usize;
    let rows_per_region = OBJECTS.div_ceil(7);
    bencher.bench_local(|| {
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
        support::publish_texture_update(&mut fixture.reconciler, &mut fixture.view, &fixture.world, &mut fixture.now, &rows[0], texture);
        if generation.is_multiple_of(20) {
            let texture_indexes = [generation as usize % 508, (generation as usize + 1) % 508];
            let refinement_size = 128 << ((generation as usize / 20).min(4));
            support::complete_texture_refinements(&mut fixture.textures, texture_indexes, refinement_size, Instant::now());
        }
        for row in &rows[1..] {
            support::update_one(&mut fixture.world, row, &mut order, generation);
            support::first_publish(&mut fixture.reconciler, &mut fixture.view, &fixture.world, &mut fixture.now);
        }
        fixture.sample(device, queue, camera_at(768.0 + step, step, 1024.0));
        let stats = fixture.prepare.stats();
        assert!(stats.items_updated > 0);
        faces_patched += stats.faces_patched;
        assert_eq!(stats.omitted_faces, 0);
        complete_texture_work(&mut fixture.prepare, 5);
        black_box(stats);
    });
    assert!(faces_patched > 0);
    report("live_walk_reprepare_pose4_texture_arrivals", fixture.prepare.stats());
}
