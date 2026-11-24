use std::{
    future,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tokio::time::{self, MissedTickBehavior};

use super::{ToolAnnotationsSpec, ToolCallContext, ToolContent, ToolFuture, ToolSpec};
use crate::{inspection::ViewerInspectionHandle, mcp::MAX_JSON_RESPONSE_BYTES};

const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const MAX_TIMEOUT_MS: u64 = 30_000;
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

pub const SPEC: ToolSpec = ToolSpec {
    name: "wait_for",
    title: "Wait For Event",
    description: "Wait on watch channels until world_revision >= target, asset_queue_empty, or texture_event advances, with a hard timeout.",
    annotations: ToolAnnotationsSpec::read_only(),
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
    Box::pin(run_wait_for(handle, args, None))
}

pub(in crate::mcp) fn call_with_context(handle: ViewerInspectionHandle, args: Value, ctx: ToolCallContext) -> ToolFuture {
    Box::pin(run_wait_for(handle, args, Some(ctx)))
}

#[derive(Debug)]
enum WaitOutcome<T> {
    Ready(Result<T, String>),
    TimedOut,
    Cancelled,
}

async fn await_with_cancellation_and_progress<T, F>(
    condition: &str,
    timeout: Duration,
    ctx: Option<&ToolCallContext>,
    future: F,
) -> WaitOutcome<T>
where
    F: Future<Output = Result<T, String>>,
{
    let started = Instant::now();
    let deadline = time::sleep(timeout);
    tokio::pin!(deadline);
    tokio::pin!(future);

    let report_progress = ctx.is_some_and(ToolCallContext::has_progress_token);
    let mut progress_tick = time::interval_at(time::Instant::now() + PROGRESS_INTERVAL, PROGRESS_INTERVAL);
    progress_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            () = async {
                match ctx {
                    Some(ctx) => ctx.cancelled().await,
                    None => future::pending::<()>().await,
                }
            } => return WaitOutcome::Cancelled,
            result = &mut future => return WaitOutcome::Ready(result),
            () = &mut deadline => return WaitOutcome::TimedOut,
            _ = progress_tick.tick(), if report_progress => {
                if let Some(ctx) = ctx {
                    ctx.report_progress(
                        started.elapsed().as_secs_f64(),
                        Some(timeout.as_secs_f64()),
                        Some(format!("waiting for {condition}")),
                    )
                    .await;
                }
            }
        }
    }
}

async fn run_wait_for(handle: ViewerInspectionHandle, args: Value, ctx: Option<ToolCallContext>) -> Result<ToolContent, String> {
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
            let waited = await_with_cancellation_and_progress(condition, timeout, ctx.as_ref(), async {
                loop {
                    let rev = world.changed().await.map_err(|e| format!("world watch closed: {e}"))?;
                    if rev.get() >= target {
                        return Ok(rev.get());
                    }
                }
            })
            .await;
            match waited {
                WaitOutcome::Ready(Ok(rev)) => Ok(ToolContent::Json(json!({
                    "satisfied": true,
                    "condition": condition,
                    "revision": rev,
                }))),
                WaitOutcome::Ready(Err(error)) => Err(error),
                WaitOutcome::Cancelled => Err("wait_for cancelled".to_owned()),
                WaitOutcome::TimedOut => {
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
            let waited = await_with_cancellation_and_progress(condition, timeout, ctx.as_ref(), async {
                loop {
                    watch.changed().await.map_err(|_| "asset queue watch closed".to_owned())?;
                    if watch.borrow_and_update().is_empty() {
                        return Ok(());
                    }
                }
            })
            .await;
            match waited {
                WaitOutcome::Ready(Ok(())) => Ok(ToolContent::Json(json!({
                    "satisfied": true,
                    "condition": condition,
                }))),
                WaitOutcome::Ready(Err(error)) => Err(error),
                WaitOutcome::Cancelled => Err("wait_for cancelled".to_owned()),
                WaitOutcome::TimedOut => Ok(ToolContent::Json(json!({
                    "satisfied": false,
                    "timed_out": true,
                    "condition": condition,
                }))),
            }
        }
        "texture_event" => {
            let mut watch = session.assets.texture_events();
            let initial = *watch.borrow_and_update();
            let waited = await_with_cancellation_and_progress(condition, timeout, ctx.as_ref(), async {
                watch.changed().await.map_err(|_| "texture event watch closed".to_owned())?;
                Ok(*watch.borrow_and_update())
            })
            .await;
            match waited {
                WaitOutcome::Ready(Ok(event)) => Ok(ToolContent::Json(json!({
                    "satisfied": true,
                    "condition": condition,
                    "event": event,
                }))),
                WaitOutcome::Ready(Err(error)) => Err(error),
                WaitOutcome::Cancelled => Err("wait_for cancelled".to_owned()),
                WaitOutcome::TimedOut => Ok(ToolContent::Json(json!({
                    "satisfied": false,
                    "timed_out": true,
                    "condition": condition,
                    "event": initial,
                }))),
            }
        }
        other => Err(format!("unknown wait_for condition: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use std::{assert_matches, sync::Arc, time::Duration};

    use tokio::{
        sync::{Notify, Semaphore, oneshot, watch},
        time,
    };

    use super::WaitOutcome;

    #[tokio::test]
    async fn cancelling_wait_for_returns_promptly_and_releases_permit() {
        let (_tx, mut rx) = watch::channel(0u64);
        let semaphore = Arc::new(Semaphore::new(1));
        let cancel = Arc::new(Notify::new());
        let (entered_tx, entered_rx) = oneshot::channel::<()>();

        let task = tokio::spawn({
            let semaphore = Arc::clone(&semaphore);
            let cancel = Arc::clone(&cancel);
            async move {
                let _permit = semaphore.try_acquire_owned().expect("acquire permit");
                let _ = entered_tx.send(());
                tokio::select! {
                    biased;
                    () = cancel.notified() => WaitOutcome::Cancelled,
                    _ = time::timeout(Duration::from_secs(30), rx.changed()) => WaitOutcome::<u64>::TimedOut,
                }
            }
        });

        entered_rx.await.expect("wait task entered");
        assert!(semaphore.try_acquire().is_err(), "permit must be held while waiting");

        cancel.notify_one();
        let outcome = task.await.expect("wait task finished");
        assert_matches!(outcome, WaitOutcome::Cancelled);
        assert!(semaphore.try_acquire().is_ok(), "permit must be released after cancellation");
    }
}
