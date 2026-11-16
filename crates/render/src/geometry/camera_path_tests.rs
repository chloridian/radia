use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

use math::{DVec3, Frustum, Vec3};
use wgpu::{Device, PollType, Queue, TextureFormat};
use world::{PrimShape, World};

use super::{
    Accounting, GeometryRenderer, INDIRECT_DRAW_BYTES, MESH_BYTES, Resource,
    fixtures::{object_scene_with, settle, vulkan_device},
    items::{CachedFace, ItemId},
    visibility::WorldSphere,
};
use crate::{
    FrameInput,
    pipeline::projection,
    scene::{Camera, MeshId, Reconciler, SceneView},
};

const SIZE: [u32; 2] = [1280, 720];
const DRAW_DISTANCE: f64 = 96.0;
const EYE_HEIGHT: f64 = 25.0;
const FRAME: Duration = Duration::from_micros(16_667);
const SETTLE_FRAMES: u32 = 4_000;
const CHURN_FRAMES: u32 = 240;

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
    cycles: Latencies,
    work: usize,
    bytes_changes: u32,
    revision_changes: u32,
    retry_frames: u32,
    longest_retry: u32,
    restart_frames: u32,
    appearance_frames: u32,
    budget_blocked_frames: u32,
    mesh_wait: Latencies,
    mesh_unresolved: Latencies,
    parity: Latencies,
    parity_unresolved: Latencies,
    parity_abandoned: u32,
    first_entry: Latencies,
    reentry: Latencies,
    unresolved: Latencies,
    abandoned: u32,
    pop_outs: u32,
    missed: u32,
    extras: u32,
    mesh_switches: u32,
    removals: u32,
    returned_during_build: u32,
    examples: Vec<String>,
}

impl Report {
    fn parity_within_one(&self) -> f64 {
        let total = self.parity.0.len() + self.parity_unresolved.0.len();
        let within = self.parity.0.iter().filter(|latency| **latency <= 1).count();
        within as f64 * 100.0 / total.max(1) as f64
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let per_hundred = f64::from(self.commits) * 100.0 / f64::from(self.frames.max(1));
        write!(
            f,
            "frames={} commits={} commits_per_100={per_hundred:.1} stale={:?} invalidations={:?} build_frames=({}) cycles=({}) work={} building={} draining={} layout_required={} phases={:?} mesh_bytes_changes={} mesh_revision_changes={} retry_frames={} longest_retry={} restart_frames={} appearance_frames={} budget_blocked_frames={} mesh_wait=({}) mesh_unresolved=({}) parity=({}) parity_within_1={:.1}% parity_unresolved=({}) parity_abandoned={} pop_in_first=({}) pop_in_reentry=({}) unresolved=({}) abandoned={} returned_during_build={} pop_outs={} missed={} extras={} mesh_switches={} removals={} examples={:?}",
            self.frames,
            self.commits,
            self.stale,
            self.invalidations,
            self.committed_builds,
            self.cycles,
            self.work,
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
            self.mesh_wait,
            self.mesh_unresolved,
            self.parity,
            self.parity_within_one(),
            self.parity_unresolved,
            self.parity_abandoned,
            self.first_entry,
            self.reentry,
            self.unresolved,
            self.abandoned,
            self.returned_during_build,
            self.pop_outs,
            self.missed,
            self.extras,
            self.mesh_switches,
            self.removals,
            self.examples,
        )
    }
}

