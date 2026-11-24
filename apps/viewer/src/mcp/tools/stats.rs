use serde_json::{Value, json};

use super::{ToolAnnotationsSpec, ToolContent, ToolFuture, ToolSpec};
use crate::{
    inspection::{FRAME_SPIKE_CAPACITY, ViewerInspectionHandle},
    mcp::MAX_JSON_RESPONSE_BYTES,
};

pub const SPEC: ToolSpec = ToolSpec {
    name: "stats",
    title: "Viewer Stats",
    description: "Return frame timing, recent frame spikes (>2x median), renderer stats, asset queue stats, and memory stats.",
    annotations: ToolAnnotationsSpec::read_only(),
    input_schema,
    handler,
    max_response_bytes: MAX_JSON_RESPONSE_BYTES,
};

fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "limit": { "type": "integer", "minimum": 1, "maximum": FRAME_SPIKE_CAPACITY }
        }
    })
}

fn handler(handle: ViewerInspectionHandle, args: Value) -> ToolFuture {
    Box::pin(async move {
        let Some(session) = handle.session() else {
            return Ok(ToolContent::Json(json!({ "active_session": false })));
        };
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(FRAME_SPIKE_CAPACITY)
            .clamp(1, FRAME_SPIKE_CAPACITY);
        let render_snapshot = *session.render_stats.borrow();
        let rs = render_snapshot.render_stats;
        let queue = session.assets.queue_stats();
        let memory = session.assets.memory_stats();
        let total_spikes = render_snapshot.frame_spikes.len();
        let spikes = render_snapshot
            .frame_spikes
            .iter()
            .skip(total_spikes.saturating_sub(limit))
            .map(|spike| {
                json!({
                    "work_ms": spike.work_ms,
                    "frame_ms": spike.frame_ms,
                    "game_ms": spike.game_ms,
                    "render_ms": spike.render_ms,
                    "gpu_timings_ms": spike.gpu_timings_ms,
                })
            })
            .collect::<Vec<_>>();
        Ok(ToolContent::Json(json!({
            "active_session": true,
            "viewport_size": render_snapshot.viewport_size,
            "frame": {
                "fps": render_snapshot.fps,
                "frame_ms": render_snapshot.frame_ms,
                "game_ms": render_snapshot.game_ms,
                "render_ms": render_snapshot.render_ms,
                "gpu_timings_ms": render_snapshot.gpu_timings,
                "spikes": spikes,
                "has_more_spikes": total_spikes > limit,
            },
            "render": {
                "instances": rs.instances,
                "draw_calls": rs.draw_calls,
                "draw_commands": rs.draw_commands,
                "triangles": rs.triangles,
                "pending_meshes": rs.pending_meshes,
                "pending_textures": rs.pending_textures,
                "omitted_faces": rs.omitted_faces,
                "mesh_budget_blocked": rs.mesh_budget_blocked,
                "omission_reason": rs.omission_reason.map(|reason| format!("{reason:?}")),
                "mesh_bytes": rs.mesh_bytes,
                "texture_bytes": rs.texture_bytes,
                "texture_count": rs.texture_count,
                "vram_bytes": rs.vram_bytes(),
                "prepare_ms": rs.prepare_ms,
                "rebuild_pending": rs.rebuild_pending,
            },
            "assets": {
                "queue": {
                    "queued_mesh": queue.queued_mesh,
                    "queued_legacy_material": queue.queued_legacy_material,
                    "queued_other": queue.queued_other,
                    "running_fetches": queue.running_fetches,
                    "running_cache_reads": queue.running_cache_reads,
                    "running_texture_decodes": queue.running_texture_decodes,
                    "running_other_decodes": queue.running_other_decodes,
                    "empty": queue.is_empty(),
                },
                "memory": format!("{memory:?}"),
            }
        })))
    })
}
