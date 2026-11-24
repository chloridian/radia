mod png;
mod prompts;
mod resources;
mod server;
mod tools;

use std::time::Duration;

pub use self::server::{McpServer, McpServerConfig};

pub const DEFAULT_PORT: u16 = 47100;
pub const MAX_CONCURRENT_REQUESTS: usize = 4;
pub const MAX_SESSIONS: usize = 8;
pub const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
pub const MAX_JSON_RESPONSE_BYTES: usize = 512 * 1024;
pub const MAX_PNG_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(35);

pub const SERVER_INSTRUCTIONS: &str = "\
Verify runtime viewer state through these tools and `viewer://` resources rather than guessing from code or logs.

Tool guide:
- Scene state: `world` (regions/objects/detail), `changes` (cursor-paged World change stream), `region` (SimStats and parcel), `camera`, `environment`.
- Render state: `stats` (frame/GPU timing, render stats, memory), `explain_object` (World + renderer visibility/mesh/material state), `pick` (viewport pixel pick), `screenshot` (PNG capture).
- Assets and textures: `textures` (fetch/decode queues, discard histogram, GPU memory), `queues` (bounded subsystem queues).
- Network: `network` (per-region circuits, throttles, EventQueueGet, capabilities).
- Logs, inventory, cache, settings: `logs` (recent WARN/ERROR), `inventory` (folder fetch counts/failures), `cache` (object/asset cache stats), `settings` (cvars and provenance).
- Resources: `viewer://world`, `viewer://stats`, `viewer://region`, `viewer://settings`, `viewer://logs`, `viewer://screenshot`, and `viewer://object/{object_id}`.

Debugging order: start with `build`, then `stats` and `world`, then `explain_object` or `pick` for specific objects, then `logs`.
Use `wait_for` instead of sleeping or polling when waiting for `world_revision`, `asset_queue_empty`, or `texture_event`.
Write tools (`set_setting`, `set_log_level`, `set_camera`) change local viewer state only, except `set_camera`, whose pose reaches the simulator through normal `AgentUpdate` messages.
Asset UUIDs and raw user content are never exposed.";