#[derive(Default)]
struct Track {
    shown: bool,
    ever_shown: bool,
    strict: bool,
    parity_since: Option<u32>,
    entered: Option<u32>,
    waiting_since: Option<u32>,
    slots: Vec<u32>,
    hidden_during: Option<u32>,
    mesh: Option<MeshId>,
    seen: Option<u32>,
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
    fn new(first: Camera, objects: u32) -> Self {
        let (world, scene, view) = object_scene_with(objects, first, layout);
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
                && !geometry.mesh_admission.active
            {
                return;
            }
        }
        panic!("renderer did not settle");
    }

    fn layout_cycle(&mut self, camera: Camera) -> u32 {
        let geometry = &mut self.geometry;
        assert!(
            geometry.draw_list.request_layout(&geometry.cache.visible, camera, &geometry.cache.items),
            "a settled list accepts a layout"
        );
        geometry.prepared = None;
        let mut frames = 0;
        while self.geometry.draw_list.layout_pending() {
            self.frame(camera);
            frames += 1;
            assert!(frames < SETTLE_FRAMES, "the layout cycle completes");
        }
        frames
    }

    fn resident_fraction(&self) -> f64 {
        let geometry = &self.geometry;
        let range = &geometry.cache.range;
        let resident = range
            .iter()
            .filter_map(|index| geometry.cache.items.get(*index))
            .filter(|item| geometry.meshes.contains_key(&item.mesh.id()))
            .count();
        resident as f64 / range.len().max(1) as f64
    }

    fn walk(&mut self, path: &Path) -> Report {
        let mut report = Report::default();
        let mut tracks = HashMap::<ItemId, Track>::new();
        let mut state = self.geometry.memory.mesh_state();
        let mut retry_run = 0u32;
        let mut build_started = None;
        let mut cycle_started = None;
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
                cycle_started.get_or_insert(frame_index);
            }
            if !geometry.draw_list.layout_pending()
                && let Some(started) = cycle_started.take()
            {
                report.cycles.0.push(frame_index - started + 1);
            }
            report.work += geometry.stats.candidate_items_visited
                + geometry.stats.visible_candidates_checked
                + geometry.stats.mesh_demand_items_visited;
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

            let frustum = Frustum::from_clip_matrix(projection(camera, SIZE));
            let (emitted, extras) = emitted_slots(geometry);
            report.extras += extras;
            for (_, item) in geometry.cache.items.iter() {
                let sphere = WorldSphere::new(item.position, item.rotation, item.scale, item.info.bounds, camera);
                let strict = sphere.is_some_and(|sphere| sphere.visible(item.shading, camera, &frustum));
                let resident = geometry.meshes.contains_key(&item.mesh.id());
                let track = tracks.entry(item.id).or_insert_with(|| Track {
                    slots: committed_slots(geometry, &item.faces),
                    strict,
                    ..Track::default()
                });
                if committed_now {
                    track.slots = committed_slots(geometry, &item.faces);
                }
                let mesh = item.mesh.id();
                report.mesh_switches += u32::from(track.mesh.is_some_and(|previous| previous != mesh));
                track.mesh = Some(mesh);
                track.seen = Some(frame_index);
                let shown = drawn(geometry, &emitted, &track.slots);
                if strict && !shown && track.slots.iter().any(|slot| written(geometry, *slot)) {
                    report.missed += 1;
                }
                if strict && !track.strict && resident {
                    track.parity_since = Some(frame_index);
                }
                if !strict && track.parity_since.take().is_some() {
                    report.parity_abandoned += 1;
                }
                if shown && let Some(since) = track.parity_since.take() {
                    report.parity.0.push(frame_index - since);
                }
                track.strict = strict;
                if track.shown && strict && !shown {
                    report.pop_outs += 1;
                    if report.examples.len() < 4 {
                        report.examples.push(format!("frame={frame_index} item={:?} building={building}", item.id.1));
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
                    track.entered = None;
                    track.hidden_during = None;
                } else if strict {
                    let entered = *track.entered.get_or_insert(frame_index);
                    if resident && track.waiting_since.is_none() {
                        if track.hidden_during.is_some() && track.hidden_during == build_started {
                            report.returned_during_build += 1;
                        }
                        report.mesh_wait.0.push(frame_index - entered);
                        track.waiting_since = Some(frame_index);
                    }
                } else {
                    track.entered = None;
                    if track.waiting_since.take().is_some() {
                        report.abandoned += 1;
                    }
                }
                track.shown = shown;
            }
            for track in tracks.values_mut() {
                if track.seen.is_some_and(|seen| seen != frame_index) {
                    report.removals += 1;
                    track.seen = None;
                    track.mesh = None;
                }
            }
        }
        let frames = report.frames;
        report.unresolved.0.extend(tracks.values().filter_map(|track| track.waiting_since.map(|since| frames - since)));
        report.parity_unresolved.0.extend(tracks.values().filter_map(|track| track.parity_since.map(|since| frames - since)));
        report.mesh_unresolved.0.extend(
            tracks.values().filter(|track| track.waiting_since.is_none()).filter_map(|track| track.entered.map(|entered| frames - entered)),
        );
        report
    }
}

fn committed_slots(geometry: &GeometryRenderer, faces: &[CachedFace]) -> Vec<u32> {
    faces.iter().filter_map(|face| geometry.draw_list.committed_slot(face)).collect()
}

