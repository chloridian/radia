use std::env::consts::{ARCH, OS};

use serde_json::{Value, json};

use super::{ToolAnnotationsSpec, ToolContent, ToolFuture, ToolSpec};
use crate::{inspection::ViewerInspectionHandle, mcp::MAX_JSON_RESPONSE_BYTES};

pub const SPEC: ToolSpec = ToolSpec {
    name: "build",
    title: "Viewer Build Info",
    description: "Return viewer version, build profile, target OS/arch, GPU adapter details, and key wgpu device limits.",
    annotations: ToolAnnotationsSpec::read_only(),
    input_schema,
    handler,
    max_response_bytes: MAX_JSON_RESPONSE_BYTES,
};

fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {}
    })
}

fn handler(handle: ViewerInspectionHandle, _args: Value) -> ToolFuture {
    Box::pin(async move {
        let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
        let gpu = handle.gpu_info().map(|info| {
            json!({
                "name": info.name,
                "backend": info.backend,
                "driver": info.driver,
                "driver_info": info.driver_info,
                "device_type": info.device_type,
                "limits": {
                    "max_texture_dimension_2d": info.max_texture_dimension_2d,
                    "max_buffer_size": info.max_buffer_size,
                    "max_bind_groups": info.max_bind_groups,
                },
            })
        });

        Ok(ToolContent::Json(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "profile": profile,
            "target": {
                "os": OS,
                "arch": ARCH,
            },
            "gpu": gpu,
        })))
    })
}
