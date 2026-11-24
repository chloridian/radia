use assets::MaterialDescription;
use uuid::Uuid;
use world::{Entity, Region};

use super::GeometryRenderer;
use crate::{
    material::{FaceMaterial, Resolution},
    scene::{Geometry, Object, Placement, SceneView},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VisibilityStatus {
    Visible,
    Culled,
    NotCached,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MeshResidency {
    Resident,
    Pending,
    BudgetBlocked,
    NoMesh,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FaceExplanation {
    pub material_slot: u8,
    pub submitted: bool,
    pub omitted: bool,
    pub blended: bool,
    pub double_sided: bool,
    pub alpha_hidden: bool,
    pub has_base_texture: bool,
    pub base_texture_ready: Option<bool>,
    pub has_emissive_texture: bool,
    pub emissive_texture_ready: Option<bool>,
    pub material_shading: &'static str,
    pub material_reason: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectExplanation {
    pub entity: Entity,
    pub in_scene: bool,
    pub object_id: Option<Uuid>,
    pub region: Option<Region>,
    pub placement: &'static str,
    pub hidden_reason: Option<String>,
    pub geometry: &'static str,
    pub has_mesh: bool,
    pub mesh_matches_geometry: bool,
    pub in_cache: bool,
    pub visibility: VisibilityStatus,
    pub mesh_residency: MeshResidency,
    pub faces: Vec<FaceExplanation>,
}

impl GeometryRenderer {
    pub(crate) fn explain_object(&self, view: &SceneView, entity: Entity) -> ObjectExplanation {
        let scene_object = view.object(entity);
        let (object_id, region, placement, hidden_reason, geometry, has_mesh, mesh_matches_geometry) = match scene_object {
            Some(object) => {
                let (placement, hidden_reason) = match object.placement() {
                    Placement::Ready(_) => ("ready", None),
                    Placement::Hidden(reason) => ("hidden", Some(format!("{reason:?}"))),
                };
                let geometry = match object.geometry() {
                    Geometry::Prim(_) => "prim",
                    Geometry::Mesh(_) => "mesh",
                    Geometry::Sculpt { .. } => "sculpt",
                    Geometry::Unsupported(_) => "unsupported",
                    Geometry::Missing => "missing",
                };
                (
                    Some(object.detail().summary.id),
                    Some(object.detail().summary.address.region()),
                    placement,
                    hidden_reason,
                    geometry,
                    object.mesh().is_some(),
                    object.mesh_matches_geometry(),
                )
            }
            None => (None, None, "missing", None, "missing", false, false),
        };

        let cached = self.cache.items.iter().find(|(_, item)| item.entity == Some(entity));
        let Some((index, item)) = cached else {
            return ObjectExplanation {
                entity,
                in_scene: scene_object.is_some(),
                object_id,
                region,
                placement,
                hidden_reason,
                geometry,
                has_mesh,
                mesh_matches_geometry,
                in_cache: false,
                visibility: VisibilityStatus::NotCached,
                mesh_residency: MeshResidency::NoMesh,
                faces: Vec::new(),
            };
        };

        let is_visible = self.cache.visible.contains(&index);
        let visibility = if is_visible { VisibilityStatus::Visible } else { VisibilityStatus::Culled };
        let mesh_id = item.mesh.id();
        let mesh_residency = if self.meshes.contains_key(&mesh_id) {
            MeshResidency::Resident
        } else if self.mesh_budget_blocked.contains(&mesh_id) {
            MeshResidency::BudgetBlocked
        } else if self.pending_meshes.contains(&mesh_id) {
            MeshResidency::Pending
        } else {
            MeshResidency::NoMesh
        };

        let materials = scene_object.or(item.source.as_deref()).and_then(Object::materials);
        let faces = item
            .faces
            .iter()
            .map(|face| {
                let has_base_texture = face.images.base.is_some();
                let base_texture_ready = face.images.base.as_ref().map(|handle| !self.pending_textures.contains(&handle.id()));
                let has_emissive_texture = face.images.emissive.is_some();
                let emissive_texture_ready = face.images.emissive.as_ref().map(|handle| !self.pending_textures.contains(&handle.id()));
                let alpha_hidden = face.params.color[3] <= 0.0;
                let submitted = face.slot.is_some();
                let omitted = is_visible && !submitted && !alpha_hidden;
                let face_material = materials.and_then(|materials| {
                    face.material_index
                        .and_then(|idx| materials.get(idx))
                        .or_else(|| materials.iter().find(|material| material.slot == face.order.3))
                });
                let (material_shading, material_reason) = material_selection(face_material);
                FaceExplanation {
                    material_slot: face.order.3,
                    submitted,
                    omitted,
                    blended: face.blended,
                    double_sided: face.params.double_sided,
                    alpha_hidden,
                    has_base_texture,
                    base_texture_ready,
                    has_emissive_texture,
                    emissive_texture_ready,
                    material_shading,
                    material_reason,
                }
            })
            .collect();

        ObjectExplanation {
            entity,
            in_scene: scene_object.is_some(),
            object_id: object_id.or_else(|| item.source.as_ref().map(|source| source.detail().summary.id)),
            region: region.or(Some(item.region)),
            placement,
            hidden_reason,
            geometry,
            has_mesh: true,
            mesh_matches_geometry,
            in_cache: true,
            visibility,
            mesh_residency,
            faces,
        }
    }
}

fn material_selection(face_material: Option<&FaceMaterial>) -> (&'static str, &'static str) {
    let Some(face_material) = face_material else {
        return ("legacy", "no_material_override");
    };
    match face_material.material {
        MaterialDescription::Pbr(_) => {
            let reason = match face_material.pbr_state {
                Some(state) if state.resolution == Resolution::Ready => "pbr_material_applied",
                _ => "pbr_override_applied",
            };
            ("pbr", reason)
        }
        MaterialDescription::Legacy(_) => {
            let reason = match face_material.pbr_state.map(|state| state.resolution) {
                Some(Resolution::Pending) => "pbr_material_pending",
                Some(Resolution::Failed(_)) => "pbr_material_failed",
                Some(Resolution::Ready) => "pbr_material_unavailable",
                None => match face_material.legacy_state.map(|state| state.resolution) {
                    Some(Resolution::Ready) => "legacy_material_applied",
                    Some(Resolution::Pending) => "legacy_material_pending",
                    Some(Resolution::Failed(_)) => "legacy_material_failed",
                    None => "no_material_override",
                },
            };
            ("legacy", reason)
        }
    }
}
