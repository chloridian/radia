#![allow(dead_code, unused_imports)]

pub(super) use std::{collections::HashSet, num::NonZeroU32};

pub(super) use math::{Quat, Vec3};
pub(super) use utils::AssetId;
pub(super) use uuid::Uuid;
pub(super) use world::{
    Appearance,
    ApplyError::{InvalidValue, KindMismatch},
    ApplyOutcome::{AgentHandedOff, Deferred, IgnoredStale, Introduced, Removed, TombstoneRecorded, Unchanged, Updated},
    AvatarAppearanceUpdate, AvatarTextureSlot, AvatarTextureSlots, AvatarVisualParamIndex,
    DeferredReason::{AmbiguousOrder, UnknownObject},
    FaceAppearance, FieldGroup,
    FieldGroup::{AngularVelocity, Scale},
    FieldRejection, FieldRejectionReason,
    FieldRejectionReason::{InvalidRotation, ParentCycle, ParentDependsOnRejectedParent, UntrustedSource},
    FieldUpdate, Kind,
    Kind::{Avatar, Object, Unknown},
    LinearMotion, LocalAddress, MAX_AVATAR_TEXTURE_SLOTS, MAX_PBR_MATERIAL_BINDINGS, MediaEntry, MotionUpdateKind, ObjectMediaUpdate,
    ObjectObservation, ObjectPatch,
    OrderEvidence::{Ambiguous, Degraded, Ordered},
    Parent,
    Parent::{Pending, Resolved},
    ParentUpdate,
    ParentUpdate::{Local, Root},
    PbrMaterialAssignments, PbrMaterialAssignmentsError, Pose, Region, RegionInfo, SculptFlags, SculptType, Shape, SitTarget,
    SurfaceAppearance, SurfaceAppearanceError, TextureAnimation, TextureAnimationError, UpdateAuthority, UpdateSource, World, WorldLimits,
};

pub(super) fn limits() -> WorldLimits {
    WorldLimits {
        max_regions: 4,
        max_entities: 8,
        max_entities_per_region: 8,
        max_tombstones_per_region: 4,
        max_tombstones: 8,
        max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(4),
        max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(8),
        max_journal_entries: 16,
        max_journal_changes: 64,
        max_region_name_bytes: 64,
        max_recovery_targets: 4,
    }
}

pub(super) fn uuid(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

pub(super) fn surface(default_texture: u128, face_texture: u128) -> SurfaceAppearance {
    SurfaceAppearance::new(
        FaceAppearance { texture: Some(AssetId::new(uuid(default_texture))), ..FaceAppearance::default() },
        vec![(44, FaceAppearance { texture: Some(AssetId::new(uuid(face_texture))), ..FaceAppearance::default() })],
    )
    .expect("valid face overrides")
}

pub(super) fn pbr_materials(default_material: u128, face_material: u128) -> PbrMaterialAssignments {
    PbrMaterialAssignments::new(vec![(0, AssetId::new(uuid(default_material))), (44, AssetId::new(uuid(face_material)))])
        .expect("valid PBR face bindings")
}

pub(super) fn sculpt_shape(asset: u128) -> Shape {
    Shape::Sculpt { asset_id: AssetId::new(uuid(asset)), sculpt_type: SculptType::Sphere, flags: SculptFlags::default() }
}

pub(super) fn mesh_shape(asset: u128) -> Shape {
    Shape::Mesh { asset_id: AssetId::new(uuid(asset)), flags: SculptFlags::default() }
}

pub(super) fn local_id(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).expect("local ID must be nonzero")
}

pub(super) fn address(region: Region, local_id_value: u32) -> LocalAddress {
    LocalAddress::new(region, local_id(local_id_value))
}

pub(super) fn source(region: Region, order: u64) -> UpdateSource {
    ordered(region, order, 0)
}

pub(super) fn ordered(region: Region, order: u64, block_ordinal: u32) -> UpdateSource {
    UpdateSource::new(region, order as u32, Ordered(order), block_ordinal)
}

pub(super) fn untracked(region: Region, order: u64) -> UpdateSource {
    source(region, order).with_authority(UpdateAuthority::Untracked)
}

pub(super) fn object(source: UpdateSource, id: u128, local_id_value: u32, kind: Kind, parent: ParentUpdate) -> ObjectObservation {
    ObjectObservation::new(source, uuid(id), local_id(local_id_value), kind, parent)
}

pub(super) fn with_region() -> (World, Region) {
    let mut world = World::new(limits());
    let region = world.register_region().unwrap();
    (world, region)
}

pub(super) fn drain_removals(world: &mut World) -> Vec<world::RemovalProgress> {
    let mut progress = Vec::new();
    while world.has_pending_removals() {
        let turn = world.reclaim_removals(world::MAX_REMOVAL_WORK_PER_TURN);
        assert!(turn.work_units <= world::MAX_REMOVAL_WORK_PER_TURN);
        assert!(turn.work_units != 0 || !turn.pending);
        progress.push(turn);
    }
    progress
}
