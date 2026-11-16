mod history;
mod identity;
#[cfg(test)]
mod invariants;
mod mutation;
mod object;
mod read;
mod region;
mod removal;
mod update;

use std::collections::{BTreeMap, HashMap, HashSet};

use uuid::Uuid;

use crate::history::Journal;
use crate::history::address::Tombstones;
use crate::history::retained::RetainedOverrides;
use crate::identity::WorldId;
use crate::object::components::{
    Binding, Fences, ObjectOrdering, ObjectRecord, ObjectState, ParentRejectionBarrier, RegionalStamp, newer_field, newer_region_field,
};
use crate::object::relationships::{PendingChildKey, PendingResolution};
use crate::object::storage::ObjectStore;
use crate::object::view::IndexedObjects;
use crate::region::RegionState;
use crate::removal::{ReclamationFrame, ReclamationTask, RemovalCause, TransferProgress, VisibilityLookupCache};
use crate::update::Stamp;

pub use history::{WorldChange, WorldChangeClasses, WorldEntityRemovalReason};
pub use identity::{Entity, LocalAddress, ObjectBindingTarget, Region, RegionHandle, RegionInfo};
pub use object::appearance::{
    Appearance, FaceAppearance, MAX_PBR_MATERIAL_BINDINGS, MAX_SURFACE_FACES, PbrAlphaMode, PbrMaterialAssignments,
    PbrMaterialAssignmentsError, PbrMaterialOverride, PbrMaterialOverrides, PbrMaterialOverridesError, PbrTextureOverride,
    PbrTextureTransformOverride, SurfaceAppearance, SurfaceAppearanceError, TextureAnimation, TextureAnimationError, TextureMapping,
};
pub use object::avatar::{
    AvatarAppearance, AvatarAppearanceError, AvatarAppearanceUpdate, AvatarBakeSlot, AvatarTextureSlot, AvatarTextureSlots,
    AvatarVisualParamIndex, MAX_AVATAR_TEXTURE_SLOTS, MAX_AVATAR_VISUAL_PARAMS,
};
pub use object::media::{
    MAX_MEDIA_DIMENSION, MAX_MEDIA_URL_BYTES, MAX_MEDIA_WHITELIST_BYTES, MAX_MEDIA_WHITELIST_ENTRIES, MediaControls, MediaEntry,
    ObjectMedia, ObjectMediaError, ObjectMediaUpdate,
};
pub use object::relationships::{ParentResolutionCursor, ParentResolutionPlan, ParentResolutionProgress, RelationshipResolution};
pub use object::shape::{PrimShape, SculptFlags, SculptType, Shape};
pub use object::transform::{
    AttachmentPoint, CoordinateFrame, LinearMotion, LocalTransform, Pose, PoseError, PoseState, SitTarget, Transform, UnresolvedReason,
    WorldPose, WorldTransform, WorldTransformState,
};
pub use object::view::{Object, Objects};
pub use object::{Kind, Parent, ParentUpdate};
pub use read::{
    MAX_OBJECT_DETAIL_BATCH_BYTES, MAX_OBJECT_DETAIL_BATCH_ITEMS, MAX_OBJECT_DETAIL_BYTES, MAX_OBJECT_DETAIL_WORK_ITEMS,
    MAX_OBJECT_LOOKUP_WORK_ITEMS, MAX_OBJECT_MEDIA_READ_BYTES, MAX_OBJECT_PAGE_ITEMS, MAX_OBJECT_PAGE_WORK_ITEMS, MAX_REGION_PAGE_BYTES,
    MAX_REGION_PAGE_ITEMS, MAX_WORLD_CHANGE_PAGE_ITEMS, ObjectBindingFence, ObjectDetail, ObjectDetailBatch, ObjectDetailBatchPage,
    ObjectDetailCursor, ObjectDetailResult, ObjectListItem, ObjectListPage, ObjectLookupCursor, ObjectLookupProgress, ObjectMediaRead,
    ObjectPageCursor, ObjectSummary, RegionPage, RegionPageCursor, RegionSummary, WorldChangeCursor, WorldChangePage, WorldChangeRecord,
    WorldReadBarrier, WorldReadError,
};
pub use region::environment::{
    EnvironmentAssignment, EnvironmentRead, EnvironmentScope, EnvironmentSource, EnvironmentTarget, MAX_ENVIRONMENT_BYTES,
    MAX_ENVIRONMENT_READ_BYTES, MAX_ENVIRONMENT_SOURCE_BYTES, ParcelEnvironment, ParcelLocalId,
};
pub use region::parcel::{
    CurrentParcel, EffectiveRestrictions, MAX_PARCEL_BYTES, MAX_PARCEL_DESCRIPTION_BYTES, MAX_PARCEL_NAME_BYTES, MAX_PARCEL_READ_BYTES,
    MAX_PARCEL_REGION_BYTES, MAX_PARCEL_URL_BYTES, PARCEL_BITMAP_BYTES, PARCEL_GRID_EDGE, PARCEL_OVERLAY_CELLS, PARCEL_OVERLAY_PART_BYTES,
    PARCEL_OVERLAY_PARTS, ParcelAccessSettings, ParcelBitmap, ParcelFlags, ParcelOverlayCell, ParcelOverlayGrid, ParcelOverlayObservation,
    ParcelOwnership, ParcelPropertiesData, ParcelPropertiesObservation, ParcelRead, RegionFlags,
};
pub use region::terrain::{
    MAX_TERRAIN_PAGE_PATCHES, MAX_TERRAIN_PATCHES, TERRAIN_PATCH_SAMPLES, TERRAIN_PATCH_SIZE, TERRAIN_REGION_SIZE, TerrainPage,
    TerrainPageCursor, TerrainPatch, TerrainPatchError, TerrainPatchId, TerrainSettings, TerrainSettingsError,
};
pub use removal::{MAX_REMOVAL_WORK_PER_TURN, RemovalFence, RemovalProgress};
pub use update::{
    Applied, ApplyError, ApplyOutcome, DeferredReason, FieldGroup, FieldRejection, FieldRejectionReason, FieldUpdate, MotionUpdateKind,
    ObjectObservation, ObjectPatch, OrderEvidence, Recovery, RecoveryCancellation, RegionApplied, RegionOutcome, Rejections,
    UpdateAuthority, UpdateSource, WorldLimits, WorldRevision,
};

