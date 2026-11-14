use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap, HashSet},
    hash::Hash,
    ops::Range,
    sync::Arc,
    time::{Duration, Instant},
};

use assets::TextureHandle;
use math::{DVec3, Quat, Vec3};
use wgpu::{Buffer, Queue};
use world::Region;

use super::{
    INSTANCE_BUFFER_BYTES, TRANSFORM_BYTES,
    instance::{PARAMETER_BYTES, instance_base},
    items::{CachedFace, CachedItem, ItemId, ItemIndex, ItemSlab, draw_priority, surface_pipeline},
    mesh::GpuMesh,
    storage::Buffers,
    texture_bindings::{TerrainDemand, TextureDemand},
};
use crate::{
    pipeline::{INSTANCE_BYTES, Pipeline},
    scene::{Camera, MeshId},
    texture::TextureKey,
};

pub(super) const MAX_FACE_INSTANCES: usize = INSTANCE_BUFFER_BYTES / INSTANCE_BYTES;
pub(super) const INDIRECT_DRAW_BYTES: usize = 20;
pub(super) type FaceOrder = (u8, u128, u16, u8);
const MAX_PATCH_RANGES: usize = 32;
const MAX_LAYOUT_UPLOAD_RANGES: usize = 256;
const MAX_PATCH_GAP_SLOTS: u32 = 4;
const LAYOUT_ITEMS_PER_FRAME: usize = 1_024;
const RETIRED_LAYOUT_ENTRIES_PER_FRAME: usize = 256;
const LAYOUT_BYTES_PER_FRAME: usize = 8 * 1024 * 1024;
const LAYOUT_TIME_PER_FRAME: Duration = Duration::from_millis(1);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct LayoutId(u64);

#[derive(Clone, Copy, Debug)]
pub(super) struct FaceSlot {
    layout: LayoutId,
    slot: u32,
    blended_index: Option<usize>,
}

#[derive(Default)]
pub(super) struct UploadStats {
    pub(super) bytes: usize,
    pub(super) calls: usize,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(super) struct DrawKey {
    pub(super) mesh: MeshId,
    pub(super) start: u32,
    pub(super) end: u32,
    pub(super) pipeline: Pipeline,
    pub(super) texture: Option<TextureKey>,
    pub(super) terrain: Option<Region>,
}

impl DrawKey {
    pub(super) fn blended(self) -> bool {
        matches!(self.pipeline, Pipeline::Blend | Pipeline::BlendDoubleSided | Pipeline::BlendFlipped)
    }

    pub(super) fn fullbright(self) -> bool {
        matches!(self.pipeline, Pipeline::Fullbright | Pipeline::FullbrightDoubleSided | Pipeline::FullbrightFlipped)
    }

    fn command(self, group: u32, first: u32, count: u32) -> DrawCommand {
        DrawCommand {
            group,
            mesh: self.mesh,
            pipeline: self.pipeline,
            first,
            count,
            indices: self.start..self.end,
            texture: self.texture,
            terrain: self.terrain,
        }
    }
}

pub(super) struct DrawCommand {
    group: u32,
    pub(super) mesh: MeshId,
    pub(super) pipeline: Pipeline,
    pub(super) first: u32,
    pub(super) count: u32,
    pub(super) indices: Range<u32>,
    pub(super) texture: Option<TextureKey>,
    pub(super) terrain: Option<Region>,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(super) struct DrawBatchKey {
    pub(super) pipeline: Pipeline,
    pub(super) texture: Option<TextureKey>,
    pub(super) terrain: Option<Region>,
    page: usize,
}

pub(super) struct DrawBatch {
    id: usize,
    pub(super) key: DrawBatchKey,
    pub(super) buffers: Arc<Buffers>,
    pub(super) offset: u64,
    pub(super) count: u32,
}

struct BatchGroup {
    key: DrawBatchKey,
    buffers: Arc<Buffers>,
    commands: Vec<DrawIndexedArgs>,
    refs: usize,
}

#[derive(Clone, Copy)]
struct BatchBinding {
    id: usize,
    index_offset: u32,
    base_vertex: i32,
}

#[derive(Clone, Copy)]
struct DrawIndexedArgs {
    index_count: u32,
    instance_count: u32,
    first_index: u32,
    base_vertex: i32,
    first_instance: u32,
}

struct DrawGroup {
    key: DrawKey,
    refs: usize,
    texture: Option<TextureHandle>,
    terrain_textures: Option<[Option<TextureHandle>; 4]>,
    batch: Option<BatchBinding>,
}

#[derive(Default)]
struct DrawInterner {
    by_key: HashMap<DrawKey, u32>,
    groups: Vec<Option<DrawGroup>>,
    free: Vec<u32>,
    batches_by_key: HashMap<DrawBatchKey, usize>,
    batches: Vec<Option<BatchGroup>>,
    free_batches: Vec<usize>,
}

impl DrawInterner {
    pub(super) fn intern(
        &mut self,
        key: DrawKey,
        texture: Option<&TextureHandle>,
        terrain_textures: Option<&[Option<TextureHandle>; 4]>,
    ) -> Option<u32> {
        if let Some(id) = self.by_key.get(&key).copied() {
            let group = self.groups.get_mut(id as usize).and_then(Option::as_mut)?;
            group.refs = group.refs.checked_add(1)?;
            if texture.is_some() {
                group.texture = texture.cloned();
            }
            if terrain_textures.is_some() {
                group.terrain_textures = terrain_textures.cloned();
            }
            return Some(id);
        }
        let id = match self.free.pop() {
            Some(id) => id,
            None => u32::try_from(self.groups.len()).ok()?,
        };
        if id as usize == self.groups.len() {
            self.groups.push(None);
        }
        let slot = self.groups.get_mut(id as usize)?;
        *slot = Some(DrawGroup { key, refs: 1, texture: texture.cloned(), terrain_textures: terrain_textures.cloned(), batch: None });
        self.by_key.insert(key, id);
        Some(id)
    }

    pub(super) fn key(&self, id: u32) -> Option<DrawKey> {
        self.groups.get(id as usize).and_then(Option::as_ref).map(|group| group.key)
    }

    fn group(&self, id: u32) -> Option<&DrawGroup> {
        self.groups.get(id as usize).and_then(Option::as_ref)
    }

    fn retain(&mut self, id: u32) -> bool {
        let Some(group) = self.groups.get_mut(id as usize).and_then(Option::as_mut) else { return false };
        let Some(refs) = group.refs.checked_add(1) else { return false };
        group.refs = refs;
        true
    }

    fn batch(&self, id: u32) -> Option<BatchBinding> {
        self.group(id).and_then(|group| group.batch)
    }

    fn set_batch(&mut self, id: u32, batch: BatchBinding) {
        if let Some(group) = self.groups.get_mut(id as usize).and_then(Option::as_mut) {
            group.batch = Some(batch);
        }
    }

    fn batch_group(&self, id: usize) -> Option<&BatchGroup> {
        self.batches.get(id).and_then(Option::as_ref)
    }

    fn batch_group_mut(&mut self, id: usize) -> Option<&mut BatchGroup> {
        self.batches.get_mut(id).and_then(Option::as_mut)
    }

    pub(super) fn release(&mut self, id: u32) {
        let batch = {
            let Some(group) = self.groups.get_mut(id as usize).and_then(Option::as_mut) else { return };
            if group.refs == 0 {
                return;
            }
            group.refs -= 1;
            if group.refs != 0 {
                return;
            }
            group.batch.map(|batch| batch.id)
        };
        let key = self.groups.get(id as usize).and_then(Option::as_ref).map(|group| group.key);
        let Some(key) = key else { return };
        self.by_key.remove(&key);
        if let Some(slot) = self.groups.get_mut(id as usize) {
            *slot = None;
            self.free.push(id);
        }
        if let Some(batch) = batch {
            self.release_batch(batch);
        }
    }

    fn intern_batch(&mut self, key: DrawBatchKey, buffers: &Arc<Buffers>) -> usize {
        if let Some(id) = self.batches_by_key.get(&key).copied() {
            if let Some(group) = self.batches.get_mut(id).and_then(Option::as_mut) {
                group.refs += 1;
            }
            return id;
        }
        let id = match self.free_batches.pop() {
            Some(id) => id,
            None => self.batches.len(),
        };
        if id == self.batches.len() {
            self.batches.push(None);
        }
        if let Some(slot) = self.batches.get_mut(id) {
            *slot = Some(BatchGroup { key, buffers: Arc::clone(buffers), commands: Vec::new(), refs: 1 });
        }
        self.batches_by_key.insert(key, id);
        id
    }

    fn release_batch(&mut self, id: usize) {
        let Some(group) = self.batches.get_mut(id).and_then(Option::as_mut) else { return };
        if group.refs == 0 {
            return;
        }
        group.refs -= 1;
        if group.refs != 0 {
            return;
        }
        self.batches_by_key.remove(&group.key);
        if let Some(slot) = self.batches.get_mut(id) {
            *slot = None;
            self.free_batches.push(id);
        }
    }

    fn invalidate_batches(&mut self) {
        self.batches_by_key.clear();
        self.batches.clear();
        self.free_batches.clear();
        for group in self.groups.iter_mut().flatten() {
            group.batch = None;
        }
    }

