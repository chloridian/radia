use std::{
    collections::BTreeMap,
    mem,
    ops::Bound::{Excluded, Unbounded},
};

use utils::{FastHashMap, FastHashSet};
use uuid::Uuid;
use world::{Entity, Region};

use crate::scene::{
    Budget, Camera, Detail, Geometry, MeshBuildError, MeshKey, Object, Placement, PreparedMesh,
    asset_demand::{SculptCompletion, SculptJob},
    mesh_cache::{MeshCache, PrimCompletion, PrimJob},
    model::{MeshChange, Mirror},
    region_environment::{EnvironmentDecodeCompletion, EnvironmentDecodeJob},
    surface::{SurfaceCompletion, SurfaceJob},
};

pub struct MeshJob(pub(in crate::scene) Work);

impl MeshJob {
    pub fn is_visible(&self) -> bool {
        match &self.0 {
            Work::Prim(job) => job.is_visible(),
            Work::Sculpt(_) | Work::Surface(_) | Work::Environment(_) => true,
        }
    }

    pub fn run(self) -> MeshCompletion {
        MeshCompletion(match self.0 {
            Work::Prim(job) => Completion::Prim(job.run()),
            Work::Sculpt(job) => Completion::Sculpt(job.run()),
            Work::Surface(job) => Completion::Surface(job.run()),
            Work::Environment(job) => Completion::Environment(job.run()),
        })
    }
}

pub(in crate::scene) enum Work {
    Prim(PrimJob),
    Sculpt(SculptJob),
    Surface(Box<SurfaceJob>),
    Environment(EnvironmentDecodeJob),
}

pub struct MeshCompletion(pub(in crate::scene) Completion);

pub(in crate::scene) enum Completion {
    Prim(PrimCompletion),
    Sculpt(SculptCompletion),
    Surface(SurfaceCompletion),
    Environment(EnvironmentDecodeCompletion),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stamp {
    Pending,
    Ready,
    Failed(MeshBuildError),
    Capacity,
}

struct MeshTarget {
    key: MeshKey,
    sent: Option<Stamp>,
    admitted: bool,
    priority: u32,
}

#[derive(Clone, Copy)]
enum ScanState {
    Idle { dirty: bool },
    Scanning { cursor: Option<Uuid>, rescan: bool },
}

pub(in crate::scene) struct MeshPlanner {
    camera: Option<Camera>,
    meshes: MeshCache,
    targets: FastHashMap<Entity, MeshTarget>,
    target_users: FastHashMap<MeshKey, FastHashSet<Entity>>,
    rejected: FastHashSet<Entity>,
    last_retry_capacity_revision: Option<u64>,
    scan: ScanState,
}

impl MeshPlanner {
    pub(in crate::scene) fn new(budget: Budget) -> Self {
        Self {
            camera: None,
            meshes: MeshCache::new(budget),
            targets: FastHashMap::default(),
            target_users: FastHashMap::default(),
            rejected: FastHashSet::default(),
            last_retry_capacity_revision: None,
            scan: ScanState::Idle { dirty: false },
        }
    }

    #[cfg(test)]
    fn new_with_cache_bytes(budget: Budget, cache_bytes: usize) -> Self {
        let mut planner = Self::new(budget.clone());
        planner.meshes = MeshCache::new_with_cache_bytes(budget, cache_bytes);
        planner
    }

    pub(in crate::scene) fn set_camera(&mut self, camera: Camera) {
        if self.camera != Some(camera) {
            self.camera = Some(camera);
            self.invalidate_scan();
        }
    }

    pub(in crate::scene) fn invalidate(&mut self) {
        self.invalidate_scan();
    }

    pub(in crate::scene) fn remove(&mut self, entity: Entity) {
        if self.detach_target(entity).is_some() {
            self.invalidate_scan();
        }
    }

    pub(in crate::scene) fn retire(&mut self, region: Region, mirror: &Mirror) {
        let retired = self
            .targets
            .keys()
            .copied()
            .filter(|entity| mirror.objects.get(entity).is_none_or(|object| object.detail().summary.address.region() == region))
            .collect::<Vec<_>>();
        for entity in retired {
            self.detach_target(entity);
        }
        self.rejected.retain(|entity| mirror.objects.get(entity).is_some_and(|object| object.detail().summary.address.region() != region));
        self.invalidate_scan();
    }

