use std::{borrow::Cow, sync::Arc};

use cvars::{CvarError, CvarValue, ValueDomain, lookup_by_name};
use rmcp::{
    ErrorData,
    handler::server::wrapper::{Json, Parameters},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::settings::cvar_value_to_json;
use crate::{mcp::server::RadiaServer, settings::SettingsStore};

#[derive(Deserialize, JsonSchema)]
pub struct SetSettingArgs {
    #[schemars(description = "CVar name to update.")]
    pub name: String,
    #[schemars(description = "New JSON value matching the CVar's type and bounds.")]
    pub value: Value,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SetSettingOutput {
    pub name: &'static str,
    pub old_value: Value,
    pub new_value: Value,
    pub revision: u64,
    pub pending_restart: bool,
}

fn json_to_cvar_value(name: &'static str, domain: ValueDomain, value: &Value) -> Result<CvarValue, CvarError> {
    match domain {
        ValueDomain::Bool => value.as_bool().map(CvarValue::Bool).ok_or(CvarError::TypeMismatch { name, expected: domain.type_name() }),
        ValueDomain::I64 { .. } => value.as_i64().map(CvarValue::I64).ok_or(CvarError::TypeMismatch { name, expected: domain.type_name() }),
        ValueDomain::F32 { .. } => {
            let number = value.as_f64().ok_or(CvarError::TypeMismatch { name, expected: domain.type_name() })?;
            Ok(CvarValue::F32(number as f32))
        }
        ValueDomain::F64 { .. } => value.as_f64().map(CvarValue::F64).ok_or(CvarError::TypeMismatch { name, expected: domain.type_name() }),
        ValueDomain::String { .. } => value
            .as_str()
            .map(|text| CvarValue::String(Cow::Owned(text.to_owned())))
            .ok_or(CvarError::TypeMismatch { name, expected: domain.type_name() }),
        ValueDomain::Enum { variants } => {
            let text = value.as_str().ok_or(CvarError::TypeMismatch { name, expected: domain.type_name() })?;
            variants
                .iter()
                .copied()
                .find(|variant| *variant == text)
                .map(CvarValue::Enum)
                .ok_or_else(|| CvarError::InvalidEnumVariant { name, value: text.to_owned() })
        }
    }
}

fn apply_setting(settings: &Arc<SettingsStore>, name: &str, raw_value: &Value) -> Result<SetSettingOutput, String> {
    let descriptor = lookup_by_name(name).ok_or_else(|| CvarError::UnknownName(name.to_owned()).to_string())?;
    let parsed = json_to_cvar_value(descriptor.name, descriptor.domain, raw_value).map_err(|e| e.to_string())?;
    let (old_value, new_value, outcome) = settings.set_by_name(name, &parsed).map_err(|e| e.to_string())?;
    Ok(SetSettingOutput {
        name: descriptor.name,
        old_value: cvar_value_to_json(&old_value),
        new_value: cvar_value_to_json(&new_value),
        revision: outcome.revision.get(),
        pending_restart: outcome.pending_restart,
    })
}

#[tool_router(router = set_setting_router, vis = "pub(super)")]
impl RadiaServer {
    #[tool(
        name = "set_setting",
        title = "Set Setting",
        description = "Changes local viewer state: set a local cvar setting by name through SettingsStore validation and persistence.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn set_setting(&self, Parameters(args): Parameters<SetSettingArgs>) -> Result<Json<SetSettingOutput>, ErrorData> {
        let Some(settings) = self.handle.settings() else {
            return Err(ErrorData::internal_error("settings store is unavailable", None));
        };
        let output = apply_setting(&settings, &args.name, &args.value).map_err(|msg| ErrorData::invalid_params(msg, None))?;
        Ok(Json(output))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cvars::RENDER_DRAW_DISTANCE;
    use serde_json::json;
    use tempfile::tempdir;

    use super::apply_setting;
    use crate::settings::SettingsStore;

    #[test]
    fn set_setting_validates_and_persists() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("settings.toml");
        let (store, report) = SettingsStore::load_or_default(path.clone(), None);
        assert!(report.is_ok());
        let store = Arc::new(store);

        let unknown = apply_setting(&store, "no.such.cvar", &json!(1)).unwrap_err();
        assert!(unknown.contains("unknown CVar"), "unexpected error: {unknown}");

        let bad_type = apply_setting(&store, "render.draw_distance", &json!("far")).unwrap_err();
        assert!(bad_type.contains("expects a"), "unexpected error: {bad_type}");

        let out_of_bounds = apply_setting(&store, "render.draw_distance", &json!(99999.0)).unwrap_err();
        assert!(out_of_bounds.contains("outside declared bounds"), "unexpected error: {out_of_bounds}");

        let result = apply_setting(&store, "render.draw_distance", &json!(256.0)).expect("valid setting change");
        assert_eq!(result.name, "render.draw_distance");
        assert_eq!(result.new_value, 256.0);
        assert_eq!(store.get(RENDER_DRAW_DISTANCE), 256.0);

        let (reloaded, report) = SettingsStore::load_or_default(path, None);
        assert!(report.is_ok());
        assert_eq!(reloaded.get(RENDER_DRAW_DISTANCE), 256.0);
    }
}
