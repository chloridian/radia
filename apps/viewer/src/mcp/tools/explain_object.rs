use serde_json::{Value, json};
use tokio::sync::oneshot;
use world::ObjectDetailResult;

use super::{ToolContent, ToolFuture, ToolSpec, lookup_object_entity};
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
        session
            .viewport_queries
            .try_send(ViewportQuery::ExplainObject { entity, reply: reply_tx })
            .map_err(|_| "viewport query queue busy or closed".to_owned())?;
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

        let world_json = world_detail.map(|detail| {
            json!({
                "entity": format!("{:?}", detail.summary.entity),
                "id": detail.summary.id.to_string(),
                "region": detail.summary.address.region().get(),
                "local_id": detail.summary.address.local_id(),
                "kind": format!("{:?}", detail.summary.kind),
                "shape": detail.shape.map(|shape| format!("{shape:?}")),
                "is_physical": detail.is_physical,
                "attachment_point": detail.attachment_point.map(|point| format!("{point:?}")),
            })
        });

        Ok(ToolContent::Json(json!({
            "found": true,
            "object_id": id_str,
            "world": world_json,
            "render": render_json,
        })))
    })
}