    pub(in crate::scene) fn reset(&mut self, budget: Budget) {
        let camera = self.camera;
        *self = Self::new(budget);
        self.camera = camera;
        if camera.is_some() {
            self.invalidate_scan();
        }
    }

    pub(in crate::scene) fn complete(&mut self, completion: PrimCompletion) {
        self.meshes.complete(completion);
    }

    pub(in crate::scene) fn start(&mut self) -> Option<MeshJob> {
        self.meshes.start().map(|job| MeshJob(Work::Prim(job)))
    }

    pub(in crate::scene) fn has_work(&self) -> bool {
        self.camera.is_some()
            && (!matches!(self.scan, ScanState::Idle { dirty: false }) || self.meshes.has_updates() || self.has_capacity_retry())
    }

    pub(in crate::scene) fn waiting(&self) -> bool {
        self.meshes.waiting()
    }

    pub(in crate::scene) fn retry_admission(&mut self) -> bool {
        self.has_capacity_retry()
    }

    fn has_capacity_retry(&self) -> bool {
        !self.rejected.is_empty() && self.last_retry_capacity_revision != Some(self.meshes.capacity_revision())
    }

    fn record_rejected(&mut self, entity: Entity) {
        self.rejected.insert(entity);
        self.last_retry_capacity_revision = Some(self.meshes.capacity_revision());
    }

    fn retry_rejected(&mut self, camera: Camera, mirror: &Mirror, changes: &mut Vec<MeshChange>) {
        let candidates = mem::take(&mut self.rejected);
        for entity in candidates {
            let Some(object) = mirror.objects.get(&entity) else {
                self.detach_target(entity);
                continue;
            };
            if !mirror.object_visible(entity) {
                if let Some(target) = self.detach_target(entity) {
                    changes.push(MeshChange::Clear(entity, target.key));
                }
                continue;
            }
            self.reconcile_entity(camera, entity, object, changes);
        }
        self.last_retry_capacity_revision = if self.rejected.is_empty() { None } else { Some(self.meshes.capacity_revision()) };
    }

    fn reconcile_entity(&mut self, camera: Camera, entity: Entity, object: &Object, changes: &mut Vec<MeshChange>) {
        let desired = self.desired(camera, object);
        let priority = object_priority(camera, object);
        let current = self.targets.get(&entity).map(|target| target.key);
        if current != desired {
            if let Some(key) = desired {
                let admitted = self.meshes.request(key, priority);
                if admitted || current.is_none() {
                    self.detach_target(entity);
                    self.targets.insert(entity, MeshTarget { key, sent: None, admitted, priority });
                    self.target_users.entry(key).or_default().insert(entity);
                    if admitted {
                        self.rejected.remove(&entity);
                    } else {
                        self.record_rejected(entity);
                    }
                } else {
                    self.record_rejected(entity);
                }
            } else {
                self.rejected.remove(&entity);
                if let Some(target) = self.detach_target(entity) {
                    changes.push(MeshChange::Clear(entity, target.key));
                }
            }
        } else if let Some(target) = self.targets.get_mut(&entity) {
            if !target.admitted {
                target.admitted = self.meshes.request(target.key, priority);
                if target.admitted {
                    self.rejected.remove(&entity);
                } else {
                    self.record_rejected(entity);
                }
            } else {
                self.rejected.remove(&entity);
            }
        } else {
            self.rejected.remove(&entity);
        }
        if let Some(target) = self.targets.get_mut(&entity) {
            target.priority = priority;
            self.meshes.prioritize(target.key, target.priority);
            let state = if target.admitted { self.meshes.get(target.key) } else { PreparedMesh::Capacity };
            let stamp = mesh_stamp(&state);
            if target.sent != Some(stamp) {
                changes.push(MeshChange::Set(entity, target.key, state));
                target.sent = Some(stamp);
            }
        }
    }