const MAX_OBJECT_MEDIA_BYTES: usize = 64 * 1024 * 1024;

pub struct World {
    id: WorldId,
    store: ObjectStore,
    regions: BTreeMap<Region, RegionState>,
    retiring_regions: HashMap<Region, RegionState>,
    removal_tasks: std::collections::VecDeque<ReclamationTask>,
    reclamation: Vec<ReclamationFrame>,
    transfer_cursors: HashMap<Entity, TransferProgress>,
    transfer_address_count: usize,
    invalidations: HashMap<Entity, RemovalCause>,
    counted_invalidations: HashSet<Entity>,
    lookup_visibility: VisibilityLookupCache,
    removal_fences: HashMap<LocalAddress, (Entity, Uuid, RemovalCause)>,
    invalidated_region_counts: HashMap<Region, usize>,
    last_region_generation: u64,
    self_agent: Option<Uuid>,
    children: HashMap<Entity, BTreeMap<Uuid, Entity>>,
    pending_children: HashMap<LocalAddress, BTreeMap<PendingChildKey, Entity>>,
    relationship_work: BTreeMap<(Uuid, LocalAddress), PendingResolution>,
    relationship_work_parents: HashMap<LocalAddress, BTreeMap<Uuid, Entity>>,
    relationship_cursor: Option<(Uuid, LocalAddress)>,
    tombstones: Tombstones,
    retained: RetainedOverrides,
    floors: HashMap<Region, Stamp>,
    object_media_bytes: usize,
    terrain_patches: usize,
    environment_bytes: usize,
    parcel_bytes: usize,
    limits: WorldLimits,
    journal: Journal,
}

impl World {
    pub fn new(limits: WorldLimits) -> Self {
        let id = WorldId::fresh();
        Self {
            id,
            store: ObjectStore::new(id),
            regions: BTreeMap::new(),
            retiring_regions: HashMap::new(),
            removal_tasks: std::collections::VecDeque::new(),
            reclamation: Vec::new(),
            transfer_cursors: HashMap::new(),
            transfer_address_count: 0,
            invalidations: HashMap::new(),
            counted_invalidations: HashSet::new(),
            lookup_visibility: VisibilityLookupCache::default(),
            removal_fences: HashMap::new(),
            invalidated_region_counts: HashMap::new(),
            last_region_generation: 0,
            self_agent: None,
            children: HashMap::new(),
            pending_children: HashMap::new(),
            relationship_work: BTreeMap::new(),
            relationship_work_parents: HashMap::new(),
            relationship_cursor: None,
            tombstones: Tombstones::default(),
            retained: RetainedOverrides::default(),
            floors: HashMap::new(),
            object_media_bytes: 0,
            terrain_patches: 0,
            environment_bytes: 0,
            parcel_bytes: 0,
            limits,
            journal: Journal::new(),
        }
    }

