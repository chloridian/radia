use rmcp::{
    ErrorData,
    handler::server::wrapper::{Json, Parameters},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use world::{ObjectDetail, ObjectDetailResult};

use super::{ShapeOutput, format_shape, lookup_object_entity};
use crate::{
    inspection::{ViewerInspectionHandle, ViewportQuery},
    mcp::server::Handler,
};

#[derive(Deserialize, JsonSchema)]
pub struct ExplainObjectArgs {
    #[schemars(description = "Object UUID to explain.")]
    pub object_id: String,
}

#[derive(Serialize, JsonSchema)]
pub struct FaceExplanationOutput {
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

#[derive(Serialize, JsonSchema)]
pub struct RenderExplanationOutput {
    pub in_scene: bool,
    pub placement: &'static str,
    pub hidden_reason: Option<String>,
    pub geometry: &'static str,
    pub has_mesh: bool,
    pub mesh_matches_geometry: bool,
    pub in_cache: bool,
    pub visibility: String,
    pub mesh_residency: String,
    pub faces: Vec<FaceExplanationOutput>,
}

#[derive(Serialize, JsonSchema)]
pub struct WorldDetailOutput {
    pub entity: String,
    pub id: String,
    pub region: u64,
    pub local_id: u32,
    pub kind: String,
    pub shape: Option<ShapeOutput>,
    pub is_physical: bool,
    pub attachment_point: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct ExplainObjectOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_session: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub found: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub world: Option<WorldDetailOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub render: Option<RenderExplanationOutput>,
}

#[tool_router(router = explain_object_router, vis = "pub(super)")]
impl Handler {
    #[tool(
        name = "explain_object",
        title = "Explain Object",
        description = "Explain why an object is or is not drawn by combining World detail with renderer visibility, mesh, and texture state.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn explain_object(
        &self,
        Parameters(args): Parameters<ExplainObjectArgs>,
    ) -> Result<Json<ExplainObjectOutput>, ErrorData> {
        let output = explain_object_data(&self.handle, &args.object_id).await.map_err(|msg| ErrorData::invalid_params(msg, None))?;
        Ok(Json(output))
    }
}

pub(in crate::mcp) async fn explain_object_data(handle: &ViewerInspectionHandle, id_str: &str) -> Result<ExplainObjectOutput, String> {
    let Some(session) = handle.session() else {
        return Ok(ExplainObjectOutput { active_session: Some(false), found: None, object_id: None, world: None, render: None });
    };
    let Some(entity) = lookup_object_entity(&session.world, id_str).await? else {
        return Ok(ExplainObjectOutput {
            active_session: None,
            found: Some(false),
            object_id: Some(id_str.to_owned()),
            world: None,
            render: None,
        });
    };

    let world_detail =
        session.world.details(&[entity]).await.map_err(|e| format!("world details failed: {e}"))?.details.into_iter().find_map(|entry| {
            match entry {
                ObjectDetailResult::Found(detail) => Some(detail),
                _ => None,
            }
        });

    let (reply_tx, reply_rx) = oneshot::channel();
    session.try_send_viewport_query(ViewportQuery::ExplainObject { entity, reply: reply_tx }).map_err(str::to_owned)?;
    let render_explanation = reply_rx.await.map_err(|_| "viewport dropped explain request".to_owned())?;

    let render = render_explanation.map(|exp| {
        let faces = exp
            .faces
            .into_iter()
            .map(|face| FaceExplanationOutput {
                material_slot: face.material_slot,
                submitted: face.submitted,
                omitted: face.omitted,
                blended: face.blended,
                double_sided: face.double_sided,
                alpha_hidden: face.alpha_hidden,
                has_base_texture: face.has_base_texture,
                base_texture_ready: face.base_texture_ready,
                has_emissive_texture: face.has_emissive_texture,
                emissive_texture_ready: face.emissive_texture_ready,
                material_shading: face.material_shading,
                material_reason: face.material_reason,
            })
            .collect();
        RenderExplanationOutput {
            in_scene: exp.in_scene,
            placement: exp.placement,
            hidden_reason: exp.hidden_reason,
            geometry: exp.geometry,
            has_mesh: exp.has_mesh,
            mesh_matches_geometry: exp.mesh_matches_geometry,
            in_cache: exp.in_cache,
            visibility: format!("{:?}", exp.visibility),
            mesh_residency: format!("{:?}", exp.mesh_residency),
            faces,
        }
    });

    Ok(ExplainObjectOutput {
        active_session: None,
        found: Some(true),
        object_id: Some(id_str.to_owned()),
        world: world_detail.as_ref().map(format_world_detail),
        render,
    })
}

fn format_world_detail(detail: &ObjectDetail) -> WorldDetailOutput {
    WorldDetailOutput {
        entity: format!("{:?}", detail.summary.entity),
        id: detail.summary.id.to_string(),
        region: detail.summary.address.region().get(),
        local_id: detail.summary.address.local_id().get(),
        kind: format!("{:?}", detail.summary.kind),
        shape: detail.shape.as_ref().map(format_shape),
        is_physical: detail.is_physical,
        attachment_point: detail.attachment_point.map(|point| format!("{point:?}")),
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use utils::AssetId;
    use uuid::Uuid;
    use world::{FieldUpdate, Kind, ObjectObservation, OrderEvidence, ParentUpdate, SculptFlags, Shape, UpdateSource, World, WorldLimits};

    use super::format_world_detail;

    #[test]
    fn explain_object_output_omits_shape_asset_uuids() {
        let mut world = World::new(WorldLimits {
            max_regions: 2,
            max_entities: 8,
            max_entities_per_region: 8,
            max_tombstones_per_region: 8,
            max_tombstones: 8,
            max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(8),
            max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(8),
            max_journal_entries: 8,
            max_journal_changes: 32,
            max_region_name_bytes: 64,
            max_recovery_targets: 4,
        });
        let region = world.register_region().expect("register region");
        let secret_mesh = Uuid::from_u128(0xdead_beef_cafe_babe_0123_4567_89ab_cdef);
        let local_id = NonZeroU32::new(1).expect("nonzero local id");
        let source = UpdateSource::new(region, 1, OrderEvidence::Ordered(1), 0);
        let mesh_id = Uuid::from_u128(1);
        let mut observation = ObjectObservation::new(source, mesh_id, local_id, Kind::Object, ParentUpdate::Root);
        observation.shape =
            FieldUpdate::Set(Shape::Mesh { asset_id: AssetId::new(secret_mesh), flags: SculptFlags { invert: false, mirror: true } });
        drop(world.observe_object(observation).expect("observe mesh"));
        let entity = world.entity(mesh_id).expect("mesh entity");
        let detail = world.object_detail(entity).expect("mesh detail");
        let output = format_world_detail(&detail);
        let serialized = serde_json::to_value(&output).expect("serialize detail");
        assert_eq!(serialized["shape"]["kind"], "mesh");
        assert_eq!(serialized["shape"]["flags"]["mirror"], true);
        assert!(!serialized.to_string().contains(&secret_mesh.to_string()));
    }
}
