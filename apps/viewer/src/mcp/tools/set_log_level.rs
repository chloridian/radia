use rmcp::{
    ErrorData,
    handler::server::wrapper::{Json, Parameters},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    logging::{LogControl, LogLevel},
    mcp::server::RadiaServer,
};

#[derive(Deserialize, JsonSchema)]
pub struct SetLogLevelArgs {
    #[schemars(description = "Global log level: error, warn, info, debug, or trace.")]
    pub level: Option<String>,
    #[schemars(description = "Rust tracing target name for a per-target override.")]
    pub target: Option<String>,
    #[schemars(description = "Per-target log level: error, warn, info, debug, trace, or clear.")]
    pub target_level: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct SetLogLevelOutput {
    pub level: String,
    pub filter: String,
}

fn apply_log_level(control: &LogControl, args: &SetLogLevelArgs) -> Result<SetLogLevelOutput, String> {
    let level = match args.level.as_deref() {
        Some(text) => Some(LogLevel::from_directive(text).ok_or_else(|| format!("invalid log level `{text}`"))?),
        None => None,
    };
    let target = match args.target.as_deref() {
        Some(name) => {
            let level_raw = args.target_level.as_deref().ok_or_else(|| "target_level is required when target is specified".to_owned())?;
            let target_level = if level_raw == "clear" {
                None
            } else {
                Some(LogLevel::from_directive(level_raw).ok_or_else(|| format!("invalid target_level `{level_raw}`"))?)
            };
            Some((name, target_level))
        }
        None => {
            if args.target_level.is_some() {
                return Err("target is required when target_level is specified".to_owned());
            }
            None
        }
    };
    if level.is_none() && target.is_none() {
        return Err("at least one of level or target is required".to_owned());
    }
    let filter = control.set_filter(level, target).map_err(|e| e.to_string())?;
    Ok(SetLogLevelOutput { level: control.level().label().to_ascii_lowercase(), filter })
}

#[tool_router(router = set_log_level_router, vis = "pub(super)")]
impl RadiaServer {
    #[tool(
        name = "set_log_level",
        title = "Set Log Level",
        description = "Change the local viewer's runtime log filter (global level and/or per-target override) through LogControl and return the active filter.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn set_log_level(&self, Parameters(args): Parameters<SetLogLevelArgs>) -> Result<Json<SetLogLevelOutput>, ErrorData> {
        let Some(control) = self.handle.log_control() else {
            return Err(ErrorData::internal_error("log control is unavailable", None));
        };
        let output = apply_log_level(&control, &args).map_err(|msg| ErrorData::invalid_params(msg, None))?;
        Ok(Json(output))
    }
}

#[cfg(test)]
mod tests {
    use super::{SetLogLevelArgs, apply_log_level};
    use crate::logging::{LogControl, LogLevel};

    #[test]
    fn set_log_level_rejects_invalid_and_applies_valid_filter() {
        let control = LogControl::noop();
        let initial_filter = control.filter();

        assert!(apply_log_level(&control, &SetLogLevelArgs { level: Some("verbose".into()), target: None, target_level: None }).is_err());
        assert!(
            apply_log_level(
                &control,
                &SetLogLevelArgs { level: None, target: Some("render,viewer=trace".into()), target_level: Some("debug".into()) },
            )
            .is_err()
        );
        assert!(
            apply_log_level(&control, &SetLogLevelArgs { level: None, target: Some(String::new()), target_level: Some("debug".into()) })
                .is_err()
        );
        assert!(apply_log_level(&control, &SetLogLevelArgs { level: None, target: Some("render".into()), target_level: None }).is_err());
        assert!(apply_log_level(&control, &SetLogLevelArgs { level: None, target: None, target_level: None }).is_err());
        assert_eq!(control.filter(), initial_filter);

        let updated = apply_log_level(
            &control,
            &SetLogLevelArgs { level: Some("debug".into()), target: Some("render".into()), target_level: Some("trace".into()) },
        )
        .expect("valid filter");
        assert_eq!(control.level(), LogLevel::Debug);
        assert_eq!(updated.level, "debug");
        assert!(updated.filter.contains("render=trace"));

        let cleared =
            apply_log_level(&control, &SetLogLevelArgs { level: None, target: Some("render".into()), target_level: Some("clear".into()) })
                .expect("clear target");
        assert!(!cleared.filter.contains("render=trace"));
    }
}
