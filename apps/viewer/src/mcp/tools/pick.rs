use rmcp::{
    ErrorData,
    handler::server::wrapper::{Json, Parameters},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::{inspection::ViewportQuery, mcp::server::Handler};

#[derive(Deserialize, JsonSchema)]
pub struct PickArgs {
    #[schemars(description = "Viewport pixel X coordinate.")]
    pub x: f64,
    #[schemars(description = "Viewport pixel Y coordinate.")]
    pub y: f64,
}

#[derive(Serialize, JsonSchema)]
pub struct PickOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_session: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub point: Option<[f64; 3]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub face_range: Option<[u32; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub material_slot: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_base_texture: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_emissive_texture: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub double_sided: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blended: Option<bool>,
}

#[tool_router(router = pick_router, vis = "pub(super)")]
impl Handler {
    #[tool(
        name = "pick",
        title = "Viewport Pick",
        description = "Pick the rendered geometry at viewport pixel coordinates (x, y) and return hit point, object, face, and texture presence.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn pick(&self, Parameters(args): Parameters<PickArgs>) -> Result<Json<PickOutput>, ErrorData> {
        let Some(session) = self.handle.session() else {
            return Ok(Json(PickOutput {
                active_session: Some(false),
                hit: None,
                point: None,
                entity: None,
                object_id: None,
                region: None,
                face_range: None,
                material_slot: None,
                has_base_texture: None,
                has_emissive_texture: None,
                double_sided: None,
                blended: None,
            }));
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        session
            .try_send_viewport_query(ViewportQuery::PixelPick { x: args.x as f32, y: args.y as f32, reply: reply_tx })
            .map_err(|msg| ErrorData::internal_error(msg, None))?;
        let pick_result = reply_rx
            .await
            .map_err(|_| ErrorData::internal_error("viewport dropped pick request", None))?
            .map_err(|e| ErrorData::internal_error(format!("pick failed: {e}"), None))?;

        match pick_result {
            Some(detail) => Ok(Json(PickOutput {
                active_session: None,
                hit: Some(true),
                point: Some([detail.point.x, detail.point.y, detail.point.z]),
                entity: detail.entity.map(|entity| format!("{entity:?}")),
                object_id: detail.object_id.map(|id| id.to_string()),
                region: Some(detail.region.get()),
                face_range: Some([detail.face_range.start, detail.face_range.end]),
                material_slot: Some(detail.material_slot),
                has_base_texture: Some(detail.has_base_texture),
                has_emissive_texture: Some(detail.has_emissive_texture),
                double_sided: Some(detail.double_sided),
                blended: Some(detail.blended),
            })),
            None => Ok(Json(PickOutput {
                active_session: None,
                hit: Some(false),
                point: None,
                entity: None,
                object_id: None,
                region: None,
                face_range: None,
                material_slot: None,
                has_base_texture: None,
                has_emissive_texture: None,
                double_sided: None,
                blended: None,
            })),
        }
    }
}