    pub(in crate::scene) fn step(&mut self, mirror: &Mirror, identities: &BTreeMap<Uuid, Entity>) -> Vec<MeshChange> {
        let Some(camera) = self.camera else {
            return Vec::new();
        };
        let mut changes = Vec::new();
        let cursor = match self.scan {
            ScanState::Idle { dirty: false } => {
                if self.has_capacity_retry() {
                    self.retry_rejected(camera, mirror, &mut changes);
                }
                self.publish_cache_updates(&mut changes);
                return changes;
            }
            ScanState::Idle { dirty: true } => {
                self.meshes.begin_priority_generation();
                self.scan = ScanState::Scanning { cursor: None, rescan: false };
                None
            }
            ScanState::Scanning { cursor, .. } => cursor,
        };
        if self.has_capacity_retry() {
            self.retry_rejected(camera, mirror, &mut changes);
        }
        let mut entries = identities.range((cursor.map_or(Unbounded, Excluded), Unbounded));
        let mut finished = false;
        for _ in 0..128 {
            let Some((id, entity)) = entries.next() else {
                finished = true;
                break;
            };
            if let ScanState::Scanning { cursor, .. } = &mut self.scan {
                *cursor = Some(*id);
            }
            if !mirror.object_visible(*entity) {
                self.rejected.remove(entity);
                if let Some(target) = self.detach_target(*entity) {
                    changes.push(MeshChange::Clear(*entity, target.key));
                }
                continue;
            }
            let object = &mirror.objects[entity];
            self.reconcile_entity(camera, *entity, object, &mut changes);
        }
        if finished {
            let dirty = match self.scan {
                ScanState::Scanning { rescan, .. } => rescan,
                ScanState::Idle { dirty } => dirty,
            };
            self.scan = ScanState::Idle { dirty };
        }
        self.publish_cache_updates(&mut changes);
        changes
    }

    fn invalidate_scan(&mut self) {
        match &mut self.scan {
            ScanState::Idle { dirty } => *dirty = true,
            ScanState::Scanning { rescan, .. } => *rescan = true,
        }
    }

    fn detach_target(&mut self, entity: Entity) -> Option<MeshTarget> {
        self.rejected.remove(&entity);
        let target = self.targets.remove(&entity)?;
        if target.admitted {
            self.meshes.release(target.key);
        }
        if let Some(users) = self.target_users.get_mut(&target.key) {
            users.remove(&entity);
            if users.is_empty() {
                self.target_users.remove(&target.key);
            }
        }
        Some(target)
    }

    fn publish_cache_updates(&mut self, changes: &mut Vec<MeshChange>) {
        for key in self.meshes.take_updated() {
            let entities = self.target_users.get(&key).map(|users| users.iter().copied().collect::<Vec<_>>()).unwrap_or_default();
            let state = self.meshes.get(key);
            let stamp = mesh_stamp(&state);
            for entity in entities {
                let Some(target) = self.targets.get_mut(&entity) else { continue };
                if target.key == key && target.sent != Some(stamp) {
                    changes.push(MeshChange::Set(entity, key, state.clone()));
                    target.sent = Some(stamp);
                }
            }
        }
        let mut evicted = self.meshes.take_evicted();
        while let Some(key) = evicted.pop() {
            let entities = self.target_users.get(&key).map(|users| users.iter().copied().collect::<Vec<_>>()).unwrap_or_default();
            for entity in entities {
                let Some(target) = self.targets.get(&entity) else { continue };
                if target.key != key {
                    continue;
                }
                let fallback = lower_detail(key.detail).map(|detail| MeshKey { shape: key.shape, detail });
                let priority = target.priority;
                if let Some(fallback) = fallback {
                    self.detach_target(entity);
                    let admitted = self.meshes.request(fallback, priority);
                    if !admitted {
                        self.record_rejected(entity);
                    }
                    let mut target = MeshTarget { key: fallback, sent: None, admitted, priority };
                    let state = if admitted { self.meshes.get(fallback) } else { PreparedMesh::Capacity };
                    target.sent = Some(mesh_stamp(&state));
                    changes.push(MeshChange::Set(entity, fallback, state));
                    self.targets.insert(entity, target);
                    self.target_users.entry(fallback).or_default().insert(entity);
                    evicted.extend(self.meshes.take_evicted());
                } else {
                    let state = self.meshes.get(key);
                    let stamp = mesh_stamp(&state);
                    if target.sent != Some(stamp) {
                        changes.push(MeshChange::Set(entity, key, state));
                        self.targets.get_mut(&entity).expect("evicted target remains cached").sent = Some(stamp);
                    }
                }
            }
        }
    }

