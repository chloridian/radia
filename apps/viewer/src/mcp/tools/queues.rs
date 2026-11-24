use rmcp::{ErrorData, handler::server::wrapper::Json, tool, tool_router};
use schemars::JsonSchema;
use serde::Serialize;

use crate::{inspection::BoundedQueueStats, mcp::server::Handler};

#[derive(Serialize, JsonSchema)]
pub struct QueueStatsOutput {
    pub depth: usize,
    pub capacity: usize,
    pub full_count: u64,
}

#[derive(Serialize, JsonSchema)]
pub struct DepthCapacityOutput {
    pub depth: usize,
    pub capacity: usize,
}

#[derive(Serialize, JsonSchema)]
pub struct ObjectCacheWriteQueueStatsOutput {
    pub depth: usize,
    pub capacity: usize,
    pub bytes: usize,
    pub byte_capacity: usize,
    pub full_count: u64,
    pub busy_count: u64,
    pub dropped_writes: u64,
}

#[derive(Serialize, JsonSchema)]
pub struct ObjectCacheQueuesOutput {
    pub write_queue: ObjectCacheWriteQueueStatsOutput,
    pub pending_submissions: DepthCapacityOutput,
}

#[derive(Serialize, JsonSchema)]
pub struct WorldQueuesOutput {
    pub read_requests: QueueStatsOutput,
}

#[derive(Serialize, JsonSchema)]
pub struct AssetQueuesOutput {
    pub commands: QueueStatsOutput,
}

#[derive(Serialize, JsonSchema)]
pub struct InventoryQueuesOutput {
    pub fetch_requests: QueueStatsOutput,
    pub edit_requests: QueueStatsOutput,
}

#[derive(Serialize, JsonSchema)]
pub struct NetworkQueuesOutput {
    pub session_commands: QueueStatsOutput,
    pub client_events: DepthCapacityOutput,
}

#[derive(Serialize, JsonSchema)]
pub struct SessionQueuesOutput {
    pub experience_environment: QueueStatsOutput,
    pub object_media_requests: Option<QueueStatsOutput>,
}

#[derive(Serialize, JsonSchema)]
pub struct TextureCompletionsQueueOutput {
    pub depth: Option<usize>,
    pub capacity: usize,
    pub full_count: u64,
}

#[derive(Serialize, JsonSchema)]
pub struct ViewportQueuesOutput {
    pub queries: QueueStatsOutput,
    pub texture_completions: TextureCompletionsQueueOutput,
}

#[derive(Serialize, JsonSchema)]
pub struct QueuesOutput {
    pub active_session: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_cache: Option<ObjectCacheQueuesOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub world: Option<WorldQueuesOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assets: Option<AssetQueuesOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inventory: Option<InventoryQueuesOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkQueuesOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionQueuesOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub viewport: Option<ViewportQueuesOutput>,
}

fn queue_output(stats: BoundedQueueStats) -> QueueStatsOutput {
    QueueStatsOutput { depth: stats.depth, capacity: stats.capacity, full_count: stats.full_count }
}

#[tool_router(router = queues_router, vis = "pub(super)")]
impl Handler {
    #[tool(
        name = "queues",
        title = "Subsystem Queues",
        description = "Return depth, capacity, and lifetime full/rejected counts for bounded viewer queues grouped by subsystem.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn queues(&self) -> Result<Json<QueuesOutput>, ErrorData> {
        let Some(session) = self.handle.session() else {
            return Ok(Json(QueuesOutput {
                active_session: false,
                object_cache: None,
                world: None,
                assets: None,
                inventory: None,
                network: None,
                session: None,
                viewport: None,
            }));
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

        Ok(Json(QueuesOutput {
            active_session: true,
            object_cache: Some(ObjectCacheQueuesOutput {
                write_queue: ObjectCacheWriteQueueStatsOutput {
                    depth: obj_cache.write_queue_depth,
                    capacity: obj_cache.write_queue_capacity,
                    bytes: obj_cache.write_queue_bytes,
                    byte_capacity: obj_cache.write_queue_byte_capacity,
                    full_count: obj_cache.full_drops,
                    busy_count: obj_cache.busy_drops,
                    dropped_writes: obj_cache.dropped_writes,
                },
                pending_submissions: DepthCapacityOutput {
                    depth: obj_cache.pending_submissions,
                    capacity: obj_cache.pending_submissions_capacity,
                },
            }),
            world: Some(WorldQueuesOutput { read_requests: queue_output(world_reads) }),
            assets: Some(AssetQueuesOutput { commands: queue_output(asset_commands) }),
            inventory: Some(InventoryQueuesOutput {
                fetch_requests: queue_output(inventory_fetch),
                edit_requests: queue_output(inventory_edits),
            }),
            network: Some(NetworkQueuesOutput {
                session_commands: QueueStatsOutput {
                    depth: network_commands.depth,
                    capacity: network_commands.capacity,
                    full_count: network_commands.full_count,
                },
                client_events: DepthCapacityOutput {
                    depth: session_queues.client_events_depth,
                    capacity: session_queues.client_events_capacity,
                },
            }),
            session: Some(SessionQueuesOutput {
                experience_environment: queue_output(session_queues.experience_environment),
                object_media_requests: session_queues.object_media.map(queue_output),
            }),
            viewport: Some(ViewportQueuesOutput {
                queries: queue_output(viewport_queries),
                texture_completions: TextureCompletionsQueueOutput {
                    depth: None,
                    capacity: render_stats.texture_completions_capacity,
                    full_count: render_stats.texture_completions_dropped,
                },
            }),
        }))
    }
}
