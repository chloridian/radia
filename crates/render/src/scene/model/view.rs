use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque, btree_map::Entry},
    error::Error,
    fmt::{self, Display, Formatter},
    ops::Bound,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use assets::TextureHandle;
use math::motion::RegionBounds;
use utils::AssetId;
use uuid::Uuid;
use world::{Entity, Region, WorldRevision};

use super::{Batch, Change, MeshChange, Mirror, SceneLifetime, Status};
use crate::scene::{Object, Patch, RegionData, Surface, SurfaceKind, ViewerEnvironment, region_environment::RegionEnvironment, surface};

const CHANGE_HISTORY: usize = 64;
const CHANGE_HISTORY_BYTES: usize = 4 * 1024 * 1024;
const FILTERED_OBJECT_PAGE_LIMIT: usize = 128;

#[derive(Default)]
pub(crate) struct SceneChanges {
    pub(crate) objects: Vec<Entity>,
    pub(crate) surfaces: Vec<(Region, SurfaceKind)>,
    pub(crate) regions: Vec<Region>,
    pub(crate) retired: Vec<Region>,
    pub(crate) motion_changed: Vec<Entity>,
    pub(crate) full: bool,
    pub(crate) reconcile_all: bool,
}

impl SceneChanges {
    fn from_batch(batch: &Batch) -> Self {
        let mut changes = Self::default();
        if batch.replace {
            changes.full = true;
            return changes;
        }
        for change in &batch.changes {
            match change {
                Change::Object(object) => changes.objects.push(object.detail().summary.entity),
                Change::Remove(entity) => changes.objects.push(*entity),
                Change::InvalidateSubtree(entity, _) => {
                    changes.objects.push(*entity);
                    changes.reconcile_all = true;
                }
                Change::Mesh(MeshChange::Set(entity, ..) | MeshChange::Clear(entity, _)) => changes.objects.push(*entity),
                Change::Material(change) => changes.objects.push(change.source.detail().summary.entity),
                Change::Region(region) => changes.regions.push(region.summary().region),
                Change::Retire(region) => changes.retired.push(*region),
                Change::Surface(surface) => changes.surfaces.push((surface.region, surface.kind)),
                Change::RemoveSurface(region, kind) => changes.surfaces.push((*region, *kind)),
                Change::ViewerEnvironment(_)
                | Change::Environment(..)
                | Change::EnvironmentTexture(..)
                | Change::Water(..)
                | Change::Patch(..) => {}
            }
        }
        changes
    }

    fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            + self.objects.capacity() * size_of::<Entity>()
            + self.surfaces.capacity() * size_of::<(Region, SurfaceKind)>()
            + self.regions.capacity() * size_of::<Region>()
            + self.retired.capacity() * size_of::<Region>()
            + self.motion_changed.capacity() * size_of::<Entity>()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyError {
    StaleSession,
    OutOfOrder,
}

impl Display for ApplyError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::StaleSession => "the Scene session ended or the batch belongs to another session",
            Self::OutOfOrder => "the Scene publication sequence has a gap or duplicate",
        })
    }
}

impl Error for ApplyError {}

#[derive(Clone, Debug)]
pub struct ViewVersion {
    session: SceneLifetime,
    sequence: u64,
    status: Status,
}

impl PartialEq for ViewVersion {
    fn eq(&self, other: &Self) -> bool {
        self.session.same(&other.session) && self.sequence == other.sequence && self.status == other.status
    }
}

impl Eq for ViewVersion {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct VisibilityVersion {
    sequence: u64,
    active: bool,
}

pub(crate) struct ObjectPage<'a> {
    pub(crate) entries: Vec<(Uuid, Option<&'a Object>)>,
    pub(crate) cursor: Option<Uuid>,
    pub(crate) done: bool,
}

pub(crate) type ChangedObject = (u64, Uuid, Entity, bool);
pub(crate) type AddedObject = (u64, Uuid, Entity);
pub(crate) type SequenceCursor = (u64, Uuid);

#[derive(Clone, Copy)]
struct ObjectChangeSequence {
    latest: u64,
    added: u64,
}

pub struct SceneView {
    session: SceneLifetime,
    sequence: u64,
    geometry_sequence: u64,
    revision: Option<WorldRevision>,
    status: Status,
    mirror: Mirror,
    object_order: BTreeMap<Uuid, Entity>,
    changed_objects: BTreeMap<Uuid, ObjectChangeSequence>,
    changed_sequences: BTreeSet<SequenceCursor>,
    added_objects: BTreeSet<SequenceCursor>,
    visibility_sequence: u64,
    change_history: VecDeque<(u64, SceneChanges)>,
    change_history_bytes: usize,
    reconcile_marker: SceneChanges,
    replacement_marker: SceneChanges,
    replacement_sequence: Option<u64>,
    detail_batch_waits: Arc<AtomicU64>,
}

impl SceneView {
    pub fn detail_batch_waits(&self) -> u64 {
        self.detail_batch_waits.load(Ordering::Relaxed)
    }

