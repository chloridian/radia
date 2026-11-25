mod cache;
mod draw_list;
mod explain;
mod frame;
mod instance;
mod items;
mod mesh;
mod pick;
mod storage;
mod texture_bindings;
mod visibility;

use std::{
    collections::{HashMap, HashSet},
    mem,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use assets::{TextureHandle, TextureId};
use math::{DVec3, Vec4, motion::RegionBounds};
use utils::Summary;
use uuid::Uuid;
use wgpu::{
    Buffer, BufferDescriptor, BufferUsages, Device, Extent3d, Queue, RenderPass, Texture, TextureDescriptor, TextureDimension,
    TextureFormat, TextureUsages, TextureView, TextureViewDescriptor,
};
use world::{Entity, WorldTransform};

use self::{
    cache::FrameCache,
    draw_list::{DrawList, INDIRECT_DRAW_BYTES, ViewDraws},
    instance::{INITIAL_FACE_INSTANCES, InstanceBuffers},
    items::ItemIndex,
    mesh::GpuMesh,
    storage::Storage,
};
pub use self::{
    explain::{FaceExplanation, MeshResidency, ObjectExplanation, VisibilityStatus},
    pick::{PickDetail, PickError},
};
use crate::{
    Limits,
    pipeline::{FACE_INDEX_BYTES, Pipeline, Sky, SurfacePipelines, VoidWater},
    scene::{AgentPose, Camera, MAX_REGIONS, MeshId, ViewVersion},
    texture::{GeometryTextureState, TextureCompletion, TextureJob, TextureState, Textures},
};

#[cfg(test)]
const MESH_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const DEPTH_BYTES: usize = 128 * 1024 * 1024;
pub(crate) const INSTANCE_BUFFER_BYTES: usize = 64 * 1024 * 1024;
const UPLOAD_BYTES: usize = 4 * 1024 * 1024;
const MAX_MESH_UPLOADS: usize = 16;
const TRANSFORM_BYTES: usize = 112;

#[derive(Clone, Copy, Default)]
pub struct Stats {
    pub instances: usize,
    pub instance_buffer_bytes: usize,
    pub instance_peak: usize,
    pub instance_buffer_growths: usize,
    pub draw_calls: usize,
    pub draw_commands: usize,
    pub triangles: u64,
    pub pending_meshes: usize,
    pub uploaded_bytes: usize,
    pub mesh_bytes: usize,
    pub mesh_peak: usize,
    pub depth_bytes: usize,
    pub prepare_ms: f64,
    pub prepare_cache_ms: f64,
    pub prepare_visibility_ms: f64,
    pub prepare_motion_ms: f64,
    pub prepare_candidates_ms: f64,
    pub prepare_requests_ms: f64,
    pub prepare_textures_ms: f64,
    pub prepare_texture_bindings_ms: f64,
    pub prepare_meshes_ms: f64,
    pub prepare_mesh_demands_ms: f64,
    pub prepare_mesh_uploads_ms: f64,
    pub prepare_appearances_ms: f64,
    pub prepare_draw_list_ms: f64,
    pub prepare_layout_ms: f64,
    pub prepare_layout_commit_ms: f64,
    pub prepare_draw_upload_ms: f64,
    pub prepare_draw_batches_ms: f64,
    pub prepare_finalize_ms: f64,
    pub texture_bytes: usize,
    pub texture_budget_bytes: usize,
    pub texture_count: usize,
    pub texture_failure: Option<assets::Failure>,
    pub staging_bytes: usize,
    pub texture_upload_bytes: usize,
    pub pending_textures: usize,
    pub omitted_faces: usize,
    pub mesh_budget_blocked: usize,
    pub omission_reason: Option<OmissionReason>,
    pub layouts: usize,
    pub items_updated: usize,
    pub faces_patched: usize,
    pub blended_sorts: usize,
    pub animated_faces_visited: usize,
    pub animated_faces_changed: usize,
    pub rebuild_objects: usize,
    pub rebuild_surfaces: usize,
    pub rebuild_sweep_slots: usize,
    pub rebuild_cleanup_entries: usize,
    pub rebuild_mesh_info_bytes: usize,
    pub rebuild_mesh_info_entries: usize,
    pub rebuild_mesh_info_centers_dropped: usize,
    pub item_order_items: usize,
    pub candidate_items_visited: usize,
    pub mesh_demand_items_visited: usize,
    pub mesh_admission_items_visited: usize,
    pub mesh_demand_order_updates: usize,
    pub mesh_upload_attempts: usize,
    pub mesh_upload_attempt_bytes: usize,
    pub appearance_candidates_checked: usize,
    pub visible_candidates_checked: usize,
    pub motion_cache_items_scanned: usize,
    pub motion_entities_evaluated: usize,
    pub motion_redraw_items_checked: usize,
    pub layout_items_visited: usize,
    pub layout_faces_visited: usize,
    pub draw_instance_bytes_uploaded: usize,
    pub draw_instance_write_calls: usize,
    pub draw_command_bytes_uploaded: usize,
    pub draw_batch_commands_built: usize,
    pub texture_binding_entries: usize,
    pub gpu_mesh_retirements: usize,
    pub gpu_mesh_retired_bytes: usize,
    pub rebuild_pending: bool,
}

impl Stats {
    pub fn vram_bytes(&self) -> usize {
        self.mesh_bytes.saturating_add(self.texture_bytes).saturating_add(self.depth_bytes).saturating_add(self.instance_buffer_bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OmissionReason {
    InstanceBufferBudget,
    MeshMemoryBudget,
    CombinedBudgets,
}

struct Accounting {
    bytes: AtomicUsize,
    instances: AtomicUsize,
    instance_peak: AtomicUsize,
    mesh_revision: AtomicU64,
    depth: AtomicUsize,
    peak: AtomicUsize,
    frames: AtomicUsize,
    mesh_bytes: usize,
}

impl Default for Accounting {
    fn default() -> Self {
        Self::new(Limits::default().mesh_bytes)
    }
}

impl Accounting {
    fn new(mesh_bytes: usize) -> Self {
        Self {
            bytes: AtomicUsize::new(0),
            instances: AtomicUsize::new(0),
            instance_peak: AtomicUsize::new(0),
            mesh_revision: AtomicU64::new(0),
            depth: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            frames: AtomicUsize::new(0),
            mesh_bytes,
        }
    }

    fn mesh_state(&self) -> MeshState {
        MeshState { bytes: self.bytes.load(Ordering::Acquire), revision: self.mesh_revision.load(Ordering::Acquire) }
    }

    fn reserve(self: &Arc<Self>, bytes: usize, resource: Resource) -> Option<Lease> {
        let limit = match resource {
            Resource::Mesh => self.mesh_bytes,
            Resource::Instances => return Some(self.charge(bytes)),
            Resource::Depth => DEPTH_BYTES,
        };
        let before = self
            .account(&resource)
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| used.checked_add(bytes).filter(|n| *n <= limit))
            .ok()?;
        if matches!(resource, Resource::Mesh) {
            self.peak.fetch_max(before + bytes, Ordering::AcqRel);
        }
        Some(Lease { memory: self.clone(), bytes, resource })
    }

    fn charge(self: &Arc<Self>, bytes: usize) -> Lease {
        let before = self.instances.fetch_add(bytes, Ordering::AcqRel);
        self.instance_peak.fetch_max(before + bytes, Ordering::AcqRel);
        Lease { memory: self.clone(), bytes, resource: Resource::Instances }
    }

    fn account(&self, resource: &Resource) -> &AtomicUsize {
        match resource {
            Resource::Mesh => &self.bytes,
            Resource::Instances => &self.instances,
            Resource::Depth => &self.depth,
        }
    }
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
struct MeshState {
    bytes: usize,
    revision: u64,
}

struct Lease {
    memory: Arc<Accounting>,
    bytes: usize,
    resource: Resource,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.memory.account(&self.resource).fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

enum Resource {
    Mesh,
    Instances,
    Depth,
}

pub(crate) struct FrameLease(Arc<Accounting>);

impl Drop for FrameLease {
    fn drop(&mut self) {
        self.0.frames.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ShadingMode {
    Surface,
    Terrain,
    RegionWater,
}

struct Depth {
    _texture: Texture,
    view: TextureView,
    size: [u32; 2],
    _lease: Lease,
}

#[derive(PartialEq)]
struct FramePreparationKey {
    version: ViewVersion,
    camera: Camera,
    agent_pose: Option<AgentPose>,
    size: [u32; 2],
}

struct FramePreparation {
    key: FramePreparationKey,
    textures: TextureState,
    geometry_textures: GeometryTextureState,
    meshes: MeshState,
    more_meshes: bool,
    redraw_deadline: Option<Instant>,
}

impl FramePreparation {
    fn needs_frame(&self, textures: TextureState, geometry_textures: GeometryTextureState, meshes: MeshState, now: Instant) -> bool {
        self.redraw_deadline.is_some_and(|deadline| deadline <= now)
            || self.more_meshes
            || self.textures != textures
            || self.geometry_textures != geometry_textures
            || self.meshes != meshes
    }

    fn reusable(&self, key: &FramePreparationKey, geometry_textures: GeometryTextureState, meshes: MeshState, now: Instant) -> bool {
        self.key == *key
            && self.geometry_textures == geometry_textures
            && self.meshes == meshes
            && !self.more_meshes
            && self.redraw_deadline.is_none_or(|deadline| deadline > now)
    }
}

#[derive(Default)]
struct PrepareTimes {
    cache: Duration,
    visibility: Duration,
    motion: Duration,
    candidates: Duration,
    requests: Duration,
    textures: Duration,
    texture_bindings: Duration,
    meshes: Duration,
    mesh_demands: Duration,
    mesh_uploads: Duration,
    appearances: Duration,
    draw_list: Duration,
    layout: Duration,
    draw_upload: Duration,
    draw_batches: Duration,
    finalize: Duration,
}

pub(crate) struct GeometryRenderer {
    queue: Queue,
    memory: Arc<Accounting>,
    pipelines: SurfacePipelines,
    void_water: VoidWater,
    sky: Sky,
    textures: Textures,
    requested_textures: HashSet<TextureId>,
    visible_textures: HashSet<TextureId>,
    visible_texture_requests: Vec<TextureHandle>,
    region_water_bounds: [Vec4; MAX_REGIONS],
    region_water_count: usize,
    region_water_origin: Option<DVec3>,
    instances: InstanceBuffers,
    indirect_first_instance: bool,
    depth: Option<Depth>,
    meshes: HashMap<MeshId, GpuMesh>,
    storage: Storage,
    retired: Vec<GpuMesh>,
    cache: FrameCache,
    active_origin: DVec3,
    draw_list: DrawList,
    main_view: ViewDraws,
    layout_pose_items: HashSet<(ItemIndex, Entity)>,
    recovery_publication_pending: bool,
    mesh_admission: frame::MeshAdmission,
    pending_meshes: HashSet<MeshId>,
    pending_textures: HashSet<TextureId>,
    budget_reports: Summary<()>,
    omitted_faces: usize,
    mesh_budget_blocked: HashSet<MeshId>,
    placements: HashMap<Entity, WorldTransform>,
    future_placements: HashMap<Entity, WorldTransform>,
    moving_entities: HashSet<Entity>,
    motion_entities: Vec<Entity>,
    regions: Vec<RegionBounds>,
    motion_redraw: bool,
    layout_required: bool,
    replacement_retry_pending: bool,
    mesh_omitted_faces: usize,
    epoch: u64,
    prepared: Option<FramePreparation>,
    applied_agent: Option<Uuid>,
    slow_prepares: Summary<frame::SlowPrepares>,
    pub(crate) stats: Stats,
}

impl GeometryRenderer {
    pub(crate) fn release_textures(&mut self) {
        self.textures.release_resources();
    }

    pub(crate) fn needs_frame(&self, now: Instant) -> bool {
        self.prepared.as_ref().is_some_and(|prepared| {
            prepared.needs_frame(self.textures.state(), self.textures.geometry_state(), self.memory.mesh_state(), now)
        }) || self.motion_redraw
            || self.textures.uploading()
    }

    pub(crate) fn redraw_deadline(&self) -> Option<Instant> {
        self.prepared.as_ref().and_then(|prepared| prepared.redraw_deadline)
    }

    pub(crate) fn texture_revision(&self) -> u64 {
        self.textures.state().revision
    }
    pub(crate) fn new(device: &Device, queue: &Queue, format: TextureFormat, limits: Limits) -> Self {
        let memory = Arc::new(Accounting::new(limits.mesh_bytes));
        let textures = Textures::new(device, queue, limits);
        let instances = InstanceBuffers::new(device, &memory, INITIAL_FACE_INSTANCES);
        let pipelines = SurfacePipelines::new(device, format, &textures.layout, &textures.terrain_layout, &textures, &instances.instances);
        let void_water = VoidWater::new(device, format, &textures);
        let sky = Sky::new(device, format, &textures);
        Self {
            queue: queue.clone(),
            memory,
            pipelines,
            textures,
            requested_textures: HashSet::new(),
            visible_textures: HashSet::new(),
            visible_texture_requests: Vec::new(),
            region_water_bounds: [Vec4::ZERO; MAX_REGIONS],
            region_water_count: 0,
            region_water_origin: None,
            void_water,
            sky,
            draw_list: DrawList::new(instances.cap()),
            instances,
            indirect_first_instance: device.features().contains(wgpu::Features::INDIRECT_FIRST_INSTANCE),
            depth: None,
            meshes: HashMap::new(),
            storage: Storage::default(),
            retired: Vec::new(),
            cache: FrameCache::default(),
            active_origin: DVec3::ZERO,
            main_view: ViewDraws::default(),
            layout_pose_items: HashSet::new(),
            recovery_publication_pending: false,
            mesh_admission: frame::MeshAdmission::default(),
            pending_meshes: HashSet::new(),
            pending_textures: HashSet::new(),
            budget_reports: Summary::new(frame::BUDGET_REPORT_INTERVAL),
            omitted_faces: 0,
            mesh_budget_blocked: HashSet::new(),
            placements: HashMap::new(),
            future_placements: HashMap::new(),
            moving_entities: HashSet::new(),
            motion_entities: Vec::new(),
            regions: Vec::new(),
            motion_redraw: false,
            layout_required: false,
            replacement_retry_pending: false,
            mesh_omitted_faces: 0,
            epoch: 0,
            prepared: None,
            applied_agent: None,
            slow_prepares: Summary::new(frame::SLOW_PREPARE_REPORT_INTERVAL),
            stats: Stats::default(),
        }
    }

    pub(crate) fn admit(&self) -> Option<FrameLease> {
        self.memory.frames.try_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < 3).then_some(n + 1)).ok()?;
        Some(FrameLease(self.memory.clone()))
    }

    pub(crate) fn vram_budget_bytes(&self) -> usize {
        self.memory
            .mesh_bytes
            .saturating_add(DEPTH_BYTES)
            .saturating_add(INSTANCE_BUFFER_BYTES)
            .saturating_add((self.texture_soft_cap_mib() as usize).saturating_mul(1024 * 1024))
    }

    pub(crate) fn depth(&mut self, device: &Device, size: [u32; 2]) -> Option<TextureView> {
        if self.depth.as_ref().is_some_and(|depth| depth.size != size) {
            let old = self.depth.take();
            self.queue.on_submitted_work_done(move || drop(old));
        }
        if self.depth.is_none() {
            let bytes = (size[0] as usize).checked_mul(size[1] as usize)?.checked_mul(4)?;
            let lease = self.memory.reserve(bytes, Resource::Depth)?;
            let texture = device.create_texture(&TextureDescriptor {
                label: Some("Scene depth"),
                size: Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: TextureDimension::D2,
                format: TextureFormat::Depth32Float,
                usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = texture.create_view(&TextureViewDescriptor::default());
            self.depth = Some(Depth { _texture: texture, view, size, _lease: lease });
        }
        self.stats.depth_bytes = self.memory.depth.load(Ordering::Acquire);
        Some(self.depth.as_ref().expect("depth initialized").view.clone())
    }

    pub(crate) fn skip(&mut self) {
        self.motion_redraw = false;
        self.motion_entities.clear();
        self.placements.clear();
        self.future_placements.clear();
        self.layout_required = false;
        self.replacement_retry_pending = false;
        self.mesh_omitted_faces = 0;
        if let Some(agent_id) = self.applied_agent.take() {
            self.restore_agent_pose(agent_id);
        }
        self.prepared = None;
        self.region_water_origin = None;
        self.cache.camera = None;
        self.cache.range.clear();
        self.cache.range_camera = None;
        self.cache.nonresident_demands = false;
        self.cache.visible.clear();
        self.cache.next_visible.clear();
        self.cache.clear_mesh_demands();
        self.pending_meshes.clear();
        for (_, item) in self.cache.items.iter_mut() {
            for face in &mut item.faces {
                face.slot = None;
                face.blended_index = None;
            }
            if let Some(replacement) = item.replacement.as_mut() {
                for face in &mut replacement.item.faces {
                    face.slot = None;
                    face.blended_index = None;
                }
            }
        }
        self.draw_list.clear();
        self.main_view.clear();
        self.visible_textures.clear();
        self.visible_texture_requests.clear();
        self.stats.instances = 0;
        self.stats.draw_calls = 0;
        self.stats.draw_commands = 0;
        self.omitted_faces = 0;
        self.mesh_budget_blocked.clear();
        self.stats.omitted_faces = 0;
        self.stats.mesh_budget_blocked = 0;
        self.stats.omission_reason = None;
    }

    fn memory_stats(&mut self) {
        self.stats.depth_bytes = self.memory.depth.load(Ordering::Acquire);
        self.stats.mesh_bytes = self.memory.bytes.load(Ordering::Acquire);
        self.stats.mesh_peak = self.memory.peak.load(Ordering::Acquire);
        self.stats.instance_buffer_bytes = self.instances.bytes();
        self.stats.instance_peak = self.memory.instance_peak.load(Ordering::Acquire);
        self.stats.instance_buffer_growths = self.instances.growths();
        self.stats.texture_bytes = self.textures.bytes();
        self.stats.texture_budget_bytes = self.textures.soft_cap();
        self.stats.texture_count = self.textures.count();
        self.stats.texture_failure = self.textures.failure();
        self.stats.staging_bytes = self.textures.staging_bytes();
        self.stats.texture_upload_bytes = self.textures.uploaded;
    }

    pub(crate) fn next_texture_job(&mut self) -> Option<TextureJob> {
        self.textures.next_conversion_job()
    }
    pub(crate) fn complete_texture(&mut self, completion: TextureCompletion) {
        self.textures.complete(completion);
    }
    pub(crate) fn texture_soft_cap_mib(&self) -> u32 {
        self.textures.soft_cap_mib()
    }
    pub(crate) fn set_texture_soft_cap(&mut self, mib: u32) {
        self.textures.set_soft_cap(mib);
    }
    pub(crate) fn draw(&self, pass: &mut RenderPass<'_>) {
        self.sky.draw(pass);
        self.void_water.draw(pass);
        let mut pipeline = None;
        let mut textures = None;
        let mut terrain = None;
        let mut buffers = None;
        for draw in self.main_view.batches() {
            if pipeline != Some(draw.key.pipeline) {
                self.pipelines.bind_pipeline(pass, draw.key.pipeline);
                pipeline = Some(draw.key.pipeline);
            }
            if textures != Some(draw.key.textures) {
                pass.set_bind_group(1, self.textures.material_binding(draw.key.textures), &[]);
                textures = Some(draw.key.textures);
            }
            if draw.key.pipeline == Pipeline::Terrain {
                let key = draw.key.terrain.expect("terrain batch has detail texture bindings");
                if terrain != Some(key) {
                    pass.set_bind_group(2, self.textures.terrain_binding(key), &[]);
                    terrain = Some(key);
                }
            }
            let page = Arc::as_ptr(&draw.buffers) as usize;
            if buffers != Some(page) {
                pass.set_vertex_buffer(0, draw.buffers.vertices.slice(..));
                pass.set_index_buffer(draw.buffers.indices.slice(..), wgpu::IndexFormat::Uint32);
                buffers = Some(page);
            }
            let face_slot = if draw.key.pipeline == Pipeline::Terrain { 2 } else { 1 };
            if draw.key.pipeline == Pipeline::Terrain {
                let composition = draw.buffers.composition.as_ref().expect("terrain buffer has composition attributes");
                pass.set_vertex_buffer(1, composition.slice(..));
            }
            if self.indirect_first_instance {
                pass.set_vertex_buffer(face_slot, self.instances.face_indices.slice(..));
                pass.multi_draw_indexed_indirect(&self.instances.indirect, draw.offset, draw.count);
            } else {
                for command in 0..draw.count {
                    let Some(first_instance) = self.main_view.first_instance(draw.offset, command) else { continue };
                    let start = u64::from(first_instance) * FACE_INDEX_BYTES as u64;
                    pass.set_vertex_buffer(face_slot, self.instances.face_indices.slice(start..));
                    pass.draw_indexed_indirect(&self.instances.indirect, draw.offset + u64::from(command) * INDIRECT_DRAW_BYTES as u64);
                }
            }
        }
    }

    pub(crate) fn submitted(&mut self, frame: FrameLease) {
        self.textures.submitted(&self.queue);
        let retired = mem::take(&mut self.retired);
        self.queue.on_submitted_work_done(move || {
            drop(retired);
            drop(frame);
        });
    }
}

impl Drop for GeometryRenderer {
    fn drop(&mut self) {
        if let Some(due) = self.slow_prepares.flush() {
            due.report();
        }
        let mut meshes: Vec<_> = self.meshes.drain().map(|(_, mesh)| mesh).collect();
        meshes.append(&mut self.retired);
        let depth = self.depth.take();
        self.queue.on_submitted_work_done(move || {
            drop(meshes);
            drop(depth);
        });
    }
}

fn buffer(device: &Device, label: &str, bytes: usize, usage: BufferUsages) -> Buffer {
    device.create_buffer(&BufferDescriptor {
        label: Some(label),
        size: bytes as u64,
        usage: usage | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

#[cfg(test)]
mod fixtures;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_admission() {
        let memory = Arc::new(Accounting::default());
        let a = memory.reserve(MESH_BYTES, Resource::Mesh).unwrap();
        assert!(memory.reserve(1, Resource::Mesh).is_none());
        let b = memory.reserve(DEPTH_BYTES, Resource::Depth).unwrap();
        assert!(memory.reserve(1, Resource::Depth).is_none());
        drop(a);
        drop(b);
        assert_eq!(memory.bytes.load(Ordering::Acquire), 0);
        assert_eq!(memory.depth.load(Ordering::Acquire), 0);

        let instances = memory.charge(INSTANCE_BUFFER_BYTES);
        let replacement = memory.charge(1024);
        let mesh = memory.reserve(MESH_BYTES, Resource::Mesh).expect("instance bytes leave the mesh budget whole");
        drop(instances);
        assert_eq!(memory.instances.load(Ordering::Acquire), 1024);
        assert_eq!(memory.instance_peak.load(Ordering::Acquire), INSTANCE_BUFFER_BYTES + 1024, "the peak keeps overlapping charges");
        drop(replacement);
        drop(mesh);
        assert_eq!(memory.instances.load(Ordering::Acquire), 0);
    }
}

#[cfg(test)]
mod water_distance_tests;

#[cfg(test)]
mod surface_pipeline_tests;

#[cfg(test)]
mod radiance_tests;