fn emitted_slots(geometry: &GeometryRenderer) -> (HashSet<u32>, u32) {
    let view = &geometry.main_view;
    let commands = geometry.draw_list.draws().iter().map(|draw| (draw.first, draw.count)).collect::<HashSet<_>>();
    let mut slots = HashSet::new();
    let mut extras = 0;
    for batch in view.batches() {
        for command in 0..batch.count {
            let start = usize::try_from(batch.offset).expect("indirect offset fits") + command as usize * INDIRECT_DRAW_BYTES;
            let count = view.indirect_bytes().get(start + 4..start + 8).expect("emitted command is encoded");
            let count = u32::from_le_bytes(count.try_into().expect("instance count is four bytes"));
            let first = view.first_instance(batch.offset, command).expect("emitted command has a first instance");
            extras += u32::from(!commands.contains(&(first, count)));
            slots.extend(first..first + count);
        }
    }
    (slots, extras)
}

fn written(geometry: &GeometryRenderer, slot: u32) -> bool {
    geometry.draw_list.instance_transform(slot).is_some_and(|bytes| bytes.iter().any(|byte| *byte != 0))
}

fn drawn(geometry: &GeometryRenderer, emitted: &HashSet<u32>, slots: &[u32]) -> bool {
    slots.iter().any(|slot| emitted.contains(slot) && written(geometry, *slot))
}

fn run(path: &Path, regime: Regime, objects: u32) {
    let first = path.cameras[0];
    let mut rig = Rig::new(first, objects);
    rig.settle_at(first);
    let resident_fraction = rig.resident_fraction();
    let mut cycle = None;
    if matches!(regime, Regime::Resident) {
        rig.walk(path);
        rig.settle_at(first);
        cycle = Some((rig.layout_cycle(first), rig.geometry.cache.visible.len(), rig.geometry.stats.instances));
        rig.settle_at(first);
    }
    let _pressure = matches!(regime, Regime::Pressured).then(|| {
        let used = rig.geometry.memory.mesh_state().bytes;
        let headroom = (used / 2).max(1024 * 1024);
        rig.geometry.memory.reserve(MESH_BYTES.saturating_sub(used + headroom), Resource::Mesh).expect("pressure reservation fits")
    });
    let report = rig.walk(path);
    let (prim_meshes, prim_bytes) = rig.scene.prim_mesh_usage();
    println!(
        "camera_path path={} objects={objects} regime={regime:?} range={} resident_fraction={resident_fraction:.3} layout_cycle={cycle:?} mesh_bytes={} prim_meshes={prim_meshes} prim_bytes={prim_bytes} {report}",
        path.name,
        rig.geometry.cache.range.len(),
        rig.geometry.memory.mesh_state().bytes
    );
    assert_eq!(report.missed, 0, "a strictly visible face with written instance bytes was not emitted");
    assert_eq!(report.extras, 0, "an emitted command matches no committed command");
    assert_eq!(report.pop_outs, 0, "a shown strictly visible item stopped drawing");
    if matches!(regime, Regime::Resident) && path.name != "walk" {
        assert_eq!(report.commits, 0, "turning a settled camera commits no layout");
        assert_eq!(report.work, 0, "turning a settled camera revisits no range members");
    }
}

fn run_regimes(path: &Path, objects: u32) {
    for regime in [Regime::Resident, Regime::Streaming, Regime::Pressured] {
        run(path, regime, objects);
    }
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --release --run-ignored only -E 'test(camera_path)' --no-capture --test-threads=1"]
fn gpu_camera_path_yaw_sweep() {
    run_regimes(&yaw_sweep(), 3_000);
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --release --run-ignored only -E 'test(camera_path)' --no-capture --test-threads=1"]
fn gpu_camera_path_yaw_reversal() {
    run_regimes(&yaw_reversal(), 3_000);
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --release --run-ignored only -E 'test(camera_path)' --no-capture --test-threads=1"]
fn gpu_camera_path_walk() {
    run_regimes(&walk(), 3_000);
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --release --run-ignored only -E 'test(mesh_memory_churn)' --test-threads=1"]
fn gpu_layout_commits_through_mesh_memory_churn() {
    let center = DVec3::new(128.0, 128.0, EYE_HEIGHT);
    let first = camera(center, 0.0);
    let turned = camera(center, 90.0);
    let mut rig = Rig::new(first, 3_000);
    rig.settle_at(turned);
    rig.settle_at(first);
    let mut lease = None;
    for _ in 0..CHURN_FRAMES {
        lease = match lease.take() {
            Some(_) => None,
            None => Some(rig.geometry.memory.reserve(4096, Resource::Mesh).expect("churn lease fits")),
        };
        rig.frame(turned);
        assert_ne!(rig.geometry.stats.layout_stale_reason, Some("batches_invalidated"), "mesh memory churn discarded the pending layout");
        if rig.geometry.stats.layouts > 0 {
            return;
        }
    }
    panic!("no layout committed while mesh memory changed every frame");
}