    pub(in crate::scene) fn detail_batch_waits_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.detail_batch_waits)
    }

    pub fn viewer_environment(&self) -> Option<&ViewerEnvironment> {
        if self.session.is_active() { self.mirror.viewer_environment.as_ref() } else { None }
    }

    pub fn scene_mesh_capacity_omitted(&self) -> usize {
        if !self.session.is_active() {
            return 0;
        }
        self.mirror
            .objects
            .values()
            .filter(|object| object.mesh().is_none() && matches!(object.preparation, Some((_, super::PreparedMesh::Capacity))))
            .count()
    }

    pub(in crate::scene) fn new(session: SceneLifetime) -> Self {
        Self {
            session,
            sequence: 0,
            geometry_sequence: 0,
            revision: None,
            status: Status::Loading,
            mirror: Mirror::default(),
            object_order: BTreeMap::new(),
            changed_objects: BTreeMap::new(),
            changed_sequences: BTreeSet::new(),
            added_objects: BTreeSet::new(),
            visibility_sequence: 0,
            change_history: VecDeque::with_capacity(CHANGE_HISTORY),
            change_history_bytes: 0,
            reconcile_marker: SceneChanges { reconcile_all: true, ..SceneChanges::default() },
            replacement_marker: SceneChanges { full: true, ..SceneChanges::default() },
            replacement_sequence: None,
            detail_batch_waits: Arc::new(AtomicU64::new(0)),
        }
    }

    fn insert_object_order(&mut self, id: Uuid, entity: Entity, sequence: u64) {
        self.object_order.insert(id, entity);
        if let Entry::Vacant(entry) = self.changed_objects.entry(id) {
            entry.insert(ObjectChangeSequence { latest: sequence, added: sequence });
            self.changed_sequences.insert((sequence, id));
            self.added_objects.insert((sequence, id));
        }
    }

    fn remove_object_order(&mut self, id: Uuid, entity: Entity) {
        if self.object_order.get(&id) == Some(&entity) {
            self.object_order.remove(&id);
            if let Some(change) = self.changed_objects.remove(&id) {
                self.changed_sequences.remove(&(change.latest, id));
                self.added_objects.remove(&(change.added, id));
            }
        }
    }

    fn note_object_change(&mut self, id: Uuid, sequence: u64) {
        let added = self.changed_objects.get(&id).map_or(sequence, |change| change.added);
        if let Some(previous) = self.changed_objects.insert(id, ObjectChangeSequence { latest: sequence, added }) {
            self.changed_sequences.remove(&(previous.latest, id));
        }
        self.changed_sequences.insert((sequence, id));
    }

    fn note_entity_change(&mut self, entity: Entity, sequence: u64) {
        let Some(id) = self.mirror.objects.get(&entity).map(|object| object.detail().summary.id) else { return };
        if self.object_order.get(&id) == Some(&entity) {
            self.note_object_change(id, sequence);
        }
    }

    pub fn apply(&mut self, batch: Batch) -> Result<(), ApplyError> {
        if !self.session.is_active() || !self.session.same(&batch.session) {
            return Err(ApplyError::StaleSession);
        }
        if self.sequence.checked_add(1) != Some(batch.sequence) {
            return Err(ApplyError::OutOfOrder);
        }
        let geometry_changed = batch.replace || batch.changes.iter().any(change_affects_geometry);
        let mut changes = SceneChanges::from_batch(&batch);
        if !batch.replace {
            for change in &batch.changes {
                if let Change::Object(object) = change {
                    let entity = object.detail().summary.entity;
                    if self.mirror.objects.get(&entity).is_some_and(|previous| object.motion_changed_from(previous))
                        && !changes.motion_changed.contains(&entity)
                    {
                        changes.motion_changed.push(entity);
                    }
                }
            }
        }
        if changes.retained_bytes() > CHANGE_HISTORY_BYTES {
            changes = SceneChanges { reconcile_all: true, ..SceneChanges::default() };
        }
        if batch.replace {
            self.mirror = Mirror::default();
            self.object_order.clear();
            self.changed_objects.clear();
            self.changed_sequences.clear();
            self.added_objects.clear();
        }
        let now = Instant::now();
        let mut visibility_changed = batch.replace;
        for change in batch.changes {
            match change {
                Change::Object(object) => {
                    let entity = object.detail().summary.entity;
                    let incoming_id = object.detail().summary.id;
                    let incoming_region = object.detail().summary.address.region();
                    let reused_entity = (!self.mirror.is_retired(incoming_region))
                        .then(|| self.object_order.get(&incoming_id).copied())
                        .flatten()
                        .filter(|previous| *previous != entity);
                    if let Some(previous) = reused_entity {
                        self.mirror.objects.remove(&previous);
                        self.mirror.invalidations.remove(&previous);
                        self.remove_object_order(incoming_id, previous);
                        changes.reconcile_all = true;
                        visibility_changed = true;
                    }
                    let previous = self
                        .mirror
                        .objects
                        .get(&entity)
                        .map(|object| (object.detail().summary.id, object.detail().summary.address.region()));
                    let was_visible = self.object(entity).is_some();
                    self.mirror.apply_at(Change::Object(object), now);
                    let Some(current) = self.mirror.objects.get(&entity) else { continue };
                    let current_id = current.detail().summary.id;
                    let current_region = current.detail().summary.address.region();
                    if previous.map(|(id, _)| id) != Some(current_id) {
                        if let Some((previous_id, _)) = previous {
                            self.remove_object_order(previous_id, entity);
                        }
                        self.insert_object_order(current_id, entity, batch.sequence);
                    }
                    self.note_object_change(current_id, batch.sequence);
                    let is_visible = self.object(entity).is_some();
                    visibility_changed |=
                        previous.is_some_and(|(id, region)| id != current_id || region != current_region) || (was_visible && !is_visible);
                }
                Change::Remove(entity) => {
                    if let Some(object) = self.mirror.objects.get(&entity) {
                        let id = object.detail().summary.id;
                        self.remove_object_order(id, entity);
                        visibility_changed = true;
                    }
                    self.mirror.apply_at(Change::Remove(entity), now);
                }
                change @ Change::InvalidateSubtree(..) | change @ Change::Retire(_) => {
                    visibility_changed = true;
                    self.mirror.apply_at(change, now);
                }
                Change::Mesh(mesh) => {
                    let (MeshChange::Set(entity, ..) | MeshChange::Clear(entity, _)) = mesh;
                    self.mirror.apply_at(Change::Mesh(mesh), now);
                    self.note_entity_change(entity, batch.sequence);
                }
                Change::Material(material) => {
                    let entity = material.source.detail().summary.entity;
                    self.mirror.apply_at(Change::Material(material), now);
                    self.note_entity_change(entity, batch.sequence);
                }
                change => self.mirror.apply_at(change, now),
            }
        }
        self.sequence = batch.sequence;
        if visibility_changed {
            self.visibility_sequence = self.visibility_sequence.checked_add(1).expect("scene visibility version space exhausted");
        }
        if geometry_changed {
            self.geometry_sequence = self.geometry_sequence.checked_add(1).expect("scene geometry version space exhausted");
        }
        if batch.replace {
            self.replacement_sequence = Some(self.sequence);
        }
        self.revision = batch.revision;
        self.status = batch.status;
        self.change_history_bytes += changes.retained_bytes();
        self.change_history.push_back((self.sequence, changes));
        while self.change_history.len() > CHANGE_HISTORY || self.change_history_bytes > CHANGE_HISTORY_BYTES {
            if let Some((_, removed)) = self.change_history.pop_front() {
                self.change_history_bytes -= removed.retained_bytes();
            }
        }
        batch.delivery.acknowledge();
        Ok(())
    }

    pub fn status(&self) -> Status {
        if self.session.is_active() { self.status } else { Status::Closed }
    }
    pub fn version(&self) -> ViewVersion {
        ViewVersion { session: self.session.clone(), sequence: self.sequence, status: self.status() }
    }
    pub(crate) fn geometry_version(&self) -> ViewVersion {
        ViewVersion { session: self.session.clone(), sequence: self.geometry_sequence, status: self.status() }
    }
    pub fn revision(&self) -> Option<WorldRevision> {
        self.revision
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn object_count(&self) -> usize {
        if !self.session.is_active() {
            return 0;
        }
        if self.mirror.invalidations.is_empty() {
            if self.mirror.retired_regions.is_empty() {
                return self.object_order.len();
            }
            return self.object_order.values().filter(|entity| self.mirror.object_visible(**entity)).count();
        }
        let mut visibility = HashMap::new();
        self.object_order.values().filter(|entity| self.mirror.visibility(**entity, &mut visibility).visible).count()
    }
    pub fn tracked_object_count(&self) -> usize {
        if self.session.is_active() { self.mirror.objects.len() } else { 0 }
    }
    pub fn region_count(&self) -> usize {
        if self.session.is_active() { self.mirror.regions.len() } else { 0 }
    }
    pub fn patch_count(&self) -> usize {
        if self.session.is_active() { self.mirror.patches.len() } else { 0 }
    }
    pub fn start_motion_phase_out(&mut self, region: Region, at: Option<Instant>) {
        for object in self.mirror.objects.values_mut().filter(|object| object.detail().summary.address.region() == region) {
            object.start_motion_phase_out(at);
        }
    }

    pub fn resume_motion(&mut self, region: Region, now: Instant, regions: &[RegionBounds]) {
        for object in self.mirror.objects.values_mut().filter(|object| object.detail().summary.address.region() == region) {
            object.resume_motion_at(now, regions);
        }
    }
    pub fn objects(&self) -> impl Iterator<Item = &Object> {
        let active = self.session.is_active();
        let has_invalidations = !self.mirror.invalidations.is_empty();
        let has_retired = !self.mirror.retired_regions.is_empty();
        let mirror = &self.mirror;
        let object_order = &self.object_order;
        let mut visibility = HashMap::new();
        self.mirror.objects.values().filter(move |object| {
            let summary = &object.detail().summary;
            active
                && object_order.get(&summary.id) == Some(&summary.entity)
                && if has_invalidations {
                    mirror.visibility(summary.entity, &mut visibility).visible
                } else {
                    !has_retired || !mirror.is_retired(summary.address.region())
                }
        })
    }

    pub(crate) fn changed_objects_page(
        &self,
        since: u64,
        after: Option<SequenceCursor>,
        limit: usize,
    ) -> (Vec<ChangedObject>, Option<SequenceCursor>, bool) {
        let (page, cursor, done) = self.sequence_page(&self.changed_sequences, since, after, limit);
        let objects = page
            .into_iter()
            .filter_map(|(latest, id)| {
                let entity = self.object_order.get(&id).copied()?;
                let change = self.changed_objects.get(&id)?;
                Some((latest, id, entity, change.added > since))
            })
            .collect();
        (objects, cursor, done)
    }

    pub(crate) fn added_objects_page(
        &self,
        since: u64,
        after: Option<SequenceCursor>,
        limit: usize,
    ) -> (Vec<AddedObject>, Option<SequenceCursor>, bool) {
        let (page, cursor, done) = self.sequence_page(&self.added_objects, since, after, limit);
        let objects = page
            .into_iter()
            .filter_map(|(sequence, id)| {
                let entity = self.object_order.get(&id).copied()?;
                Some((sequence, id, entity))
            })
            .collect();
        (objects, cursor, done)
    }

    fn sequence_page(
        &self,
        index: &BTreeSet<SequenceCursor>,
        since: u64,
        after: Option<SequenceCursor>,
        limit: usize,
    ) -> (Vec<SequenceCursor>, Option<SequenceCursor>, bool) {
        if !self.session.is_active() || limit == 0 {
            return (Vec::new(), after, true);
        }
        let lower = after.unwrap_or((since, Uuid::from_u128(u128::MAX)));
        let page = index.range((Bound::Excluded(lower), Bound::Unbounded)).take(limit).copied().collect::<Vec<_>>();
        let cursor = page.last().copied().or(after);
        let done = index.range((Bound::Excluded(cursor.unwrap_or(lower)), Bound::Unbounded)).next().is_none();
        (page, cursor, done)
    }

    pub(crate) fn object_page(&self, after: Option<Uuid>, limit: usize) -> ObjectPage<'_> {
        if !self.session.is_active() || limit == 0 {
            return ObjectPage { entries: Vec::new(), cursor: after, done: true };
        }
        let needs_filter = !self.mirror.invalidations.is_empty() || !self.mirror.retired_regions.is_empty();
        let limit = if needs_filter { limit.min(FILTERED_OBJECT_PAGE_LIMIT) } else { limit };
        let entries = match after {
            Some(after) => self.object_order.range((Bound::Excluded(after), Bound::Unbounded)),
            None => self.object_order.range(..),
        }
        .take(limit.saturating_add(1))
        .map(|(id, entity)| (*id, *entity))
        .collect::<Vec<_>>();
        let done = entries.len() <= limit;
        let next = entries.get(limit.saturating_sub(1)).map(|(id, _)| *id);
        let mut visibility = HashMap::new();
        let objects = entries
            .into_iter()
            .take(limit)
            .map(|(id, entity)| {
                let object = self.mirror.objects.get(&entity);
                let visible = object.is_some_and(|object| {
                    !self.mirror.is_retired(object.detail().summary.address.region())
                        && (!needs_filter || self.mirror.visibility(entity, &mut visibility).visible)
                });
                (id, object.filter(|_| visible))
            })
            .collect();
        ObjectPage { entries: objects, cursor: next, done }
    }

    pub fn object(&self, entity: Entity) -> Option<&Object> {
        if !self.session.is_active() {
            return None;
        }
        let object = self.mirror.objects.get(&entity)?;
        (self.object_order.get(&object.detail().summary.id) == Some(&entity) && self.mirror.object_visible(entity)).then_some(object)
    }
    pub(crate) fn object_is_live(&self, entity: Entity) -> bool {
        self.object(entity).is_some()
    }
    pub(crate) fn visibility_version(&self) -> VisibilityVersion {
        VisibilityVersion { sequence: self.visibility_sequence, active: self.session.is_active() }
    }
    pub fn regions(&self) -> impl Iterator<Item = &RegionData> {
        self.mirror.regions.values().filter(|_| self.session.is_active())
    }
    pub fn region(&self, region: Region) -> Option<&RegionData> {
        if self.session.is_active() { self.mirror.regions.get(&region) } else { None }
    }

    pub fn environment(&self, region: Region) -> Option<&RegionEnvironment> {
        if self.session.is_active() { self.mirror.environments.get(&region) } else { None }
    }
    pub fn environment_texture(&self, id: AssetId) -> Option<&TextureHandle> {
        if self.session.is_active() { self.mirror.environment_textures.get(&id) } else { None }
    }
    pub fn patches(&self) -> impl Iterator<Item = (Region, &Patch)> {
        self.mirror.patches.iter().filter(|_| self.session.is_active()).map(|((region, _), patch)| (*region, patch))
    }

    pub fn terrain_height_at(&self, position: math::DVec3) -> Option<f64> {
        if !self.session.is_active() || !position.is_finite() {
            return None;
        }
        surface::height_at(&self.mirror, position)
    }
    pub fn water_height(&self, region: Region) -> Option<f32> {
        if self.session.is_active() { self.mirror.water.get(&region).copied().flatten() } else { None }
    }

    pub fn surfaces(&self) -> impl Iterator<Item = &Surface> {
        self.mirror.surfaces.values().filter(|_| self.session.is_active())
    }

    pub(crate) fn surface_page(
        &self,
        after: Option<(Region, SurfaceKind)>,
        limit: usize,
    ) -> (Vec<&Surface>, Option<(Region, SurfaceKind)>, bool) {
        if !self.session.is_active() || limit == 0 {
            return (Vec::new(), after, true);
        }
        let entries = match after {
            Some(after) => self.mirror.surfaces.range((Bound::Excluded(after), Bound::Unbounded)),
            None => self.mirror.surfaces.range(..),
        }
        .take(limit.saturating_add(1))
        .map(|(key, surface)| (*key, surface))
        .collect::<Vec<_>>();
        let done = entries.len() <= limit;
        let next = entries.get(limit.saturating_sub(1)).map(|(key, _)| *key);
        let surfaces = entries.into_iter().take(limit).map(|(_, surface)| surface).collect();
        (surfaces, next, done)
    }

    pub(crate) fn surface(&self, region: Region, kind: SurfaceKind) -> Option<&Surface> {
        if self.session.is_active() { self.mirror.surfaces.get(&(region, kind)) } else { None }
    }

    pub(crate) fn changes_since<'a>(&'a self, version: &ViewVersion) -> Option<impl Iterator<Item = &'a SceneChanges>> {
        if !self.session.same(&version.session) || version.sequence > self.sequence {
            return None;
        }
        let next = version.sequence.checked_add(1)?;
        let replaced = self.replacement_sequence.is_some_and(|sequence| version.sequence < sequence);
        let missed = !replaced && next <= self.sequence && self.change_history.front().is_none_or(|(sequence, _)| *sequence > next);
        let status_changed = version.status != self.status();
        let sequence = version.sequence;
        let marker = if replaced { Some(&self.replacement_marker) } else { (missed || status_changed).then_some(&self.reconcile_marker) };
        let history = self.change_history.iter().filter(move |(current, _)| *current > sequence).map(|(_, changes)| changes);
        Some(marker.into_iter().chain(history))
    }
}

