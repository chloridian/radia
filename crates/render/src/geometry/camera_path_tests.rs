use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

use math::{DVec3, Vec3};
use wgpu::{Device, PollType, Queue, TextureFormat};
use world::{PrimShape, World};

use super::{
    Accounting, GeometryRenderer, MESH_BYTES, Resource,
    fixtures::{object_scene_with, settle, vulkan_device},
    items::{CachedFace, ItemId},
    visibility::{VisibilityBands, WorldSphere},
};
use crate::{
    FrameInput,
    pipeline::projection,
    scene::{Camera, Reconciler, SceneView},
};

const OBJECTS: u32 = 3_000;
const SIZE: [u32; 2] = [1280, 720];
const DRAW_DISTANCE: f64 = 96.0;
const EYE_HEIGHT: f64 = 25.0;
const FRAME: Duration = Duration::from_micros(16_667);
const SETTLE_FRAMES: u32 = 4_000;

#[derive(Clone, Copy, Debug)]
enum Regime {
    Resident,
    Streaming,
    Pressured,
}

#[derive(Default)]
struct Latencies(Vec<u32>);

impl Latencies {
    fn percentile(&self, percent: usize) -> u32 {
        let mut sorted = self.0.clone();
        sorted.sort_unstable();
        sorted.get((sorted.len().saturating_sub(1)) * percent / 100).copied().unwrap_or_default()
    }
}

impl fmt::Display for Latencies {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "n={} p50={} p95={} max={}",
            self.0.len(),
            self.percentile(50),
            self.percentile(95),
            self.0.iter().max().copied().unwrap_or(0)
        )
    }
}

#[derive(Default)]
struct Report {
    frames: u32,
    commits: u32,
    stale: BTreeMap<&'static str, u32>,
    invalidations: BTreeMap<&'static str, u32>,
    phases: BTreeMap<&'static str, u32>,
    building_frames: u32,
    draining_frames: u32,
    required_frames: u32,
    committed_builds: Latencies,
    bytes_changes: u32,
    revision_changes: u32,
    retry_frames: u32,
    longest_retry: u32,
    restart_frames: u32,
    appearance_frames: u32,
    budget_blocked_frames: u32,
    first_entry: Latencies,
    reentry: Latencies,
    unresolved: Latencies,
    abandoned: u32,
    pop_outs: u32,
    margin_pop_outs: u32,
    returned_during_build: u32,
    examples: Vec<String>,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let per_hundred = f64::from(self.commits) * 100.0 / f64::from(self.frames.max(1));
        write!(
            f,
            "frames={} commits={} commits_per_100={per_hundred:.1} stale={:?} invalidations={:?} build_frames=({}) building={} draining={} layout_required={} phases={:?} mesh_bytes_changes={} mesh_revision_changes={} retry_frames={} longest_retry={} restart_frames={} appearance_frames={} budget_blocked_frames={} pop_in_first=({}) pop_in_reentry=({}) unresolved=({}) abandoned={} returned_during_build={} pop_outs={} margin_pop_outs={} examples={:?}",
            self.frames,
            self.commits,
            self.stale,
            self.invalidations,
            self.committed_builds,
            self.building_frames,
            self.draining_frames,
            self.required_frames,
            self.phases,
            self.bytes_changes,
            self.revision_changes,
            self.retry_frames,
            self.longest_retry,
            self.restart_frames,
            self.appearance_frames,
            self.budget_blocked_frames,
            self.first_entry,
            self.reentry,
            self.unresolved,
            self.abandoned,
            self.returned_during_build,
            self.pop_outs,
            self.margin_pop_outs,
            self.examples,
        )
    }
}

#[derive(Default)]
struct Track {
    shown: bool,
    ever_shown: bool,
    waiting_since: Option<u32>,
    slots: Vec<u32>,
    hidden_during: Option<u32>,
}

struct Path {
    name: &'static str,
    cameras: Vec<Camera>,
}

struct Rig {
    device: Device,
    queue: Queue,
    geometry: GeometryRenderer,
    world: World,
    scene: Reconciler,
    view: SceneView,
    now: Instant,
}

fn camera(position: DVec3, yaw_degrees: f64) -> Camera {
    let yaw = yaw_degrees.to_radians();
    Camera::new(position, Vec3::new(yaw.cos() as f32, yaw.sin() as f32, 0.0), 1.0, SIZE)
        .expect("path camera")
        .with_quality(DRAW_DISTANCE, 1.0)
        .expect("path draw distance")
}

fn yaw_sweep() -> Path {
    let center = DVec3::new(128.0, 128.0, EYE_HEIGHT);
    Path { name: "yaw_sweep", cameras: (0..240).map(|frame| camera(center, f64::from(frame) * 1.5)).collect() }
}

