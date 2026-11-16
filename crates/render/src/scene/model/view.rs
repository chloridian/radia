use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    error::Error,
    fmt::{self, Display, Formatter},
    mem::size_of,
    time::Instant,
};

use assets::TextureHandle;
use utils::AssetId;
use uuid::Uuid;
use world::{Entity, Region, WorldRevision};

use super::{Batch, Change, MeshChange, Mirror, SceneLifetime, Status};
use crate::scene::{Object, Patch, RegionData, Surface, ViewerEnvironment, region_environment::RegionEnvironment};

const CHANGE_HISTORY: usize = 64;
const CHANGE_HISTORY_BYTES: usize = 4 * 1024 * 1024;
const FILTERED_OBJECT_PAGE_LIMIT: usize = 128;

#[derive(Default)]
pub(crate) struct SceneChanges {
    pub(crate) objects: Vec<Entity>,
    pub(crate) surfaces: Vec<(Region, crate::scene::SurfaceKind)>,
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
            + self.surfaces.capacity() * size_of::<(Region, crate::scene::SurfaceKind)>()
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

pub(crate) struct ObjectPage<'a> {
    pub(crate) entries: Vec<(Uuid, Option<&'a Object>)>,
    pub(crate) cursor: Option<Uuid>,
    pub(crate) done: bool,
}

pub struct SceneView {
    session: SceneLifetime,
    sequence: u64,
    geometry_sequence: u64,
    revision: Option<WorldRevision>,
    status: Status,
    mirror: Mirror,
    object_order: BTreeMap<Uuid, Entity>,
    membership_sequence: u64,
    change_history: VecDeque<(u64, SceneChanges)>,
    change_history_bytes: usize,
    reconcile_marker: SceneChanges,
    replacement_marker: SceneChanges,
    replacement_sequence: Option<u64>,
}

impl SceneView {
    pub fn viewer_environment(&self) -> Option<&ViewerEnvironment> {
        if self.session.is_active() { self.mirror.viewer_environment.as_ref() } else { None }
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
            membership_sequence: 0,
            change_history: VecDeque::with_capacity(CHANGE_HISTORY),
            change_history_bytes: 0,
            reconcile_marker: SceneChanges { reconcile_all: true, ..SceneChanges::default() },
            replacement_marker: SceneChanges { full: true, ..SceneChanges::default() },
            replacement_sequence: None,
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
        }
        let now = Instant::now();
        let mut membership_changed = batch.replace;
        for change in batch.changes {
            match change {
                Change::Object(object) => {
                    let entity = object.detail().summary.entity;
                    let previous = self
                        .mirror
                        .objects
                        .get(&entity)
                        .map(|object| (object.detail().summary.id, object.detail().summary.address.region()));
                    let was_visible = self.mirror.object_visible(entity);
                    self.mirror.apply_at(Change::Object(object), now);
                    let Some(current) = self.mirror.objects.get(&entity) else { continue };
                    let current_id = current.detail().summary.id;
                    let current_region = current.detail().summary.address.region();
                    if previous.map(|(id, _)| id) != Some(current_id) {
                        if let Some((previous_id, _)) = previous
                            && self.object_order.get(&previous_id) == Some(&entity)
                        {
                            self.object_order.remove(&previous_id);
                        }
                        self.object_order.insert(current_id, entity);
                        membership_changed = true;
                    }
                    membership_changed |= previous.is_some_and(|(_, region)| region != current_region);
                    membership_changed |= was_visible != self.mirror.object_visible(entity);
                }
                Change::Remove(entity) => {
                    if let Some(object) = self.mirror.objects.get(&entity) {
                        let id = object.detail().summary.id;
                        if self.object_order.get(&id) == Some(&entity) {
                            self.object_order.remove(&id);
                        }
                        membership_changed = true;
                    }
                    self.mirror.apply_at(Change::Remove(entity), now);
                }
                change @ Change::InvalidateSubtree(..) | change @ Change::Retire(_) => {
                    membership_changed = true;
                    self.mirror.apply_at(change, now);
                }
                change => self.mirror.apply_at(change, now),
            }
        }
        self.sequence = batch.sequence;
        if membership_changed {
            self.membership_sequence = self.membership_sequence.checked_add(1).expect("scene membership version space exhausted");
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
        if self.mirror.invalidations.is_empty() && self.mirror.retired_regions.is_empty() {
            return self.mirror.objects.len();
        }
        let mut visibility = HashMap::new();
        self.mirror.objects.keys().filter(|entity| self.mirror.visibility(**entity, &mut visibility).visible).count()
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

    pub fn resume_motion(&mut self, region: Region, now: Instant, regions: &[math::motion::RegionBounds]) {
        for object in self.mirror.objects.values_mut().filter(|object| object.detail().summary.address.region() == region) {
            object.resume_motion_at(now, regions);
        }
    }
    pub fn objects(&self) -> impl Iterator<Item = &Object> {
        let active = self.session.is_active();
        let needs_filter = !self.mirror.invalidations.is_empty() || !self.mirror.retired_regions.is_empty();
        let mirror = &self.mirror;
        let mut visibility = HashMap::new();
        self.mirror
            .objects
            .values()
            .filter(move |object| active && (!needs_filter || mirror.visibility(object.detail().summary.entity, &mut visibility).visible))
    }

    pub(crate) fn object_page(&self, after: Option<Uuid>, limit: usize) -> ObjectPage<'_> {
        if !self.session.is_active() || limit == 0 {
            return ObjectPage { entries: Vec::new(), cursor: after, done: true };
        }
        let needs_filter = !self.mirror.invalidations.is_empty() || !self.mirror.retired_regions.is_empty();
        let limit = if needs_filter { limit.min(FILTERED_OBJECT_PAGE_LIMIT) } else { limit };
        let entries = match after {
            Some(after) => self.object_order.range((std::ops::Bound::Excluded(after), std::ops::Bound::Unbounded)),
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
        if !self.session.is_active() || !self.mirror.object_visible(entity) {
            return None;
        }
        self.mirror.objects.get(&entity)
    }
    pub(crate) fn object_is_live(&self, entity: Entity) -> bool {
        self.object(entity).is_some()
    }
    pub(crate) fn membership_sequence(&self) -> u64 {
        self.membership_sequence
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
        crate::scene::surface::terrain_height_at(&self.mirror, position)
    }
    pub fn water_height(&self, region: Region) -> Option<f32> {
        if self.session.is_active() { self.mirror.water.get(&region).copied().flatten() } else { None }
    }

    pub fn surfaces(&self) -> impl Iterator<Item = &Surface> {
        self.mirror.surfaces.values().filter(|_| self.session.is_active())
    }

    pub(crate) fn surface_page(
        &self,
        after: Option<(Region, crate::scene::SurfaceKind)>,
        limit: usize,
    ) -> (Vec<&Surface>, Option<(Region, crate::scene::SurfaceKind)>, bool) {
        if !self.session.is_active() || limit == 0 {
            return (Vec::new(), after, true);
        }
        let entries = match after {
            Some(after) => self.mirror.surfaces.range((std::ops::Bound::Excluded(after), std::ops::Bound::Unbounded)),
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

    pub(crate) fn surface(&self, region: Region, kind: crate::scene::SurfaceKind) -> Option<&Surface> {
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
    use super::*;
    use math::DVec3;
    use world::{RegionInfo, RegionSummary, TERRAIN_PATCH_SAMPLES, TerrainPatch};

    use crate::scene::model::Receipt;
    use crate::scene::model::SceneLifetime;
    use crate::scene::{Budget, Reconciler};

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
                    RegionSummary { region, info: RegionInfo { origin: Some(origin), ..RegionInfo::default() }, terrain_settings: None },
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
        let local_id = std::num::NonZeroU32::new(local_id).expect("fixture local id");
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
    fn status_change_requests_reconciliation_without_full_rebuild() {
        let (_scene, mut view) = Reconciler::new(crate::scene::Budget::default());
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
            world::ParentUpdate::Local(std::num::NonZeroU32::new(1).expect("parent local id")),
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

        let membership = view.membership_sequence();
        apply_changes(&mut view, false, vec![Change::InvalidateSubtree(first, world::RemovalFence::new(region, 4, 0))]);
        assert!(view.membership_sequence() > membership, "subtree invalidation advances membership");
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

        let membership = view.membership_sequence();
        apply_changes(&mut view, false, vec![Change::Remove(third)]);
        assert!(view.membership_sequence() > membership);
        assert_eq!(view.object_count(), 0);
        assert_eq!(view.tracked_object_count(), 2);
        let page = object_page_state(&view, None, 1);
        assert_eq!(page.entries, [(first_id, None)]);
        assert_eq!(page.cursor, Some(first_id));
        assert!(!page.done);

        let _removal = world
            .remove_object(
                world::UpdateSource::new(region, 4, world::OrderEvidence::Ordered(4), 0),
                std::num::NonZeroU32::new(3).expect("removed local id"),
            )
            .expect("fixture removal applies");
        let reused = membership_entity(&mut world, region, third_id, 4, 5, world::ParentUpdate::Root);
        assert_ne!(reused, third);
        apply_changes(&mut view, false, vec![Change::Object(scene_object(&world, reused, &budget))]);
        assert_eq!(view.object_count(), 1);
        assert_eq!(view.tracked_object_count(), 3);
        assert_eq!(view.object(reused).map(|object| object.detail().summary.id), Some(third_id));
        let page = object_page_state(&view, Some(second_id), 1);
        assert_eq!(page.entries, [(third_id, Some(reused))]);
        assert_eq!(page.cursor, Some(third_id));
        assert!(page.done);

        let membership = view.membership_sequence();
        apply_changes(&mut view, false, vec![Change::Retire(region)]);
        assert!(view.membership_sequence() > membership);
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
    fn terrain_height_interpolates_across_patch_seam() {
        let (_region_world, region) = crate::test_support::registered_region();
        let view = terrain_view(&[(region, DVec3::ZERO)], &[(region, 0, 0, 0.0), (region, 1, 0, 16.0)]);

        assert_eq!(view.terrain_height_at(DVec3::new(15.5, 4.0, 0.0)), Some(8.0));
    }

    #[test]
    fn terrain_height_interpolates_across_region_seam() {
        let (_region_world, regions) = crate::test_support::registered_regions(2);
        let [west, east] = regions.as_slice() else { unreachable!() };
        let (west, east) = (*west, *east);
        let view = terrain_view(&[(west, DVec3::ZERO), (east, DVec3::new(256.0, 0.0, 0.0))], &[(west, 15, 0, 4.0), (east, 0, 0, 20.0)]);

        assert_eq!(view.terrain_height_at(DVec3::new(255.5, 4.0, 0.0)), Some(12.0));
    }

    #[test]
    fn unloaded_region_neighbor_clamps_to_mesh_edge() {
        let (_region_world, region) = crate::test_support::registered_region();
        let view = terrain_view(&[(region, DVec3::ZERO)], &[(region, 15, 0, 4.0)]);

        assert_eq!(view.terrain_height_at(DVec3::new(255.5, 4.0, 0.0)), Some(4.0));
    }

    #[test]
    fn exact_row_ignores_unloaded_neighbor() {
        let (_region_world, region) = crate::test_support::registered_region();
        let view = terrain_view(&[(region, DVec3::ZERO)], &[(region, 1, 0, 7.0)]);

        assert_eq!(view.terrain_height_at(DVec3::new(20.0, 15.0, 0.0)), Some(7.0));
    }
}
