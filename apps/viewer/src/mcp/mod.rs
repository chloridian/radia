mod png;
mod server;
mod tools;

use std::time::Duration;

pub use self::server::{McpServer, McpServerConfig};

pub const DEFAULT_PORT: u16 = 47100;
pub const MAX_CONCURRENT_REQUESTS: usize = 4;
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
pub const MAX_JSON_RESPONSE_BYTES: usize = 512 * 1024;
pub const MAX_PNG_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(35);