    pub fn set_self_agent(&mut self, id: Uuid) -> Result<(), ApplyError> {
        if id.is_nil() {
            return Err(ApplyError::InvalidValue { field: "agent_id" });
        }
        if self.self_agent.is_some_and(|current| current != id) {
            return Err(ApplyError::InvalidValue { field: "agent_id" });
        }
        self.self_agent = Some(id);
        Ok(())
    }

    pub const fn revision(&self) -> WorldRevision {
        self.journal.revision
    }

    pub fn entity(&self, id: Uuid) -> Option<Entity> {
        let entity = self.store.entity(id)?;
        self.entity_is_visible(entity).then_some(entity)
    }

    pub fn entity_at(&self, address: LocalAddress) -> Option<Entity> {
        let entity = self.store.entity_at(address)?;
        self.entity_is_visible(entity).then_some(entity)
    }

    pub fn object_binding_target(&self, entity: Entity) -> Option<ObjectBindingTarget> {
        let object = self.object(entity)?.record();
        Some(ObjectBindingTarget {
            entity,
            id: object.id,
            address: object.state.binding.address,
            address_generation: object.state.binding.address_generation,
        })
    }

    pub fn object_binding_target_is_current(&self, target: ObjectBindingTarget) -> bool {
        if target.entity.1 != self.id || self.entity(target.id) != Some(target.entity) {
            return false;
        }
        let Some(object) = self.store.object_record(target.entity) else { return false };
        object.state.binding.address == target.address
            && object.state.binding.address_generation == target.address_generation
            && self.entity_at(target.address) == Some(target.entity)
    }

    pub fn object(&self, entity: Entity) -> Option<Object<'_>> {
        if !self.entity_is_visible(entity) {
            return None;
        }
        let record = self.store.object_record(entity)?;
        Some(Object::new(entity, record, self))
    }

    pub fn objects(&self) -> Objects<'_> {
        Objects { records: self.store.objects(), world: self }
    }

    pub fn object_count(&self) -> usize {
        self.visible_entity_count()
    }

    pub fn objects_in_region(&self, region: Region) -> impl Iterator<Item = Object<'_>> {
        let needs_filter = self.has_pending_removals();
        self.store.region_ids(region).into_iter().flat_map(move |ids| IndexedObjects::new(ids.values().copied(), self, needs_filter))
    }

    pub fn children(&self, entity: Entity) -> impl Iterator<Item = Object<'_>> {
        self.children
            .get(&entity)
            .into_iter()
            .flat_map(|children| children.values())
            .filter_map(move |child| self.object(*child).filter(|object| object.parent() == Parent::Resolved(entity)))
    }

    pub fn avatars(&self) -> impl Iterator<Item = Object<'_>> {
        IndexedObjects::new(self.store.avatar_ids().values().copied(), self, self.has_pending_removals())
    }

    pub fn regions(&self) -> impl Iterator<Item = (Region, &RegionInfo)> {
        self.regions.iter().map(|(region, info)| (*region, &info.info))
    }

    pub fn region(&self, region: Region) -> Option<&RegionInfo> {
        self.regions.get(&region).map(|state| &state.info)
    }

    fn require_active_region(&self, region: Region) -> Result<(), ApplyError> {
        if !self.region_was_issued(region) {
            return Err(ApplyError::StaleRegion);
        }
        if self.regions.contains_key(&region) { Ok(()) } else { Err(ApplyError::RegionRetired) }
    }

    fn region_was_issued(&self, region: Region) -> bool {
        region.belongs_to(self.id) && region.get() <= self.last_region_generation
    }

    #[cfg(test)]
    fn state_mut(&mut self, entity: Entity) -> &mut ObjectState {
        &mut self.store.record_mut(entity.0).state
    }
}