    pub(super) fn update_terrain(&mut self, region: Region, textures: &[Option<TextureHandle>; 4]) {
        for group in self.groups.iter_mut().flatten().filter(|group| group.key.terrain == Some(region)) {
            group.terrain_textures = Some(textures.clone());
        }
    }
}

#[derive(Clone, Copy)]
struct BlendedFace {
    distance_squared: f64,
    order: FaceOrder,
    item: ItemIndex,
    face: usize,
    group: u32,
    slot: u32,
}

#[derive(Default)]
pub(super) struct LayoutStats {
    pub(super) omitted_faces: usize,
    pub(super) blended: bool,
    pub(super) items_visited: usize,
    pub(super) faces_visited: usize,
}

pub(super) struct LayoutProgress {
    pub(super) stats: LayoutStats,
    pub(super) complete: bool,
    pub(super) stale: bool,
    pub(super) commit_ms: f64,
}

#[derive(Clone, Copy)]
struct PriorityItem {
    priority: f64,
    distance: f64,
    id: ItemId,
    index: ItemIndex,
    faces: usize,
}

impl Eq for PriorityItem {}

impl PartialEq for PriorityItem {
    fn eq(&self, other: &Self) -> bool {
        self.priority.total_cmp(&other.priority) == Ordering::Equal
            && self.distance.total_cmp(&other.distance) == Ordering::Equal
            && self.id == other.id
            && self.index == other.index
            && self.faces == other.faces
    }
}

impl Ord for PriorityItem {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority.total_cmp(&other.priority).then_with(|| other.distance.total_cmp(&self.distance)).then_with(|| other.id.cmp(&self.id))
    }
}

impl PartialOrd for PriorityItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

struct LayoutBuild {
    layout: LayoutId,
    state: LayoutState,
    uploads: HashSet<u32>,
    previous: Vec<ItemIndex>,
    visible: Vec<ItemIndex>,
    mesh_pins: HashSet<MeshId>,
    layout_items: Vec<ItemIndex>,
    selected_items: HashSet<ItemIndex>,
    phase: LayoutPhase,
    camera: Camera,
    item_cursor: usize,
    face_cursor: usize,
    item_has_slot: bool,
    batch_cursor: usize,
    batch_order_cursor: usize,
    batch_command_cursor: usize,
    blended_batch_start: usize,
    batch_seen: HashSet<usize>,
    group_cursor: usize,
    required: usize,
    group_limits: Vec<usize>,
    remaining: usize,
    omitted: usize,
    opaque_count: usize,
    instance_bytes: usize,
    instance_reserved: bool,
    blend_index: usize,
    visible_items: usize,
    priority: BinaryHeap<PriorityItem>,
    blended: BinaryHeap<BlendedFace>,
    items_visited: usize,
    faces_visited: usize,
    stale: bool,
}

#[derive(Clone, Copy)]
enum LayoutPhase {
    RetirePrevious,
    CountVisible,
    ClearInitialCounts,
    Priority,
    Select,
    CountSelected,
    HideOmitted,
    Commands,
    FullbrightCommands,
    Allocate,
    Fill,
    EmitBlended,
    ClearFinalCounts,
    BatchOpaque,
    BatchOpaqueEncode,
    BatchBlended,
    Commit,
}

impl LayoutPhase {
    fn batching(self) -> bool {
        matches!(self, Self::BatchOpaque | Self::BatchOpaqueEncode | Self::BatchBlended | Self::Commit)
    }
}

impl LayoutBuild {
    fn new(
        layout: LayoutId,
        previous: &[ItemIndex],
        visible: &[ItemIndex],
        camera: Camera,
        retire_previous: bool,
        items: &ItemSlab,
    ) -> Self {
        Self {
            layout,
            state: LayoutState::default(),
            uploads: HashSet::new(),
            previous: if retire_previous { previous.to_vec() } else { Vec::new() },
            visible: visible.to_vec(),
            mesh_pins: visible.iter().filter_map(|index| items.get(*index).map(|item| item.mesh.id())).collect(),
            layout_items: Vec::new(),
            selected_items: HashSet::new(),
            phase: if retire_previous { LayoutPhase::RetirePrevious } else { LayoutPhase::CountVisible },
            camera,
            item_cursor: 0,
            face_cursor: 0,
            item_has_slot: false,
            batch_cursor: 0,
            batch_order_cursor: 0,
            batch_command_cursor: 0,
            blended_batch_start: 0,
            batch_seen: HashSet::new(),
            group_cursor: 0,
            required: 0,
            group_limits: Vec::new(),
            remaining: MAX_FACE_INSTANCES,
            omitted: 0,
            opaque_count: 0,
            instance_bytes: 0,
            instance_reserved: false,
            blend_index: 0,
            visible_items: 0,
            priority: BinaryHeap::new(),
            blended: BinaryHeap::new(),
            items_visited: 0,
            faces_visited: 0,
            stale: false,
        }
    }

    fn upload(&mut self, slot: u32) {
        if self.uploads.insert(slot) {
            self.state.dirty_slots.push(slot);
        }
    }

    fn input_items(&self) -> &[ItemIndex] {
        if self.required > MAX_FACE_INSTANCES { &self.layout_items } else { &self.visible }
    }

    fn retired(&self) -> bool {
        let state = &self.state;
        state.draw_group_refs.is_empty()
            && state.active_textures.is_empty()
            && state.active_terrain.is_empty()
            && state.draw_batches.is_empty()
    }
}

impl Eq for BlendedFace {}

impl PartialEq for BlendedFace {
    fn eq(&self, other: &Self) -> bool {
        self.distance_squared.total_cmp(&other.distance_squared) == Ordering::Equal && self.order == other.order
    }
}

impl Ord for BlendedFace {
    fn cmp(&self, other: &Self) -> Ordering {
        self.distance_squared.total_cmp(&other.distance_squared).then_with(|| other.order.cmp(&self.order))
    }
}

impl PartialOrd for BlendedFace {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Default)]
pub(super) struct PatchStats {
    pub(super) faces_patched: usize,
    pub(super) blended_sorted: bool,
}

#[derive(Default)]
pub(super) struct LayoutState {
    draws: Vec<DrawCommand>,
    mesh_references: HashSet<MeshId>,
    instance_bytes: Vec<u8>,
    active_groups: Vec<u32>,
    active_textures: Vec<TextureDemand>,
    active_terrain: Vec<TerrainDemand>,
    texture_seen: HashSet<TextureKey>,
    terrain_seen: HashSet<Region>,
    group_counts: Vec<usize>,
    group_offsets: Vec<usize>,
    group_cursors: Vec<usize>,
    layout_items: Vec<ItemIndex>,
    priority_items: Vec<ItemIndex>,
    draw_group_refs: Vec<u32>,
    draw_group_seen: HashSet<u32>,
    blended: Vec<BlendedFace>,
    dirty_slots: Vec<u32>,
    opaque_draws: usize,
    opaque_instances: usize,
    instance_count: usize,
    visible_items: usize,
    omitted: usize,
    valid: bool,
    batch_order: Vec<usize>,
    draw_batches: Vec<DrawBatch>,
    indirect_bytes: Vec<u8>,
    first_instances: Vec<u32>,
    blended_batches: usize,
    blended_commands: usize,
}

fn retire_vec<T>(values: &mut Vec<T>, budget: &mut usize) {
    while *budget > 0 && values.pop().is_some() {
        *budget -= 1;
    }
}

#[derive(Default)]
pub(super) struct DrawList {
    groups: DrawInterner,
    draws: Vec<DrawCommand>,
    mesh_references: HashSet<MeshId>,
    instance_bytes: Vec<u8>,
    hidden_slots: HashSet<u32>,
    active_groups: Vec<u32>,
    active_textures: Vec<TextureDemand>,
    active_terrain: Vec<TerrainDemand>,
    texture_seen: HashSet<TextureKey>,
    terrain_seen: HashSet<Region>,
    group_counts: Vec<usize>,
    group_offsets: Vec<usize>,
    group_cursors: Vec<usize>,
    layout_items: Vec<ItemIndex>,
    priority_items: Vec<ItemIndex>,
    draw_group_refs: Vec<u32>,
    draw_group_seen: HashSet<u32>,
    blended: Vec<BlendedFace>,
    dirty_slots: Vec<u32>,
    opaque_draws: usize,
    opaque_instances: usize,
    instance_count: usize,
    visible_items: usize,
    omitted: usize,
    valid: bool,
    batch_order: Vec<usize>,
    draw_batches: Vec<DrawBatch>,
    indirect_bytes: Vec<u8>,
    first_instances: Vec<u32>,
    blended_batches: usize,
    blended_commands: usize,
    batches_dirty: bool,
    batches_stale: bool,
    pending: Option<LayoutBuild>,
    retired: Option<LayoutBuild>,
    layout: LayoutId,
    last_layout: LayoutId,
}

impl DrawList {
    pub(super) fn request_layout(&mut self, visible: &[ItemIndex], camera: Camera, retire_previous: bool, items: &ItemSlab) -> bool {
        if self.pending.is_some() {
            return false;
        }
        if self.retired.is_some() {
            return false;
        }
        let layout = self.next_layout();
        self.pending = Some(LayoutBuild::new(layout, &self.layout_items, visible, camera, retire_previous, items));
        true
    }

    fn next_layout(&mut self) -> LayoutId {
        self.last_layout = LayoutId(self.last_layout.0.checked_add(1).expect("layout id space exhausted"));
        self.last_layout
    }

