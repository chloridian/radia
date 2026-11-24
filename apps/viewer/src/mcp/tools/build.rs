use std::env::consts::{ARCH, OS};

use rmcp::{ErrorData, handler::server::wrapper::Json, tool, tool_router};
use schemars::JsonSchema;
use serde::Serialize;

use crate::mcp::server::Handler;

#[derive(Serialize, JsonSchema)]
pub struct BuildTarget {
    pub os: &'static str,
    pub arch: &'static str,
}

#[derive(Serialize, JsonSchema)]
pub struct GpuLimitsOutput {
    pub max_texture_dimension_2d: u32,
    pub max_buffer_size: u64,
    pub max_bind_groups: u32,
}

#[derive(Serialize, JsonSchema)]
pub struct GpuInfoOutput {
    pub name: String,
    pub backend: String,
    pub driver: String,
    pub driver_info: String,
    pub device_type: String,
    pub limits: GpuLimitsOutput,
}

#[derive(Serialize, JsonSchema)]
pub struct BuildOutput {
    pub version: &'static str,
    pub profile: &'static str,
    pub target: BuildTarget,
    pub gpu: Option<GpuInfoOutput>,
}

#[tool_router(router = build_router, vis = "pub(super)")]
impl Handler {
    #[tool(
        name = "build",
        title = "Viewer Build Info",
        description = "Return viewer version, build profile, target OS/arch, GPU adapter details, and key wgpu device limits.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn build(&self) -> Result<Json<BuildOutput>, ErrorData> {
        let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
        let gpu = self.handle.gpu_info().map(|info| GpuInfoOutput {
            name: info.name,
            backend: info.backend,
            driver: info.driver,
            driver_info: info.driver_info,
            device_type: info.device_type,
            limits: GpuLimitsOutput {
                max_texture_dimension_2d: info.max_texture_dimension_2d,
                max_buffer_size: info.max_buffer_size,
                max_bind_groups: info.max_bind_groups,
            },
        });
        Ok(Json(BuildOutput { version: env!("CARGO_PKG_VERSION"), profile, target: BuildTarget { os: OS, arch: ARCH }, gpu }))
    }
}
