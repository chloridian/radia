use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::{ToolContent, ToolFuture, ToolSpec};
use crate::{
    inspection::{ViewerInspectionHandle, ViewportQuery},
    mcp::MAX_JSON_RESPONSE_BYTES,
};

pub const SPEC: ToolSpec = ToolSpec {
    name: "camera",
    description: "Return camera mode, position, orientation, agent region name and handle, and agent region-local position.",
    input_schema,
    handler,
    max_response_bytes: MAX_JSON_RESPONSE_BYTES,
};

fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {}
    })
}

fn handler(handle: ViewerInspectionHandle, _args: Value) -> ToolFuture {
    Box::pin(async move {
        let Some(session) = handle.session() else {
            return Ok(ToolContent::Json(json!({ "active_session": false })));
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        session
            .viewport_queries
            .try_send(ViewportQuery::CameraState { reply: reply_tx })
            .map_err(|_| "viewport query queue busy or closed".to_owned())?;
        let camera = reply_rx.await.map_err(|_| "viewport dropped camera request".to_owned())?;

        let sample = *session.agent_motion.borrow();
        let agent = match sample {
            Some(sample) => {
                let regions = session.world.regions(None, 64).await.map_err(|e| format!("regions query failed: {e}"))?;
                let region_info = regions.regions.into_iter().find(|entry| entry.region == sample.region).map(|entry| entry.info);
                let local_position = region_info.as_ref().and_then(|info| info.resolved_origin()).map(|origin| {
                    let local = sample.position - origin;
                    [local.x, local.y, local.z]
                });
                json!({
                    "region": sample.region.get(),
                    "region_name": region_info.as_ref().and_then(|info| info.name.as_deref()),
                    "region_handle": region_info.as_ref().and_then(|info| info.handle).map(|handle| handle.get()),
                    "global_position": [sample.position.x, sample.position.y, sample.position.z],
                    "local_position": local_position,
                })
            }
            None => Value::Null,
        };

        Ok(ToolContent::Json(json!({
            "active_session": true,
            "camera": {
                "mode": camera.mode,
                "position": camera.position,
                "direction": camera.direction,
                "vertical_fov_deg": camera.vertical_fov_deg,
                "draw_distance": camera.draw_distance,
                "lod_factor": camera.lod_factor,
            },
            "agent": agent,
        })))
    })
}
