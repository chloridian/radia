mod cache;
mod explain_object;
mod inventory_fetch;
mod logs;
mod pick;
mod screenshot;
mod stats;
mod wait_for;
mod world_query;

use std::{future::Future, pin::Pin};

use serde_json::Value;
use uuid::Uuid;
use world::Entity;

use crate::{inspection::ViewerInspectionHandle, world_reader::WorldReader};

pub type ToolFuture = Pin<Box<dyn Future<Output = ToolResult> + Send>>;
pub type ToolHandler = fn(ViewerInspectionHandle, Value) -> ToolFuture;

pub enum ToolContent {
    Json(Value),
    ImagePng { data_base64: String, width: u32, height: u32 },
}

pub type ToolResult = Result<ToolContent, String>;

pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: fn() -> Value,
    pub handler: ToolHandler,
    pub max_response_bytes: usize,
}

pub const TOOLS: &[ToolSpec] = &[
    world_query::SPEC,
    stats::SPEC,
    cache::SPEC,
    inventory_fetch::SPEC,
    logs::SPEC,
    screenshot::SPEC,
    explain_object::SPEC,
    pick::SPEC,
    wait_for::SPEC,
];

pub fn find_tool(name: &str) -> Option<&'static ToolSpec> {
    TOOLS.iter().find(|tool| tool.name == name)
}

pub(super) async fn lookup_object_entity(world: &WorldReader, id_str: &str) -> Result<Option<Entity>, String> {
    let target_id = Uuid::parse_str(id_str).map_err(|e| format!("invalid object_id: {e}"))?;
    world.entity(target_id).await.map_err(|e| format!("object lookup failed: {e}"))
}
