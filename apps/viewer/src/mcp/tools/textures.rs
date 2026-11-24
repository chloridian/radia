use std::collections::BTreeMap;

use rmcp::{ErrorData, handler::server::wrapper::Json, tool, tool_router};
use schemars::JsonSchema;
use serde::Serialize;

use crate::mcp::server::Handler;

#[derive(Serialize, JsonSchema)]
pub struct TextureFetchQueueOutput {
    pub visible: usize,
    pub background: usize,
    pub total: usize,
}

#[derive(Serialize, JsonSchema)]
pub struct TextureDecodeQueueOutput {
    pub visible: usize,
    pub background: usize,
    pub running: usize,
    pub total_queued: usize,
}

#[derive(Serialize, JsonSchema)]
pub struct TextureGpuMemoryOutput {
    pub used_bytes: usize,
    pub budget_bytes: usize,
    pub texture_count: usize,
    pub pending_textures: usize,
    pub staging_bytes: usize,
    pub upload_bytes: usize,
}

#[derive(Serialize, JsonSchema)]
pub struct TexturesOutput {
    pub active_session: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetch_queue: Option<TextureFetchQueueOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decode_queue: Option<TextureDecodeQueueOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discard_levels: Option<BTreeMap<String, usize>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_memory: Option<TextureGpuMemoryOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetches_in_flight_over_30s: Option<usize>,
}

#[tool_router(router = textures_router, vis = "pub(super)")]
impl Handler {
    #[tool(
        name = "textures",
        title = "Texture Pipeline Stats",
        description = "Return aggregate texture fetch/decode queue depths by priority, discard-level histogram, GPU texture memory vs budget, and slow in-flight fetches (>30s).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn textures(&self) -> Result<Json<TexturesOutput>, ErrorData> {
        let Some(session) = self.handle.session() else {
            return Ok(Json(TexturesOutput {
                active_session: false,
                fetch_queue: None,
                decode_queue: None,
                discard_levels: None,
                gpu_memory: None,
                fetches_in_flight_over_30s: None,
            }));
        };
        let tex = session.assets.texture_stats().await.map_err(|_| ErrorData::internal_error("asset coordinator is unavailable", None))?;
        let rs = session.render_stats.borrow().render_stats;
        let mut discard_levels = BTreeMap::new();
        for (level, count) in tex.discard_histogram.iter().enumerate() {
            if *count > 0 {
                discard_levels.insert(level.to_string(), *count);
            }
        }
        Ok(Json(TexturesOutput {
            active_session: true,
            fetch_queue: Some(TextureFetchQueueOutput {
                visible: tex.fetch_queued_visible,
                background: tex.fetch_queued_background,
                total: tex.fetch_queued_visible.saturating_add(tex.fetch_queued_background),
            }),
            decode_queue: Some(TextureDecodeQueueOutput {
                visible: tex.decode_queued_visible,
                background: tex.decode_queued_background,
                running: tex.running_texture_decodes,
                total_queued: tex.decode_queued_visible.saturating_add(tex.decode_queued_background),
            }),
            discard_levels: Some(discard_levels),
            gpu_memory: Some(TextureGpuMemoryOutput {
                used_bytes: rs.texture_bytes,
                budget_bytes: rs.texture_budget_bytes,
                texture_count: rs.texture_count,
                pending_textures: rs.pending_textures,
                staging_bytes: rs.staging_bytes,
                upload_bytes: rs.texture_upload_bytes,
            }),
            fetches_in_flight_over_30s: Some(tex.fetches_in_flight_over_30s),
        }))
    }
}
