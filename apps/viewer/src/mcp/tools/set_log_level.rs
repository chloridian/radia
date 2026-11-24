use rmcp::{
    ErrorData,
    handler::server::wrapper::{Json, Parameters},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    logging::{LogControl, LogLevel},
    mcp::server::Handler,
};

#[derive(Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TargetLogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
    Clear,
}

impl TargetLogLevel {
    fn into_option(self) -> Option<LogLevel> {
        match self {
            Self::Error => Some(LogLevel::Error),
            Self::Warn => Some(LogLevel::Warn),
            Self::Info => Some(LogLevel::Info),
            Self::Debug => Some(LogLevel::Debug),
            Self::Trace => Some(LogLevel::Trace),
            Self::Clear => None,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct SetLogLevelArgs {
    #[schemars(description = "Global log level.")]
    pub level: Option<LogLevel>,
    #[schemars(description = "Rust tracing target name for a per-target override.")]
    pub target: Option<String>,
    #[schemars(description = "Per-target log level, or 'clear' to remove the override.")]
    pub target_level: Option<TargetLogLevel>,
}

#[derive(Serialize, JsonSchema)]
pub struct SetLogLevelOutput {
    pub level: String,
    pub filter: String,
}

fn apply_log_level(control: &LogControl, args: &SetLogLevelArgs) -> Result<SetLogLevelOutput, String> {
    let target = match args.target.as_deref() {
        Some(name) => {
            let level = args.target_level.ok_or_else(|| "target_level is required when target is specified".to_owned())?;
            Some((name, level.into_option()))
        }
        None => {
            if args.target_level.is_some() {
                return Err("target is required when target_level is specified".to_owned());
            }
            None
        }
    };
    if args.level.is_none() && target.is_none() {
        return Err("at least one of level or target is required".to_owned());
    }
    let filter = control.set_filter(args.level, target).map_err(|e| e.to_string())?;
    Ok(SetLogLevelOutput { level: control.level().label().to_ascii_lowercase(), filter })
}

#[tool_router(router = set_log_level_router, vis = "pub(super)")]
impl Handler {
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
    use super::{SetLogLevelArgs, TargetLogLevel, apply_log_level};
    use crate::logging::{LogControl, LogLevel};

    #[test]
    fn set_log_level_rejects_invalid_and_applies_valid_filter() {
        let control = LogControl::noop();
        let initial_filter = control.filter();

        assert!(
            apply_log_level(
                &control,
                &SetLogLevelArgs { level: None, target: Some("render,viewer=trace".into()), target_level: Some(TargetLogLevel::Debug) },
            )
            .is_err()
        );
        assert!(
            apply_log_level(
                &control,
                &SetLogLevelArgs { level: None, target: Some(String::new()), target_level: Some(TargetLogLevel::Debug) },
            )
            .is_err()
        );
        assert!(apply_log_level(&control, &SetLogLevelArgs { level: None, target: Some("render".into()), target_level: None }).is_err());
        assert!(apply_log_level(&control, &SetLogLevelArgs { level: None, target: None, target_level: None }).is_err());
        assert_eq!(control.filter(), initial_filter);

        let updated = apply_log_level(
            &control,
            &SetLogLevelArgs { level: Some(LogLevel::Debug), target: Some("render".into()), target_level: Some(TargetLogLevel::Trace) },
        )
        .expect("valid filter");
        assert_eq!(control.level(), LogLevel::Debug);
        assert_eq!(updated.level, "debug");
        assert!(updated.filter.contains("render=trace"));

        let cleared = apply_log_level(
            &control,
            &SetLogLevelArgs { level: None, target: Some("render".into()), target_level: Some(TargetLogLevel::Clear) },
        )
        .expect("clear target");
        assert!(!cleared.filter.contains("render=trace"));
    }
}