    #[cfg(test)]
    pub(super) fn committed_slot(&self, face: &CachedFace) -> Option<u32> {
        self.committed_address(face).map(|address| address.slot)
    }

    fn committed_address(&self, face: &CachedFace) -> Option<FaceSlot> {
        if face.layout == self.layout {
            return face.slot.map(|slot| FaceSlot { layout: face.layout, slot, blended_index: face.blended_index });
        }
        face.retained.filter(|retained| retained.layout == self.layout)
    }

    pub(super) fn pins_mesh(&self, mesh: MeshId) -> bool {
        self.mesh_references.contains(&mesh) || self.pending.as_ref().is_some_and(|build| !build.stale && build.mesh_pins.contains(&mesh))
    }

    #[cfg(test)]
    pub(super) fn committed_pins_mesh(&self, mesh: MeshId) -> bool {
        self.mesh_references.contains(&mesh)
    }

    #[cfg(test)]
    pub(super) fn committed_items(&self) -> &[ItemIndex] {
        &self.layout_items
    }

    #[cfg(test)]
    pub(super) fn blended_order(&self) -> Vec<u32> {
        self.draws.get(self.opaque_draws..).unwrap_or_default().iter().map(|draw| draw.first).collect()
    }

    #[cfg(test)]
    pub(super) fn staged_draw_count(&self) -> usize {
        self.pending.as_ref().map_or(0, |build| build.state.draws.len())
    }

    #[cfg(test)]
    pub(super) fn staged_progress(&self) -> Option<(&'static str, usize, usize, usize, usize, usize, usize)> {
        let build = self.pending.as_ref()?;
        let phase = match build.phase {
            LayoutPhase::RetirePrevious => "retire_previous",
            LayoutPhase::CountVisible => "count_visible",
            LayoutPhase::ClearInitialCounts => "clear_initial_counts",
            LayoutPhase::Priority => "priority",
            LayoutPhase::Select => "select",
            LayoutPhase::CountSelected => "count_selected",
            LayoutPhase::HideOmitted => "hide_omitted",
            LayoutPhase::Commands => "commands",
            LayoutPhase::FullbrightCommands => "fullbright_commands",
            LayoutPhase::Allocate => "allocate",
            LayoutPhase::Fill => "fill",
            LayoutPhase::EmitBlended => "emit_blended",
            LayoutPhase::ClearFinalCounts => "clear_final_counts",
            LayoutPhase::BatchOpaque => "batch_opaque",
            LayoutPhase::BatchOpaqueEncode => "batch_opaque_encode",
            LayoutPhase::BatchBlended => "batch_blended",
            LayoutPhase::Commit => "commit",
        };
        Some((
            phase,
            build.item_cursor,
            build.face_cursor,
            build.visible.len(),
            build.layout_items.len(),
            build.items_visited,
            build.faces_visited,
        ))
    }

    #[cfg(test)]
    pub(super) fn pending_stale(&self) -> bool {
        self.pending.as_ref().is_some_and(|build| build.stale)
    }

    pub(super) fn cancel_layout(&mut self) {
        if self.retired.is_none()
            && let Some(build) = self.pending.take()
        {
            self.retired = Some(build);
        }
    }

    pub(super) fn layout_pending(&self) -> bool {
        self.pending.is_some() || self.retired.is_some()
    }

    #[cfg(test)]
    pub(super) fn layout_queues(&self) -> (bool, bool) {
        (self.pending.is_some(), self.retired.is_some())
    }

    #[cfg(test)]
    pub(super) fn retired_entries_remaining(&self) -> usize {
        let Some(build) = self.retired.as_ref() else { return 0 };
        let state = &build.state;
        state.draw_group_refs.len() + state.active_textures.len() + state.active_terrain.len() + state.draw_batches.len()
    }

    pub(super) fn layout_building(&self) -> bool {
        self.pending.is_some()
    }

    pub(super) fn mark_layout_stale(&mut self) {
        if let Some(build) = self.pending.as_mut() {
            build.stale = true;
        }
    }