fn change_affects_geometry(change: &Change) -> bool {
    matches!(
        change,
        Change::Object(_)
            | Change::Mesh(_)
            | Change::Material(_)
            | Change::Remove(_)
            | Change::InvalidateSubtree(..)
            | Change::Region(_)
            | Change::Retire(_)
            | Change::Patch(..)
            | Change::Surface(_)
            | Change::RemoveSurface(..)
    )
}

#[cfg(test)]
mod tests {
    use std::{assert_matches, num::NonZeroU32};

    use math::DVec3;
    use world::{RegionInfo, RegionSummary, TERRAIN_PATCH_SAMPLES, TerrainPatch};

    use super::*;
    use crate::{
        scene::{
            Budget, Camera, ReadFailure, ReadQuery, ReadReply, ReconcileStep, Reconciler,
            model::{Receipt, SceneLifetime},
        },
        test_support,
    };

    struct PageState {
        entries: Vec<(Uuid, Option<Entity>)>,
        cursor: Option<Uuid>,
        done: bool,
    }

    fn terrain_view(regions: &[(Region, DVec3)], patches: &[(Region, u8, u8, f32)]) -> SceneView {
        let budget = Budget::default();
        let mut view = SceneView::new(SceneLifetime::new());
        for &(region, origin) in regions {
            view.mirror.regions.insert(
                region,
                RegionData::new(
                    RegionSummary {
                        region,
                        info: RegionInfo { origin: Some(origin), ..RegionInfo::default() },
                        terrain_settings: None,
                        water_height: None,
                        object_count: 0,
                    },
                    budget.reserve(size_of::<RegionSummary>()).expect("region lease"),
                ),
            );
        }
        for &(region, x, y, height) in patches {
            let patch = TerrainPatch::new(x, y, Box::new([height; TERRAIN_PATCH_SAMPLES])).expect("terrain patch");
            view.mirror
                .patches
                .insert((region, patch.id()), Patch::new(patch, budget.reserve(size_of::<TerrainPatch>()).expect("patch lease")));
        }
        view
    }

