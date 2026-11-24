use std::{
    collections::Bound::{Excluded, Unbounded},
    error::Error,
    fmt::{self, Display, Formatter},
    mem::{self},
};

use uuid::Uuid;

use crate::{
    Appearance, AttachmentPoint, AvatarAppearance, Entity, Kind, LinearMotion, LocalAddress, Object, ObjectMedia, Parent,
    PbrMaterialAssignments, PbrMaterialOverrides, PoseError, PoseState, Region, RegionInfo, Shape, SitTarget, SurfaceAppearance,
    TerrainSettings, TextureAnimation, Transform, UpdateSource, World, WorldChange, WorldId, WorldRevision, WorldTransformState,
    removal::{VisibilityCache, VisibilityCursor, VisibilityLookupCache, VisibilityStep},
    update::Stamp,
};

pub const MAX_OBJECT_PAGE_ITEMS: usize = 256;
pub const MAX_OBJECT_PAGE_WORK_ITEMS: usize = 256;
pub const MAX_OBJECT_LOOKUP_WORK_ITEMS: usize = 256;
pub const MAX_REGION_PAGE_ITEMS: usize = 64;
pub const MAX_REGION_PAGE_BYTES: usize = 1024 * 1024;
pub const MAX_WORLD_CHANGE_PAGE_ITEMS: usize = 256;
pub const MAX_OBJECT_DETAIL_BYTES: usize = 64 * 1024;
pub const MAX_OBJECT_MEDIA_READ_BYTES: usize = 256 * 1024;
pub const MAX_OBJECT_DETAIL_BATCH_ITEMS: usize = 8;
pub const MAX_OBJECT_DETAIL_WORK_ITEMS: usize = 256;
pub const MAX_OBJECT_DETAIL_BATCH_BYTES: usize = MAX_OBJECT_DETAIL_BATCH_ITEMS * MAX_OBJECT_DETAIL_BYTES
    + MAX_OBJECT_DETAIL_BATCH_ITEMS * size_of::<ObjectDetailResult>()
    + size_of::<ObjectDetailBatchPage>();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorldReadBarrier {
    pub revision: WorldRevision,
    pub object_count_upper_bound: usize,
    pub region_count: usize,
    pub changes: WorldChangeCursor,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ObjectSummary {
    pub entity: Entity,
    pub id: Uuid,
    pub binding_fence: ObjectBindingFence,
    pub kind: Kind,
    pub address: LocalAddress,
    pub parent: Parent,
    pub local_transform: Transform,
    pub pose: Result<PoseState, PoseError>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObjectBindingFence {
    order: u64,
    block_ordinal: u32,
}

impl ObjectBindingFence {
    pub const fn order(self) -> u64 {
        self.order
    }

    pub const fn block_ordinal(self) -> u32 {
        self.block_ordinal
    }
}

impl From<Stamp> for ObjectBindingFence {
    fn from(stamp: Stamp) -> Self {
        Self { order: stamp.order, block_ordinal: stamp.block_ordinal }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObjectListItem {
    pub entity: Entity,
    pub id: Uuid,
    pub kind: Kind,
    pub address: LocalAddress,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectListPage {
    pub hydration_revision: WorldRevision,
    pub objects: Vec<ObjectListItem>,
    pub work_units: usize,
    pub next: Option<ObjectPageCursor>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObjectPageCursor {
    world: WorldId,
    revision: WorldRevision,
    after: Option<Uuid>,
    walk: Option<VisibilityCursor>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RegionSummary {
    pub region: Region,
    pub info: RegionInfo,
    pub terrain_settings: Option<TerrainSettings>,
    pub object_count: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RegionPage {
    pub hydration_revision: WorldRevision,
    pub regions: Vec<RegionSummary>,
    pub next: Option<RegionPageCursor>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegionPageCursor {
    world: WorldId,
    after: Region,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ObjectDetail {
    pub summary: ObjectSummary,
    pub hydration_revision: WorldRevision,
    pub world_transform: Result<WorldTransformState, PoseError>,
    pub parent_id: Option<Uuid>,
    pub linear_motion: LinearMotion,
    pub angular_velocity: Option<math::Vec3>,
    pub is_physical: bool,
    pub time_dilation: f32,
    pub motion_source: Option<UpdateSource>,
    pub shape: Option<Shape>,
    pub appearance: Option<Appearance>,
    pub surface_appearance: Option<SurfaceAppearance>,
    pub texture_animation: Option<TextureAnimation>,
    pub pbr_materials: Option<PbrMaterialAssignments>,
    pub pbr_material_overrides: Option<PbrMaterialOverrides>,
    pub avatar_appearance: Option<AvatarAppearance>,
    pub attachment_point: Option<AttachmentPoint>,
    pub sit_target: Option<SitTarget>,
}

impl ObjectDetail {
    pub fn owned_bytes(&self) -> usize {
        detail_bytes(
            self.surface_appearance.as_ref(),
            self.pbr_materials.as_ref(),
            self.pbr_material_overrides.as_ref(),
            self.avatar_appearance.as_ref(),
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectMediaRead {
    pub entity: Entity,
    pub hydration_revision: WorldRevision,
    pub media: Option<ObjectMedia>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ObjectDetailResult {
    Found(ObjectDetail),
    Gone(Entity),
    TooLarge(Entity),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ObjectDetailBatch {
    pub hydration_revision: WorldRevision,
    pub details: Vec<ObjectDetailResult>,
    pub has_more: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObjectDetailCursor {
    world: WorldId,
    entity: Entity,
    revision: WorldRevision,
    walk: Option<VisibilityCursor>,
}

impl ObjectDetailCursor {
    pub const fn entity(self) -> Entity {
        self.entity
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ObjectDetailBatchPage {
    pub batch: ObjectDetailBatch,
    pub work_units: usize,
    pub next: Option<ObjectDetailCursor>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorldChangeCursor {
    world: WorldId,
    through_revision: WorldRevision,
    next_change: Option<usize>,
}

impl WorldChangeCursor {
    pub fn encode(self) -> String {
        match self.next_change {
            Some(next) => format!("{}:{}:{next}", self.world.raw(), self.through_revision.0),
            None => format!("{}:{}", self.world.raw(), self.through_revision.0),
        }
    }

    pub fn decode(token: &str) -> Option<Self> {
        let mut parts = token.split(':');
        let world = parts.next()?.parse::<u64>().ok()?;
        let through_revision = parts.next()?.parse::<u64>().ok()?;
        let next_change = match parts.next() {
            Some(raw) => Some(raw.parse::<usize>().ok()?),
            None => None,
        };
        if parts.next().is_some() || (next_change.is_some() && through_revision == 0) {
            return None;
        }
        Some(Self { world: WorldId::from_raw(world), through_revision: WorldRevision(through_revision), next_change })
    }

    fn completed_revision(self) -> WorldRevision {
        match self.next_change {
            Some(_) => WorldRevision(self.through_revision.0.checked_sub(1).expect("a partial revision is nonzero")),
            None => self.through_revision,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorldChangeRecord {
    pub revision: WorldRevision,
    pub change: WorldChange,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorldChangePage {
    pub observed_revision: WorldRevision,
    pub completed_revision: WorldRevision,
    pub records: Vec<WorldChangeRecord>,
    pub cursor: WorldChangeCursor,
    pub has_more: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorldReadError {
    StaleSession,
    StaleIdentity,
    ResyncRequired,
    InvalidCursor,
    InvalidLimit,
    TooLarge,
}

impl Display for WorldReadError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleSession => f.write_str("cursor or entity belongs to another World session"),
            Self::StaleIdentity => f.write_str("object incarnation is no longer live"),
            Self::ResyncRequired => f.write_str("journal history was evicted; rebuild from a fresh read barrier"),
            Self::InvalidCursor => f.write_str("invalid World read cursor"),
            Self::InvalidLimit => f.write_str("read limit is zero or exceeds the operation maximum"),
            Self::TooLarge => f.write_str("read result exceeds the byte limit"),
        }
    }
}

impl Error for WorldReadError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObjectLookupTarget {
    Id(Uuid),
    Address(LocalAddress),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObjectLookupCursor {
    world: WorldId,
    revision: WorldRevision,
    target: ObjectLookupTarget,
    entity: Entity,
    id: Uuid,
    walk: VisibilityCursor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectLookupProgress {
    Visible { entity: Entity, work_units: usize },
    Hidden { work_units: usize },
    Pending { cursor: ObjectLookupCursor, work_units: usize },
}

impl World {
    pub fn lookup_entity(&mut self, id: Uuid, cursor: Option<ObjectLookupCursor>) -> ObjectLookupProgress {
        self.lookup_object(ObjectLookupTarget::Id(id), cursor, MAX_OBJECT_LOOKUP_WORK_ITEMS)
    }

    pub fn lookup_entity_at(&mut self, address: LocalAddress, cursor: Option<ObjectLookupCursor>) -> ObjectLookupProgress {
        self.lookup_object(ObjectLookupTarget::Address(address), cursor, MAX_OBJECT_LOOKUP_WORK_ITEMS)
    }

    pub fn lookup_entity_with_budget(&mut self, id: Uuid, cursor: Option<ObjectLookupCursor>, work_limit: usize) -> ObjectLookupProgress {
        self.lookup_object(ObjectLookupTarget::Id(id), cursor, work_limit)
    }

    pub fn lookup_entity_at_with_budget(
        &mut self,
        address: LocalAddress,
        cursor: Option<ObjectLookupCursor>,
        work_limit: usize,
    ) -> ObjectLookupProgress {
        self.lookup_object(ObjectLookupTarget::Address(address), cursor, work_limit)
    }

    fn lookup_object(&mut self, target: ObjectLookupTarget, cursor: Option<ObjectLookupCursor>, work_limit: usize) -> ObjectLookupProgress {
        let revision = self.revision();
        let current = self.lookup_target(target);
        let Some(entity) = current else { return ObjectLookupProgress::Hidden { work_units: 0 } };
        let Some(record) = self.store.object_record(entity) else { return ObjectLookupProgress::Hidden { work_units: 0 } };
        let resumed = cursor.filter(|cursor| {
            cursor.world == self.id
                && cursor.revision == revision
                && cursor.target == target
                && cursor.entity == entity
                && cursor.id == record.id
        });
        if let Some(visibility) = self.lookup_visibility.get(entity, revision) {
            return if visibility.visible {
                ObjectLookupProgress::Visible { entity, work_units: 0 }
            } else {
                ObjectLookupProgress::Hidden { work_units: 0 }
            };
        }
        let mut walk = resumed.map_or_else(|| self.begin_visibility(entity, record.id), |cursor| cursor.walk);
        let mut visibility = VisibilityCache::default();
        let mut work_units = 0;
        for _ in 0..work_limit.min(MAX_OBJECT_LOOKUP_WORK_ITEMS) {
            work_units += 1;
            match self.advance_visibility_with_lookup_cache(&mut walk, &mut visibility, &self.lookup_visibility) {
                VisibilityStep::Pending => {}
                VisibilityStep::Complete(result) => {
                    self.lookup_visibility.insert(entity, revision, result);
                    return if result.visible {
                        ObjectLookupProgress::Visible { entity, work_units }
                    } else {
                        ObjectLookupProgress::Hidden { work_units }
                    };
                }
            }
        }
        ObjectLookupProgress::Pending {
            cursor: ObjectLookupCursor { world: self.id, revision, target, entity, id: record.id, walk },
            work_units,
        }
    }

    fn lookup_target(&self, target: ObjectLookupTarget) -> Option<Entity> {
        match target {
            ObjectLookupTarget::Id(id) => self.store.entity(id),
            ObjectLookupTarget::Address(address) => self.store.entity_at(address),
        }
    }

    pub fn read_barrier(&self) -> WorldReadBarrier {
        let revision = self.revision();
        WorldReadBarrier {
            revision,
            object_count_upper_bound: self.visible_entity_upper_bound(),
            region_count: self.regions.len(),
            changes: WorldChangeCursor { world: self.id, through_revision: revision, next_change: None },
        }
    }

    pub fn object_list_page(&self, cursor: Option<ObjectPageCursor>, limit: usize) -> Result<ObjectListPage, WorldReadError> {
        self.object_list_page_inner(cursor, limit, None)
    }

    pub fn object_list_page_with_cache(
        &mut self,
        cursor: Option<ObjectPageCursor>,
        limit: usize,
    ) -> Result<ObjectListPage, WorldReadError> {
        let mut visibility_lookup = mem::take(&mut self.lookup_visibility);
        let page = self.object_list_page_inner(cursor, limit, Some(&mut visibility_lookup));
        self.lookup_visibility = visibility_lookup;
        page
    }

    fn object_list_page_inner(
        &self,
        cursor: Option<ObjectPageCursor>,
        limit: usize,
        mut visibility_lookup: Option<&mut VisibilityLookupCache>,
    ) -> Result<ObjectListPage, WorldReadError> {
        if limit == 0 || limit > MAX_OBJECT_PAGE_ITEMS {
            return Err(WorldReadError::InvalidLimit);
        }
        if cursor.is_some_and(|cursor| cursor.world != self.id) {
            return Err(WorldReadError::StaleSession);
        }

        let revision = self.revision();
        let mut after = cursor.and_then(|cursor| cursor.after);
        let mut walk = cursor.filter(|cursor| cursor.revision == revision).and_then(|cursor| cursor.walk);
        let mut visibility = VisibilityCache::default();
        let mut objects = Vec::with_capacity(limit);
        let mut work = 0;
        while objects.len() < limit && work < MAX_OBJECT_PAGE_WORK_ITEMS {
            if walk.is_none() {
                let lower = after.map_or(Unbounded, Excluded);
                let Some(id) = self.store.ids().range((lower, Unbounded)).next().copied() else { break };
                let Some(entity) = self.store.entity(id) else {
                    after = Some(id);
                    work += 1;
                    continue;
                };
                walk = Some(self.begin_visibility(entity, id));
            }

            let mut cursor = walk.expect("visibility walk was just initialized");
            work += 1;
            let step = if let Some(lookup) = visibility_lookup.as_deref() {
                self.advance_visibility_with_lookup_cache(&mut cursor, &mut visibility, lookup)
            } else {
                self.advance_visibility(&mut cursor, &mut visibility)
            };
            match step {
                VisibilityStep::Pending => walk = Some(cursor),
                VisibilityStep::Complete(result) => {
                    after = Some(cursor.id());
                    walk = None;
                    if let Some(lookup) = visibility_lookup.as_deref_mut() {
                        lookup.insert(cursor.entity(), revision, result);
                    }
                    if result.visible {
                        let record = self.store.object_record(cursor.entity()).expect("visible identity has a stored record");
                        objects.push(ObjectListItem {
                            entity: cursor.entity(),
                            id: record.id,
                            kind: record.kind,
                            address: record.state.binding.address,
                        });
                    }
                }
            }
        }
        let lower = after.map_or(Unbounded, Excluded);
        let has_more = walk.is_some() || self.store.ids().range((lower, Unbounded)).next().is_some();
        let next = has_more.then_some(ObjectPageCursor { world: self.id, revision, after, walk });
        Ok(ObjectListPage { hydration_revision: revision, objects, work_units: work, next })
    }

    pub fn region_page(&self, cursor: Option<RegionPageCursor>, limit: usize) -> Result<RegionPage, WorldReadError> {
        if limit == 0 || limit > MAX_REGION_PAGE_ITEMS {
            return Err(WorldReadError::InvalidLimit);
        }
        if cursor.is_some_and(|cursor| cursor.world != self.id) {
            return Err(WorldReadError::StaleSession);
        }

        let hydration_revision = self.revision();
        let mut regions = Vec::with_capacity(limit);
        let mut after = cursor.map(|cursor| cursor.after);
        let mut iter = match after {
            Some(region) => self.regions.range((Excluded(region), Unbounded)),
            None => self.regions.range(..),
        };
        let mut bytes = 0usize;
        let mut oversized_item = false;
        for (region, state) in iter.by_ref().take(limit) {
            let info = &state.info;
            let charge = region_summary_bytes(info);
            if bytes.saturating_add(charge) > MAX_REGION_PAGE_BYTES {
                if regions.is_empty() {
                    return Err(WorldReadError::TooLarge);
                }
                oversized_item = true;
                break;
            }
            bytes = bytes.saturating_add(charge);
            after = Some(*region);
            regions.push(RegionSummary {
                region: *region,
                info: info.clone(),
                terrain_settings: state.terrain.settings(),
                object_count: self.region_object_count_upper_bound(*region),
            });
        }
        let has_more = oversized_item || iter.next().is_some();
        let next = has_more.then(|| RegionPageCursor { world: self.id, after: after.expect("nonzero page limit") });
        Ok(RegionPage { hydration_revision, regions, next })
    }

    pub fn object_detail(&self, entity: Entity) -> Result<ObjectDetail, WorldReadError> {
        self.object_detail_at(entity, self.revision())
    }

    pub fn object_detail_batch(&self, entities: &[Entity]) -> Result<ObjectDetailBatch, WorldReadError> {
        if entities.is_empty() || entities.len() > MAX_OBJECT_DETAIL_BATCH_ITEMS {
            return Err(WorldReadError::InvalidLimit);
        }
        if entities.iter().any(|entity| entity.1 != self.id) {
            return Err(WorldReadError::StaleSession);
        }

        let hydration_revision = self.revision();
        let mut details = Vec::with_capacity(entities.len());
        for entity in entities {
            let detail = match self.object_detail_at(*entity, hydration_revision) {
                Ok(detail) => ObjectDetailResult::Found(detail),
                Err(WorldReadError::StaleIdentity) => ObjectDetailResult::Gone(*entity),
                Err(WorldReadError::TooLarge) => ObjectDetailResult::TooLarge(*entity),
                Err(error) => return Err(error),
            };
            details.push(detail);
        }

        Ok(ObjectDetailBatch { hydration_revision, details, has_more: false })
    }

    pub fn object_detail_batch_page(
        &self,
        entities: &[Entity],
        cursor: Option<ObjectDetailCursor>,
    ) -> Result<ObjectDetailBatchPage, WorldReadError> {
        if entities.is_empty() || entities.len() > MAX_OBJECT_DETAIL_BATCH_ITEMS {
            return Err(WorldReadError::InvalidLimit);
        }
        if entities.iter().any(|entity| entity.1 != self.id) {
            return Err(WorldReadError::StaleSession);
        }
        if cursor.is_some_and(|cursor| cursor.world != self.id) {
            return Err(WorldReadError::StaleSession);
        }
        let hydration_revision = self.revision();
        let start = match cursor {
            Some(cursor) => entities.iter().position(|entity| *entity == cursor.entity).ok_or(WorldReadError::InvalidCursor)?,
            None => 0,
        };
        let mut visibility = VisibilityCache::default();
        let mut details = Vec::with_capacity(entities.len());
        let mut work = 0;
        let mut next = None;
        for (index, entity) in entities.iter().copied().enumerate().skip(start) {
            if work == MAX_OBJECT_DETAIL_WORK_ITEMS {
                next = Some(ObjectDetailCursor { world: self.id, entity, revision: hydration_revision, walk: None });
                break;
            }
            let Some(record) = self.store.object_record(entity).filter(|_| self.store.is_current_entity(entity)) else {
                work += 1;
                details.push(ObjectDetailResult::Gone(entity));
                continue;
            };
            let mut walk = if index == start {
                cursor.filter(|cursor| cursor.revision == hydration_revision).and_then(|cursor| cursor.walk)
            } else {
                None
            };
            walk.get_or_insert_with(|| self.begin_visibility(entity, record.id));
            let mut completed = None;
            while work < MAX_OBJECT_DETAIL_WORK_ITEMS {
                work += 1;
                let current = walk.as_mut().expect("detail visibility walk was initialized");
                match self.advance_visibility(current, &mut visibility) {
                    VisibilityStep::Pending => {}
                    VisibilityStep::Complete(result) => {
                        completed = Some(result.visible);
                        break;
                    }
                }
            }
            if let Some(visible) = completed {
                let detail = if visible {
                    let parent_id = match record.state.parent {
                        Parent::Resolved(parent) => self.store.object_record(parent).map(|parent| parent.id),
                        Parent::Unknown | Parent::Root | Parent::Pending { .. } => None,
                    };
                    match self.object_detail_from_object(Object::new(entity, record, self), hydration_revision, parent_id) {
                        Ok(detail) => ObjectDetailResult::Found(detail),
                        Err(WorldReadError::StaleIdentity) => ObjectDetailResult::Gone(entity),
                        Err(WorldReadError::TooLarge) => ObjectDetailResult::TooLarge(entity),
                        Err(error) => return Err(error),
                    }
                } else {
                    ObjectDetailResult::Gone(entity)
                };
                details.push(detail);
            } else {
                next = Some(ObjectDetailCursor { world: self.id, entity, revision: hydration_revision, walk });
                break;
            }
            if work == MAX_OBJECT_DETAIL_WORK_ITEMS
                && let Some(next_entity) = entities.get(index + 1).copied()
            {
                next = Some(ObjectDetailCursor { world: self.id, entity: next_entity, revision: hydration_revision, walk: None });
                break;
            }
        }

        Ok(ObjectDetailBatchPage {
            batch: ObjectDetailBatch { hydration_revision, details, has_more: next.is_some() },
            work_units: work,
            next,
        })
    }

    pub fn object_media(&self, entity: Entity) -> Result<ObjectMediaRead, WorldReadError> {
        if entity.1 != self.id {
            return Err(WorldReadError::StaleSession);
        }
        let object = self.object(entity).ok_or(WorldReadError::StaleIdentity)?;
        let media = object.object_media();
        let bytes = size_of::<ObjectMediaRead>()
            .saturating_add(media.map_or(0, |media| media.retained_bytes().saturating_sub(size_of::<ObjectMedia>())));
        if bytes > MAX_OBJECT_MEDIA_READ_BYTES {
            return Err(WorldReadError::TooLarge);
        }
        Ok(ObjectMediaRead { entity, hydration_revision: self.revision(), media: media.cloned() })
    }

    fn object_detail_at(&self, entity: Entity, hydration_revision: WorldRevision) -> Result<ObjectDetail, WorldReadError> {
        if entity.1 != self.id {
            return Err(WorldReadError::StaleSession);
        }
        let object = self.object(entity).ok_or(WorldReadError::StaleIdentity)?;
        let parent_id = match object.parent() {
            Parent::Resolved(parent) => self.object(parent).map(|parent| parent.id()),
            Parent::Unknown | Parent::Root | Parent::Pending { .. } => None,
        };
        self.object_detail_from_object(object, hydration_revision, parent_id)
    }

    fn object_detail_from_object(
        &self,
        object: Object<'_>,
        hydration_revision: WorldRevision,
        parent_id: Option<Uuid>,
    ) -> Result<ObjectDetail, WorldReadError> {
        let entity = object.entity();
        let world_transform = object.world_transform();
        let pose = match world_transform {
            Ok(WorldTransformState::Resolved(transform)) => Ok(PoseState::Resolved(transform.pose)),
            Ok(WorldTransformState::Provisional { pose, reason }) => Ok(PoseState::Provisional { pose, reason }),
            Ok(WorldTransformState::Unresolved(reason)) => Ok(PoseState::Unresolved(reason)),
            Ok(WorldTransformState::MissingScale(pose)) => Ok(PoseState::Resolved(pose)),
            Err(error) => Err(error),
        };
        let summary = ObjectSummary {
            entity,
            id: object.id(),
            binding_fence: object.record().state.binding.fence().into(),
            kind: object.kind(),
            address: object.address(),
            parent: object.parent(),
            local_transform: object.transform(),
            pose,
        };
        if object_detail_bytes(&object) > MAX_OBJECT_DETAIL_BYTES {
            return Err(WorldReadError::TooLarge);
        }
        let detail = ObjectDetail {
            summary,
            hydration_revision,
            world_transform,
            parent_id,
            linear_motion: object.linear_motion(),
            angular_velocity: object.angular_velocity(),
            is_physical: object.is_physical(),
            time_dilation: object.time_dilation(),
            motion_source: object.motion_source(),
            shape: object.shape(),
            appearance: object.appearance(),
            surface_appearance: object.surface_appearance().cloned(),
            texture_animation: object.texture_animation(),
            pbr_materials: object.pbr_materials().cloned(),
            pbr_material_overrides: object.pbr_material_overrides().cloned(),
            avatar_appearance: object.avatar_appearance().cloned(),
            attachment_point: object.attachment_point(),
            sit_target: object.sit_target(),
        };
        Ok(detail)
    }

    pub fn change_page(&self, cursor: WorldChangeCursor, limit: usize) -> Result<WorldChangePage, WorldReadError> {
        if limit == 0 || limit > MAX_WORLD_CHANGE_PAGE_ITEMS {
            return Err(WorldReadError::InvalidLimit);
        }
        if cursor.world != self.id {
            return Err(WorldReadError::StaleSession);
        }
        if cursor.through_revision > self.revision() {
            return Err(WorldReadError::InvalidCursor);
        }
        if let Some(lost_through) = self.journal.lost_through {
            let loss_affects_cursor = match cursor.next_change {
                Some(_) => lost_through >= cursor.through_revision,
                None => cursor.through_revision < lost_through,
            };
            if loss_affects_cursor {
                return Err(WorldReadError::ResyncRequired);
            }
        }

        let (mut entry_index, mut change_index) = if let Some(next_change) = cursor.next_change {
            let entry_index = self.journal.entry_index(cursor.through_revision);
            let Some(entry) = self.journal.entries.get(entry_index).filter(|entry| entry.revision == cursor.through_revision) else {
                return Err(WorldReadError::ResyncRequired);
            };
            if next_change >= entry.changes.len() {
                return Err(WorldReadError::InvalidCursor);
            }
            (entry_index, next_change)
        } else {
            let entry_index = self.journal.entry_index(cursor.through_revision);
            let entry_index = if self.journal.entries.get(entry_index).is_some_and(|entry| entry.revision == cursor.through_revision) {
                entry_index + 1
            } else {
                entry_index
            };
            (entry_index, 0)
        };

        let mut records = Vec::with_capacity(limit);
        let mut next_cursor = cursor;
        while let Some(entry) = self.journal.entries.get(entry_index) {
            for (index, change) in entry.changes.iter().copied().enumerate().skip(change_index) {
                records.push(WorldChangeRecord { revision: entry.revision, change });
                let next_index = index + 1;
                if records.len() == limit {
                    let has_more_in_entry = next_index < entry.changes.len();
                    let has_more = has_more_in_entry || entry_index + 1 < self.journal.entries.len();
                    next_cursor = if has_more_in_entry {
                        WorldChangeCursor { world: self.id, through_revision: entry.revision, next_change: Some(next_index) }
                    } else {
                        WorldChangeCursor { world: self.id, through_revision: entry.revision, next_change: None }
                    };
                    return Ok(WorldChangePage {
                        observed_revision: self.revision(),
                        completed_revision: next_cursor.completed_revision(),
                        records,
                        cursor: next_cursor,
                        has_more,
                    });
                }
            }
            next_cursor = WorldChangeCursor { world: self.id, through_revision: entry.revision, next_change: None };
            entry_index += 1;
            change_index = 0;
        }

        Ok(WorldChangePage {
            observed_revision: self.revision(),
            completed_revision: next_cursor.completed_revision(),
            records,
            cursor: next_cursor,
            has_more: false,
        })
    }
}

fn region_summary_bytes(info: &RegionInfo) -> usize {
    size_of::<RegionSummary>().saturating_add(info.name.as_ref().map_or(0, String::len))
}

fn object_detail_bytes(object: &Object<'_>) -> usize {
    detail_bytes(object.surface_appearance(), object.pbr_materials(), object.pbr_material_overrides(), object.avatar_appearance())
}

fn detail_bytes(
    surface_appearance: Option<&SurfaceAppearance>,
    pbr_materials: Option<&PbrMaterialAssignments>,
    pbr_material_overrides: Option<&PbrMaterialOverrides>,
    avatar_appearance: Option<&AvatarAppearance>,
) -> usize {
    size_of::<ObjectDetail>()
        .saturating_add(surface_appearance.map_or(0, SurfaceAppearance::heap_bytes))
        .saturating_add(pbr_materials.map_or(0, PbrMaterialAssignments::heap_bytes))
        .saturating_add(pbr_material_overrides.map_or(0, PbrMaterialOverrides::heap_bytes))
        .saturating_add(avatar_appearance.map_or(0, AvatarAppearance::heap_bytes))
}
