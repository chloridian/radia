use serde_json::{Value, json};

use super::{ToolContent, ToolFuture, ToolSpec};
use crate::{
    inspection::{BoundedQueueStats, ViewerInspectionHandle},
    mcp::MAX_JSON_RESPONSE_BYTES,
};

pub const SPEC: ToolSpec = ToolSpec {
    name: "queues",
    description: "Return depth, capacity, and lifetime full/rejected counts for bounded viewer queues grouped by subsystem.",
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

fn queue_json(stats: BoundedQueueStats) -> Value {
    json!({
        "depth": stats.depth,
        "capacity": stats.capacity,
        "full_count": stats.full_count,
    })
}

fn handler(handle: ViewerInspectionHandle, _args: Value) -> ToolFuture {
    Box::pin(async move {
        let Some(session) = handle.session() else {
            return Ok(ToolContent::Json(json!({ "active_session": false })));
        };

        let obj_cache = *session.diagnostics.object_cache.borrow();
        let session_queues = *session.diagnostics.session_queues.borrow();
        let render_stats = *session.render_stats.borrow();

        let world_reads = session.world.queue_stats();
        let asset_commands = session.assets.command_queue_stats();
        let inventory_fetch = session.diagnostics.inventory_fetch.queue_stats();
        let inventory_edits = session.diagnostics.inventory_fetch.edits().queue_stats();
        let network_commands = session.diagnostics.network_session.command_queue_stats();
        let viewport_queries = session.viewport_query_queue_stats();

        Ok(ToolContent::Json(json!({
            "active_session": true,
            "object_cache": {
                "write_queue": {
                    "depth": obj_cache.write_queue_depth,
                    "capacity": obj_cache.write_queue_capacity,
                    "bytes": obj_cache.write_queue_bytes,
                    "byte_capacity": obj_cache.write_queue_byte_capacity,
                    "full_count": obj_cache.full_drops,
                    "busy_count": obj_cache.busy_drops,
                    "dropped_writes": obj_cache.dropped_writes,
                },
                "pending_submissions": {
                    "depth": obj_cache.pending_submissions,
                    "capacity": obj_cache.pending_submissions_capacity,
                },
            },
            "world": {
                "read_requests": queue_json(world_reads),
            },
            "assets": {
                "commands": queue_json(asset_commands),
            },
            "inventory": {
                "fetch_requests": queue_json(inventory_fetch),
                "edit_requests": queue_json(inventory_edits),
            },
            "network": {
                "session_commands": {
                    "depth": network_commands.depth,
                    "capacity": network_commands.capacity,
                    "full_count": network_commands.full_count,
                },
                "client_events": {
                    "depth": session_queues.client_events_depth,
                    "capacity": session_queues.client_events_capacity,
                },
            },
            "session": {
                "experience_environment": queue_json(session_queues.experience_environment),
                "object_media_requests": session_queues.object_media.map(queue_json),
            },
            "viewport": {
                "queries": queue_json(viewport_queries),
                "texture_completions": {
                    "depth": Value::Null,
                    "capacity": render_stats.texture_completions_capacity,
                    "full_count": render_stats.texture_completions_dropped,
                },
            },
        })))
    })
}
