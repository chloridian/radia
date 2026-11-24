use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::{ToolContent, ToolFuture, ToolSpec};
use crate::{
    inspection::{CameraModeRequest, SetCameraRequest, ViewerInspectionHandle, ViewportQuery},
    mcp::MAX_JSON_RESPONSE_BYTES,
};

pub const SPEC: ToolSpec = ToolSpec {
    name: "set_camera",
    description: "Changes local viewer state: set the local camera mode (follow or free) and/or free camera pose (position and look_at) on the GUI thread. Never moves the agent; camera pose reaches the simulator through the normal agent update like any manual camera move.",
    input_schema,
    handler,
    max_response_bytes: MAX_JSON_RESPONSE_BYTES,
};

fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "mode": {
                "type": "string",
                "enum": ["follow", "free"],
                "description": "Target local camera mode."
            },
            "position": {
                "type": "array",
                "items": { "type": "number" },
                "minItems": 3,
                "maxItems": 3,
                "description": "Free camera world position [x, y, z]."
            },
            "look_at": {
                "type": "array",
                "items": { "type": "number" },
                "minItems": 3,
                "maxItems": 3,
                "description": "Free camera world focus point [x, y, z]."
            }
        }
    })
}

fn parse_vec3(value: &Value, field: &str) -> Result<[f64; 3], String> {
    let array = value.as_array().ok_or_else(|| format!("{field} must be a 3-element array"))?;
    if array.len() != 3 {
        return Err(format!("{field} must have exactly 3 numbers"));
    }
    let mut out = [0.0; 3];
    for (slot, item) in out.iter_mut().zip(array) {
        let number = item.as_f64().ok_or_else(|| format!("{field} elements must be finite numbers"))?;
        if !number.is_finite() {
            return Err(format!("{field} elements must be finite numbers"));
        }
        *slot = number;
    }
    Ok(out)
}

fn parse_request(args: &Value) -> Result<SetCameraRequest, String> {
    let mode = match args.get("mode") {
        None | Some(Value::Null) => None,
        Some(Value::String(raw)) => match raw.as_str() {
            "follow" => Some(CameraModeRequest::Follow),
            "free" => Some(CameraModeRequest::Free),
            other => return Err(format!("invalid camera mode: {other}")),
        },
        Some(_) => return Err("mode must be 'follow' or 'free'".to_owned()),
    };

    let position = match args.get("position") {
        None | Some(Value::Null) => None,
        Some(value) => Some(parse_vec3(value, "position")?),
    };

    let look_at = match args.get("look_at") {
        None | Some(Value::Null) => None,
        Some(value) => Some(parse_vec3(value, "look_at")?),
    };

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

fn handler(handle: ViewerInspectionHandle, args: Value) -> ToolFuture {
    Box::pin(async move {
        let request = parse_request(&args)?;
        let Some(session) = handle.session() else {
            return Err("no active viewer session".to_owned());
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        session.try_send_viewport_query(ViewportQuery::SetCamera { request, reply: reply_tx }).map_err(str::to_owned)?;
        let applied = reply_rx.await.map_err(|_| "viewport dropped set_camera request".to_owned())??;

        Ok(ToolContent::Json(json!({
            "mode": applied.mode,
            "position": applied.position,
            "direction": applied.direction,
            "vertical_fov_deg": applied.vertical_fov_deg,
            "draw_distance": applied.draw_distance,
            "lod_factor": applied.lod_factor,
        })))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_camera_validates_inputs() {
        assert!(parse_request(&json!({})).is_err(), "empty request should fail");
        assert!(parse_request(&json!({ "mode": "orbit" })).is_err(), "unknown mode should fail");
        assert!(
            parse_request(&json!({ "mode": "follow", "position": [1.0, 2.0, 3.0] })).is_err(),
            "follow mode with free pose should fail"
        );
        assert!(
            parse_request(&json!({ "position": [1.0, 2.0, 3.0], "look_at": [1.0, 2.0, 3.0] })).is_err(),
            "coincident position and look_at should fail"
        );
        assert!(parse_request(&json!({ "position": [1.0, 2.0] })).is_err(), "short vector should fail");

        let valid = parse_request(&json!({
            "mode": "free",
            "position": [128.0, 128.0, 30.0],
            "look_at": [128.0, 138.0, 25.0],
        }))
        .expect("valid free pose");
        assert_eq!(valid.mode, Some(CameraModeRequest::Free));
        assert_eq!(valid.position, Some([128.0, 128.0, 30.0]));
        assert_eq!(valid.look_at, Some([128.0, 138.0, 25.0]));
    }
}
