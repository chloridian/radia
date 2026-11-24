use rmcp::{
    ErrorData,
    handler::server::wrapper::{Json, Parameters},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{inspection::ViewerInspectionHandle, logging::LogLevel, mcp::server::RadiaServer};

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 256;

#[derive(Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LogsLevelFilter {
    Warn,
    Error,
}

#[derive(Deserialize, JsonSchema)]
pub struct LogsArgs {
    #[schemars(description = "Maximum number of log entries to return (1..=256).")]
    pub limit: Option<usize>,
    #[schemars(description = "Minimum log level filter.")]
    pub level: Option<LogsLevelFilter>,
}

#[derive(Serialize, JsonSchema)]
pub struct LogEntryOutput {
    pub timestamp_ms: u64,
    pub level: &'static str,
    pub target: String,
    pub message: String,
}

#[derive(Serialize, JsonSchema)]
pub struct LogsOutput {
    pub entries: Vec<LogEntryOutput>,
}

#[tool_router(router = logs_router, vis = "pub(super)")]
impl RadiaServer {
    #[tool(
        name = "logs",
        title = "Recent Logs",
        description = "List recent WARN and ERROR log entries from the viewer's in-memory ring buffer.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn logs(&self, Parameters(args): Parameters<LogsArgs>) -> Result<Json<LogsOutput>, ErrorData> {
        Ok(Json(logs_data(&self.handle, args.limit, args.level)))
    }
}

pub(in crate::mcp) fn logs_data(handle: &ViewerInspectionHandle, limit: Option<usize>, level: Option<LogsLevelFilter>) -> LogsOutput {
    let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let min_level = match level {
        Some(LogsLevelFilter::Error) => Some(LogLevel::Error),
        Some(LogsLevelFilter::Warn) | None => Some(LogLevel::Warn),
    };
    let entries = handle
        .recent_logs()
        .snapshot(limit, min_level)
        .into_iter()
        .map(|entry| LogEntryOutput {
            timestamp_ms: entry.timestamp_ms,
            level: entry.level.directive(),
            target: entry.target,
            message: entry.message,
        })
        .collect();
    LogsOutput { entries }
}
