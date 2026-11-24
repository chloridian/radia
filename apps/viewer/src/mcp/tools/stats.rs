use render::OmissionReason;
use rmcp::{
    ErrorData,
    handler::server::wrapper::{Json, Parameters},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    inspection::{FRAME_SPIKE_CAPACITY, ViewerInspectionHandle},
    mcp::server::Handler,
};

#[derive(Deserialize, JsonSchema)]
pub struct StatsArgs {
    #[schemars(description = "Maximum number of recent frame spikes to return (1..=32).")]
    pub limit: Option<usize>,
}

#[derive(Serialize, JsonSchema)]
pub struct FrameSpikeOutput {
    pub work_ms: f64,
    pub frame_ms: f64,
    pub game_ms: f64,
    pub render_ms: f64,
    pub gpu_timings_ms: Option<(f64, f64)>,
}

#[derive(Serialize, JsonSchema)]
pub struct FrameStatsOutput {
    pub fps: f64,
    pub frame_ms: f64,
    pub game_ms: f64,
    pub render_ms: f64,
    pub gpu_timings_ms: Option<(f64, f64)>,
    pub spikes: Vec<FrameSpikeOutput>,
    pub has_more_spikes: bool,
}

#[derive(Serialize, JsonSchema)]
pub struct RenderStatsOutput {
    pub instances: usize,
    pub draw_calls: usize,
    pub draw_commands: usize,
    pub triangles: u64,
    pub pending_meshes: usize,
    pub pending_textures: usize,
    pub omitted_faces: usize,
    pub mesh_budget_blocked: usize,
    pub omission_reason: Option<&'static str>,
    pub mesh_bytes: usize,
    pub texture_bytes: usize,
    pub texture_count: usize,
    pub vram_bytes: usize,
    pub prepare_ms: f64,
    pub rebuild_pending: bool,
}

#[derive(Serialize, JsonSchema)]
pub struct AssetQueueOutput {
    pub queued_mesh: usize,
    pub queued_legacy_material: usize,
    pub queued_other: usize,
    pub running_fetches: usize,
    pub running_cache_reads: usize,
    pub running_texture_decodes: usize,
    pub running_other_decodes: usize,
    pub empty: bool,
}

#[derive(Serialize, JsonSchema)]
pub struct AssetMemoryOutput {
    pub charged_bytes: usize,
    pub resident_bytes: usize,
    pub high_water_bytes: usize,
}

#[derive(Serialize, JsonSchema)]
pub struct AssetsStatsOutput {
    pub queue: AssetQueueOutput,
    pub memory: AssetMemoryOutput,
}

#[derive(Serialize, JsonSchema)]
pub struct StatsOutput {
    pub active_session: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub viewport_size: Option<[u32; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame: Option<FrameStatsOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub render: Option<RenderStatsOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assets: Option<AssetsStatsOutput>,
}

#[tool_router(router = stats_router, vis = "pub(super)")]
impl Handler {
    #[tool(
        name = "stats",
        title = "Viewer Stats",
        description = "Return frame timing, recent frame spikes (>2x median), renderer stats, asset queue stats, and memory stats.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn stats(&self, Parameters(args): Parameters<StatsArgs>) -> Result<Json<StatsOutput>, ErrorData> {
        Ok(Json(stats_data(&self.handle, args.limit)))
    }
}

pub(in crate::mcp) fn stats_data(handle: &ViewerInspectionHandle, limit: Option<usize>) -> StatsOutput {
    let Some(session) = handle.session() else {
        return StatsOutput { active_session: false, viewport_size: None, frame: None, render: None, assets: None };
    };
    let limit = limit.unwrap_or(FRAME_SPIKE_CAPACITY).clamp(1, FRAME_SPIKE_CAPACITY);
    let render_snapshot = *session.render_stats.borrow();
    let rs = render_snapshot.render_stats;
    let queue = session.assets.queue_stats();
    let memory = session.assets.memory_stats();
    let total_spikes = render_snapshot.frame_spikes.len();
    let spikes = render_snapshot
        .frame_spikes
        .iter()
        .skip(total_spikes.saturating_sub(limit))
        .map(|spike| FrameSpikeOutput {
            work_ms: spike.work_ms,
            frame_ms: spike.frame_ms,
            game_ms: spike.game_ms,
            render_ms: spike.render_ms,
            gpu_timings_ms: spike.gpu_timings_ms,
        })
        .collect();
    StatsOutput {
        active_session: true,
        viewport_size: Some(render_snapshot.viewport_size),
        frame: Some(FrameStatsOutput {
            fps: render_snapshot.fps,
            frame_ms: render_snapshot.frame_ms,
            game_ms: render_snapshot.game_ms,
            render_ms: render_snapshot.render_ms,
            gpu_timings_ms: render_snapshot.gpu_timings,
            spikes,
            has_more_spikes: total_spikes > limit,
        }),
        render: Some(RenderStatsOutput {
            instances: rs.instances,
            draw_calls: rs.draw_calls,
            draw_commands: rs.draw_commands,
            triangles: rs.triangles,
            pending_meshes: rs.pending_meshes,
            pending_textures: rs.pending_textures,
            omitted_faces: rs.omitted_faces,
            mesh_budget_blocked: rs.mesh_budget_blocked,
            omission_reason: rs.omission_reason.map(|reason| match reason {
                OmissionReason::InstanceBufferBudget => "instance_buffer_budget",
                OmissionReason::MeshMemoryBudget => "mesh_memory_budget",
                OmissionReason::CombinedBudgets => "combined_budgets",
            }),
            mesh_bytes: rs.mesh_bytes,
            texture_bytes: rs.texture_bytes,
            texture_count: rs.texture_count,
            vram_bytes: rs.vram_bytes(),
            prepare_ms: rs.prepare_ms,
            rebuild_pending: rs.rebuild_pending,
        }),
        assets: Some(AssetsStatsOutput {
            queue: AssetQueueOutput {
                queued_mesh: queue.queued_mesh,
                queued_legacy_material: queue.queued_legacy_material,
                queued_other: queue.queued_other,
                running_fetches: queue.running_fetches,
                running_cache_reads: queue.running_cache_reads,
                running_texture_decodes: queue.running_texture_decodes,
                running_other_decodes: queue.running_other_decodes,
                empty: queue.is_empty(),
            },
            memory: AssetMemoryOutput {
                charged_bytes: memory.charged_bytes,
                resident_bytes: memory.resident_bytes,
                high_water_bytes: memory.high_water_bytes,
            },
        }),
    }
}
