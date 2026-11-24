use serde_json::{Map, Value, json};

use super::{ToolContent, ToolFuture, ToolSpec};
use crate::{inspection::ViewerInspectionHandle, mcp::MAX_JSON_RESPONSE_BYTES};

pub const SPEC: ToolSpec = ToolSpec {
    name: "textures",
    description: "Return aggregate texture fetch/decode queue depths by priority, discard-level histogram, GPU texture memory vs budget, and slow in-flight fetches (>30s).",
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
        let tex = session.assets.texture_stats();
        let rs = session.render_stats.borrow().render_stats;
        let mut discard_levels = Map::new();
        for (level, count) in tex.discard_histogram.iter().enumerate() {
            if *count > 0 {
                discard_levels.insert(level.to_string(), Value::from(*count));
            }
        }
        Ok(ToolContent::Json(json!({
            "active_session": true,
            "fetch_queue": {
                "visible": tex.fetch_queued_visible,
                "background": tex.fetch_queued_background,
                "total": tex.fetch_queued_visible.saturating_add(tex.fetch_queued_background),
            },
            "decode_queue": {
                "visible": tex.decode_queued_visible,
                "background": tex.decode_queued_background,
                "running": tex.running_texture_decodes,
                "total_queued": tex.decode_queued_visible.saturating_add(tex.decode_queued_background),
            },
            "discard_levels": discard_levels,
            "gpu_memory": {
                "used_bytes": rs.texture_bytes,
                "budget_bytes": rs.texture_budget_bytes,
                "texture_count": rs.texture_count,
                "pending_textures": rs.pending_textures,
                "staging_bytes": rs.staging_bytes,
                "upload_bytes": rs.texture_upload_bytes,
            },
            "fetches_in_flight_over_30s": tex.fetches_in_flight_over_30s,
        })))
    })
}
