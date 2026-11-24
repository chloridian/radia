use rmcp::{ErrorData, handler::server::wrapper::Json, tool, tool_router};
use schemars::JsonSchema;
use serde::Serialize;

use crate::mcp::server::Handler;

#[derive(Serialize, JsonSchema)]
pub struct ObjectCacheHandshakeOutput {
    pub started: bool,
    pub ready: bool,
    pub disabled: bool,
    pub degraded: bool,
    pub identified_regions: usize,
    pub loaded_regions: usize,
    pub in_flight_region_loads: usize,
    pub loaded_entries: usize,
    pub loaded_bytes: usize,
}

#[derive(Serialize, JsonSchema)]
pub struct ObjectCacheWriteQueueOutput {
    pub depth: usize,
    pub capacity: usize,
    pub bytes: usize,
    pub byte_capacity: usize,
    pub outstanding_writes: usize,
    pub pending_submissions: usize,
    pub pending_submissions_capacity: usize,
    pub busy_drops: u64,
    pub full_drops: u64,
    pub dropped_writes: u64,
}

#[derive(Serialize, JsonSchema)]
pub struct ObjectCacheOutput {
    pub handshake: ObjectCacheHandshakeOutput,
    pub write_queue: ObjectCacheWriteQueueOutput,
}

#[derive(Serialize, JsonSchema)]
pub struct AssetEntriesByKindOutput {
    pub textures: usize,
    pub pbr_materials: usize,
    pub legacy_materials: usize,
    pub environments: usize,
    pub meshes: usize,
}

#[derive(Serialize, JsonSchema)]
pub struct PreparedCacheOutput {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub corrupt_entries: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub size_bytes: u64,
    pub size_limit_bytes: u64,
    pub entries: usize,
}

#[derive(Serialize, JsonSchema)]
pub struct AssetDiskCacheOutput {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub corrupt_entries: u64,
    pub dropped_writes: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub size_bytes: u64,
    pub size_limit_bytes: u64,
    pub protected_bytes: u64,
    pub entries_by_kind: AssetEntriesByKindOutput,
    pub prepared: PreparedCacheOutput,
}

#[derive(Serialize, JsonSchema)]
pub struct CacheOutput {
    pub active_session: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_cache: Option<ObjectCacheOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset_disk_cache: Option<AssetDiskCacheOutput>,
}

#[tool_router(router = cache_router, vis = "pub(super)")]
impl Handler {
    #[tool(
        name = "cache",
        title = "Cache Statistics",
        description = "Return object cache write-queue depth, capacity, busy/full drops, handshake state, and asset disk cache stats.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn cache(&self) -> Result<Json<CacheOutput>, ErrorData> {
        let Some(session) = self.handle.session() else {
            return Ok(Json(CacheOutput { active_session: false, object_cache: None, asset_disk_cache: None }));
        };
        let obj = *session.diagnostics.object_cache.borrow();
        let asset_disk_cache = session.assets.cache_stats().map(|stats| AssetDiskCacheOutput {
            hits: stats.hits,
            misses: stats.misses,
            evictions: stats.evictions,
            corrupt_entries: stats.corrupt_entries,
            dropped_writes: stats.dropped_writes,
            bytes_read: stats.bytes_read,
            bytes_written: stats.bytes_written,
            size_bytes: stats.size_bytes,
            size_limit_bytes: stats.size_limit_bytes,
            protected_bytes: stats.protected_bytes,
            entries_by_kind: AssetEntriesByKindOutput {
                textures: stats.entries_by_kind.textures,
                pbr_materials: stats.entries_by_kind.pbr_materials,
                legacy_materials: stats.entries_by_kind.legacy_materials,
                environments: stats.entries_by_kind.environments,
                meshes: stats.entries_by_kind.meshes,
            },
            prepared: PreparedCacheOutput {
                hits: stats.prepared_hits,
                misses: stats.prepared_misses,
                evictions: stats.prepared_evictions,
                corrupt_entries: stats.prepared_corrupt_entries,
                bytes_read: stats.prepared_bytes_read,
                bytes_written: stats.prepared_bytes_written,
                size_bytes: stats.prepared_size_bytes,
                size_limit_bytes: stats.prepared_size_limit_bytes,
                entries: stats.prepared_entries,
            },
        });
        Ok(Json(CacheOutput {
            active_session: true,
            object_cache: Some(ObjectCacheOutput {
                handshake: ObjectCacheHandshakeOutput {
                    started: obj.started,
                    ready: obj.ready,
                    disabled: obj.disabled,
                    degraded: obj.degraded,
                    identified_regions: obj.identified_regions,
                    loaded_regions: obj.loaded_regions,
                    in_flight_region_loads: obj.in_flight_region_loads,
                    loaded_entries: obj.loaded_entries,
                    loaded_bytes: obj.loaded_bytes,
                },
                write_queue: ObjectCacheWriteQueueOutput {
                    depth: obj.write_queue_depth,
                    capacity: obj.write_queue_capacity,
                    bytes: obj.write_queue_bytes,
                    byte_capacity: obj.write_queue_byte_capacity,
                    outstanding_writes: obj.outstanding_writes,
                    pending_submissions: obj.pending_submissions,
                    pending_submissions_capacity: obj.pending_submissions_capacity,
                    busy_drops: obj.busy_drops,
                    full_drops: obj.full_drops,
                    dropped_writes: obj.dropped_writes,
                },
            }),
            asset_disk_cache,
        }))
    }
}
