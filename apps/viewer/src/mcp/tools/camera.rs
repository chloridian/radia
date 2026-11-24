use rmcp::{ErrorData, handler::server::wrapper::Json, tool, tool_router};
use schemars::JsonSchema;
use serde::Serialize;
use tokio::sync::oneshot;

use crate::{inspection::ViewportQuery, mcp::server::RadiaServer};

#[derive(Serialize, JsonSchema)]
pub struct CameraStateOutput {
    pub mode: &'static str,
    pub position: Option<[f64; 3]>,
    pub direction: Option<[f32; 3]>,
    pub vertical_fov_deg: Option<f64>,
    pub draw_distance: Option<f64>,
    pub lod_factor: Option<f64>,
}

#[derive(Serialize, JsonSchema)]
pub struct AgentRegionPositionOutput {
    pub region: u64,
    pub region_name: Option<String>,
    pub region_handle: Option<u64>,
    pub global_position: [f64; 3],
    pub local_position: Option<[f64; 3]>,
}

#[derive(Serialize, JsonSchema)]
pub struct CameraOutput {
    pub active_session: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub camera: Option<CameraStateOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentRegionPositionOutput>,
}

#[tool_router(router = camera_router, vis = "pub(super)")]
impl RadiaServer {
    #[tool(
        name = "camera",
        title = "Camera State",
        description = "Return camera mode, position, orientation, agent region name and handle, and agent region-local position.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn camera(&self) -> Result<Json<CameraOutput>, ErrorData> {
        let Some(session) = self.handle.session() else {
            return Ok(Json(CameraOutput { active_session: false, camera: None, agent: None }));
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        session
            .try_send_viewport_query(ViewportQuery::CameraState { reply: reply_tx })
            .map_err(|msg| ErrorData::internal_error(msg, None))?;
        let camera = reply_rx.await.map_err(|_| ErrorData::internal_error("viewport dropped camera request", None))?;

        let sample = *session.agent_motion.borrow();
        let agent = match sample {
            Some(sample) => {
                let regions = session
                    .world
                    .regions(None, 64)
                    .await
                    .map_err(|e| ErrorData::internal_error(format!("regions query failed: {e}"), None))?;
                let region_info = regions.regions.into_iter().find(|entry| entry.region == sample.region).map(|entry| entry.info);
                let local_position = region_info.as_ref().and_then(|info| info.resolved_origin()).map(|origin| {
                    let local = sample.position - origin;
                    [local.x, local.y, local.z]
                });
                Some(AgentRegionPositionOutput {
                    region: sample.region.get(),
                    region_name: region_info.as_ref().and_then(|info| info.name.clone()),
                    region_handle: region_info.as_ref().and_then(|info| info.handle).map(|handle| handle.get()),
                    global_position: [sample.position.x, sample.position.y, sample.position.z],
                    local_position,
                })
            }
            None => None,
        };

        Ok(Json(CameraOutput {
            active_session: true,
            camera: Some(CameraStateOutput {
                mode: camera.mode,
                position: camera.position,
                direction: camera.direction,
                vertical_fov_deg: camera.vertical_fov_deg,
                draw_distance: camera.draw_distance,
                lod_factor: camera.lod_factor,
            }),
            agent,
        }))
    }
}
