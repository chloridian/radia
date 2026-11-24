mod build;
mod cache;
mod camera;
mod changes;
mod environment;
mod explain_object;
mod inventory_fetch;
mod logs;
mod network_stats;
mod pick;
mod queues;
mod region;
pub(super) mod screenshot;
mod set_camera;
mod set_log_level;
mod set_setting;
mod settings;
mod stats;
mod textures;
pub(super) mod wait_for;
mod world_query;

use std::{future::Future, pin::Pin};

use rmcp::{RoleServer, model::ProgressNotificationParam, service::RequestContext};
use serde_json::{Value, json};
use uuid::Uuid;
use world::{Entity, SculptType, Shape};

use crate::{inspection::ViewerInspectionHandle, world_reader::WorldReader};

pub struct ToolCallContext {
    context: RequestContext<RoleServer>,
}

impl ToolCallContext {
    pub fn new(context: RequestContext<RoleServer>) -> Self {
        Self { context }
    }

    pub fn is_cancelled(&self) -> bool {
        self.context.ct.is_cancelled()
    }

    pub async fn cancelled(&self) {
        self.context.ct.cancelled().await;
    }

    pub fn has_progress_token(&self) -> bool {
        self.context.meta.get_progress_token().is_some()
    }

    pub async fn report_progress(&self, progress: f64, total: Option<f64>, message: Option<String>) {
        let Some(progress_token) = self.context.meta.get_progress_token() else {
            return;
        };
        let mut param = ProgressNotificationParam::new(progress_token, progress);
        param.total = total;
        param.message = message;
        if let Err(error) = self.context.peer.notify_progress(param).await {
            tracing::debug!(%error, "MCP progress notification dropped");
        }
    }
}

pub type ToolFuture = Pin<Box<dyn Future<Output = ToolResult> + Send>>;
pub type ToolHandler = fn(ViewerInspectionHandle, Value) -> ToolFuture;

pub enum ToolContent {
    Json(Value),
    ImagePng { data_base64: String, width: u32, height: u32 },
}

pub type ToolResult = Result<ToolContent, String>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolAnnotationsSpec {
    pub read_only: bool,
    pub destructive: Option<bool>,
    pub idempotent: Option<bool>,
    pub open_world: bool,
}

impl ToolAnnotationsSpec {
    pub const fn read_only() -> Self {
        Self { read_only: true, destructive: None, idempotent: None, open_world: false }
    }

    pub const fn local_write() -> Self {
        Self { read_only: false, destructive: Some(false), idempotent: Some(true), open_world: false }
    }

    pub const fn camera_write() -> Self {
        Self { read_only: false, destructive: Some(false), idempotent: Some(true), open_world: true }
    }
}

pub struct ToolSpec {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub annotations: ToolAnnotationsSpec,
    pub input_schema: fn() -> Value,
    pub handler: ToolHandler,
    pub max_response_bytes: usize,
}

pub const TOOLS: &[ToolSpec] = &[
    world_query::SPEC,
    stats::SPEC,
    cache::SPEC,
    inventory_fetch::SPEC,
    network_stats::SPEC,
    changes::SPEC,
    textures::SPEC,
    environment::SPEC,
    camera::SPEC,
    settings::SPEC,
    logs::SPEC,
    screenshot::SPEC,
    explain_object::SPEC,
    pick::SPEC,
    queues::SPEC,
    region::SPEC,
    build::SPEC,
    wait_for::SPEC,
    set_setting::SPEC,
    set_log_level::SPEC,
    set_camera::SPEC,
];

pub fn find_tool(name: &str) -> Option<&'static ToolSpec> {
    TOOLS.iter().find(|tool| tool.name == name)
}

pub(super) async fn lookup_object_entity(world: &WorldReader, id_str: &str) -> Result<Option<Entity>, String> {
    let target_id = Uuid::parse_str(id_str).map_err(|e| format!("invalid object_id: {e}"))?;
    world.entity(target_id).await.map_err(|e| format!("object lookup failed: {e}"))
}

pub(super) fn format_shape(shape: &Shape) -> Value {
    match shape {
        Shape::Prim(prim) => json!({
            "kind": "prim",
            "path_curve": prim.path_curve,
            "profile_curve": prim.profile_curve,
            "path_begin": prim.path_begin,
            "path_end": prim.path_end,
            "path_scale_x": prim.path_scale_x,
            "path_scale_y": prim.path_scale_y,
            "path_shear_x": prim.path_shear_x,
            "path_shear_y": prim.path_shear_y,
            "path_twist": prim.path_twist,
            "path_twist_begin": prim.path_twist_begin,
            "path_radius_offset": prim.path_radius_offset,
            "path_taper_x": prim.path_taper_x,
            "path_taper_y": prim.path_taper_y,
            "path_revolutions": prim.path_revolutions,
            "path_skew": prim.path_skew,
            "profile_begin": prim.profile_begin,
            "profile_end": prim.profile_end,
            "profile_hollow": prim.profile_hollow,
        }),
        Shape::Sculpt { asset_id: _, sculpt_type, flags } => {
            let sculpt_type = match sculpt_type {
                SculptType::Sphere => "sphere",
                SculptType::Torus => "torus",
                SculptType::Plane => "plane",
                SculptType::Cylinder => "cylinder",
            };
            json!({
                "kind": "sculpt",
                "sculpt_type": sculpt_type,
                "flags": {
                    "invert": flags.invert,
                    "mirror": flags.mirror,
                },
            })
        }
        Shape::Mesh { asset_id: _, flags } => json!({
            "kind": "mesh",
            "flags": {
                "invert": flags.invert,
                "mirror": flags.mirror,
            },
        }),
        Shape::Avatar => json!({
            "kind": "avatar",
        }),
    }
}
