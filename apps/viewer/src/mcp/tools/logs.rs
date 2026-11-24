use serde_json::{Value, json};

use super::{ToolContent, ToolFuture, ToolSpec};
use crate::{inspection::ViewerInspectionHandle, logging::LogLevel, mcp::MAX_JSON_RESPONSE_BYTES};

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 256;

pub const SPEC: ToolSpec = ToolSpec {
    name: "viewer_logs",
    description: "List recent WARN and ERROR log entries from the viewer's in-memory ring buffer.",
    input_schema,
    handler,
    max_response_bytes: MAX_JSON_RESPONSE_BYTES,
};

fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT },
            "level": { "type": "string", "enum": ["warn", "error"] }
        }
    })
}

fn handler(handle: ViewerInspectionHandle, args: Value) -> ToolFuture {
    Box::pin(async move {
        let limit =
            args.get("limit").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let min_level = match args.get("level").and_then(Value::as_str) {
            Some("error") => Some(LogLevel::Error),
            Some("warn") | None => Some(LogLevel::Warn),
            Some(other) => return Err(format!("unsupported level filter: {other}")),
        };
        let entries: Vec<Value> = handle
            .recent_logs()
            .snapshot(limit, min_level)
            .into_iter()
            .map(|entry| {
                json!({
                    "timestamp_ms": entry.timestamp_ms,
                    "level": entry.level.directive(),
                    "target": entry.target,
                    "message": entry.message,
                })
            })
            .collect();
        Ok(ToolContent::Json(json!({ "entries": entries })))
    })
}