fn yaw_reversal() -> Path {
    let center = DVec3::new(128.0, 128.0, EYE_HEIGHT);
    let yaw = |frame: u32| {
        let phase = f64::from(frame % 120) * 2.0;
        if phase < 120.0 { phase - 60.0 } else { 180.0 - phase }
    };
    Path { name: "yaw_reversal", cameras: (0..360).map(|frame| camera(center, yaw(frame))).collect() }
}

fn walk() -> Path {
    Path {
        name: "walk",
        cameras: (0..320).map(|frame| camera(DVec3::new(272.0 - f64::from(frame) * 0.1, 128.0, EYE_HEIGHT), 180.0)).collect(),
    }
}

fn layout(id: u32) -> (Vec3, PrimShape) {
    let mut state = 0x9E37_79B9_7F4A_7C15u64 ^ u64::from(id).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % 10_000) as f32 / 10_000.0
    };
    let position = Vec3::new(4.0 + next() * 248.0, 4.0 + next() * 248.0, 20.0 + next() * 10.0);
    let (path_curve, profile_curve) = if id.is_multiple_of(2) { (0x20, 0x05) } else { (0x10, 0x01) };
    let twist = i8::try_from(i64::from(id / 2 % 200) - 100).expect("twist fits");
    let taper = i8::try_from(id / 400 % 10 * 10).expect("taper fits");
    (position, PrimShape { path_curve, profile_curve, path_twist: twist, path_taper_x: taper, ..PrimShape::default() })
}

impl Rig {
    fn new(first: Camera) -> Self {
        let (world, scene, view) = object_scene_with(OBJECTS, first, layout);
        let (device, queue) = vulkan_device();
        let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
        geometry.memory = Arc::new(Accounting::default());
        Self { device, queue, geometry, world, scene, view, now: Instant::now() }
    }

    fn frame(&mut self, camera: Camera) {
        self.scene.set_camera(camera);
        settle(&self.world, &mut self.scene, &mut self.view);
        let lease = self.geometry.admit().expect("frame admitted");
        let mut encoder = self.device.create_command_encoder(&Default::default());
        self.geometry.prepare_at(&self.device, &mut encoder, &self.view, None, camera, SIZE, FrameInput::default(), self.now);
        self.queue.submit([encoder.finish()]);
        self.geometry.submitted(lease);
        self.device.poll(PollType::Wait { submission_index: None, timeout: None }).expect("frame completes");
        self.now += FRAME;
        for draw in self.geometry.draw_list.draws() {
            assert!(self.geometry.meshes.contains_key(&draw.mesh), "committed draw lost GPU mesh residency");
            assert!(self.geometry.draw_list.committed_pins_mesh(draw.mesh), "committed draw lost its mesh pin");
        }
    }

    fn settle_at(&mut self, camera: Camera) {
        for _ in 0..SETTLE_FRAMES {
            self.frame(camera);
            let geometry = &self.geometry;
            if geometry.cache.rebuild.is_none()
                && geometry.cache.version.as_ref() == Some(&self.view.version())
                && geometry.draw_list.valid()
                && !geometry.draw_list.layout_pending()
                && !geometry.layout_required
                && geometry.stats.pending_meshes == 0
                && !geometry.stats.rebuild_pending
            {
                return;
            }
        }
        panic!("renderer did not settle");
    }

