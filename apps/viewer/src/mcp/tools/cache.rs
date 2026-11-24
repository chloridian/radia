use serde_json::{Value, json};

use super::{ToolContent, ToolFuture, ToolSpec};
use crate::{inspection::ViewerInspectionHandle, mcp::MAX_JSON_RESPONSE_BYTES};

pub const SPEC: ToolSpec = ToolSpec {
    name: "cache",
    description: "Return object cache write-queue depth, capacity, busy/full drops, handshake state, and asset disk cache stats.",
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
        let obj = *session.diagnostics.object_cache.borrow();
        let asset_disk_cache = session.assets.cache_stats().map(|stats| {
            json!({
                "hits": stats.hits,
                "misses": stats.misses,
                "evictions": stats.evictions,
                "corrupt_entries": stats.corrupt_entries,
                "dropped_writes": stats.dropped_writes,
                "bytes_read": stats.bytes_read,
                "bytes_written": stats.bytes_written,
                "size_bytes": stats.size_bytes,
                "size_limit_bytes": stats.size_limit_bytes,
                "protected_bytes": stats.protected_bytes,
                "entries_by_kind": {
                    "textures": stats.entries_by_kind.textures,
                    "pbr_materials": stats.entries_by_kind.pbr_materials,
                    "legacy_materials": stats.entries_by_kind.legacy_materials,
                    "environments": stats.entries_by_kind.environments,
                    "meshes": stats.entries_by_kind.meshes,
                },
                "prepared": {
                    "hits": stats.prepared_hits,
                    "misses": stats.prepared_misses,
                    "evictions": stats.prepared_evictions,
                    "corrupt_entries": stats.prepared_corrupt_entries,
                    "bytes_read": stats.prepared_bytes_read,
                    "bytes_written": stats.prepared_bytes_written,
                    "size_bytes": stats.prepared_size_bytes,
                    "size_limit_bytes": stats.prepared_size_limit_bytes,
                    "entries": stats.prepared_entries,
                },
            })
        });

        Ok(ToolContent::Json(json!({
            "active_session": true,
            "object_cache": {
                "handshake": {
                    "started": obj.started,
                    "ready": obj.ready,
                    "disabled": obj.disabled,
                    "degraded": obj.degraded,
                    "identified_regions": obj.identified_regions,
                    "loaded_regions": obj.loaded_regions,
                    "in_flight_region_loads": obj.in_flight_region_loads,
                    "loaded_entries": obj.loaded_entries,
                    "loaded_bytes": obj.loaded_bytes,
                },
                "write_queue": {
                    "depth": obj.write_queue_depth,
                    "capacity": obj.write_queue_capacity,
                    "bytes": obj.write_queue_bytes,
                    "byte_capacity": obj.write_queue_byte_capacity,
                    "outstanding_writes": obj.outstanding_writes,
                    "worker_capacity": obj.worker_queue_capacity,
                    "pending_submissions": obj.pending_submissions,
                    "pending_submissions_capacity": obj.pending_submissions_capacity,
                    "busy_drops": obj.busy_drops,
                    "full_drops": obj.full_drops,
                    "dropped_writes": obj.dropped_writes,
                },
            },
            "asset_disk_cache": asset_disk_cache,
        })))
    })
}