    fn desired(&self, camera: Camera, object: &Object) -> Option<MeshKey> {
        let Geometry::Prim(shape) = object.geometry() else {
            return None;
        };
        let Placement::Ready(transform) = object.placement() else {
            return None;
        };
        let radius = 3.0 * transform.scale.as_dvec3().abs().max_element();
        let previous =
            self.targets.get(&object.detail().summary.entity).filter(|target| target.key.shape == shape).map(|target| target.key.detail);
        camera.detail(transform.pose.position, radius, previous).map(|detail| MeshKey { shape, detail })
    }
}

fn lower_detail(detail: Detail) -> Option<Detail> {
    match detail {
        Detail::Lowest => None,
        Detail::Low => Some(Detail::Lowest),
        Detail::Medium => Some(Detail::Low),
        Detail::High => Some(Detail::Medium),
    }
}

fn mesh_stamp(mesh: &PreparedMesh) -> Stamp {
    match mesh {
        PreparedMesh::Pending => Stamp::Pending,
        PreparedMesh::Ready(_) => Stamp::Ready,
        PreparedMesh::Failed(error) => Stamp::Failed(*error),
        PreparedMesh::Capacity => Stamp::Capacity,
    }
}

fn object_priority(camera: Camera, object: &Object) -> u32 {
    let Placement::Ready(transform) = object.placement() else { return 0 };
    camera.importance(transform.pose.position, 3.0 * transform.scale.as_dvec3().abs().max_element()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::{assert_matches, collections::BTreeMap, f64::consts::FRAC_PI_2, num::NonZeroU32, sync::atomic::AtomicBool};

    use math::{DVec3, Quat, Vec3};
    use uuid::Uuid;
    use world::{
        Appearance, ApplyOutcome, Entity, FieldUpdate, Kind, ObjectObservation, OrderEvidence, ParentUpdate, Pose, PrimShape, RegionInfo,
        RegionOutcome, Shape, UpdateSource, World, WorldLimits,
    };

    use super::{Completion, Detail, MeshCompletion, MeshPlanner};
    use crate::scene::{
        Camera, DrawnMeshKey, MeshBuildError, PreparedMesh,
        budget::Budget,
        mesh::retained_bytes,
        mesh_cache::{ENTRY_BYTES, MeshKey},
        model::{Change, MeshChange, Mirror, Object},
        prim::tessellate,
    };

    #[derive(Debug, PartialEq, Eq)]
    enum MeshStatus {
        Pending,
        Ready,
        Failed(MeshBuildError),
        Capacity,
    }

    fn status(state: &PreparedMesh) -> MeshStatus {
        match state {
            PreparedMesh::Pending => MeshStatus::Pending,
            PreparedMesh::Ready(_) => MeshStatus::Ready,
            PreparedMesh::Failed(error) => MeshStatus::Failed(*error),
            PreparedMesh::Capacity => MeshStatus::Capacity,
        }
    }

    fn mesh_state(object: &Object) -> Option<(MeshKey, MeshStatus)> {
        object.mesh_state().map(|(key, state)| (key, status(state)))
    }

    fn fixture(objects: &[(u128, Vec3, Vec3)], budget: &Budget) -> (Mirror, BTreeMap<Uuid, Entity>) {
        let capacity = objects.len().max(8);
        let journal_entries = capacity.saturating_mul(2).max(64);
        let mut world = World::new(WorldLimits {
            max_regions: 1,
            max_entities: capacity,
            max_entities_per_region: capacity,
            max_tombstones_per_region: capacity,
            max_tombstones: capacity,
            max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(capacity),
            max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(capacity),
            max_journal_entries: journal_entries,
            max_journal_changes: journal_entries.saturating_mul(2),
            max_region_name_bytes: 256,
            max_recovery_targets: capacity,
        });
        let region = world.register_region().expect("fixture region registers");
        assert_matches!(
            world
                .patch_region(region, RegionInfo { origin: Some(DVec3::ZERO), ..RegionInfo::default() })
                .expect("fixture origin applies")
                .outcome,
            RegionOutcome::Patched { .. }
        );
        for (index, (id, position, scale)) in objects.iter().enumerate() {
            let mut object = ObjectObservation::new(
                UpdateSource::new(region, index as u32 + 1, OrderEvidence::Ordered(index as u64 + 1), 0),
                Uuid::from_u128(*id),
                NonZeroU32::new(index as u32 + 1).expect("fixture local ID is nonzero"),
                Kind::Object,
                ParentUpdate::Root,
            );
            object.pose = Some(Pose { position: Some(*position), rotation: Some(Quat::IDENTITY) });
            object.scale = Some(*scale);
            object.shape = FieldUpdate::Set(Shape::Prim(PrimShape::default()));
            object.appearance = FieldUpdate::Set(Appearance::Surface);
            assert_matches!(world.observe_object(object).expect("fixture object applies").outcome, ApplyOutcome::Introduced(_));
        }
        let mut mirror = Mirror::default();
        let mut identities = BTreeMap::new();
        let mut cursor = None;
        loop {
            let page = world.object_list_page(cursor, 256).expect("fixture object page reads");
            let next = page.next;
            for item in page.objects {
                let detail = world.object_detail(item.entity).expect("fixture detail reads");
                let lease = budget.reserve(detail.owned_bytes() + 32).expect("fixture object fits budget");
                mirror.apply(Change::Object(Object::new(detail, lease)));
                identities.insert(item.id, item.entity);
            }
            cursor = next;
            if cursor.is_none() {
                break;
            }
        }
        (mirror, identities)
    }

    fn camera(x: f64, draw_distance: f64) -> Camera {
        Camera::new(DVec3::new(x, 0.0, 0.0), Vec3::X, FRAC_PI_2, [1000, 1000])
            .expect("fixture camera")
            .with_quality(draw_distance, 1.0)
            .expect("fixture camera quality")
    }

    fn cache_limit(key: MeshKey) -> usize {
        let mesh = tessellate(key.shape, key.detail, &AtomicBool::new(false)).expect("fixture mesh tessellates");
        ENTRY_BYTES + retained_bytes(&mesh)
    }

    fn apply_changes(mirror: &mut Mirror, changes: &[MeshChange]) {
        for change in changes {
            mirror.apply_mesh(change);
        }
    }

    fn complete_mesh(planner: &mut MeshPlanner) {
        let MeshCompletion(completion) = planner.start().expect("admitted mesh starts").run();
        let Completion::Prim(completion) = completion else {
            panic!("planner only starts prim work");
        };
        planner.complete(completion);
    }

    #[test]
    fn capacity_retry_publishes_ready() {
        let budget = Budget::default();
        let high = MeshKey { shape: PrimShape::default(), detail: Detail::High };
        let low = MeshKey { shape: PrimShape::default(), detail: Detail::Low };
        let mut planner = MeshPlanner::new_with_cache_bytes(budget.clone(), cache_limit(high));
        let (mut mirror, mut identities) =
            fixture(&[(1, Vec3::new(100.0, 0.0, 0.0), Vec3::splat(40.0)), (2, Vec3::new(100.0, 0.0, 0.0), Vec3::ONE)], &budget);
        let blocker = identities[&Uuid::from_u128(1)];
        let target = identities[&Uuid::from_u128(2)];
        identities.remove(&Uuid::from_u128(2));
        planner.set_camera(camera(0.0, 512.0));
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);
        complete_mesh(&mut planner);
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);
        assert_matches!(mesh_state(&mirror.objects[&blocker]), Some((_, MeshStatus::Ready)));

        identities.insert(Uuid::from_u128(2), target);
        planner.invalidate();
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);
        assert_matches!(mesh_state(&mirror.objects[&target]), Some((key, MeshStatus::Capacity)) if key == low);
        assert!(!planner.retry_admission());
        assert!(!planner.has_work());