    fn membership_entity(
        world: &mut world::World,
        region: Region,
        id: Uuid,
        local_id: u32,
        order: u64,
        parent: world::ParentUpdate,
    ) -> Entity {
        let local_id = NonZeroU32::new(local_id).expect("fixture local id");
        let mut observation = world::ObjectObservation::new(
            world::UpdateSource::new(region, u32::try_from(order).expect("fixture order"), world::OrderEvidence::Ordered(order), 0),
            id,
            local_id,
            world::Kind::Object,
            parent,
        );
        observation.pose = Some(world::Pose { position: Some(math::Vec3::new(10.0, 0.0, 0.0)), rotation: Some(math::Quat::IDENTITY) });
        observation.scale = Some(math::Vec3::ONE);
        let world::ApplyOutcome::Introduced(entity) = world.observe_object(observation).expect("fixture object applies").outcome else {
            panic!("fixture object must be new");
        };
        entity
    }

    fn scene_object(world: &world::World, entity: Entity, budget: &Budget) -> Object {
        let detail = world.object_detail(entity).expect("fixture object detail");
        let lease = budget.reserve(detail.owned_bytes() + 32).expect("object lease");
        Object::new(detail, lease)
    }

    fn apply_changes(view: &mut SceneView, replace: bool, changes: Vec<Change>) {
        let (_, delivery) = Receipt::new();
        let sequence = view.sequence.checked_add(1).expect("fixture sequence");
        view.apply(Batch { session: view.session.clone(), sequence, revision: None, status: Status::Ready, replace, changes, delivery })
            .expect("fixture publication applies");
    }

