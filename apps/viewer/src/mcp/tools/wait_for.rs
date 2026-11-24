use std::time::{Duration, Instant};

use rmcp::{
    ErrorData, RoleServer,
    handler::server::wrapper::{Json, Parameters},
    model::{ErrorCode, ProgressNotificationParam},
    service::RequestContext,
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::time::{self, MissedTickBehavior};

use crate::mcp::server::Handler;

const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const MAX_TIMEOUT_MS: u64 = 30_000;
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WaitCondition {
    WorldRevision,
    AssetQueueEmpty,
    TextureEvent,
}

impl WaitCondition {
    fn as_str(self) -> &'static str {
        match self {
            Self::WorldRevision => "world_revision",
            Self::AssetQueueEmpty => "asset_queue_empty",
            Self::TextureEvent => "texture_event",
        }
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct WaitForArgs {
    #[schemars(description = "Condition to wait for.")]
    pub condition: WaitCondition,
    #[schemars(description = "Target World revision (required when condition='world_revision').")]
    pub revision: Option<u64>,
    #[schemars(description = "Timeout in milliseconds (1..=30000).")]
    pub timeout_ms: Option<u64>,
}

#[derive(Serialize, JsonSchema)]
pub struct WaitForOutput {
    pub satisfied: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timed_out: Option<bool>,
    pub condition: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<u64>,
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
    context: &RequestContext<RoleServer>,
    future: F,
) -> WaitOutcome<T>
where
    F: Future<Output = Result<T, String>>,
{
    let started = Instant::now();
    let deadline = time::sleep(timeout);
    tokio::pin!(deadline);
    tokio::pin!(future);

    let progress_token = context.meta.get_progress_token();
    let report_progress = progress_token.is_some();
    let mut progress_tick = time::interval_at(time::Instant::now() + PROGRESS_INTERVAL, PROGRESS_INTERVAL);
    progress_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            () = context.ct.cancelled() => return WaitOutcome::Cancelled,
            result = &mut future => return WaitOutcome::Ready(result),
            () = &mut deadline => return WaitOutcome::TimedOut,
            _ = progress_tick.tick(), if report_progress => {
                if let Some(token) = progress_token.clone() {
                    let mut param = ProgressNotificationParam::new(token, started.elapsed().as_secs_f64());
                    param.total = Some(timeout.as_secs_f64());
                    param.message = Some(format!("waiting for {condition}"));
                    if let Err(error) = context.peer.notify_progress(param).await {
                        tracing::debug!(%error, "MCP wait_for progress notification dropped");
                    }
                }
            }
        }
    }
}

#[tool_router(router = wait_for_router, vis = "pub(super)")]
impl Handler {
    #[tool(
        name = "wait_for",
        title = "Wait For Event",
        description = "Wait on watch channels until world_revision >= target, asset_queue_empty, or texture_event advances, with a hard timeout.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn wait_for(
        &self,
        Parameters(args): Parameters<WaitForArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<Json<WaitForOutput>, ErrorData> {
        let Some(session) = self.handle.session() else {
            return Err(ErrorData::internal_error("no active scene session", None));
        };
        let timeout_ms = args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).clamp(1, MAX_TIMEOUT_MS);
        let timeout = Duration::from_millis(timeout_ms);

        let condition_name = args.condition.as_str();
        match args.condition {
            WaitCondition::WorldRevision => {
                let Some(target) = args.revision else {
                    return Err(ErrorData::invalid_params("revision is required for condition=world_revision", None));
                };
                let mut world = session.world.clone();
                let current = world.revision().map_err(|e| ErrorData::internal_error(format!("world revision failed: {e}"), None))?;
                if current.get() >= target {
                    return Ok(Json(WaitForOutput {
                        satisfied: true,
                        timed_out: None,
                        condition: condition_name,
                        revision: Some(current.get()),
                        event: None,
                    }));
                }
                let waited = await_with_cancellation_and_progress(condition_name, timeout, &context, async {
                    loop {
                        let rev = world.changed().await.map_err(|e| format!("world watch closed: {e}"))?;
                        if rev.get() >= target {
                            return Ok(rev.get());
                        }
                    }
                })
                .await;
                match waited {
                    WaitOutcome::Ready(Ok(rev)) => Ok(Json(WaitForOutput {
                        satisfied: true,
                        timed_out: None,
                        condition: condition_name,
                        revision: Some(rev),
                        event: None,
                    })),
                    WaitOutcome::Ready(Err(error)) => Err(ErrorData::internal_error(error, None)),
                    WaitOutcome::Cancelled => Err(ErrorData::new(ErrorCode(-32000), "wait_for cancelled", None)),
                    WaitOutcome::TimedOut => {
                        let latest = world.revision().map(|rev| rev.get()).unwrap_or(current.get());
                        Ok(Json(WaitForOutput {
                            satisfied: false,
                            timed_out: Some(true),
                            condition: condition_name,
                            revision: Some(latest),
                            event: None,
                        }))
                    }
                }
            }
            WaitCondition::AssetQueueEmpty => {
                let mut watch = session.assets.queue_stats_watch();
                if watch.borrow_and_update().is_empty() {
                    return Ok(Json(WaitForOutput {
                        satisfied: true,
                        timed_out: None,
                        condition: condition_name,
                        revision: None,
                        event: None,
                    }));
                }
                let waited = await_with_cancellation_and_progress(condition_name, timeout, &context, async {
                    loop {
                        watch.changed().await.map_err(|_| "asset queue watch closed".to_owned())?;
                        if watch.borrow_and_update().is_empty() {
                            return Ok(());
                        }
                    }
                })
                .await;
                match waited {
                    WaitOutcome::Ready(Ok(())) => {
                        Ok(Json(WaitForOutput { satisfied: true, timed_out: None, condition: condition_name, revision: None, event: None }))
                    }
                    WaitOutcome::Ready(Err(error)) => Err(ErrorData::internal_error(error, None)),
                    WaitOutcome::Cancelled => Err(ErrorData::new(ErrorCode(-32000), "wait_for cancelled", None)),
                    WaitOutcome::TimedOut => Ok(Json(WaitForOutput {
                        satisfied: false,
                        timed_out: Some(true),
                        condition: condition_name,
                        revision: None,
                        event: None,
                    })),
                }
            }
            WaitCondition::TextureEvent => {
                let mut watch = session.assets.texture_events();
                let initial = *watch.borrow_and_update();
                let waited = await_with_cancellation_and_progress(condition_name, timeout, &context, async {
                    watch.changed().await.map_err(|_| "texture event watch closed".to_owned())?;
                    Ok(*watch.borrow_and_update())
                })
                .await;
                match waited {
                    WaitOutcome::Ready(Ok(event)) => Ok(Json(WaitForOutput {
                        satisfied: true,
                        timed_out: None,
                        condition: condition_name,
                        revision: None,
                        event: Some(event),
                    })),
                    WaitOutcome::Ready(Err(error)) => Err(ErrorData::internal_error(error, None)),
                    WaitOutcome::Cancelled => Err(ErrorData::new(ErrorCode(-32000), "wait_for cancelled", None)),
                    WaitOutcome::TimedOut => Ok(Json(WaitForOutput {
                        satisfied: false,
                        timed_out: Some(true),
                        condition: condition_name,
                        revision: None,
                        event: Some(initial),
                    })),
                }
            }
        }
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
