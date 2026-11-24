use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use rmcp::{
    ErrorData, RoleServer,
    model::{CallToolResult, ContentBlock, ProgressNotificationParam},
    service::RequestContext,
    tool, tool_router,
};
use serde_json::json;
use tokio::{sync::oneshot, task};
use wgpu::{MapMode, PollType};

use crate::{
    inspection::{ViewerInspectionHandle, ViewportQuery},
    mcp::{png::encode_rgba_png, server::Handler},
};

#[tool_router(router = screenshot_router, vis = "pub(super)")]
impl Handler {
    #[tool(
        name = "screenshot",
        title = "Viewport Screenshot",
        description = "Capture the current viewport texture as a PNG image without blocking the GUI thread.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn screenshot(&self, context: RequestContext<RoleServer>) -> Result<CallToolResult, ErrorData> {
        let (data_base64, width, height) =
            capture_screenshot_base64(&self.handle, context).await.map_err(|msg| ErrorData::internal_error(msg, None))?;
        Ok(CallToolResult::success(vec![
            ContentBlock::image(data_base64, "image/png"),
            ContentBlock::text(json!({ "width": width, "height": height }).to_string()),
        ]))
    }
}

pub(in crate::mcp) async fn capture_screenshot_base64(
    handle: &ViewerInspectionHandle,
    context: RequestContext<RoleServer>,
) -> Result<(String, u32, u32), String> {
    let Some(session) = handle.session() else {
        return Err("no active scene session".to_owned());
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    session.try_send_viewport_query(ViewportQuery::BeginScreenshot { reply: reply_tx }).map_err(str::to_owned)?;

    if let Some(progress_token) = context.meta.get_progress_token() {
        let mut param = ProgressNotificationParam::new(progress_token, 0.0);
        param.total = Some(1.0);
        param.message = Some("waiting for viewport frame".to_owned());
        if let Err(error) = context.peer.notify_progress(param).await {
            tracing::debug!(%error, "MCP screenshot progress notification dropped");
        }
    }

    let readback = tokio::select! {
        biased;
        () = context.ct.cancelled() => return Err("screenshot cancelled".to_owned()),
        reply = reply_rx => {
            reply
                .map_err(|_| "viewport dropped screenshot request".to_owned())?
                .ok_or_else(|| "viewport texture unavailable".to_owned())?
        }
    };

    if context.ct.is_cancelled() {
        return Err("screenshot cancelled".to_owned());
    }

    let (map_tx, map_rx) = oneshot::channel();
    readback.buffer.slice(..).map_async(MapMode::Read, move |result| {
        drop(map_tx.send(result));
    });

    let device = readback.device;
    let poll_result =
        task::spawn_blocking(move || device.poll(PollType::Wait { submission_index: None, timeout: Some(Duration::from_secs(5)) }))
            .await
            .map_err(|e| format!("screenshot poll task failed: {e}"))?;
    poll_result.map_err(|e| format!("gpu poll failed: {e}"))?;

    map_rx.await.map_err(|_| "buffer map callback dropped".to_owned())?.map_err(|e| format!("buffer map failed: {e}"))?;

    if context.ct.is_cancelled() {
        readback.buffer.unmap();
        return Err("screenshot cancelled".to_owned());
    }

    let width = readback.width;
    let height = readback.height;
    let bytes_per_row = readback.bytes_per_row;
    let format = readback.format;
    let buffer = readback.buffer;

    let png_bytes = task::spawn_blocking(move || {
        let mapped = buffer.slice(..).get_mapped_range().map_err(|e| format!("mapped range failed: {e}"))?;
        let encoded = encode_rgba_png(&mapped, width, height, bytes_per_row, format);
        drop(mapped);
        buffer.unmap();
        encoded
    })
    .await
    .map_err(|e| format!("png encode task failed: {e}"))??;

    Ok((STANDARD.encode(&png_bytes), width, height))
}
