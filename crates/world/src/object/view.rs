use std::sync::Arc;

use hecs::{QueryBorrow, Ref};
use math::{Quat, Vec3};
use uuid::Uuid;

use crate::{
    Appearance, AttachmentPoint, AvatarAppearance, CoordinateFrame, Entity, Kind, LinearMotion, LocalAddress, LocalTransform, ObjectMedia,
    ObjectOrdering, ObjectRecord, Parent, PbrMaterialAssignments, PbrMaterialOverrides, PoseError, PoseState, Shape, SitTarget,
    SurfaceAppearance, TextureAnimation, Transform, UpdateSource, World, WorldTransformState,
    mutation::facet::Facet,
    object::{components::Fenced, storage::ComponentView},
    removal::VisibilityCache,
};

#[derive(Clone, Copy)]
pub struct Object<'a> {
    pub(crate) entity: Entity,
    record: &'a ObjectRecord,
    pub(crate) world: &'a World,
}

impl<'a> Object<'a> {
    pub(crate) fn new(entity: Entity, record: &'a ObjectRecord, world: &'a World) -> Self {
        Self { entity, record, world }
    }

    pub(crate) fn record(&self) -> &'a ObjectRecord {
        self.record
    }

    fn facet<T: Facet>(&self) -> Option<&'a T::View> {
        let store = &self.world.store;
        store.component::<Fenced<T>>(self.entity.0)?.value.as_ref().map(T::view)
    }

    pub(crate) fn ordering(&self) -> Ref<'a, ObjectOrdering> {
        self.world.store.ordering(self.entity.0)
    }

    pub const fn entity(&self) -> Entity {
        self.entity
    }

    pub fn id(&self) -> Uuid {
        self.record.id
    }

    pub fn kind(&self) -> Kind {
        self.record.kind
    }

    pub fn transform(&self) -> Transform {
        self.record.state.transform
    }

    pub fn position(&self) -> Option<Vec3> {
        self.record.state.transform.position()
    }

    pub fn rotation(&self) -> Option<Quat> {
        self.record.state.transform.rotation()
    }

    pub fn scale(&self) -> Option<Vec3> {
        self.record.state.transform.scale()
    }

    pub fn local_transform(&self) -> Option<LocalTransform> {
        self.record.state.transform.local_transform()
    }

    pub fn address(&self) -> LocalAddress {
        self.record.state.binding.address
    }

    pub fn parent(&self) -> Parent {
        self.world.logical_parent_for(self.entity, self.record.state.parent)
    }

    pub fn linear_motion(&self) -> LinearMotion {
        self.record.state.linear_motion
    }

    pub fn angular_velocity(&self) -> Option<Vec3> {
        self.record.state.angular_velocity
    }

    pub fn is_physical(&self) -> bool {
        self.record.state.is_physical
    }

    pub fn time_dilation(&self) -> f32 {
        self.record.state.time_dilation
    }

    pub fn motion_source(&self) -> Option<UpdateSource> {
        self.record.state.motion_source
    }

    pub fn shape(&self) -> Option<Shape> {
        self.facet::<Shape>().copied()
    }

    pub fn appearance(&self) -> Option<Appearance> {
        self.facet::<Appearance>().copied()
    }

    pub fn surface_appearance(&self) -> Option<&'a SurfaceAppearance> {
        self.facet::<Arc<SurfaceAppearance>>()
    }

    pub fn texture_animation(&self) -> Option<TextureAnimation> {
        self.facet::<TextureAnimation>().copied()
    }

    pub fn pbr_materials(&self) -> Option<&'a PbrMaterialAssignments> {
        self.facet::<Arc<PbrMaterialAssignments>>()
    }

    pub fn pbr_material_overrides(&self) -> Option<&'a PbrMaterialOverrides> {
        self.facet::<Arc<PbrMaterialOverrides>>()
    }

    pub fn object_media(&self) -> Option<&'a ObjectMedia> {
        self.facet::<Arc<ObjectMedia>>()
    }

    pub fn avatar_appearance(&self) -> Option<&'a AvatarAppearance> {
        self.facet::<Arc<AvatarAppearance>>()
    }

    pub fn attachment_point(&self) -> Option<AttachmentPoint> {
        self.record.state.attachment_point
    }

    pub fn sit_target(&self) -> Option<SitTarget> {
        self.facet::<SitTarget>().copied()
    }

    pub fn coordinate_frame(&self) -> Result<CoordinateFrame, PoseError> {
        self.world.coordinate_frame(self.entity)
    }

    pub fn world_pose(&self) -> Result<PoseState, PoseError> {
        self.world.resolve_pose(self.entity)
    }

    pub fn world_transform(&self) -> Result<WorldTransformState, PoseError> {
        self.world.resolve_transform(self.entity)
    }
}

pub struct Objects<'a> {
    pub(crate) records: QueryBorrow<'a, (hecs::Entity, &'a ObjectRecord)>,
    pub(crate) world: &'a World,
}

impl<'a> Objects<'a> {
    pub fn iter<'b>(&'b mut self) -> impl Iterator<Item = Object<'b>> + 'b {
        let world = self.world;
        let needs_filter = world.has_pending_removals();
        let mut visibility = VisibilityCache::default();
        self.records.iter().filter_map(move |(raw, record)| {
            let entity = Entity(raw, world.id);
            if needs_filter && !world.visibility(entity, &mut visibility).visible {
                return None;
            }
            Some(Object::new(entity, record, world))
        })
    }
}

pub(crate) struct IndexedObjects<'a> {
    entities: Box<dyn Iterator<Item = Entity> + 'a>,
    records: ComponentView<'a, ObjectRecord>,
    world: &'a World,
    needs_filter: bool,
    visibility: VisibilityCache,
}

impl<'a> IndexedObjects<'a> {
    pub(crate) fn new(entities: impl Iterator<Item = Entity> + 'a, world: &'a World, needs_filter: bool) -> Self {
        Self {
            entities: Box::new(entities),
            records: world.store.component_view::<ObjectRecord>(),
            world,
            needs_filter,
            visibility: VisibilityCache::default(),
        }
    }
}

impl<'a> Iterator for IndexedObjects<'a> {
    type Item = Object<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        for entity in self.entities.by_ref() {
            if self.needs_filter && !self.world.visibility(entity, &mut self.visibility).visible {
                continue;
            }
            let record = self.records.get(entity.0).expect("object index contains only complete objects");
            return Some(Object::new(entity, record, self.world));
        }
        None
    }
}