    fn walk(&mut self, path: &Path) -> Report {
        let mut report = Report::default();
        let mut tracks = HashMap::<ItemId, Track>::new();
        let mut state = self.geometry.memory.mesh_state();
        let mut retry_run = 0u32;
        let mut build_started = None;
        for (frame_index, camera) in (0u32..).zip(path.cameras.iter().copied()) {
            let was_building = self.geometry.draw_list.layout_building();
            self.frame(camera);
            let geometry = &self.geometry;
            report.frames += 1;
            let committed_now = geometry.stats.layouts > 0;
            report.commits += u32::from(committed_now);
            if let Some(reason) = geometry.stats.layout_stale_reason {
                *report.stale.entry(reason).or_default() += 1;
            }
            if let Some(reason) = geometry.stats.layout_invalidation_reason {
                *report.invalidations.entry(reason).or_default() += 1;
            }
            let building = geometry.draw_list.layout_building();
            if building && !was_building {
                build_started = Some(frame_index);
            }
            if committed_now && let Some(started) = build_started.take() {
                report.committed_builds.0.push(frame_index - started + 1);
            }
            if !building && geometry.stats.layout_stale_reason.is_some() {
                build_started = None;
            }
            report.building_frames += u32::from(building);
            report.draining_frames += u32::from(geometry.draw_list.layout_pending() && !building);
            report.required_frames += u32::from(geometry.layout_required);
            if let Some((phase, ..)) = geometry.draw_list.staged_progress() {
                *report.phases.entry(phase).or_default() += 1;
            }
            let current = geometry.memory.mesh_state();
            report.bytes_changes += u32::from(current.bytes != state.bytes);
            report.revision_changes += u32::from(current.revision != state.revision);
            state = current;
            retry_run = if geometry.replacement_retry_pending { retry_run + 1 } else { 0 };
            report.retry_frames += u32::from(geometry.replacement_retry_pending);
            report.longest_retry = report.longest_retry.max(retry_run);
            report.restart_frames += u32::from(geometry.mesh_admission.restart_pending);
            report.appearance_frames += u32::from(geometry.first_appearances.has_pending());
            report.budget_blocked_frames += u32::from(geometry.stats.mesh_budget_blocked > 0);

            let bands = VisibilityBands::new(projection(camera, SIZE), SIZE);
            for (_, item) in geometry.cache.items.iter() {
                let sphere = WorldSphere::new(item.position, item.rotation, item.scale, item.info.bounds, camera);
                let strict = sphere.is_some_and(|sphere| sphere.visible(item.shading, camera, &bands.strict));
                let retained = sphere.is_some_and(|sphere| sphere.visible(item.shading, camera, &bands.retained));
                let resident = geometry.meshes.contains_key(&item.mesh.id());
                let track =
                    tracks.entry(item.id).or_insert_with(|| Track { slots: committed_slots(geometry, &item.faces), ..Track::default() });
                if committed_now {
                    track.slots = committed_slots(geometry, &item.faces);
                }
                let shown = drawn(geometry, &track.slots);
                if track.shown && retained && !shown {
                    if strict {
                        report.pop_outs += 1;
                        if report.examples.len() < 4 {
                            report.examples.push(format!("frame={frame_index} item={:?} building={building}", item.id.1));
                        }
                    } else {
                        report.margin_pop_outs += 1;
                    }
                }
                if track.shown && !shown {
                    track.hidden_during = build_started;
                }
                if shown {
                    if let Some(since) = track.waiting_since.take() {
                        let latency = frame_index - since;
                        if track.ever_shown { report.reentry.0.push(latency) } else { report.first_entry.0.push(latency) }
                    }
                    track.ever_shown = true;
                    track.hidden_during = None;
                } else if strict && resident {
                    if track.waiting_since.is_none() && track.hidden_during.is_some() && track.hidden_during == build_started {
                        report.returned_during_build += 1;
                    }
                    track.waiting_since.get_or_insert(frame_index);
                } else if !strict && track.waiting_since.take().is_some() {
                    report.abandoned += 1;
                }
                track.shown = shown;
            }
        }
        let frames = report.frames;
        report.unresolved.0.extend(tracks.values().filter_map(|track| track.waiting_since.map(|since| frames - since)));
        report
    }
}

fn committed_slots(geometry: &GeometryRenderer, faces: &[CachedFace]) -> Vec<u32> {
    faces.iter().filter_map(|face| geometry.draw_list.committed_slot(face)).collect()
}

fn drawn(geometry: &GeometryRenderer, slots: &[u32]) -> bool {
    slots.iter().any(|slot| {
        !geometry.draw_list.hides_range(*slot, 1)
            && geometry.draw_list.instance_transform(*slot).is_some_and(|bytes| bytes.iter().any(|byte| *byte != 0))
    })
}

fn run(path: &Path, regime: Regime) {
    let first = path.cameras[0];
    let mut rig = Rig::new(first);
    rig.settle_at(first);
    if matches!(regime, Regime::Resident) {
        rig.walk(path);
        rig.settle_at(first);
    }
    let _pressure = matches!(regime, Regime::Pressured).then(|| {
        let used = rig.geometry.memory.mesh_state().bytes;
        let headroom = (used / 2).max(1024 * 1024);
        rig.geometry.memory.reserve(MESH_BYTES.saturating_sub(used + headroom), Resource::Mesh).expect("pressure reservation fits")
    });
    let report = rig.walk(path);
    println!("camera_path path={} regime={regime:?} mesh_bytes={} {report}", path.name, rig.geometry.memory.mesh_state().bytes);
    assert_eq!(report.pop_outs, 0, "{} {regime:?}: items on screen lost their committed draw: {:?}", path.name, report.examples);
}

fn run_regimes(path: &Path) {
    for regime in [Regime::Resident, Regime::Streaming, Regime::Pressured] {
        run(path, regime);
    }
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --release --run-ignored only -E 'test(camera_path)' --no-capture --test-threads=1"]
fn gpu_camera_path_yaw_sweep() {
    run_regimes(&yaw_sweep());
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --release --run-ignored only -E 'test(camera_path)' --no-capture --test-threads=1"]
fn gpu_camera_path_yaw_reversal() {
    run_regimes(&yaw_reversal());
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --release --run-ignored only -E 'test(camera_path)' --no-capture --test-threads=1"]
fn gpu_camera_path_walk() {
    run_regimes(&walk());
}
