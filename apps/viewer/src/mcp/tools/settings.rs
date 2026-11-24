use cvars::{ApplyScope, CvarSnapshot, CvarValue, Layer, PersistenceScope, descriptors};
use rmcp::{
    ErrorData,
    handler::server::wrapper::{Json, Parameters},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{inspection::ViewerInspectionHandle, mcp::server::Handler};

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;

#[derive(Deserialize, JsonSchema)]
pub struct SettingsArgs {
    #[schemars(description = "Optional CVar name prefix filter.")]
    pub prefix: Option<String>,
    #[schemars(description = "Maximum number of settings to return (1..=500).")]
    pub limit: Option<usize>,
}

#[derive(Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SettingSourceOutput {
    Default,
    Preset,
    Global,
    StartupDeveloper,
    Session,
}

#[derive(Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SettingApplyOutput {
    Live,
    RestartRequired,
}

#[derive(Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SettingPersistenceOutput {
    Global,
    SessionOnly,
}

#[derive(Serialize, JsonSchema)]
pub struct SettingEntryOutput {
    pub name: &'static str,
    pub value: Value,
    pub source: SettingSourceOutput,
    pub unit: Option<&'static str>,
    pub apply: SettingApplyOutput,
    pub persistence: SettingPersistenceOutput,
    pub pending_restart: Option<Value>,
}

#[derive(Serialize, JsonSchema)]
pub struct SettingsOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings: Option<Vec<SettingEntryOutput>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
}

#[tool_router(router = settings_router, vis = "pub(super)")]
impl Handler {
    #[tool(
        name = "settings",
        title = "Viewer Settings",
        description = "Return current cvar values and provenance.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn settings(&self, Parameters(args): Parameters<SettingsArgs>) -> Result<Json<SettingsOutput>, ErrorData> {
        Ok(Json(settings_data(&self.handle, args.prefix.as_deref(), args.limit)))
    }
}

pub(in crate::mcp) fn settings_data(handle: &ViewerInspectionHandle, prefix: Option<&str>, limit: Option<usize>) -> SettingsOutput {
    let Some(settings) = handle.settings() else {
        return SettingsOutput { available: Some(false), revision: None, settings: None, has_more: None };
    };
    let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let snapshot = settings.snapshot();
    format_settings_snapshot(&snapshot, prefix, limit)
}

pub(super) fn cvar_value_to_json(value: &CvarValue) -> Value {
    match value {
        CvarValue::Bool(flag) => Value::from(*flag),
        CvarValue::I64(number) => Value::from(*number),
        CvarValue::F32(number) => Value::from(*number),
        CvarValue::F64(number) => Value::from(*number),
        CvarValue::String(text) => Value::from(text.as_ref()),
        CvarValue::Enum(variant) => Value::from(*variant),
    }
}

fn format_settings_snapshot(snapshot: &CvarSnapshot, prefix: Option<&str>, limit: usize) -> SettingsOutput {
    let mut matched = descriptors().iter().filter(|desc| prefix.is_none_or(|prefix| desc.name.starts_with(prefix)));
    let mut entries = Vec::new();
    for desc in matched.by_ref().take(limit) {
        let provenance = snapshot.provenance(desc.id);
        entries.push(SettingEntryOutput {
            name: desc.name,
            value: cvar_value_to_json(&provenance.value),
            source: match provenance.source {
                Layer::Default => SettingSourceOutput::Default,
                Layer::Preset => SettingSourceOutput::Preset,
                Layer::Global => SettingSourceOutput::Global,
                Layer::StartupDeveloper => SettingSourceOutput::StartupDeveloper,
                Layer::Session => SettingSourceOutput::Session,
            },
            unit: desc.unit,
            apply: match desc.apply {
                ApplyScope::Live => SettingApplyOutput::Live,
                ApplyScope::RestartRequired => SettingApplyOutput::RestartRequired,
            },
            persistence: match desc.persistence {
                PersistenceScope::Global => SettingPersistenceOutput::Global,
                PersistenceScope::SessionOnly => SettingPersistenceOutput::SessionOnly,
            },
            pending_restart: provenance.pending_restart_value.as_ref().map(cvar_value_to_json),
        });
    }
    let has_more = matched.next().is_some();
    SettingsOutput { available: None, revision: Some(snapshot.revision().get()), settings: Some(entries), has_more: Some(has_more) }
}

#[cfg(test)]
mod tests {
    use cvars::CvarRegistry;

    use super::format_settings_snapshot;

    #[test]
    fn settings_snapshot_bounds_output() {
        let registry = CvarRegistry::new();
        let snapshot = registry.snapshot();
        let output = format_settings_snapshot(&snapshot, None, 2);
        let items = output.settings.expect("settings vec");
        assert_eq!(items.len(), 2);
        assert_eq!(output.has_more, Some(true));
    }
}
