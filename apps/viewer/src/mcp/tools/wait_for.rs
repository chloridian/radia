use std::time::Duration;

use serde_json::{Value, json};
use tokio::time;

use super::{ToolContent, ToolFuture, ToolSpec};
use crate::{inspection::ViewerInspectionHandle, mcp::MAX_JSON_RESPONSE_BYTES};

const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const MAX_TIMEOUT_MS: u64 = 30_000;

pub const SPEC: ToolSpec = ToolSpec {
    name: "wait_for",
    description: "Wait on watch channels until world_revision >= target, asset_queue_empty, or texture_event advances, with a hard timeout.",
    input_schema,
    handler,
    max_response_bytes: MAX_JSON_RESPONSE_BYTES,
};

fn input_schema() -> Value {
    json!({
        "type": "object",
        "required": ["condition"],
        "properties": {
            "condition": { "type": "string", "enum": ["world_revision", "asset_queue_empty", "texture_event"] },
            "revision": { "type": "integer", "minimum": 0 },
            "timeout_ms": { "type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_MS }
        }
    })
}

fn handler(handle: ViewerInspectionHandle, args: Value) -> ToolFuture {
    Box::pin(async move {
        let Some(session) = handle.session() else {
            return Err("no active scene session".to_owned());
        };
        let Some(condition) = args.get("condition").and_then(Value::as_str) else {
            return Err("condition is required".to_owned());
        };
        let timeout_ms = args.get("timeout_ms").and_then(Value::as_u64).unwrap_or(DEFAULT_TIMEOUT_MS).clamp(1, MAX_TIMEOUT_MS);
        let timeout = Duration::from_millis(timeout_ms);

        match condition {
            "world_revision" => {
                let Some(target) = args.get("revision").and_then(Value::as_u64) else {
                    return Err("revision is required for condition=world_revision".to_owned());
                };
                let mut world = session.world.clone();
                let current = world.revision().map_err(|e| format!("world revision failed: {e}"))?;
                if current.get() >= target {
                    return Ok(ToolContent::Json(json!({
                        "satisfied": true,
                        "condition": condition,
                        "revision": current.get(),
                    })));
                }
                let waited = time::timeout(timeout, async {
                    loop {
                        let rev = world.changed().await.map_err(|e| format!("world watch closed: {e}"))?;
                        if rev.get() >= target {
                            return Ok::<u64, String>(rev.get());
                        }
                    }
                })
                .await;
                match waited {
                    Ok(Ok(rev)) => Ok(ToolContent::Json(json!({
                        "satisfied": true,
                        "condition": condition,
                        "revision": rev,
                    }))),
                    Ok(Err(error)) => Err(error),
                    Err(_) => {
                        let latest = world.revision().map(|rev| rev.get()).unwrap_or(current.get());
                        Ok(ToolContent::Json(json!({
                            "satisfied": false,
                            "timed_out": true,
                            "condition": condition,
                            "revision": latest,
                        })))
                    }
                }
            }
            "asset_queue_empty" => {
                let mut watch = session.assets.queue_stats_watch();
                if watch.borrow_and_update().is_empty() {
                    return Ok(ToolContent::Json(json!({
                        "satisfied": true,
                        "condition": condition,
                    })));
                }
                let waited = time::timeout(timeout, async {
                    loop {
                        watch.changed().await.map_err(|_| "asset queue watch closed".to_owned())?;
                        if watch.borrow_and_update().is_empty() {
                            return Ok::<(), String>(());
                        }
                    }
                })
                .await;
                match waited {
                    Ok(Ok(())) => Ok(ToolContent::Json(json!({
                        "satisfied": true,
                        "condition": condition,
                    }))),
                    Ok(Err(error)) => Err(error),
                    Err(_) => Ok(ToolContent::Json(json!({
                        "satisfied": false,
                        "timed_out": true,
                        "condition": condition,
                    }))),
                }
            }
            "texture_event" => {
                let mut watch = session.assets.texture_events();
                let initial = *watch.borrow_and_update();
                let waited = time::timeout(timeout, async {
                    watch.changed().await.map_err(|_| "texture event watch closed".to_owned())?;
                    Ok::<u64, String>(*watch.borrow_and_update())
                })
                .await;
                match waited {
                    Ok(Ok(event)) => Ok(ToolContent::Json(json!({
                        "satisfied": true,
                        "condition": condition,
                        "event": event,
                    }))),
                    Ok(Err(error)) => Err(error),
                    Err(_) => Ok(ToolContent::Json(json!({
                        "satisfied": false,
                        "timed_out": true,
                        "condition": condition,
                        "event": initial,
                    }))),
                }
            }
            other => Err(format!("unknown wait_for condition: {other}")),
        }
    })
}