    fn object_page_state(view: &SceneView, after: Option<Uuid>, limit: usize) -> PageState {
        let page = view.object_page(after, limit);
        PageState {
            entries: page.entries.into_iter().map(|(id, object)| (id, object.map(|object| object.detail().summary.entity))).collect(),
            cursor: page.cursor,
            done: page.done,
        }
    }

    #[test]
    fn status_change_skips_full_rebuild() {
        let (_scene, mut view) = Reconciler::new(Budget::default());
        let version = view.version();
        view.status = Status::Ready;

        let changes = view.changes_since(&version).unwrap().collect::<Vec<_>>();

        assert_eq!(changes.len(), 1);
        assert!(changes[0].reconcile_all);
        assert!(!changes[0].full);
    }

    #[test]
    fn object_pages_follow_visibility() {
        let mut world = world::World::new(world::WorldLimits {
            max_regions: 1,
            max_entities: 4,
            max_entities_per_region: 4,
            max_tombstones_per_region: 4,
            max_tombstones: 4,
            max_tombstone_bytes_per_region: world::WorldLimits::tombstone_bytes_for_count(4),
            max_tombstone_bytes: world::WorldLimits::tombstone_bytes_for_count(4),
            max_journal_entries: 16,
            max_journal_changes: 64,
            max_region_name_bytes: 64,
            max_recovery_targets: 4,
        });
        let region = world.register_region().expect("fixture region registers");
        let _origin =
            world.patch_region(region, RegionInfo { origin: Some(DVec3::ZERO), ..RegionInfo::default() }).expect("fixture origin applies");
        let first_id = Uuid::from_u128(1);
        let second_id = Uuid::from_u128(2);
        let third_id = Uuid::from_u128(3);
        let first = membership_entity(&mut world, region, first_id, 1, 1, world::ParentUpdate::Root);
        let second = membership_entity(
            &mut world,
            region,
            second_id,
            2,
            2,
            world::ParentUpdate::Local(NonZeroU32::new(1).expect("parent local id")),
        );
        let third = membership_entity(&mut world, region, third_id, 3, 3, world::ParentUpdate::Root);
        let budget = Budget::default();
        let mut view = SceneView::new(SceneLifetime::new());
        apply_changes(
            &mut view,
            true,
            vec![
                Change::Object(scene_object(&world, first, &budget)),
                Change::Object(scene_object(&world, second, &budget)),
                Change::Object(scene_object(&world, third, &budget)),
            ],
        );

        assert_eq!(view.object_count(), 3);
        assert_eq!(view.tracked_object_count(), 3);
        let page = object_page_state(&view, None, 1);
        assert_eq!(page.entries, [(first_id, Some(first))]);
        assert_eq!(page.cursor, Some(first_id));
        assert!(!page.done);

        let visibility = view.visibility_version();
        apply_changes(&mut view, false, vec![Change::InvalidateSubtree(first, world::RemovalFence::new(region, 4, 0))]);
        assert_ne!(view.visibility_version(), visibility, "subtree invalidation changes drawable visibility");
        assert_eq!(view.object_count(), 1);
        assert_eq!(view.tracked_object_count(), 3);
        assert!(view.object(first).is_none());
        assert!(view.object(second).is_none());
        assert_eq!(view.objects().map(|object| object.detail().summary.entity).collect::<Vec<_>>(), [third]);
        let page = object_page_state(&view, None, 1);
        assert_eq!(page.entries, [(first_id, None)]);
        assert_eq!(page.cursor, Some(first_id));
        assert!(!page.done);
        let page = object_page_state(&view, page.cursor, 1);
        assert_eq!(page.entries, [(second_id, None)]);
        assert_eq!(page.cursor, Some(second_id));
        assert!(!page.done);
        let page = object_page_state(&view, page.cursor, 1);
        assert_eq!(page.entries, [(third_id, Some(third))]);
        assert_eq!(page.cursor, Some(third_id));
        assert!(page.done);

        let visibility = view.visibility_version();
        apply_changes(&mut view, false, vec![Change::Remove(third)]);
        assert_ne!(view.visibility_version(), visibility);
        assert_eq!(view.object_count(), 0);
        assert_eq!(view.tracked_object_count(), 2);
        let page = object_page_state(&view, None, 1);
        assert_eq!(page.entries, [(first_id, None)]);
        assert_eq!(page.cursor, Some(first_id));
        assert!(!page.done);

        let _removal = world
            .remove_object(
                world::UpdateSource::new(region, 4, world::OrderEvidence::Ordered(4), 0),
                NonZeroU32::new(3).expect("removed local id"),
            )
            .expect("fixture removal applies");
        let reused = membership_entity(&mut world, region, third_id, 4, 5, world::ParentUpdate::Root);
        assert_ne!(reused, third);
        let visibility = view.visibility_version();
        apply_changes(&mut view, false, vec![Change::Object(scene_object(&world, reused, &budget))]);
        assert_eq!(view.visibility_version(), visibility, "new membership does not invalidate visible drawables");
        assert_eq!(view.object_count(), 1);
        assert_eq!(view.tracked_object_count(), 3);
        assert_eq!(view.object(reused).map(|object| object.detail().summary.id), Some(third_id));
        let page = object_page_state(&view, Some(second_id), 1);
        assert_eq!(page.entries, [(third_id, Some(reused))]);
        assert_eq!(page.cursor, Some(third_id));
        assert!(page.done);

        let visibility = view.visibility_version();
        apply_changes(&mut view, false, vec![Change::Retire(region)]);
        assert_ne!(view.visibility_version(), visibility);
        assert_eq!(view.object_count(), 0);
        assert_eq!(view.tracked_object_count(), 3);
        assert!(view.objects().next().is_none());
        let page = object_page_state(&view, None, 1);
        assert_eq!(page.entries, [(first_id, None)]);
        assert_eq!(page.cursor, Some(first_id));
        assert!(!page.done);
        let page = object_page_state(&view, page.cursor, 1);
        assert_eq!(page.entries, [(second_id, None)]);
        assert_eq!(page.cursor, Some(second_id));
        assert!(!page.done);
        let page = object_page_state(&view, page.cursor, 1);
        assert_eq!(page.entries, [(third_id, None)]);
        assert_eq!(page.cursor, Some(third_id));
        assert!(page.done);
    }

