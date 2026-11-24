use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::{ToolContent, ToolFuture, ToolSpec};
use crate::{
    inspection::{ViewerInspectionHandle, ViewportQuery},
    mcp::MAX_JSON_RESPONSE_BYTES,
};

pub const SPEC: ToolSpec = ToolSpec {
    name: "pick",
    description: "Pick the rendered geometry at viewport pixel coordinates (x, y) and return hit point, object, face, and texture presence.",
    input_schema,
    handler,
    max_response_bytes: MAX_JSON_RESPONSE_BYTES,
};

fn input_schema() -> Value {
    json!({
        "type": "object",
        "required": ["x", "y"],
        "properties": {
            "x": { "type": "number" },
            "y": { "type": "number" }
        }
    })
}

fn handler(handle: ViewerInspectionHandle, args: Value) -> ToolFuture {
    Box::pin(async move {
        let Some(session) = handle.session() else {
            return Ok(ToolContent::Json(json!({ "active_session": false })));
        };
        let Some(x) = args.get("x").and_then(Value::as_f64) else {
            return Err("x coordinate is required".to_owned());
        };
        let Some(y) = args.get("y").and_then(Value::as_f64) else {
            return Err("y coordinate is required".to_owned());
        };

        let (reply_tx, reply_rx) = oneshot::channel();
        session
            .viewport_queries
            .try_send(ViewportQuery::PixelPick { x: x as f32, y: y as f32, reply: reply_tx })
            .map_err(|_| "viewport query queue busy or closed".to_owned())?;
        let pick_result =
            reply_rx.await.map_err(|_| "viewport dropped pick request".to_owned())?.map_err(|e| format!("pick failed: {e}"))?;

        match pick_result {
            Some(detail) => Ok(ToolContent::Json(json!({
                "hit": true,
                "point": [detail.point.x, detail.point.y, detail.point.z],
                "entity": detail.entity.map(|entity| format!("{entity:?}")),
                "object_id": detail.object_id.map(|id| id.to_string()),
                "region": detail.region.get(),
                "face_range": [detail.face_range.start, detail.face_range.end],
                "material_slot": detail.material_slot,
                "has_base_texture": detail.has_base_texture,
                "has_emissive_texture": detail.has_emissive_texture,
                "double_sided": detail.double_sided,
                "blended": detail.blended,
            }))),
            None => Ok(ToolContent::Json(json!({ "hit": false }))),
        }
    })
}