    pub(super) fn advance_layout(
        &mut self,
        items: &mut ItemSlab,
        meshes: &HashMap<MeshId, GpuMesh>,
        indirect_first_instance: bool,
    ) -> Option<LayoutProgress> {
        self.retire_layout_step();
        if self.retired.is_some() {
            return None;
        }
        let mut build = self.pending.take()?;
        if build.stale {
            self.retired = Some(build);
            return Some(LayoutProgress { stats: LayoutStats::default(), complete: false, stale: true, commit_ms: 0.0 });
        }
        let started = Instant::now();
        let mut item_work = 0usize;
        let mut byte_work = 0usize;
        let mut items_visited = 0usize;
        let mut faces_visited = 0usize;
        let mut commit_ms = 0.0;
        while item_work < LAYOUT_ITEMS_PER_FRAME && byte_work < LAYOUT_BYTES_PER_FRAME && started.elapsed() < LAYOUT_TIME_PER_FRAME {
            match build.phase {
                LayoutPhase::RetirePrevious => {
                    if build.item_cursor >= build.previous.len() {
                        build.phase = LayoutPhase::CountVisible;
                        build.item_cursor = 0;
                        build.face_cursor = 0;
                        continue;
                    }
                    let index = build.previous[build.item_cursor];
                    if build.face_cursor == 0 {
                        items_visited += 1;
                        build.items_visited += 1;
                    }
                    if contains_item(&build.visible, items, index) {
                        build.item_cursor += 1;
                        item_work += 1;
                        continue;
                    }
                    let Some(item) = items.get_mut(index) else {
                        build.item_cursor += 1;
                        item_work += 1;
                        continue;
                    };
                    if build.face_cursor >= item.faces.len() {
                        build.item_cursor += 1;
                        build.face_cursor = 0;
                        item_work += 1;
                        continue;
                    }
                    if let Some(face) = item.faces.get_mut(build.face_cursor) {
                        self.hide_face(face);
                        faces_visited += 1;
                        build.faces_visited += 1;
                        byte_work += std::mem::size_of::<CachedFace>();
                    }
                    build.face_cursor += 1;
                }
                LayoutPhase::CountVisible | LayoutPhase::CountSelected => {
                    let count_visible = matches!(build.phase, LayoutPhase::CountVisible);
                    let input = if count_visible { &build.visible } else { &build.layout_items };
                    if build.item_cursor >= input.len() {
                        build.item_cursor = 0;
                        build.face_cursor = 0;
                        if count_visible && build.required > MAX_FACE_INSTANCES {
                            build.phase = LayoutPhase::ClearInitialCounts;
                            build.group_cursor = 0;
                            build.state.visible_items = 0;
                        } else if count_visible {
                            build.phase = LayoutPhase::Commands;
                            build.group_cursor = 0;
                        } else {
                            build.phase = LayoutPhase::HideOmitted;
                        }
                        continue;
                    }
                    let index = input[build.item_cursor];
                    let Some(item) = items.get(index) else {
                        build.item_cursor += 1;
                        item_work += 1;
                        continue;
                    };
                    if build.face_cursor == 0 {
                        item_work += 1;
                        items_visited += 1;
                        build.items_visited += 1;
                    }
                    if build.face_cursor >= item.faces.len() {
                        build.item_cursor += 1;
                        build.face_cursor = 0;
                        continue;
                    }
                    let face = &item.faces[build.face_cursor];
                    let group = face.group as usize;
                    if group >= build.state.group_counts.len() {
                        let capacity = group.saturating_add(1);
                        build.state.group_counts.resize(capacity, 0);
                        build.state.group_offsets.resize(capacity, 0);
                        build.state.group_cursors.resize(capacity, 0);
                    }
                    let count = &mut build.state.group_counts[group];
                    if *count == 0 {
                        build.state.active_groups.push(face.group);
                    }
                    *count = count.saturating_add(1);
                    if count_visible {
                        build.required = build.required.saturating_add(1);
                    }
                    build.face_cursor += 1;
                    faces_visited += 1;
                    build.faces_visited += 1;
                    byte_work += std::mem::size_of::<CachedFace>();
                }
                LayoutPhase::ClearInitialCounts => {
                    if build.group_cursor >= build.state.active_groups.len() {
                        build.state.active_groups.clear();
                        build.phase = LayoutPhase::Priority;
                        build.item_cursor = 0;
                        continue;
                    }
                    let group = build.state.active_groups[build.group_cursor] as usize;
                    if let Some(count) = build.state.group_counts.get_mut(group) {
                        *count = 0;
                    }
                    build.group_cursor += 1;
                    item_work += 1;
                }
                LayoutPhase::Priority => {
                    if build.item_cursor >= build.visible.len() {
                        build.item_cursor = 0;
                        build.phase = LayoutPhase::Select;
                        continue;
                    }
                    let index = build.visible[build.item_cursor];
                    build.item_cursor += 1;
                    item_work += 1;
                    items_visited += 1;
                    build.items_visited += 1;
                    let Some(item) = items.get(index) else { continue };
                    let (priority, distance) = draw_priority(item, build.camera);
                    build.priority.push(PriorityItem { priority, distance, id: item.id, index, faces: item.faces.len() });
                }
                LayoutPhase::Select => {
                    let Some(candidate) = build.priority.pop() else {
                        build.phase = LayoutPhase::CountSelected;
                        build.item_cursor = 0;
                        build.face_cursor = 0;
                        build.group_cursor = 0;
                        continue;
                    };
                    item_work += 1;
                    items_visited += 1;
                    build.items_visited += 1;
                    if candidate.faces <= build.remaining {
                        build.remaining -= candidate.faces;
                        build.layout_items.push(candidate.index);
                        build.selected_items.insert(candidate.index);
                    } else {
                        build.omitted = build.omitted.saturating_add(candidate.faces);
                    }
                }
                LayoutPhase::HideOmitted => {
                    if build.required <= MAX_FACE_INSTANCES || build.item_cursor >= build.visible.len() {
                        build.phase = LayoutPhase::Commands;
                        build.item_cursor = 0;
                        build.face_cursor = 0;
                        build.group_cursor = 0;
                        continue;
                    }
                    let index = build.visible[build.item_cursor];
                    if build.selected_items.contains(&index) {
                        build.item_cursor += 1;
                        build.face_cursor = 0;
                        item_work += 1;
                        items_visited += 1;
                        build.items_visited += 1;
                        continue;
                    }
                    if build.face_cursor == 0 {
                        items_visited += 1;
                        build.items_visited += 1;
                    }
                    let Some(item) = items.get_mut(index) else {
                        build.item_cursor += 1;
                        item_work += 1;
                        continue;
                    };
                    if build.face_cursor >= item.faces.len() {
                        build.item_cursor += 1;
                        build.face_cursor = 0;
                        item_work += 1;
                        continue;
                    }
                    if let Some(face) = item.faces.get_mut(build.face_cursor) {
                        self.hide_face(face);
                        faces_visited += 1;
                        build.faces_visited += 1;
                        byte_work += std::mem::size_of::<CachedFace>();
                    }
                    build.face_cursor += 1;
                }
                LayoutPhase::Commands | LayoutPhase::FullbrightCommands => {
                    let fullbright = matches!(build.phase, LayoutPhase::FullbrightCommands);
                    if build.group_cursor >= build.state.active_groups.len() {
                        build.group_cursor = 0;
                        if fullbright {
                            build.state.opaque_draws = build.state.draws.len();
                            build.state.opaque_instances = build.opaque_count;
                            build.phase = LayoutPhase::Allocate;
                        } else {
                            build.phase = LayoutPhase::FullbrightCommands;
                        }
                        continue;
                    }
                    let group_id = build.state.active_groups[build.group_cursor];
                    let group_index = group_id as usize;
                    let count = build.state.group_counts.get(group_index).copied().unwrap_or_default();
                    if !fullbright {
                        if group_index >= build.group_limits.len() {
                            build.group_limits.resize(group_index + 1, 0);
                        }
                        build.group_limits[group_index] = count;
                    }
                    if let Some(group) = self.groups.group(group_id) {
                        if !fullbright
                            && let Some(key) = group.key.texture
                            && build.state.texture_seen.insert(key)
                        {
                            build.state.active_textures.push(TextureDemand { key, handle: group.texture.clone() });
                        }
                        if !fullbright
                            && let Some(region) = group.key.terrain
                            && build.state.terrain_seen.insert(region)
                        {
                            build.state.active_terrain.push(TerrainDemand {
                                region,
                                textures: group.terrain_textures.clone().unwrap_or_else(|| std::array::from_fn(|_| None)),
                            });
                        }
                        if !group.key.blended() && group.key.fullbright() == fullbright {
                            build.state.group_offsets[group_index] = build.opaque_count;
                            build.state.group_cursors[group_index] = 0;
                            build.opaque_count = build.opaque_count.saturating_add(count);
                            if count > 0
                                && let (Ok(first), Ok(count)) = (u32::try_from(build.opaque_count - count), u32::try_from(count))
                            {
                                build.state.mesh_references.insert(group.key.mesh);
                                build.state.draws.push(group.key.command(group_id, first, count));
                                self.retain_layout_group(&mut build.state, group_id);
                            }
                        }
                    }
                    build.group_cursor += 1;
                    item_work += 1;
                }
                LayoutPhase::Allocate => {
                    let target = build.opaque_count.saturating_mul(INSTANCE_BYTES);
                    build.instance_bytes = target;
                    if !build.instance_reserved {
                        build.state.instance_bytes.reserve_exact(target);
                        build.instance_reserved = true;
                    }
                    if build.state.instance_bytes.len() < target {
                        let allowance = LAYOUT_BYTES_PER_FRAME.saturating_sub(byte_work).min(target - build.state.instance_bytes.len());
                        if allowance == 0 {
                            break;
                        }
                        let end = build.state.instance_bytes.len() + allowance;
                        build.state.instance_bytes.resize(end, 0);
                        byte_work += allowance;
                        continue;
                    }
                    build.item_cursor = 0;
                    build.face_cursor = 0;
                    build.phase = LayoutPhase::Fill;
                }
                LayoutPhase::Fill => {
                    let input_len = build.input_items().len();
                    if build.item_cursor >= input_len {
                        build.phase = LayoutPhase::EmitBlended;
                        continue;
                    }
                    let index = build.input_items()[build.item_cursor];
                    let Some(item) = items.get_mut(index) else {
                        build.item_cursor += 1;
                        build.face_cursor = 0;
                        build.item_has_slot = false;
                        item_work += 1;
                        continue;
                    };
                    if build.face_cursor == 0 {
                        item_work += 1;
                        items_visited += 1;
                        build.items_visited += 1;
                        build.item_has_slot = false;
                    }
                    if build.face_cursor >= item.faces.len() {
                        build.visible_items += usize::from(build.item_has_slot);
                        build.item_cursor += 1;
                        build.face_cursor = 0;
                        build.item_has_slot = false;
                        continue;
                    }
                    let face_index = build.face_cursor;
                    let Some(face) = item.faces.get_mut(face_index) else {
                        build.stale = true;
                        break;
                    };
                    if face.blended {
                        build.item_has_slot = true;
                        build.blended.push(BlendedFace {
                            distance_squared: blend_distance(item.position, item.rotation, item.scale, face.center, build.camera),
                            order: face.order,
                            item: index,
                            face: face_index,
                            group: face.group,
                            slot: 0,
                        });
                    } else {
                        let group = face.group as usize;
                        let cursor = build.state.group_cursors.get_mut(group);
                        let offset = if let Some(cursor) = cursor {
                            let count = build.group_limits.get(group).copied().unwrap_or_default();
                            if *cursor < count {
                                let offset = build.state.group_offsets[group].saturating_add(*cursor);
                                *cursor += 1;
                                Some(offset)
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        let slot = offset.and_then(|offset| u32::try_from(offset).ok()).filter(|slot| {
                            write_layout_instance(
                                &self.instance_bytes,
                                &mut build.state.instance_bytes,
                                &mut build.state.dirty_slots,
                                *slot,
                                &item.transform,
                                &face.parameter_bytes,
                            )
                        });
                        assign_slot(face, build.layout, self.layout, slot, None);
                        build.item_has_slot |= slot.is_some();
                    }
                    build.face_cursor += 1;
                    faces_visited += 1;
                    build.faces_visited += 1;
                    byte_work += std::mem::size_of::<CachedFace>();
                }
                LayoutPhase::EmitBlended => {
                    let Some(blended) = build.blended.pop() else {
                        build.group_cursor = 0;
                        build.phase = LayoutPhase::ClearFinalCounts;
                        continue;
                    };
                    item_work += 1;
                    let Some(item) = items.get_mut(blended.item) else {
                        build.stale = true;
                        continue;
                    };
                    let Some(face) = item.faces.get_mut(blended.face) else {
                        build.stale = true;
                        continue;
                    };
                    if face.group != blended.group || face.order != blended.order || !face.blended {
                        build.stale = true;
                        continue;
                    }
                    let Ok(slot) = u32::try_from(build.opaque_count.saturating_add(build.blend_index)) else {
                        build.stale = true;
                        continue;
                    };
                    let blend_index = build.blend_index;
                    build.blend_index += 1;
                    build.state.blended.push(BlendedFace { slot, ..blended });
                    let start = slot as usize * INSTANCE_BYTES;
                    let old = self.instance_bytes.get(start..start + INSTANCE_BYTES);
                    build.state.instance_bytes.extend_from_slice(&item.transform);
                    build.state.instance_bytes.extend_from_slice(&face.parameter_bytes);
                    let new = build.state.instance_bytes.get(start..start + INSTANCE_BYTES);
                    if old.zip(new).is_none_or(|(old, new)| old != new) {
                        build.state.dirty_slots.push(slot);
                    }
                    let Some(key) = self.groups.key(blended.group) else {
                        build.stale = true;
                        break;
                    };
                    assign_slot(face, build.layout, self.layout, Some(slot), Some(blend_index));
                    build.state.mesh_references.insert(key.mesh);
                    build.state.draws.push(key.command(blended.group, slot, 1));
                    self.retain_layout_group(&mut build.state, blended.group);
                    faces_visited += 1;
                    build.faces_visited += 1;
                    byte_work += INSTANCE_BYTES;
                }
                LayoutPhase::ClearFinalCounts => {
                    if build.group_cursor >= build.state.active_groups.len() {
                        build.state.active_groups.clear();
                        build.batch_cursor = 0;
                        build.phase = LayoutPhase::BatchOpaque;
                        continue;
                    }
                    let group = build.state.active_groups[build.group_cursor] as usize;
                    if let Some(count) = build.state.group_counts.get_mut(group) {
                        *count = 0;
                    }
                    build.group_cursor += 1;
                    item_work += 1;
                }
                LayoutPhase::BatchOpaque => {
                    if build.batch_cursor >= build.state.opaque_draws {
                        build.batch_order_cursor = 0;
                        build.batch_command_cursor = 0;
                        build.phase = LayoutPhase::BatchOpaqueEncode;
                        continue;
                    }
                    let draw = &build.state.draws[build.batch_cursor];
                    let (binding, args) = ensure_batch(&mut self.groups, draw, meshes);
                    if build.batch_seen.insert(binding.id) {
                        build.state.batch_order.push(binding.id);
                        if let Some(group) = self.groups.batch_group_mut(binding.id) {
                            group.commands.clear();
                        }
                    }
                    if let Some(group) = self.groups.batch_group_mut(binding.id) {
                        group.commands.push(args);
                    } else {
                        build.stale = true;
                        break;
                    }
                    build.batch_cursor += 1;
                    item_work += 1;
                    byte_work += INDIRECT_DRAW_BYTES;
                }
                LayoutPhase::BatchOpaqueEncode => {
                    if build.batch_order_cursor >= build.state.batch_order.len() {
                        build.batch_cursor = build.state.opaque_draws;
                        build.blended_batch_start = build.state.draw_batches.len();
                        build.state.blended_batches = build.state.draw_batches.len();
                        build.state.blended_commands = build.state.first_instances.len();
                        build.phase = LayoutPhase::BatchBlended;
                        continue;
                    }
                    let id = build.state.batch_order[build.batch_order_cursor];
                    let Some(group) = self.groups.batch_group(id) else {
                        build.stale = true;
                        break;
                    };
                    if build.batch_command_cursor == 0 {
                        let offset = build.state.indirect_bytes.len() as u64;
                        build.state.draw_batches.push(DrawBatch {
                            id,
                            key: group.key,
                            buffers: Arc::clone(&group.buffers),
                            offset,
                            count: group.commands.len() as u32,
                        });
                    }
                    if let Some(args) = group.commands.get(build.batch_command_cursor).copied() {
                        build.state.first_instances.push(args.first_instance);
                        encode_draw(&args, &mut build.state.indirect_bytes, indirect_first_instance);
                    } else {
                        build.stale = true;
                        break;
                    }
                    build.batch_command_cursor += 1;
                    item_work += 1;
                    byte_work += INDIRECT_DRAW_BYTES;
                    if build.batch_command_cursor >= group.commands.len() {
                        build.batch_order_cursor += 1;
                        build.batch_command_cursor = 0;
                    }
                }
                LayoutPhase::BatchBlended => {
                    if build.batch_cursor >= build.state.draws.len() {
                        build.phase = LayoutPhase::Commit;
                        continue;
                    }
                    let draw = &build.state.draws[build.batch_cursor];
                    let (binding, args) = ensure_batch(&mut self.groups, draw, meshes);
                    let merge = build.state.draw_batches.len() > build.blended_batch_start
                        && build.state.draw_batches.last().is_some_and(|batch| batch.id == binding.id);
                    if merge {
                        if let Some(batch) = build.state.draw_batches.last_mut() {
                            batch.count += 1;
                        }
                    } else if let Some(group) = self.groups.batch_group(binding.id) {
                        let offset = build.state.indirect_bytes.len() as u64;
                        build.state.draw_batches.push(DrawBatch {
                            id: binding.id,
                            key: group.key,
                            buffers: Arc::clone(&group.buffers),
                            offset,
                            count: 1,
                        });
                    } else {
                        build.stale = true;
                        break;
                    }
                    build.state.first_instances.push(args.first_instance);
                    encode_draw(&args, &mut build.state.indirect_bytes, indirect_first_instance);
                    build.batch_cursor += 1;
                    item_work += 1;
                    byte_work += INDIRECT_DRAW_BYTES;
                }
                LayoutPhase::Commit => break,
            }
        }
        if build.stale {
            self.retired = Some(build);
            return Some(LayoutProgress {
                stats: LayoutStats { items_visited, faces_visited, ..LayoutStats::default() },
                complete: false,
                stale: true,
                commit_ms,
            });
        }
        let complete = matches!(build.phase, LayoutPhase::Commit);
        if complete {
            let commit_started = Instant::now();
            build.state.layout_items = if build.required > MAX_FACE_INSTANCES {
                std::mem::take(&mut build.layout_items)
            } else {
                std::mem::take(&mut build.visible)
            };
            build.state.visible_items = build.visible_items;
            build.state.omitted = build.omitted;
            build.state.instance_count = build.state.instance_bytes.len() / INSTANCE_BYTES;
            build.state.valid = true;
            let omitted_faces = build.omitted;
            let stale = build.stale;
            self.swap_state(&mut build.state);
            self.layout = build.layout;
            self.retired = Some(build);
            commit_ms += commit_started.elapsed().as_secs_f64() * 1000.0;
            Some(LayoutProgress {
                stats: LayoutStats { omitted_faces, blended: !self.blended.is_empty(), items_visited, faces_visited },
                complete: true,
                stale,
                commit_ms,
            })
        } else {
            let stale = build.stale;
            let omitted_faces = build.omitted;
            self.pending = Some(build);
            Some(LayoutProgress {
                stats: LayoutStats { omitted_faces, blended: false, items_visited, faces_visited },
                complete: false,
                stale,
                commit_ms,
            })
        }
    }

    fn retain_layout_group(&mut self, state: &mut LayoutState, group: u32) {
        if state.draw_group_seen.insert(group) && self.groups.retain(group) {
            state.draw_group_refs.push(group);
        }
    }

    fn swap_state(&mut self, state: &mut LayoutState) {
        std::mem::swap(&mut self.draws, &mut state.draws);
        std::mem::swap(&mut self.mesh_references, &mut state.mesh_references);
        std::mem::swap(&mut self.instance_bytes, &mut state.instance_bytes);
        std::mem::swap(&mut self.active_groups, &mut state.active_groups);
        std::mem::swap(&mut self.active_textures, &mut state.active_textures);
        std::mem::swap(&mut self.active_terrain, &mut state.active_terrain);
        std::mem::swap(&mut self.texture_seen, &mut state.texture_seen);
        std::mem::swap(&mut self.terrain_seen, &mut state.terrain_seen);
        std::mem::swap(&mut self.group_counts, &mut state.group_counts);
        std::mem::swap(&mut self.group_offsets, &mut state.group_offsets);
        std::mem::swap(&mut self.group_cursors, &mut state.group_cursors);
        std::mem::swap(&mut self.layout_items, &mut state.layout_items);
        std::mem::swap(&mut self.priority_items, &mut state.priority_items);
        std::mem::swap(&mut self.draw_group_refs, &mut state.draw_group_refs);
        std::mem::swap(&mut self.draw_group_seen, &mut state.draw_group_seen);
        std::mem::swap(&mut self.blended, &mut state.blended);
        std::mem::swap(&mut self.dirty_slots, &mut state.dirty_slots);
        std::mem::swap(&mut self.opaque_draws, &mut state.opaque_draws);
        std::mem::swap(&mut self.opaque_instances, &mut state.opaque_instances);
        std::mem::swap(&mut self.instance_count, &mut state.instance_count);
        std::mem::swap(&mut self.visible_items, &mut state.visible_items);
        std::mem::swap(&mut self.omitted, &mut state.omitted);
        std::mem::swap(&mut self.valid, &mut state.valid);
        std::mem::swap(&mut self.batch_order, &mut state.batch_order);
        std::mem::swap(&mut self.draw_batches, &mut state.draw_batches);
        std::mem::swap(&mut self.indirect_bytes, &mut state.indirect_bytes);
        std::mem::swap(&mut self.first_instances, &mut state.first_instances);
        std::mem::swap(&mut self.blended_batches, &mut state.blended_batches);
        std::mem::swap(&mut self.blended_commands, &mut state.blended_commands);
        self.hidden_slots.clear();
        self.batches_dirty = false;
    }

    fn retire_layout_step(&mut self) {
        let Some(build) = self.retired.as_mut() else { return };
        let mut budget = RETIRED_LAYOUT_ENTRIES_PER_FRAME;
        while budget > 0 && !build.retired() {
            if let Some(group) = build.state.draw_group_refs.pop() {
                self.groups.release(group);
                budget -= 1;
            } else {
                retire_vec(&mut build.state.active_textures, &mut budget);
                retire_vec(&mut build.state.active_terrain, &mut budget);
                retire_vec(&mut build.state.draw_batches, &mut budget);
            }
        }
        if self.retired.as_ref().is_some_and(LayoutBuild::retired) {
            self.retired = None;
        }
    }

    #[cfg(test)]
    pub(super) fn layout(&mut self, previous: &[ItemIndex], visible: &[ItemIndex], items: &mut ItemSlab, camera: Camera) -> LayoutStats {
        let mut items_visited = 0usize;
        let mut faces_visited = 0usize;
        let layout = self.next_layout();
        self.layout = layout;
        self.release_draw_group_refs();
        self.draws.clear();
        self.mesh_references.clear();
        self.active_groups.clear();
        self.active_textures.clear();
        self.active_terrain.clear();
        self.texture_seen.clear();
        self.terrain_seen.clear();
        self.blended.clear();
        self.layout_items.clear();
        self.priority_items.clear();
        self.dirty_slots.clear();
        self.opaque_draws = 0;
        self.opaque_instances = 0;
        self.instance_count = 0;
        self.visible_items = 0;
        self.omitted = 0;

        if previous != visible {
            for index in previous {
                if !contains_item(visible, items, *index)
                    && let Some(item) = items.get_mut(*index)
                {
                    items_visited += 1;
                    faces_visited = faces_visited.saturating_add(item.faces.len());
                    for face in &mut item.faces {
                        face.slot = None;
                        face.blended_index = None;
                    }
                }
            }
        }

        self.group_counts.resize(self.groups.groups.len(), 0);
        self.group_offsets.resize(self.groups.groups.len(), 0);
        self.group_cursors.resize(self.groups.groups.len(), 0);
        let mut required = 0usize;
        for id in visible {
            let Some(item) = items.get(*id) else { continue };
            items_visited += 1;
            faces_visited = faces_visited.saturating_add(item.faces.len());
            required = required.saturating_add(item.faces.len());
            let drawable = self.count_item_faces(item);
            self.visible_items += usize::from(drawable);
        }
        if required > MAX_FACE_INSTANCES {
            self.omitted = 0;
            self.visible_items = 0;
            self.clear_group_counts();
            self.priority_items.extend_from_slice(visible);
            self.priority_items.sort_unstable_by(|a, b| {
                let a_priority = items.get(*a).map_or((0.0, f64::INFINITY), |item| draw_priority(item, camera));
                let b_priority = items.get(*b).map_or((0.0, f64::INFINITY), |item| draw_priority(item, camera));
                b_priority
                    .0
                    .total_cmp(&a_priority.0)
                    .then_with(|| a_priority.1.total_cmp(&b_priority.1))
                    .then_with(|| items.get(*a).map(|item| item.id).cmp(&items.get(*b).map(|item| item.id)))
            });
            items_visited = items_visited.saturating_add(self.priority_items.len());
            let mut remaining = MAX_FACE_INSTANCES;
            for id in &self.priority_items {
                let count = items.get(*id).map_or(0, |item| item.faces.len());
                faces_visited = faces_visited.saturating_add(count);
                if count <= remaining {
                    remaining -= count;
                    self.layout_items.push(*id);
                } else {
                    self.omitted = self.omitted.saturating_add(count);
                    if let Some(item) = items.get_mut(*id) {
                        faces_visited = faces_visited.saturating_add(item.faces.len());
                        for face in &mut item.faces {
                            face.slot = None;
                            face.blended_index = None;
                        }
                    }
                }
            }
            for index in 0..self.layout_items.len() {
                let id = self.layout_items[index];
                let Some(item) = items.get(id) else { continue };
                items_visited += 1;
                faces_visited = faces_visited.saturating_add(item.faces.len());
                let drawable = self.count_item_faces(item);
                self.visible_items += usize::from(drawable);
            }
        }
        let layout_items = if required > MAX_FACE_INSTANCES { &self.layout_items } else { visible };

        let mut opaque_count = 0usize;
        for fullbright in [false, true] {
            for group_id in &self.active_groups {
                let index = *group_id as usize;
                let count = self.group_counts[index];
                let Some(group) = self.groups.groups[index].as_ref() else { continue };
                if !fullbright
                    && let Some(key) = group.key.texture
                    && self.texture_seen.insert(key)
                {
                    self.active_textures.push(TextureDemand { key, handle: group.texture.clone() });
                }
                if !fullbright
                    && let Some(region) = group.key.terrain
                    && self.terrain_seen.insert(region)
                {
                    self.active_terrain.push(TerrainDemand {
                        region,
                        textures: group.terrain_textures.clone().unwrap_or_else(|| std::array::from_fn(|_| None)),
                    });
                }
                if !group.key.blended() && group.key.fullbright() == fullbright {
                    self.group_offsets[index] = opaque_count;
                    self.group_cursors[index] = 0;
                    opaque_count = opaque_count.saturating_add(count);
                    if count > 0
                        && let (Ok(first), Ok(count)) = (u32::try_from(opaque_count - count), u32::try_from(count))
                    {
                        self.mesh_references.insert(group.key.mesh);
                        self.draws.push(group.key.command(*group_id, first, count));
                    }
                }
            }
        }
        for group in self.active_groups.drain(..) {
            self.group_counts[group as usize] = 0;
        }
        self.opaque_draws = self.draws.len();
        self.opaque_instances = opaque_count;
        self.instance_bytes.resize(opaque_count.saturating_mul(INSTANCE_BYTES), 0);

        for id in layout_items {
            let Some(item) = items.get_mut(*id) else { continue };
            items_visited += 1;
            faces_visited = faces_visited.saturating_add(item.faces.len());
            for (face_index, face) in item.faces.iter_mut().enumerate() {
                if face.blended {
                    let Ok(slot) = u32::try_from(self.opaque_instances.saturating_add(self.blended.len())) else { continue };
                    face.slot = Some(slot);
                    face.blended_index = Some(self.blended.len());
                    face.layout = layout;
                    face.retained = None;
                    self.instance_bytes.extend_from_slice(&item.transform);
                    self.instance_bytes.extend_from_slice(&face.parameter_bytes);
                    self.blended.push(BlendedFace {
                        distance_squared: blend_distance(item.position, item.rotation, item.scale, face.center, camera),
                        order: face.order,
                        item: *id,
                        face: face_index,
                        group: face.group,
                        slot,
                    });
                } else {
                    let group_index = face.group as usize;
                    let cursor = &mut self.group_cursors[group_index];
                    let offset = self.group_offsets[group_index].saturating_add(*cursor);
                    let Ok(slot) = u32::try_from(offset) else { continue };
                    if write_instance(&mut self.instance_bytes, slot, &item.transform, &face.parameter_bytes) {
                        face.slot = Some(slot);
                        face.layout = layout;
                        face.retained = None;
                        *cursor += 1;
                    }
                }
            }
        }
        self.reorder_blended(items);
        self.instance_count = self.instance_bytes.len() / INSTANCE_BYTES;
        self.valid = true;
        LayoutStats { omitted_faces: self.omitted, blended: !self.blended.is_empty(), items_visited, faces_visited }
    }

    #[cfg(test)]
    fn count_item_faces(&mut self, item: &CachedItem) -> bool {
        let mut drawable = false;
        for face in &item.faces {
            let index = face.group as usize;
            let count = &mut self.group_counts[index];
            if *count == 0 {
                self.active_groups.push(face.group);
            }
            *count += 1;
            drawable = true;
        }
        drawable
    }

    pub(super) fn patch(
        &mut self,
        items: &ItemSlab,
        faces: &[(ItemIndex, usize)],
        pose_items: &[ItemIndex],
        camera: Camera,
        origin: DVec3,
        transform_origin: DVec3,
    ) -> PatchStats {
        let mut stats = PatchStats::default();
        for (item_id, face_index) in faces {
            let Some(item) = items.get(*item_id) else { continue };
            let Some(face) = item.faces.get(*face_index) else { continue };
            if let Some(build) = self.pending.as_mut()
                && !build.stale
                && face.layout == build.layout
                && let Some(slot) = face.slot
                && write_instance(&mut build.state.instance_bytes, slot, &item.transform, &face.parameter_bytes)
            {
                build.upload(slot);
            }
            let Some(address) = self.committed_address(face) else { continue };
            let transform = if origin == transform_origin {
                item.transform
            } else {
                instance_base(item.position, item.rotation, item.scale, item.invert_normals, origin)
            };
            if !write_instance(&mut self.instance_bytes, address.slot, &transform, &face.parameter_bytes) {
                continue;
            }
            self.committed_written(address.slot);
            stats.faces_patched += 1;
            if pose_items.binary_search(item_id).is_ok()
                && let Some(index) = address.blended_index
                && let Some(blended) = self.blended.get_mut(index)
            {
                blended.distance_squared = blend_distance(item.position, item.rotation, item.scale, face.center, camera);
                stats.blended_sorted = true;
            }
        }
        self.dirty_slots.sort_unstable();
        self.dirty_slots.dedup();
        stats
    }

    fn committed_written(&mut self, slot: u32) {
        self.dirty_slots.push(slot);
        if let Some(build) = self.pending.as_mut()
            && (slot as usize) < build.state.instance_bytes.len() / INSTANCE_BYTES
        {
            build.upload(slot);
        }
    }

    pub(super) fn sort_blended(&mut self, items: &ItemSlab, camera: Camera) -> bool {
        if self.blended.is_empty() {
            return false;
        }
        for blended in &mut self.blended {
            if let Some(item) = items.get(blended.item)
                && let Some(face) = item.faces.get(blended.face)
            {
                blended.distance_squared = blend_distance(item.position, item.rotation, item.scale, face.center, camera);
            }
        }
        self.reorder_blended(items);
        true
    }

    pub(super) fn reorder_blended(&mut self, items: &ItemSlab) {
        let mut order = (0..self.blended.len())
            .filter(|index| {
                let blended = &self.blended[*index];
                items
                    .get(blended.item)
                    .and_then(|item| item.faces.get(blended.face))
                    .is_some_and(|face| face.group == blended.group && face.order == blended.order && face.blended)
            })
            .collect::<Vec<_>>();
        self.release_draw_group_refs();
        order.sort_unstable_by(|a, b| {
            let (a, b) = (&self.blended[*a], &self.blended[*b]);
            b.distance_squared.total_cmp(&a.distance_squared).then_with(|| a.order.cmp(&b.order))
        });
        self.draws.truncate(self.opaque_draws);
        for index in order {
            let blended = &self.blended[index];
            if let Some(key) = self.groups.key(blended.group) {
                self.mesh_references.insert(key.mesh);
                self.draws.push(key.command(blended.group, blended.slot, 1));
            }
        }
        self.retain_draw_group_refs();
    }

    pub(super) fn upload_layout(&mut self, queue: &Queue, buffer: &Buffer) -> UploadStats {
        let mut stats = UploadStats::default();
        self.dirty_slots.sort_unstable();
        self.dirty_slots.dedup();
        let mut range_count = 0usize;
        let mut uploaded_bytes = 0usize;
        let mut first = None;
        let mut last = 0u32;
        for slot in self.dirty_slots.iter().copied() {
            if first.is_none_or(|_| slot.saturating_sub(last) > MAX_PATCH_GAP_SLOTS) {
                if let Some(start) = first {
                    uploaded_bytes = uploaded_bytes.saturating_add((last as usize + 1 - start as usize) * INSTANCE_BYTES);
                }
                range_count += 1;
                first = Some(slot);
            }
            last = slot;
        }
        if let Some(start) = first {
            uploaded_bytes = uploaded_bytes.saturating_add((last as usize + 1 - start as usize) * INSTANCE_BYTES);
        }
        if range_count > MAX_LAYOUT_UPLOAD_RANGES || uploaded_bytes > self.instance_bytes.len() / 2 {
            if !self.instance_bytes.is_empty() {
                queue.write_buffer(buffer, 0, &self.instance_bytes);
                stats.bytes = self.instance_bytes.len();
                stats.calls = 1;
            }
        } else {
            first = None;
            for slot in self.dirty_slots.iter().copied() {
                if let Some(start) = first
                    && slot.saturating_sub(last) > MAX_PATCH_GAP_SLOTS
                {
                    let uploaded = self.write_slot_range(queue, buffer, start, last);
                    stats.bytes += uploaded.bytes;
                    stats.calls += uploaded.calls;
                    first = None;
                }
                if first.is_none() {
                    first = Some(slot);
                }
                last = slot;
            }
            if let Some(start) = first {
                let uploaded = self.write_slot_range(queue, buffer, start, last);
                stats.bytes += uploaded.bytes;
                stats.calls += uploaded.calls;
            }
        }
        self.dirty_slots.clear();
        stats
    }

    pub(super) fn upload_patch(&mut self, queue: &Queue, buffer: &Buffer) -> UploadStats {
        let mut stats = UploadStats::default();
        let mut range_count = 0;
        let mut previous = None;
        for slot in self.dirty_slots.iter().copied() {
            if previous.is_none_or(|last: u32| slot.saturating_sub(last) > MAX_PATCH_GAP_SLOTS) {
                range_count += 1;
            }
            previous = Some(slot);
        }
        if range_count > MAX_PATCH_RANGES {
            if !self.instance_bytes.is_empty() {
                queue.write_buffer(buffer, 0, &self.instance_bytes);
                stats.bytes = self.instance_bytes.len();
                stats.calls = 1;
            }
            self.dirty_slots.clear();
            return stats;
        }
        let mut first = None;
        let mut last = 0u32;
        for slot in self.dirty_slots.iter().copied() {
            if let Some(start) = first
                && slot.saturating_sub(last) > MAX_PATCH_GAP_SLOTS
            {
                let uploaded = self.write_slot_range(queue, buffer, start, last);
                stats.bytes += uploaded.bytes;
                stats.calls += uploaded.calls;
                first = None;
            }
            if first.is_none() {
                first = Some(slot);
            }
            last = slot;
        }
        if let Some(start) = first {
            let uploaded = self.write_slot_range(queue, buffer, start, last);
            stats.bytes += uploaded.bytes;
            stats.calls += uploaded.calls;
        }
        self.dirty_slots.clear();
        stats
    }

    fn write_slot_range(&self, queue: &Queue, buffer: &Buffer, first: u32, last: u32) -> UploadStats {
        let start = first as usize * INSTANCE_BYTES;
        let end = (last as usize + 1).saturating_mul(INSTANCE_BYTES);
        if let Some(bytes) = self.instance_bytes.get(start..end) {
            queue.write_buffer(buffer, start as u64, bytes);
            return UploadStats { bytes: bytes.len(), calls: 1 };
        }
        UploadStats::default()
    }

    pub(super) fn intern_face(
        &mut self,
        key: DrawKey,
        texture: Option<&TextureHandle>,
        terrain_textures: Option<&[Option<TextureHandle>; 4]>,
    ) -> Option<u32> {
        self.groups.intern(key, texture, terrain_textures)
    }

    #[cfg(test)]
    pub(super) fn face_key(&self, group: u32) -> Option<DrawKey> {
        self.groups.key(group)
    }

    pub(super) fn refresh_winding(
        &mut self,
        faces: &mut [CachedFace],
        old_scale: Vec3,
        old_invert_normals: bool,
        scale: Vec3,
        invert_normals: bool,
    ) -> bool {
        if (math::flips_winding(old_scale) ^ old_invert_normals) == (math::flips_winding(scale) ^ invert_normals) {
            return false;
        }
        let mut changed = false;
        for face in faces {
            let Some(mut key) = self.groups.key(face.group) else { continue };
            let previous_pipeline = key.pipeline;
            key.pipeline = surface_pipeline(&face.base_params, scale, invert_normals);
            if key.pipeline == previous_pipeline {
                continue;
            }
            if let Some(group) = self.groups.intern(key, face.texture.as_ref(), None) {
                self.groups.release(face.group);
                face.group = group;
                changed = true;
            }
        }
        changed
    }

    pub(super) fn release_faces(&mut self, faces: &[CachedFace]) {
        for face in faces {
            self.groups.release(face.group);
        }
    }

    pub(super) fn hide_item(&mut self, item: &mut CachedItem) {
        let mut hidden = false;
        for face in &mut item.faces {
            hidden |= self.hide_face(face);
        }
        if hidden {
            debug_assert!(self.visible_items > 0);
            self.visible_items -= 1;
        }
    }

    pub(super) fn hide_face(&mut self, face: &mut CachedFace) -> bool {
        if let Some(build) = self.pending.as_mut()
            && face.layout == build.layout
        {
            build.stale = true;
        }
        let committed = self.committed_address(face);
        face.slot = None;
        face.blended_index = None;
        face.retained = None;
        let Some(FaceSlot { slot, .. }) = committed else { return false };
        let start = slot as usize * INSTANCE_BYTES;
        if let Some(instance) = self.instance_bytes.get_mut(start..start + INSTANCE_BYTES) {
            instance.fill(0);
            self.hidden_slots.insert(slot);
            self.batches_dirty = true;
            self.committed_written(slot);
        }
        true
    }

    fn release_draw_group_refs(&mut self) {
        for group in self.draw_group_refs.drain(..) {
            self.groups.release(group);
        }
        self.draw_group_seen.clear();
    }

    fn retain_draw_group_refs(&mut self) {
        self.draw_group_seen.clear();
        for draw in &self.draws {
            if self.draw_group_seen.insert(draw.group) && self.groups.retain(draw.group) {
                self.draw_group_refs.push(draw.group);
            }
        }
    }

    pub(super) fn release_group(&mut self, group: u32) {
        self.groups.release(group);
    }

    pub(super) fn update_terrain(&mut self, region: Region, textures: &[Option<TextureHandle>; 4]) {
        self.groups.update_terrain(region, textures);
        for demand in self.active_terrain.iter_mut().filter(|demand| demand.region == region) {
            demand.textures.clone_from(textures);
        }
    }

    pub(super) fn invalidate_batches(&mut self) {
        self.batches_stale = true;
    }

    pub(super) fn flush_batch_invalidation(&mut self) -> bool {
        let batching = self.pending.as_ref().is_some_and(|build| build.phase.batching());
        if !self.batches_stale || batching {
            return false;
        }
        self.batches_stale = false;
        self.groups.invalidate_batches();
        self.draw_batches.clear();
        self.batches_dirty = true;
        true
    }

    pub(super) fn batches_dirty(&self) -> bool {
        self.batches_dirty
    }

    pub(super) fn texture_demands(&self) -> &[TextureDemand] {
        &self.active_textures
    }

    pub(super) fn terrain_demands(&self) -> &[TerrainDemand] {
        &self.active_terrain
    }

    pub(super) fn terrain_regions(&self) -> &HashSet<Region> {
        &self.terrain_seen
    }

    pub(super) fn draws(&self) -> &[DrawCommand] {
        &self.draws
    }

    pub(super) fn command_count(&self) -> usize {
        self.first_instances.len()
    }

    pub(super) fn hides_range(&self, first: u32, count: u32) -> bool {
        (first..first.saturating_add(count)).any(|slot| self.hidden_slots.contains(&slot))
    }

    #[cfg(test)]
    pub(super) fn instance_transform(&self, slot: u32) -> Option<&[u8]> {
        let start = usize::try_from(slot).ok()?.checked_mul(INSTANCE_BYTES)?;
        self.instance_bytes.get(start..start.checked_add(TRANSFORM_BYTES)?)
    }

    pub(super) fn draw_batches(&self) -> &[DrawBatch] {
        &self.draw_batches
    }

    pub(super) fn indirect_bytes(&self) -> &[u8] {
        &self.indirect_bytes
    }

    pub(super) fn first_instance(&self, offset: u64, command: u32) -> Option<u32> {
        let index = usize::try_from(offset).ok()?.checked_div(INDIRECT_DRAW_BYTES)?.checked_add(command as usize)?;
        self.first_instances.get(index).copied()
    }

    pub(super) fn visible_items(&self) -> usize {
        self.visible_items
    }

    pub(super) fn set_visible_items(&mut self, count: usize) {
        self.visible_items = count;
    }

    pub(super) fn has_dirty_slots(&self) -> bool {
        !self.dirty_slots.is_empty()
    }

    pub(super) fn valid(&self) -> bool {
        self.valid
    }

    pub(super) fn has_blended(&self) -> bool {
        !self.blended.is_empty()
    }

    pub(super) fn clear(&mut self) {
        self.cancel_layout();
        self.release_draw_group_refs();
        self.clear_group_counts();
        self.draws.clear();
        self.mesh_references.clear();
        self.instance_bytes.clear();
        self.hidden_slots.clear();
        self.active_textures.clear();
        self.active_terrain.clear();
        self.blended.clear();
        self.dirty_slots.clear();
        self.opaque_draws = 0;
        self.opaque_instances = 0;
        self.instance_count = 0;
        self.visible_items = 0;
        self.omitted = 0;
        self.valid = false;
        self.layout = self.next_layout();
        self.batch_order.clear();
        self.draw_batches.clear();
        self.indirect_bytes.clear();
        self.first_instances.clear();
        self.blended_batches = 0;
        self.blended_commands = 0;
        self.batches_dirty = false;
        for group in self.groups.batches.iter_mut().flatten() {
            group.commands.clear();
        }
    }

    pub(super) fn build_draw_batches(&mut self, meshes: &HashMap<MeshId, GpuMesh>, indirect_first_instance: bool) -> usize {
        for group in self.groups.batches.iter_mut().flatten() {
            group.commands.clear();
        }
        self.batch_order.clear();
        self.draw_batches.clear();
        self.indirect_bytes.clear();
        self.first_instances.clear();

        for draw in &self.draws[..self.opaque_draws] {
            if self.hides_range(draw.first, draw.count) {
                continue;
            }
            let (binding, args) = ensure_batch(&mut self.groups, draw, meshes);
            let group = self.groups.batch_group_mut(binding.id).expect("interned batch group");
            if group.commands.is_empty() {
                self.batch_order.push(binding.id);
            }
            group.commands.push(args);
        }
        for id in &self.batch_order {
            let Some(group) = self.groups.batch_group(*id) else { continue };
            let offset = self.indirect_bytes.len() as u64;
            for args in &group.commands {
                self.first_instances.push(args.first_instance);
                encode_draw(args, &mut self.indirect_bytes, indirect_first_instance);
            }
            self.draw_batches.push(DrawBatch {
                id: *id,
                key: group.key,
                buffers: Arc::clone(&group.buffers),
                offset,
                count: group.commands.len() as u32,
            });
        }

        self.blended_batches = self.draw_batches.len();
        self.blended_commands = self.first_instances.len();
        self.push_blended_batches(meshes, indirect_first_instance);
        self.batches_dirty = false;
        self.draws.len()
    }

    pub(super) fn rebuild_blended_batches(&mut self, meshes: &HashMap<MeshId, GpuMesh>, indirect_first_instance: bool) -> usize {
        self.draw_batches.truncate(self.blended_batches);
        self.first_instances.truncate(self.blended_commands);
        let start = self.blended_commands.saturating_mul(INDIRECT_DRAW_BYTES);
        self.indirect_bytes.truncate(start);
        self.push_blended_batches(meshes, indirect_first_instance);
        start
    }

    fn push_blended_batches(&mut self, meshes: &HashMap<MeshId, GpuMesh>, indirect_first_instance: bool) {
        let blended_start = self.blended_batches;
        for draw in &self.draws[self.opaque_draws..] {
            if self.hides_range(draw.first, draw.count) {
                continue;
            }
            let (binding, args) = ensure_batch(&mut self.groups, draw, meshes);
            let merge = self.draw_batches.len() > blended_start && self.draw_batches.last().is_some_and(|batch| batch.id == binding.id);
            if merge {
                self.first_instances.push(args.first_instance);
                encode_draw(&args, &mut self.indirect_bytes, indirect_first_instance);
                if let Some(batch) = self.draw_batches.last_mut() {
                    batch.count += 1;
                }
            } else {
                let offset = self.indirect_bytes.len() as u64;
                self.first_instances.push(args.first_instance);
                encode_draw(&args, &mut self.indirect_bytes, indirect_first_instance);
                let group = self.groups.batch_group(binding.id).expect("interned batch group");
                self.draw_batches.push(DrawBatch { id: binding.id, key: group.key, buffers: Arc::clone(&group.buffers), offset, count: 1 });
            }
        }
    }

    fn clear_group_counts(&mut self) {
        for group in self.active_groups.drain(..) {
            let index = group as usize;
            if let Some(count) = self.group_counts.get_mut(index) {
                *count = 0;
            }
        }
    }
}

fn assign_slot(face: &mut CachedFace, layout: LayoutId, committed: LayoutId, slot: Option<u32>, blended_index: Option<usize>) {
    if face.layout != layout {
        if face.layout == committed {
            face.retained = face.slot.map(|slot| FaceSlot { layout: committed, slot, blended_index: face.blended_index });
        }
        face.layout = layout;
    }
    face.slot = slot;
    face.blended_index = blended_index;
}

fn write_instance(bytes: &mut [u8], slot: u32, transform: &[u8; TRANSFORM_BYTES], parameters: &[u8; PARAMETER_BYTES]) -> bool {
    let start = slot as usize * INSTANCE_BYTES;
    let Some(destination) = bytes.get_mut(start..start + INSTANCE_BYTES) else { return false };
    let (transform_destination, parameter_destination) = destination.split_at_mut(TRANSFORM_BYTES);
    transform_destination.copy_from_slice(transform);
    parameter_destination.copy_from_slice(parameters);
    true
}

fn write_layout_instance(
    active: &[u8],
    staging: &mut [u8],
    dirty_slots: &mut Vec<u32>,
    slot: u32,
    transform: &[u8; TRANSFORM_BYTES],
    parameters: &[u8; PARAMETER_BYTES],
) -> bool {
    let start = slot as usize * INSTANCE_BYTES;
    let previous = active.get(start..start + INSTANCE_BYTES);
    if !write_instance(staging, slot, transform, parameters) {
        return false;
    }
    let current = staging.get(start..start + INSTANCE_BYTES);
    if previous.zip(current).is_none_or(|(previous, current)| previous != current) {
        dirty_slots.push(slot);
    }
    true
}

fn contains_item(visible: &[ItemIndex], items: &ItemSlab, index: ItemIndex) -> bool {
    let Some(id) = items.get(index).map(|item| item.id) else { return false };
    visible.binary_search_by(|visible| items.get(*visible).map_or(std::cmp::Ordering::Less, |item| item.id.cmp(&id))).is_ok()
}

fn blend_distance(position: DVec3, rotation: Quat, scale: Vec3, center: Vec3, camera: Camera) -> f64 {
    let relative = position - camera.position() + (rotation * (center * scale)).as_dvec3();
    relative.length_squared()
}

fn ensure_batch(groups: &mut DrawInterner, draw: &DrawCommand, meshes: &HashMap<MeshId, GpuMesh>) -> (BatchBinding, DrawIndexedArgs) {
    let binding = if let Some(binding) = groups.batch(draw.group) {
        binding
    } else {
        let mesh = meshes.get(&draw.mesh).expect("draw command mesh is resident");
        let allocation = &mesh.allocation;
        let buffers = allocation.buffers();
        let key =
            DrawBatchKey { pipeline: draw.pipeline, texture: draw.texture, terrain: draw.terrain, page: Arc::as_ptr(buffers) as usize };
        let binding = BatchBinding {
            id: groups.intern_batch(key, buffers),
            index_offset: allocation.indices().start,
            base_vertex: i32::try_from(allocation.vertices().start).expect("mesh page fits the GPU budget"),
        };
        groups.set_batch(draw.group, binding);
        binding
    };
    let args = DrawIndexedArgs {
        index_count: draw.indices.end - draw.indices.start,
        instance_count: draw.count,
        first_index: binding.index_offset.checked_add(draw.indices.start).expect("mesh page index range fits the GPU budget"),
        base_vertex: binding.base_vertex,
        first_instance: draw.first,
    };
    (binding, args)
}

fn encode_draw(args: &DrawIndexedArgs, bytes: &mut Vec<u8>, indirect_first_instance: bool) {
    let start = bytes.len();
    bytes.extend_from_slice(&args.index_count.to_le_bytes());
    bytes.extend_from_slice(&args.instance_count.to_le_bytes());
    bytes.extend_from_slice(&args.first_index.to_le_bytes());
    bytes.extend_from_slice(&args.base_vertex.to_le_bytes());
    bytes.extend_from_slice(&(if indirect_first_instance { args.first_instance } else { 0 }).to_le_bytes());
    debug_assert_eq!(bytes.len() - start, INDIRECT_DRAW_BYTES);
}

#[cfg(test)]
#[path = "draw_list_tests.rs"]
mod tests;
