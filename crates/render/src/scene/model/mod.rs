mod delivery;
mod object;
mod view;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use assets::TextureHandle;
use utils::{AssetId, FastHashMap};
use world::{Entity, Parent, Region, RegionSummary, RemovalFence, TerrainPatch, TerrainPatchId, WorldRevision};

pub(in crate::scene) use self::delivery::{Delivery, DeliveryState, Receipt};
pub(crate) use self::view::VisibilityVersion;
pub use self::{
    object::{Geometry, HiddenReason, Object, Placement},
    view::{ApplyError, SceneView, ViewVersion},
};
use crate::scene::{
    DrawnMeshKey, MeshKey, PreparedMesh, Surface, SurfaceKind, ViewerEnvironment,
    asset_demand::{MaterialChange, MeshPublication},
    budget::Lease,
    motion::MotionState,
    region_environment::RegionEnvironment,
};

#[derive(Clone, Debug)]
pub(in crate::scene) struct SceneLifetime(Arc<AtomicBool>);

impl SceneLifetime {
    pub(in crate::scene) fn new() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }
    pub(in crate::scene) fn is_active(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    pub(in crate::scene) fn close(&self) {
        self.0.store(false, Ordering::Release);
    }
    pub(in crate::scene) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[derive(Clone)]
pub struct RegionData(Arc<RegionValue>, Arc<[Option<TextureHandle>; 4]>);

impl RegionData {
    pub(in crate::scene) fn new(summary: RegionSummary, lease: Lease) -> Self {
        Self(Arc::new(RegionValue { summary, _lease: lease }), Arc::new([None, None, None, None]))
    }
    pub(in crate::scene) fn with_terrain_textures(&self, textures: [Option<TextureHandle>; 4]) -> Self {
        Self(self.0.clone(), Arc::new(textures))
    }
    pub fn summary(&self) -> &RegionSummary {
        &self.0.summary
    }
    pub fn terrain_textures(&self) -> &[Option<TextureHandle>; 4] {
        &self.1
    }
}

struct RegionValue {
    summary: RegionSummary,
    _lease: Lease,
}

#[derive(Clone)]
pub struct Patch(Arc<PatchValue>);

impl Patch {
    pub(in crate::scene) fn new(patch: TerrainPatch, lease: Lease) -> Self {
        Self(Arc::new(PatchValue { patch, _lease: lease }))
    }
    pub fn source(&self) -> &TerrainPatch {
        &self.0.patch
    }
}

struct PatchValue {
    patch: TerrainPatch,
    _lease: Lease,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryReason {
    HistoryLost,
    Capacity,
    ReadFailed,
    InvalidReply,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Loading,
    Ready,
    Recovering { reason: RecoveryReason, attempt: u8 },
    Degraded { reason: RecoveryReason },
    Closed,
}

pub(in crate::scene) enum MeshChange {
    Set(Entity, MeshKey, PreparedMesh),
    Clear(Entity, MeshKey),
}

pub(in crate::scene) enum Change {
    ViewerEnvironment(Option<ViewerEnvironment>),
    Object(Object),
    Mesh(MeshChange),
    Material(MaterialChange),
    Remove(Entity),
    InvalidateSubtree(Entity, RemovalFence),
    Region(RegionData),
    Environment(Region, Option<RegionEnvironment>),
    EnvironmentTexture(AssetId, Option<TextureHandle>),
    Retire(Region),
    Patch(Region, Patch),
    Water(Region, Option<f32>),
    Surface(Surface),
    RemoveSurface(Region, SurfaceKind),
}

pub struct Batch {
    pub(in crate::scene) session: SceneLifetime,
    pub(in crate::scene) sequence: u64,
    pub(in crate::scene) revision: Option<WorldRevision>,
    pub(in crate::scene) status: Status,
    pub(in crate::scene) replace: bool,
    pub(in crate::scene) changes: Vec<Change>,
    pub(in crate::scene) delivery: Delivery,
}

impl Batch {
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn status(&self) -> Status {
        self.status
    }
}

#[derive(Default)]
pub(in crate::scene) struct RetiredRegions {
    intervals: BTreeMap<u64, u64>,
}

impl RetiredRegions {
    pub(in crate::scene) fn is_empty(&self) -> bool {
        self.intervals.is_empty()
    }

    fn contains(&self, region: Region) -> bool {
        self.intervals.range(..=region.get()).next_back().is_some_and(|(_, end)| region.get() <= *end)
    }

    fn insert(&mut self, region: Region) {
        let generation = region.get();
        let mut start = generation;
        let mut end = generation;
        if let Some((&previous_start, &previous_end)) = self.intervals.range(..=generation).next_back() {
            if previous_end >= generation {
                return;
            }
            if previous_end.checked_add(1) == Some(generation) {
                start = previous_start;
                self.intervals.remove(&previous_start);
            }
        }
        if let Some((&next_start, &next_end)) = self.intervals.range(generation..).next()
            && next_start <= end.saturating_add(1)
        {
            end = next_end;
            self.intervals.remove(&next_start);
        }
        self.intervals.insert(start, end);
    }
}

#[derive(Default)]
pub(in crate::scene) struct Mirror {
    pub(in crate::scene) viewer_environment: Option<ViewerEnvironment>,
    pub(in crate::scene) objects: FastHashMap<Entity, Object>,
    pub(in crate::scene) children: FastHashMap<Entity, HashSet<Entity>>,
    pub(in crate::scene) invalidations: HashMap<Entity, RemovalFence>,
    pub(in crate::scene) retired_regions: RetiredRegions,
    pub(in crate::scene) regions: BTreeMap<Region, RegionData>,
    pub(in crate::scene) environments: BTreeMap<Region, RegionEnvironment>,
    pub(in crate::scene) environment_textures: BTreeMap<AssetId, TextureHandle>,
    pub(in crate::scene) patches: BTreeMap<(Region, TerrainPatchId), Patch>,
    pub(in crate::scene) water: BTreeMap<Region, Option<f32>>,
    pub(in crate::scene) surfaces: BTreeMap<(Region, SurfaceKind), Surface>,
}

impl Mirror {
    pub(in crate::scene) fn is_retired(&self, region: Region) -> bool {
        self.retired_regions.contains(region)
    }

    fn resolved_parent(object: &Object) -> Option<Entity> {
        match object.detail().summary.parent {
            Parent::Resolved(parent) => Some(parent),
            Parent::Root | Parent::Unknown | Parent::Pending { .. } => None,
        }
    }

    fn link_child(&mut self, parent: Option<Entity>, child: Entity) {
        if let Some(parent) = parent {
            self.children.entry(parent).or_default().insert(child);
        }
    }

    fn unlink_child(&mut self, parent: Option<Entity>, child: Entity) {
        let Some(parent) = parent else { return };
        if let Some(children) = self.children.get_mut(&parent) {
            children.remove(&child);
            if children.is_empty() {
                self.children.remove(&parent);
            }
        }
    }

    pub(in crate::scene) fn remove_object(&mut self, entity: Entity) -> Option<Object> {
        self.invalidations.remove(&entity);
        let removed = self.objects.remove(&entity)?;
        self.unlink_child(Self::resolved_parent(&removed), entity);
        Some(removed)
    }

    pub(in crate::scene) fn object_visible(&self, entity: Entity) -> bool {
        if self.invalidations.is_empty() {
            return self.objects.get(&entity).is_some_and(|object| !self.is_retired(object.detail().summary.address.region()));
        }
        self.visibility(entity, &mut HashMap::new()).visible
    }

    pub(in crate::scene) fn visibility(&self, entity: Entity, cache: &mut HashMap<Entity, SceneVisibility>) -> SceneVisibility {
        if let Some(known) = cache.get(&entity) {
            return *known;
        }
        let mut path = Vec::new();
        let mut visited = HashSet::new();
        let mut current = entity;
        let mut visibility = loop {
            if let Some(known) = cache.get(&current).copied() {
                break known;
            }
            if !visited.insert(current) {
                let invalid = SceneVisibility { visible: false, cause: None };
                cache.insert(current, invalid);
                break invalid;
            }
            let object = self.objects.get(&current);
            if let Some(object) = object
                && self.retired_regions.contains(object.detail().summary.address.region())
            {
                let region = object.detail().summary.address.region();
                let retired = SceneVisibility { visible: false, cause: Some(SceneCause::Retired(region)) };
                cache.insert(current, retired);
                break retired;
            }
            if let Some(fence) = self.invalidations.get(&current).copied() {
                let cause = if self.retired_regions.contains(fence.region()) {
                    SceneCause::Retired(fence.region())
                } else {
                    SceneCause::Fence(fence)
                };
                let invalid = SceneVisibility { visible: false, cause: Some(cause) };
                cache.insert(current, invalid);
                break invalid;
            }
            let Some(object) = object else {
                let missing = SceneVisibility { visible: true, cause: None };
                cache.insert(current, missing);
                break missing;
            };
            match object.detail().summary.parent {
                Parent::Resolved(parent) => {
                    path.push(current);
                    current = parent;
                }
                Parent::Root | Parent::Pending { .. } | Parent::Unknown => {
                    let root = SceneVisibility { visible: true, cause: None };
                    cache.insert(current, root);
                    break root;
                }
            }
        };
        for child in path.into_iter().rev() {
            let Some(child_object) = self.objects.get(&child) else {
                visibility = SceneVisibility { visible: false, cause: None };
                cache.insert(child, visibility);
                continue;
            };
            let child_detail = child_object.detail();
            let child_region = child_detail.summary.address.region();
            let cascades = visibility.cause.is_some_and(|cause| match cause {
                SceneCause::Retired(region) => child_region == region,
                SceneCause::Fence(fence) => {
                    child_detail.summary.kind != world::Kind::Avatar
                        && fence.region() == child_region
                        && (fence.order(), fence.block_ordinal())
                            > (child_detail.summary.binding_fence.order(), child_detail.summary.binding_fence.block_ordinal())
                }
            });
            visibility = if !visibility.visible && (visibility.cause.is_none() || cascades) {
                SceneVisibility { visible: false, cause: visibility.cause }
            } else {
                SceneVisibility { visible: true, cause: None }
            };
            cache.insert(child, visibility);
        }
        cache.get(&entity).copied().unwrap_or(visibility)
    }

    pub(in crate::scene) fn apply_mesh(&mut self, change: &MeshChange) {
        match change {
            MeshChange::Set(entity, key, state) => {
                if let Some(object) = self.objects.get_mut(entity)
                    && object.geometry() == Geometry::Prim(key.shape)
                {
                    if let PreparedMesh::Ready(mesh) = state {
                        object.mesh = Some((DrawnMeshKey::Prim(*key), mesh.clone()));
                    } else if !matches!(state, PreparedMesh::Pending | PreparedMesh::Capacity) {
                        object.mesh = None;
                    }
                    object.preparation = Some((*key, state.clone()));
                }
            }
            MeshChange::Clear(entity, key) => {
                if let Some(object) = self.objects.get_mut(entity)
                    && object.geometry() == Geometry::Prim(key.shape)
                {
                    if object.preparation.as_ref().is_some_and(|(current, _)| current.shape == key.shape) {
                        object.preparation = None;
                    }
                    object.mesh = None;
                }
            }
        }
    }

    pub(in crate::scene) fn apply(&mut self, change: Change) {
        self.apply_at(change, Instant::now());
    }

    pub(in crate::scene) fn apply_at(&mut self, change: Change, now: Instant) {
        match change {
            Change::ViewerEnvironment(environment) => self.viewer_environment = environment,
            Change::Object(mut object) => {
                if self.retired_regions.contains(object.detail().summary.address.region()) {
                    return;
                }
                let entity = object.detail().summary.entity;
                let next_parent = Self::resolved_parent(&object);
                let previous_parent = if let Some(old) = self.objects.get(&entity) {
                    let previous_parent = Self::resolved_parent(old);
                    object.inherit_animation_timing(old, now);
                    object.update_motion_from(old, now);
                    if old.geometry() == object.geometry() {
                        object.mesh = old.mesh.clone();
                    }
                    if old.geometry() == object.geometry() && object.preparation.is_none() {
                        object.preparation = old.preparation.clone();
                        if object.same_appearance(old) {
                            object.materials = old.materials.clone();
                        }
                    }
                    previous_parent
                } else {
                    object.motion =
                        MotionState::new(object.detail().motion_source.and_then(world::UpdateSource::motion_received_at).unwrap_or(now));
                    None
                };
                self.objects.insert(entity, object);
                if previous_parent != next_parent {
                    self.unlink_child(previous_parent, entity);
                    self.link_child(next_parent, entity);
                }
            }
            Change::Mesh(change) => self.apply_mesh(&change),
            Change::Material(change) => {
                if let Some(object) = self.objects.get_mut(&change.source.detail().summary.entity)
                    && object.same_appearance(&change.source)
                {
                    object.materials = change.materials;
                    match &change.mesh {
                        MeshPublication::Keep => {}
                        MeshPublication::Clear => object.mesh = None,
                        MeshPublication::Set(key, mesh) => {
                            let matches = match (object.geometry(), key) {
                                (Geometry::Mesh(asset), DrawnMeshKey::Asset { asset: mesh_asset, .. }) => asset == *mesh_asset,
                                (Geometry::Sculpt { asset_id, sculpt_type, flags }, DrawnMeshKey::Sculpt(sculpt)) => {
                                    asset_id == sculpt.asset_id && sculpt_type == sculpt.sculpt_type && flags == sculpt.flags
                                }
                                (Geometry::Sculpt { .. }, DrawnMeshKey::SculptPlaceholder) => true,
                                _ => false,
                            };
                            if matches {
                                object.mesh = Some((*key, mesh.clone()));
                            }
                        }
                    }
                }
            }
            Change::Remove(entity) => {
                let _ = self.remove_object(entity);
            }
            Change::InvalidateSubtree(entity, fence) => {
                self.invalidations.insert(entity, fence);
            }
            Change::Region(region) => {
                self.regions.insert(region.summary().region, region);
            }
            Change::Retire(region) => {
                self.retired_regions.insert(region);
                self.regions.remove(&region);
                self.environments.remove(&region);
                self.water.remove(&region);
                self.patches.retain(|(r, _), _| *r != region);
                self.surfaces.retain(|(r, _), _| *r != region);
            }
            Change::Patch(region, patch) => {
                self.patches.insert((region, patch.source().id()), patch);
            }
            Change::Environment(region, source) => match source {
                Some(source) => {
                    self.environments.insert(region, source);
                }
                None => {
                    self.environments.remove(&region);
                }
            },
            Change::EnvironmentTexture(id, handle) => match handle {
                Some(handle) => {
                    self.environment_textures.insert(id, handle);
                }
                None => {
                    self.environment_textures.remove(&id);
                }
            },
            Change::Water(region, height) => {
                self.water.insert(region, height);
            }
            Change::Surface(surface) => {
                self.surfaces.insert((surface.region, surface.kind), surface);
            }
            Change::RemoveSurface(region, kind) => {
                self.surfaces.remove(&(region, kind));
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(in crate::scene) struct SceneVisibility {
    visible: bool,
    cause: Option<SceneCause>,
}

#[derive(Clone, Copy)]
enum SceneCause {
    Fence(RemovalFence),
    Retired(Region),
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, num::NonZeroU32};

    use proptest::prelude::*;
    use uuid::Uuid;
    use world::{ApplyOutcome, Kind, ObjectObservation, OrderEvidence, Parent, ParentUpdate, UpdateSource, World, WorldLimits};

    use super::{Change, Mirror, Object, RetiredRegions};
    use crate::{scene::Budget, test_support};

    #[derive(Clone, Copy, Debug)]
    enum ParentSpec {
        Root,
        Unknown,
        Missing,
        Existing(usize),
    }

    fn parent_spec_for(index: usize) -> impl Strategy<Value = ParentSpec> {
        if index == 0 {
            prop_oneof![Just(ParentSpec::Root), Just(ParentSpec::Unknown), Just(ParentSpec::Missing)].boxed()
        } else {
            prop_oneof![
                Just(ParentSpec::Root),
                Just(ParentSpec::Unknown),
                Just(ParentSpec::Missing),
                (0..index).prop_map(ParentSpec::Existing),
            ]
            .boxed()
        }
    }

    fn mirror_case() -> impl Strategy<Value = (usize, Vec<(usize, ParentSpec)>, Vec<bool>)> {
        (1usize..=3, 1usize..=12).prop_flat_map(|(region_count, object_count)| {
            let objects = (0..object_count).map(|index| (0..region_count, parent_spec_for(index))).collect::<Vec<_>>();
            let retired = prop::collection::vec(any::<bool>(), region_count);
            (Just(region_count), objects, retired)
        })
    }

    #[test]
    fn retired_ranges_spare_active_generations() {
        let (_, regions) = test_support::registered_regions(4);
        let [first, second, third, fourth] = regions.as_slice() else { panic!("four regions are registered") };
        let mut retired = RetiredRegions::default();

        retired.insert(*first);
        retired.insert(*third);
        assert!(retired.contains(*first));
        assert!(!retired.contains(*second));
        assert!(retired.contains(*third));
        assert!(!retired.contains(*fourth));

        retired.insert(*second);
        retired.insert(*fourth);
        retired.insert(*second);
        assert!(regions.iter().all(|region| retired.contains(*region)));
    }

    #[test]
    fn children_index_tracks_lifecycle() {
        let mut world = World::new(WorldLimits {
            max_regions: 1,
            max_entities: 8,
            max_entities_per_region: 8,
            max_tombstones_per_region: 8,
            max_tombstones: 8,
            max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(8),
            max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(8),
            max_journal_entries: 16,
            max_journal_changes: 16,
            max_region_name_bytes: 64,
            max_recovery_targets: 4,
        });
        let region = world.register_region().expect("region registers");
        let mut entities = Vec::new();
        for id in 1u32..=4 {
            let obs = ObjectObservation::new(
                UpdateSource::new(region, id, OrderEvidence::Ordered(u64::from(id)), 0),
                Uuid::from_u128(u128::from(id)),
                NonZeroU32::new(id).expect("local id"),
                Kind::Object,
                ParentUpdate::Root,
            );
            let ApplyOutcome::Introduced(entity) = world.observe_object(obs).expect("entity applies").outcome else {
                panic!("entity must be new");
            };
            entities.push(entity);
        }
        let [parent_a, parent_b, child, reused] = entities.as_slice() else { panic!("four entities") };
        let budget = Budget::default();
        let make_object = |entity, parent| {
            let mut detail = world.object_detail(entity).expect("entity detail");
            detail.summary.parent = parent;
            let lease = budget.reserve(detail.owned_bytes() + 32).expect("object lease");
            Object::new(detail, lease)
        };

        let mut mirror = Mirror::default();
        mirror.apply(Change::Object(make_object(*parent_a, Parent::Root)));
        mirror.apply(Change::Object(make_object(*parent_b, Parent::Root)));
        mirror.apply(Change::Object(make_object(*child, Parent::Resolved(*parent_a))));
        assert_eq!(mirror.children.get(parent_a).map(|set| set.contains(child)), Some(true));
        assert!(!mirror.children.contains_key(parent_b));

        mirror.apply(Change::Object(make_object(*child, Parent::Resolved(*parent_b))));
        assert!(!mirror.children.contains_key(parent_a));
        assert_eq!(mirror.children.get(parent_b).map(|set| set.contains(child)), Some(true));

        mirror.apply(Change::Object(make_object(*child, Parent::Root)));
        assert!(mirror.children.is_empty());

        mirror.apply(Change::Object(make_object(*child, Parent::Resolved(*parent_a))));
        mirror.apply(Change::Remove(*child));
        assert!(mirror.children.is_empty());

        mirror.apply(Change::Object(make_object(*reused, Parent::Resolved(*parent_b))));
        assert_eq!(mirror.children.get(parent_b).map(|set| set.contains(reused)), Some(true));
        assert!(mirror.remove_object(*reused).is_some());
        assert!(mirror.children.is_empty());
    }

    proptest! {
        #[test]
        fn object_visible_matches_walk((region_count, object_specs, retired_flags) in mirror_case()) {
            let entity_count = object_specs.len() + 1;
            let mut world = World::new(WorldLimits {
                max_regions: region_count,
                max_entities: entity_count,
                max_entities_per_region: entity_count,
                max_tombstones_per_region: entity_count,
                max_tombstones: entity_count,
                max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(entity_count),
                max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(entity_count),
                max_journal_entries: entity_count + region_count + 4,
                max_journal_changes: entity_count + region_count + 4,
                max_region_name_bytes: 64,
                max_recovery_targets: 4,
            });
            let regions = (0..region_count).map(|_| world.register_region().expect("region registers")).collect::<Vec<_>>();
            let missing_obs = ObjectObservation::new(
                UpdateSource::new(regions[0], 1, OrderEvidence::Ordered(1), 0),
                Uuid::from_u128(1),
                NonZeroU32::new(1).expect("local id"),
                Kind::Object,
                ParentUpdate::Root,
            );
            let ApplyOutcome::Introduced(missing_entity) = world.observe_object(missing_obs).expect("missing entity applies").outcome else {
                panic!("missing entity must be new");
            };

            let mut entities = Vec::with_capacity(object_specs.len());
            for (index, &(region_index, _)) in object_specs.iter().enumerate() {
                let id_num = (index + 2) as u32;
                let obs = ObjectObservation::new(
                    UpdateSource::new(regions[region_index], id_num, OrderEvidence::Ordered(u64::from(id_num)), 0),
                    Uuid::from_u128(u128::from(id_num)),
                    NonZeroU32::new(id_num).expect("local id"),
                    Kind::Object,
                    ParentUpdate::Root,
                );
                let ApplyOutcome::Introduced(entity) = world.observe_object(obs).expect("entity applies").outcome else {
                    panic!("entity must be new");
                };
                entities.push(entity);
            }

            let budget = Budget::default();
            let mut mirror = Mirror::default();
            for (index, &(_, parent_spec)) in object_specs.iter().enumerate() {
                let entity = entities[index];
                let mut detail = world.object_detail(entity).expect("entity detail");
                detail.summary.parent = match parent_spec {
                    ParentSpec::Root => Parent::Root,
                    ParentSpec::Unknown => Parent::Unknown,
                    ParentSpec::Missing => Parent::Resolved(missing_entity),
                    ParentSpec::Existing(parent_index) => Parent::Resolved(entities[parent_index]),
                };
                let lease = budget.reserve(detail.owned_bytes() + 32).expect("object lease");
                mirror.apply(Change::Object(Object::new(detail, lease)));
            }
            for (&region, &retired) in regions.iter().zip(retired_flags.iter()) {
                if retired {
                    mirror.apply(Change::Retire(region));
                }
            }

            for &entity in &entities {
                let walk = mirror.visibility(entity, &mut HashMap::new()).visible;
                let expected = !mirror.is_retired(mirror.objects[&entity].detail().summary.address.region());
                prop_assert_eq!(walk, expected, "walk must match own-region retirement for acyclic graph");
                prop_assert_eq!(mirror.object_visible(entity), walk, "object_visible must match walk");
            }
        }
    }
}