        planner.remove(blocker);
        assert!(planner.retry_admission());
        assert!(planner.has_work());
        mirror.objects.remove(&blocker);
        identities.remove(&Uuid::from_u128(1));
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);
        assert!(planner.targets[&target].admitted, "an unchanged desired key retries after capacity is freed");

        complete_mesh(&mut planner);
        let changes = planner.step(&mirror, &identities);
        let published_ready = changes.iter().find_map(|change| match change {
            MeshChange::Set(entity, key, state) if *entity == target && *key == low => Some(state),
            _ => None,
        });
        assert_matches!(published_ready.map(status), Some(MeshStatus::Ready));
        apply_changes(&mut mirror, &changes);
        assert_eq!(mirror.objects[&target].mesh().map(|(key, _)| key), Some(DrawnMeshKey::Prim(low)));
    }

    #[test]
    fn rejected_detail_keeps_mesh() {
        let budget = Budget::default();
        let high = MeshKey { shape: PrimShape::default(), detail: Detail::High };
        let mut planner = MeshPlanner::new_with_cache_bytes(budget.clone(), cache_limit(high));
        let (mut mirror, identities) =
            fixture(&[(1, Vec3::new(0.0, 100.0, 0.0), Vec3::splat(13.0)), (2, Vec3::new(0.0, 100.0, 0.0), Vec3::splat(13.0))], &budget);
        let target = identities[&Uuid::from_u128(1)];
        planner.set_camera(camera(0.0, 1024.0));
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);
        complete_mesh(&mut planner);
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);
        assert_matches!(mesh_state(&mirror.objects[&target]), Some((key, MeshStatus::Ready)) if key == high);

        planner.set_camera(camera(300.0, 1024.0));
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);

        assert_eq!(planner.targets[&target].key, high);
        assert!(!planner.retry_admission(), "a rejected detail switch waits for capacity to free");
        assert_eq!(mirror.objects[&target].mesh().map(|(key, _)| key), Some(DrawnMeshKey::Prim(high)));
        assert_matches!(mesh_state(&mirror.objects[&target]), Some((key, MeshStatus::Ready)) if key == high);
    }

    #[test]
    fn rejected_request_admitted_later() {
        let budget = Budget::default();
        let high = MeshKey { shape: PrimShape::default(), detail: Detail::High };
        let low = MeshKey { shape: PrimShape::default(), detail: Detail::Low };
        let mut planner = MeshPlanner::new_with_cache_bytes(budget.clone(), cache_limit(high));
        let (mut mirror, mut identities) =
            fixture(&[(1, Vec3::new(100.0, 0.0, 0.0), Vec3::splat(40.0)), (2, Vec3::new(100.0, 0.0, 0.0), Vec3::ONE)], &budget);
        let blocker = identities[&Uuid::from_u128(1)];
        let target = identities[&Uuid::from_u128(2)];
        identities.remove(&Uuid::from_u128(2));
        planner.set_camera(camera(0.0, 512.0));
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);
        complete_mesh(&mut planner);
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);

        identities.insert(Uuid::from_u128(2), target);
        planner.invalidate();
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);
        assert!(!planner.targets[&target].admitted);

        planner.remove(blocker);
        mirror.objects.remove(&blocker);
        identities.remove(&Uuid::from_u128(1));
        assert!(planner.has_work());
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);
        assert!(planner.targets[&target].admitted);

        complete_mesh(&mut planner);
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);
        assert_matches!(mesh_state(&mirror.objects[&target]), Some((key, MeshStatus::Ready)) if key == low);
    }

    #[test]
    fn rejected_request_skips_work() {
        let budget = Budget::default();
        let high = MeshKey { shape: PrimShape::default(), detail: Detail::High };
        let mut planner = MeshPlanner::new_with_cache_bytes(budget.clone(), cache_limit(high));
        let (mut mirror, mut identities) =
            fixture(&[(1, Vec3::new(100.0, 0.0, 0.0), Vec3::splat(40.0)), (2, Vec3::new(100.0, 0.0, 0.0), Vec3::ONE)], &budget);
        let target = identities[&Uuid::from_u128(2)];
        identities.remove(&Uuid::from_u128(2));
        planner.set_camera(camera(0.0, 512.0));
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);
        complete_mesh(&mut planner);
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);

        identities.insert(Uuid::from_u128(2), target);
        planner.invalidate();
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);
        assert!(!planner.targets[&target].admitted);
        assert!(!planner.has_work());
        assert!(!planner.retry_admission());
        assert!(!planner.waiting());
        assert!(planner.step(&mirror, &identities).is_empty());
    }

    #[test]
    fn irrelevant_rejection_dropped() {
        let budget = Budget::default();
        let high = MeshKey { shape: PrimShape::default(), detail: Detail::High };
        let mut planner = MeshPlanner::new_with_cache_bytes(budget.clone(), cache_limit(high));
        let (mut mirror, mut identities) =
            fixture(&[(1, Vec3::new(100.0, 0.0, 0.0), Vec3::splat(40.0)), (2, Vec3::new(100.0, 0.0, 0.0), Vec3::ONE)], &budget);
        let blocker = identities[&Uuid::from_u128(1)];
        let target = identities[&Uuid::from_u128(2)];
        identities.remove(&Uuid::from_u128(2));
        planner.set_camera(camera(0.0, 512.0));
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);
        complete_mesh(&mut planner);
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);

        identities.insert(Uuid::from_u128(2), target);
        planner.invalidate();
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);
        assert!(!planner.targets[&target].admitted);

        mirror.objects.remove(&target);
        identities.remove(&Uuid::from_u128(2));
        planner.remove(blocker);
        mirror.objects.remove(&blocker);
        identities.remove(&Uuid::from_u128(1));
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);
        assert!(planner.start().is_none());
        assert!(!planner.targets.contains_key(&target));
        assert!(!planner.retry_admission());
    }

    #[test]
    fn rejected_lod_retries_later() {
        let budget = Budget::default();
        let low = MeshKey { shape: PrimShape::default(), detail: Detail::Low };
        let medium = MeshKey { shape: PrimShape::default(), detail: Detail::Medium };
        let high = MeshKey { shape: PrimShape::default(), detail: Detail::High };
        let mut planner = MeshPlanner::new_with_cache_bytes(budget.clone(), cache_limit(medium) + cache_limit(high));
        let (mut mirror, mut identities) =
            fixture(&[(1, Vec3::new(0.0, 100.0, 0.0), Vec3::splat(13.0)), (2, Vec3::new(0.0, 100.0, 0.0), Vec3::splat(80.0))], &budget);
        let target = identities[&Uuid::from_u128(1)];
        let blocker = identities[&Uuid::from_u128(2)];
        planner.set_camera(camera(-300.0, 1024.0));
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);
        complete_mesh(&mut planner);
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);
        complete_mesh(&mut planner);
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);
        assert_matches!(mesh_state(&mirror.objects[&target]), Some((key, MeshStatus::Ready)) if key == medium);
        assert_matches!(mesh_state(&mirror.objects[&blocker]), Some((key, MeshStatus::Ready)) if key == high);

        planner.set_camera(camera(-600.0, 1024.0));
        while planner.has_work() {
            let changes = planner.step(&mirror, &identities);
            apply_changes(&mut mirror, &changes);
        }
        assert_eq!(planner.targets[&target].key, medium);
        assert_eq!(mirror.objects[&target].mesh().map(|(key, _)| key), Some(DrawnMeshKey::Prim(medium)));
        assert!(!planner.retry_admission());

        planner.remove(blocker);
        mirror.objects.remove(&blocker);
        identities.remove(&Uuid::from_u128(2));
        assert!(planner.retry_admission());
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);
        assert_eq!(planner.targets[&target].key, low);
        assert!(planner.targets[&target].admitted);
        complete_mesh(&mut planner);
        let changes = planner.step(&mirror, &identities);
        apply_changes(&mut mirror, &changes);
        assert_matches!(mesh_state(&mirror.objects[&target]), Some((key, MeshStatus::Ready)) if key == low);
    }

    #[test]
    fn camera_change_restarts_partial_scan() {
        let budget = Budget::default();
        let objects = (1..=260).map(|id| (id, Vec3::new(0.0, 100.0, 0.0), Vec3::splat(13.0))).collect::<Vec<_>>();
        let (mirror, identities) = fixture(&objects, &budget);
        let target = identities[&Uuid::from_u128(1)];
        let mut planner = MeshPlanner::new(budget);

        planner.set_camera(camera(0.0, 1024.0));
        planner.step(&mirror, &identities);
        assert_eq!(planner.targets[&target].key.detail, Detail::High);

        planner.set_camera(camera(-300.0, 1024.0));
        planner.step(&mirror, &identities);
        planner.step(&mirror, &identities);
        assert_eq!(planner.targets[&target].key.detail, Detail::High);
        assert!(planner.has_work(), "the camera change leaves a full rescan pending");

        planner.step(&mirror, &identities);
        assert_eq!(planner.targets[&target].key.detail, Detail::Medium);
    }
}
