use rmcp::{
    ErrorData,
    handler::server::wrapper::{Json, Parameters},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::{
    inspection::{CameraModeRequest, SetCameraRequest, ViewportQuery},
    mcp::server::Handler,
};

#[derive(Deserialize, JsonSchema)]
pub struct SetCameraArgs {
    #[schemars(description = "Target local camera mode.")]
    pub mode: Option<CameraModeRequest>,
    #[schemars(description = "Free camera world position [x, y, z].")]
    pub position: Option<[f64; 3]>,
    #[schemars(description = "Free camera world focus point [x, y, z].")]
    pub look_at: Option<[f64; 3]>,
}

#[derive(Serialize, JsonSchema)]
pub struct SetCameraOutput {
    pub mode: &'static str,
    pub position: Option<[f64; 3]>,
    pub direction: Option<[f32; 3]>,
    pub vertical_fov_deg: Option<f64>,
    pub draw_distance: Option<f64>,
    pub lod_factor: Option<f64>,
}

fn validate_vec3(values: [f64; 3], field: &str) -> Result<[f64; 3], String> {
    if values.iter().any(|v| !v.is_finite()) {
        return Err(format!("{field} elements must be finite numbers"));
    }
    Ok(values)
}

fn parse_request(args: SetCameraArgs) -> Result<SetCameraRequest, String> {
    let mode = args.mode;
    let position = args.position.map(|v| validate_vec3(v, "position")).transpose()?;
    let look_at = args.look_at.map(|v| validate_vec3(v, "look_at")).transpose()?;

    if mode.is_none() && position.is_none() && look_at.is_none() {
        return Err("provide at least one of mode, position, or look_at".to_owned());
    }
    if mode == Some(CameraModeRequest::Follow) && (position.is_some() || look_at.is_some()) {
        return Err("position and look_at cannot be set when mode is follow".to_owned());
    }
    if let (Some(pos), Some(target)) = (position, look_at) {
        let dx = target[0] - pos[0];
        let dy = target[1] - pos[1];
        let dz = target[2] - pos[2];
        if (dx * dx + dy * dy + dz * dz).sqrt() < 0.02 {
            return Err("position and look_at must be at least 0.02m apart".to_owned());
        }
    }
    Ok(SetCameraRequest { mode, position, look_at })
}

#[tool_router(router = set_camera_router, vis = "pub(super)")]
impl Handler {
    #[tool(
        name = "set_camera",
        title = "Set Camera",
        description = "Changes local viewer state: set the local camera mode (follow or free) and/or free camera pose (position and look_at) on the GUI thread. Never moves the agent; camera pose reaches the simulator through the normal agent update like any manual camera move.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = true)
    )]
    pub(crate) async fn set_camera(&self, Parameters(args): Parameters<SetCameraArgs>) -> Result<Json<SetCameraOutput>, ErrorData> {
        let request = parse_request(args).map_err(|msg| ErrorData::invalid_params(msg, None))?;
        let Some(session) = self.handle.session() else {
            return Err(ErrorData::internal_error("no active viewer session", None));
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        session
            .try_send_viewport_query(ViewportQuery::SetCamera { request, reply: reply_tx })
            .map_err(|msg| ErrorData::internal_error(msg, None))?;
        let applied = reply_rx
            .await
            .map_err(|_| ErrorData::internal_error("viewport dropped set_camera request", None))?
            .map_err(|msg| ErrorData::invalid_params(msg, None))?;

        Ok(Json(SetCameraOutput {
            mode: applied.mode,
            position: applied.position,
            direction: applied.direction,
            vertical_fov_deg: applied.vertical_fov_deg,
            draw_distance: applied.draw_distance,
            lod_factor: applied.lod_factor,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_camera_validates_inputs() {
        assert!(parse_request(SetCameraArgs { mode: None, position: None, look_at: None }).is_err());
        assert!(
            parse_request(SetCameraArgs { mode: Some(CameraModeRequest::Follow), position: Some([1.0, 2.0, 3.0]), look_at: None }).is_err()
        );
        assert!(parse_request(SetCameraArgs { mode: None, position: Some([1.0, 2.0, 3.0]), look_at: Some([1.0, 2.0, 3.0]) }).is_err());

        let valid = parse_request(SetCameraArgs {
            mode: Some(CameraModeRequest::Free),
            position: Some([128.0, 128.0, 30.0]),
            look_at: Some([128.0, 138.0, 25.0]),
        })
        .expect("valid free pose");
        assert_eq!(valid.mode, Some(CameraModeRequest::Free));
        assert_eq!(valid.position, Some([128.0, 128.0, 30.0]));
        assert_eq!(valid.look_at, Some([128.0, 138.0, 25.0]));
    }
}