    #[test]
    fn pages_coalesce_past_overflow() {
        let mut world = world::World::new(world::WorldLimits {
            max_regions: 1,
            max_entities: 4,
            max_entities_per_region: 4,
            max_tombstones_per_region: 4,
            max_tombstones: 4,
            max_tombstone_bytes_per_region: world::WorldLimits::tombstone_bytes_for_count(4),
            max_tombstone_bytes: world::WorldLimits::tombstone_bytes_for_count(4),
            max_journal_entries: 16,
            max_journal_changes: 64,
            max_region_name_bytes: 64,
            max_recovery_targets: 4,
        });
        let region = world.register_region().expect("fixture region registers");
        let _origin =
            world.patch_region(region, RegionInfo { origin: Some(DVec3::ZERO), ..RegionInfo::default() }).expect("fixture origin applies");
        let first_id = Uuid::from_u128(11);
        let second_id = Uuid::from_u128(12);
        let first = membership_entity(&mut world, region, first_id, 1, 1, world::ParentUpdate::Root);
        let second = membership_entity(&mut world, region, second_id, 2, 2, world::ParentUpdate::Root);
        let budget = Budget::default();
        let mut view = SceneView::new(SceneLifetime::new());
        apply_changes(
            &mut view,
            true,
            vec![Change::Object(scene_object(&world, first, &budget)), Change::Object(scene_object(&world, second, &budget))],
        );
        let since = view.sequence();
        let checkpoint = view.version();
        let third_id = Uuid::from_u128(13);
        let third = membership_entity(&mut world, region, third_id, 3, 3, world::ParentUpdate::Root);
        apply_changes(&mut view, false, vec![Change::Object(scene_object(&world, third, &budget))]);
        let third_added_sequence = view.sequence();
        apply_changes(&mut view, false, vec![Change::Object(scene_object(&world, first, &budget))]);
        apply_changes(&mut view, false, vec![Change::Object(scene_object(&world, first, &budget))]);
        for _ in 0..=CHANGE_HISTORY {
            apply_changes(&mut view, false, vec![Change::Object(scene_object(&world, second, &budget))]);
        }

        assert!(
            view.changes_since(&checkpoint).is_some_and(|mut changes| changes.any(|change| change.reconcile_all)),
            "the regular change history reports its overflow"
        );
        let (added_page, added_cursor, added_done) = view.added_objects_page(since, None, 1);
        assert_eq!(added_page, vec![(third_added_sequence, third_id, third)]);
        assert_eq!(added_cursor, Some((third_added_sequence, third_id)));
        assert!(added_done);
        let second_latest = view.sequence();
        let (first_page, first_cursor, first_done) = view.changed_objects_page(since, None, 1);
        assert_eq!(first_page, vec![(third_added_sequence, third_id, third, true)]);
        assert_eq!(first_cursor, Some((third_added_sequence, third_id)));
        assert!(!first_done);
        apply_changes(&mut view, false, vec![Change::Object(scene_object(&world, first, &budget))]);
        let first_latest = view.sequence();
        let (second_page, second_cursor, second_done) = view.changed_objects_page(since, first_cursor, 1);
        assert_eq!(second_page, vec![(second_latest, second_id, second, false)]);
        assert_eq!(second_cursor, Some((second_latest, second_id)));
        assert!(!second_done, "a re-changed object is reported once at its newest sequence");
        let (third_page, _, third_done) = view.changed_objects_page(since, second_cursor, 1);
        assert_eq!(third_page, vec![(first_latest, first_id, first, false)]);
        assert!(third_done);
    }

