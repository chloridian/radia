use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use tokio::{sync::oneshot, task};
use wgpu::{MapMode, PollType};

use super::{ToolContent, ToolFuture, ToolSpec};
use crate::{
    inspection::{ViewerInspectionHandle, ViewportQuery},
    mcp::{MAX_PNG_RESPONSE_BYTES, png::encode_rgba_png},
};

pub const SPEC: ToolSpec = ToolSpec {
    name: "viewer_screenshot",
    description: "Capture the current viewport texture as a PNG image without blocking the GUI thread.",
    input_schema,
    handler,
    max_response_bytes: MAX_PNG_RESPONSE_BYTES,
};

fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {}
    })
}

fn handler(handle: ViewerInspectionHandle, _args: Value) -> ToolFuture {
    Box::pin(async move {
        let Some(session) = handle.session() else {
            return Err("no active scene session".to_owned());
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        session
            .viewport_queries
            .try_send(ViewportQuery::BeginScreenshot { reply: reply_tx })
            .map_err(|_| "viewport query queue busy or closed".to_owned())?;
        let readback = reply_rx
            .await
            .map_err(|_| "viewport dropped screenshot request".to_owned())?
            .ok_or_else(|| "viewport texture unavailable".to_owned())?;

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

        let data_base64 = STANDARD.encode(&png_bytes);
        Ok(ToolContent::ImagePng { data_base64, width, height })
    })
}
