mod build;
mod cache;
mod camera;
mod changes;
mod environment;
pub(super) mod explain_object;
mod inventory_fetch;
pub(super) mod logs;
mod network_stats;
mod pick;
mod queues;
pub(super) mod region;
pub(super) mod screenshot;
mod set_camera;
mod set_log_level;
mod set_setting;
pub(super) mod settings;
pub(super) mod stats;
mod textures;
mod wait_for;
pub(super) mod world_query;

use rmcp::handler::server::router::tool::ToolRouter;
use schemars::JsonSchema;
use serde::Serialize;
use uuid::Uuid;
use world::{Entity, SculptType, Shape};

use super::server::Handler;
use crate::world_reader::WorldReader;

pub(super) fn build_tool_router() -> ToolRouter<Handler> {
    Handler::build_router()
        + Handler::cache_router()
        + Handler::camera_router()
        + Handler::changes_router()
        + Handler::environment_router()
        + Handler::explain_object_router()
        + Handler::inventory_router()
        + Handler::logs_router()
        + Handler::network_router()
        + Handler::pick_router()
        + Handler::queues_router()
        + Handler::region_router()
        + Handler::screenshot_router()
        + Handler::set_camera_router()
        + Handler::set_log_level_router()
        + Handler::set_setting_router()
        + Handler::settings_router()
        + Handler::stats_router()
        + Handler::textures_router()
        + Handler::wait_for_router()
        + Handler::world_router()
}

pub(super) async fn lookup_object_entity(world: &WorldReader, id_str: &str) -> Result<Option<Entity>, String> {
    let target_id = Uuid::parse_str(id_str).map_err(|e| format!("invalid object_id: {e}"))?;
    world.entity(target_id).await.map_err(|e| format!("object lookup failed: {e}"))
}

#[derive(Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum SculptTypeOutput {
    Sphere,
    Torus,
    Plane,
    Cylinder,
}

#[derive(Serialize, JsonSchema)]
pub(super) struct SculptFlagsOutput {
    pub invert: bool,
    pub mirror: bool,
}

#[derive(Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum ShapeOutput {
    Prim {
        path_curve: u8,
        profile_curve: u8,
        path_begin: u16,
        path_end: u16,
        path_scale_x: u8,
        path_scale_y: u8,
        path_shear_x: i8,
        path_shear_y: i8,
        path_twist: i8,
        path_twist_begin: i8,
        path_radius_offset: i8,
        path_taper_x: i8,
        path_taper_y: i8,
        path_revolutions: u8,
        path_skew: i8,
        profile_begin: u16,
        profile_end: u16,
        profile_hollow: u16,
    },
    Sculpt {
        sculpt_type: SculptTypeOutput,
        flags: SculptFlagsOutput,
    },
    Mesh {
        flags: SculptFlagsOutput,
    },
    Avatar,
}

pub(super) fn format_shape(shape: &Shape) -> ShapeOutput {
    match shape {
        Shape::Prim(prim) => ShapeOutput::Prim {
            path_curve: prim.path_curve,
            profile_curve: prim.profile_curve,
            path_begin: prim.path_begin,
            path_end: prim.path_end,
            path_scale_x: prim.path_scale_x,
            path_scale_y: prim.path_scale_y,
            path_shear_x: prim.path_shear_x,
            path_shear_y: prim.path_shear_y,
            path_twist: prim.path_twist,
            path_twist_begin: prim.path_twist_begin,
            path_radius_offset: prim.path_radius_offset,
            path_taper_x: prim.path_taper_x,
            path_taper_y: prim.path_taper_y,
            path_revolutions: prim.path_revolutions,
            path_skew: prim.path_skew,
            profile_begin: prim.profile_begin,
            profile_end: prim.profile_end,
            profile_hollow: prim.profile_hollow,
        },
        Shape::Sculpt { asset_id: _, sculpt_type, flags } => {
            let sculpt_type = match sculpt_type {
                SculptType::Sphere => SculptTypeOutput::Sphere,
                SculptType::Torus => SculptTypeOutput::Torus,
                SculptType::Plane => SculptTypeOutput::Plane,
                SculptType::Cylinder => SculptTypeOutput::Cylinder,
            };
            ShapeOutput::Sculpt { sculpt_type, flags: SculptFlagsOutput { invert: flags.invert, mirror: flags.mirror } }
        }
        Shape::Mesh { asset_id: _, flags } => ShapeOutput::Mesh { flags: SculptFlagsOutput { invert: flags.invert, mirror: flags.mirror } },
        Shape::Avatar => ShapeOutput::Avatar,
    }
}