    #[test]
    fn changed_pages_skip_unchanged_objects() {
        const OBJECTS: u32 = 200;
        let mut world = world::World::new(world::WorldLimits {
            max_regions: 1,
            max_entities: 256,
            max_entities_per_region: 256,
            max_tombstones_per_region: 4,
            max_tombstones: 4,
            max_tombstone_bytes_per_region: world::WorldLimits::tombstone_bytes_for_count(4),
            max_tombstone_bytes: world::WorldLimits::tombstone_bytes_for_count(4),
            max_journal_entries: 512,
            max_journal_changes: 1024,
            max_region_name_bytes: 64,
            max_recovery_targets: 4,
        });
        let region = world.register_region().expect("fixture region registers");
        let _origin =
            world.patch_region(region, RegionInfo { origin: Some(DVec3::ZERO), ..RegionInfo::default() }).expect("fixture origin applies");
        let budget = Budget::default();
        let entities = (1..=OBJECTS)
            .map(|index| {
                let id = Uuid::from_u128(u128::from(index));
                (id, membership_entity(&mut world, region, id, index, u64::from(index), world::ParentUpdate::Root))
            })
            .collect::<Vec<_>>();
        let mut view = SceneView::new(SceneLifetime::new());
        apply_changes(&mut view, true, entities.iter().map(|(_, entity)| Change::Object(scene_object(&world, *entity, &budget))).collect());
        let since = view.sequence();
        let (changed_id, changed_entity) = entities[usize::try_from(OBJECTS / 2).expect("fixture index")];
        apply_changes(&mut view, false, vec![Change::Object(scene_object(&world, changed_entity, &budget))]);

        let (page, cursor, done) = view.changed_objects_page(since, None, 8);
        assert_eq!(page, vec![(view.sequence(), changed_id, changed_entity, false)]);
        assert_eq!(cursor, Some((view.sequence(), changed_id)));
        assert!(done, "one small page covers the whole change set regardless of scene size");
    }

