use serde_json::{Value, json};

use super::{ToolAnnotationsSpec, ToolContent, ToolFuture, ToolSpec};
use crate::{
    inspection::ViewerInspectionHandle,
    logging::{LogControl, LogLevel},
    mcp::MAX_JSON_RESPONSE_BYTES,
};

pub const SPEC: ToolSpec = ToolSpec {
    name: "set_log_level",
    title: "Set Log Level",
    description: "Change the local viewer's runtime log filter (global level and/or per-target override) through LogControl and return the active filter.",
    annotations: ToolAnnotationsSpec::local_write(),
    input_schema,
    handler,
    max_response_bytes: MAX_JSON_RESPONSE_BYTES,
};

fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "level": { "type": "string", "enum": ["error", "warn", "info", "debug", "trace"] },
            "target": { "type": "string" },
            "target_level": { "type": "string", "enum": ["error", "warn", "info", "debug", "trace", "clear"] }
        }
    })
}

fn apply_log_level(control: &LogControl, args: &Value) -> Result<Value, String> {
    let level = match args.get("level") {
        Some(raw) => {
            let text = raw.as_str().ok_or_else(|| "level must be a string".to_owned())?;
            let parsed = LogLevel::from_directive(text).ok_or_else(|| format!("invalid log level `{text}`"))?;
            Some(parsed)
        }
        None => None,
    };
    let target = match args.get("target") {
        Some(raw) => {
            let name = raw.as_str().ok_or_else(|| "target must be a string".to_owned())?;
            let level_raw = args
                .get("target_level")
                .and_then(Value::as_str)
                .ok_or_else(|| "target_level is required when target is specified".to_owned())?;
            let target_level = if level_raw == "clear" {
                None
            } else {
                Some(LogLevel::from_directive(level_raw).ok_or_else(|| format!("invalid target_level `{level_raw}`"))?)
            };
            Some((name, target_level))
        }
        None => {
            if args.get("target_level").is_some() {
                return Err("target is required when target_level is specified".to_owned());
            }
            None
        }
    };
    if level.is_none() && target.is_none() {
        return Err("at least one of level or target is required".to_owned());
    }
    let filter = control.set_filter(level, target).map_err(|e| e.to_string())?;
    Ok(json!({
        "level": control.level().label().to_ascii_lowercase(),
        "filter": filter,
    }))
}

fn handler(handle: ViewerInspectionHandle, args: Value) -> ToolFuture {
    Box::pin(async move {
        let Some(control) = handle.log_control() else {
            return Err("log control is unavailable".to_owned());
        };
        let result = apply_log_level(&control, &args)?;
        Ok(ToolContent::Json(result))
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::apply_log_level;
    use crate::logging::{LogControl, LogLevel};

    #[test]
    fn set_log_level_rejects_invalid_and_applies_valid_filter() {
        let control = LogControl::noop();
        let initial_filter = control.filter();

        assert!(apply_log_level(&control, &json!({ "level": "verbose" })).is_err());
        assert!(apply_log_level(&control, &json!({ "target": "render,viewer=trace", "target_level": "debug" })).is_err());
        assert!(apply_log_level(&control, &json!({ "target": "", "target_level": "debug" })).is_err());
        assert!(apply_log_level(&control, &json!({ "target": "render" })).is_err());
        assert!(apply_log_level(&control, &json!({})).is_err());
        assert_eq!(control.filter(), initial_filter);

        let updated =
            apply_log_level(&control, &json!({ "level": "debug", "target": "render", "target_level": "trace" })).expect("valid filter");
        assert_eq!(control.level(), LogLevel::Debug);
        assert_eq!(updated["level"], "debug");
        assert!(updated["filter"].as_str().expect("filter string").contains("render=trace"));

        let cleared = apply_log_level(&control, &json!({ "target": "render", "target_level": "clear" })).expect("clear target");
        assert!(!cleared["filter"].as_str().expect("filter string").contains("render=trace"));
    }
}
