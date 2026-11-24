use serde_json::{Value, json};
use tokio::sync::oneshot;
use world::{ObjectDetail, ObjectDetailResult};

use super::{ToolContent, ToolFuture, ToolSpec, format_shape, lookup_object_entity};
use crate::{
    inspection::{ViewerInspectionHandle, ViewportQuery},
    mcp::MAX_JSON_RESPONSE_BYTES,
};

pub const SPEC: ToolSpec = ToolSpec {
    name: "explain_object",
    description: "Explain why an object is or is not drawn by combining World detail with renderer visibility, mesh, and texture state.",
    input_schema,
    handler,
    max_response_bytes: MAX_JSON_RESPONSE_BYTES,
};

fn input_schema() -> Value {
    json!({
        "type": "object",
        "required": ["object_id"],
        "properties": {
            "object_id": { "type": "string" }
        }
    })
}

fn handler(handle: ViewerInspectionHandle, args: Value) -> ToolFuture {
    Box::pin(async move {
        let Some(session) = handle.session() else {
            return Ok(ToolContent::Json(json!({ "active_session": false })));
        };
        let Some(id_str) = args.get("object_id").and_then(Value::as_str) else {
            return Err("object_id is required".to_owned());
        };
        let Some(entity) = lookup_object_entity(&session.world, id_str).await? else {
            return Ok(ToolContent::Json(json!({ "found": false, "object_id": id_str })));
        };

        let world_detail =
            session.world.details(&[entity]).await.map_err(|e| format!("world details failed: {e}"))?.details.into_iter().find_map(
                |entry| match entry {
                    ObjectDetailResult::Found(detail) => Some(detail),
                    _ => None,
                },
            );

        let (reply_tx, reply_rx) = oneshot::channel();
        session.try_send_viewport_query(ViewportQuery::ExplainObject { entity, reply: reply_tx }).map_err(str::to_owned)?;
        let render_explanation = reply_rx.await.map_err(|_| "viewport dropped explain request".to_owned())?;

        let render_json = render_explanation.map(|exp| {
            let faces: Vec<Value> = exp
                .faces
                .into_iter()
                .map(|face| {
                    json!({
                        "material_slot": face.material_slot,
                        "submitted": face.submitted,
                        "omitted": face.omitted,
                        "blended": face.blended,
                        "double_sided": face.double_sided,
                        "alpha_hidden": face.alpha_hidden,
                        "has_base_texture": face.has_base_texture,
                        "base_texture_ready": face.base_texture_ready,
                        "has_emissive_texture": face.has_emissive_texture,
                        "emissive_texture_ready": face.emissive_texture_ready,
                        "material_shading": face.material_shading,
                        "material_reason": face.material_reason,
                    })
                })
                .collect();
            json!({
                "in_scene": exp.in_scene,
                "placement": exp.placement,
                "hidden_reason": exp.hidden_reason,
                "geometry": exp.geometry,
                "has_mesh": exp.has_mesh,
                "mesh_matches_geometry": exp.mesh_matches_geometry,
                "in_cache": exp.in_cache,
                "visibility": format!("{:?}", exp.visibility),
                "mesh_residency": format!("{:?}", exp.mesh_residency),
                "faces": faces,
            })
        });

        let world_json = world_detail.as_ref().map(format_world_detail);

        Ok(ToolContent::Json(json!({
            "found": true,
            "object_id": id_str,
            "world": world_json,
            "render": render_json,
        })))
    })
}

fn format_world_detail(detail: &ObjectDetail) -> Value {
    json!({
        "entity": format!("{:?}", detail.summary.entity),
        "id": detail.summary.id.to_string(),
        "region": detail.summary.address.region().get(),
        "local_id": detail.summary.address.local_id(),
        "kind": format!("{:?}", detail.summary.kind),
        "shape": detail.shape.as_ref().map(format_shape),
        "is_physical": detail.is_physical,
        "attachment_point": detail.attachment_point.map(|point| format!("{point:?}")),
    })
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
        assert_eq!(output["shape"]["kind"], "mesh");
        assert_eq!(output["shape"]["flags"]["mirror"], true);
        assert!(!output.to_string().contains(&secret_mesh.to_string()));
    }
}