    #[test]
    fn late_meshes_reach_changed_pages() {
        let mut world = world::World::new(world::WorldLimits {
            max_regions: 1,
            max_entities: 4,
            max_entities_per_region: 4,
            max_tombstones_per_region: 4,
            max_tombstones: 4,
            max_tombstone_bytes_per_region: world::WorldLimits::tombstone_bytes_for_count(4),
            max_tombstone_bytes: world::WorldLimits::tombstone_bytes_for_count(4),
            max_journal_entries: 16,
            max_journal_changes: 64,
            max_region_name_bytes: 64,
            max_recovery_targets: 4,
        });
        let region = world.register_region().expect("fixture region registers");
        let _origin =
            world.patch_region(region, RegionInfo { origin: Some(DVec3::ZERO), ..RegionInfo::default() }).expect("fixture origin applies");
        let id = Uuid::from_u128(21);
        let mut observation = world::ObjectObservation::new(
            world::UpdateSource::new(region, 1, world::OrderEvidence::Ordered(1), 0),
            id,
            NonZeroU32::new(1).expect("fixture local id"),
            world::Kind::Object,
            world::ParentUpdate::Root,
        );
        observation.pose = Some(world::Pose { position: Some(math::Vec3::new(10.0, 0.0, 0.0)), rotation: Some(math::Quat::IDENTITY) });
        observation.scale = Some(math::Vec3::ONE);
        observation.shape = world::FieldUpdate::Set(world::Shape::Prim(world::PrimShape::default()));
        observation.appearance = world::FieldUpdate::Set(world::Appearance::Surface);
        observation.surface_appearance =
            Some(world::SurfaceAppearance::new(world::FaceAppearance::default(), Vec::new()).expect("fixture appearance"));
        assert_matches!(world.observe_object(observation).expect("fixture object applies").outcome, world::ApplyOutcome::Introduced(_));
        let (mut scene, mut view) = Reconciler::new(Budget::default());
        scene.set_camera(Camera::new(DVec3::ZERO, math::Vec3::X, 1.0, [128, 128]).expect("fixture camera"));

        let mut late = 0;
        let mut now = Instant::now();
        for _ in 0..10_000 {
            match scene.poll(now) {
                ReconcileStep::Read(job) => {
                    let reply = match job.query() {
                        ReadQuery::Barrier => Ok(ReadReply::Barrier(world.read_barrier())),
                        ReadQuery::Objects(cursor) => world.object_list_page(*cursor, 256).map(ReadReply::Objects),
                        ReadQuery::Regions(cursor) => world.region_page(*cursor, 64).map(ReadReply::Regions),
                        ReadQuery::Details(entities) => world.object_detail_batch(entities).map(ReadReply::Details),
                        ReadQuery::Changes(cursor) => world.change_page(*cursor, 256).map(ReadReply::Changes),
                        ReadQuery::Terrain(region, cursor) => world.terrain_page(*region, *cursor, 8).map(ReadReply::Terrain),
                        ReadQuery::Environment(region) => Ok(ReadReply::Environment(world.environment(*region))),
                    }
                    .map_err(ReadFailure::World);
                    scene.complete_read(job, reply, now);
                }
                ReconcileStep::Build(job) => scene.complete_mesh(job.run()),
                ReconcileStep::Publish(batch) => {
                    let since = view.sequence();
                    let objects = batch
                        .changes
                        .iter()
                        .filter_map(|change| match change {
                            Change::Object(object) => Some(object.detail().summary.entity),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    let arrivals = batch
                        .changes
                        .iter()
                        .filter_map(|change| match change {
                            Change::Mesh(MeshChange::Set(entity, ..) | MeshChange::Clear(entity, _)) => Some(*entity),
                            Change::Material(change) => Some(change.source.detail().summary.entity),
                            _ => None,
                        })
                        .filter(|entity| !objects.contains(entity))
                        .collect::<Vec<_>>();
                    view.apply(batch).expect("scene publication");
                    let (page, _, _) = view.changed_objects_page(since, None, 64);
                    for entity in arrivals {
                        late += 1;
                        assert!(
                            page.iter().any(|(_, _, changed, _)| *changed == entity),
                            "a mesh or material published after its object reaches the changed-object pages"
                        );
                    }
                }
                ReconcileStep::Continue => {}
                ReconcileStep::Wait(until) => now = until,
                ReconcileStep::Idle => break,
                ReconcileStep::Asset(_) | ReconcileStep::Closed => panic!("fixture scene stopped unexpectedly"),
            }
        }

        assert!(late > 0, "the fixture publishes a mesh after its object");
    }

    #[test]
    fn terrain_height_interpolates() {
        let (_region_world, regions) = test_support::registered_regions(2);
        let [west, east] = regions.as_slice() else { unreachable!() };
        let (west, east) = (*west, *east);
        let both = [(west, DVec3::ZERO), (east, DVec3::new(256.0, 0.0, 0.0))];
        let cases = [
            ("patch seam", terrain_view(&both[..1], &[(west, 0, 0, 0.0), (west, 1, 0, 16.0)]), DVec3::new(15.5, 4.0, 0.0), 8.0),
            ("region seam", terrain_view(&both, &[(west, 15, 0, 4.0), (east, 0, 0, 20.0)]), DVec3::new(255.5, 4.0, 0.0), 12.0),
            ("unloaded neighbor clamps edge", terrain_view(&both[..1], &[(west, 15, 0, 4.0)]), DVec3::new(255.5, 4.0, 0.0), 4.0),
            ("exact row ignores unloaded neighbor", terrain_view(&both[..1], &[(west, 1, 0, 7.0)]), DVec3::new(20.0, 15.0, 0.0), 7.0),
        ];
        for (case, view, point, expected) in cases {
            assert_eq!(view.terrain_height_at(point), Some(expected), "{case}");
        }
    }
}
