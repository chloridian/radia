use std::{borrow::Cow, sync::Arc};

use cvars::{CvarError, CvarValue, ValueDomain, lookup_by_name};
use serde_json::{Value, json};

use super::{ToolContent, ToolFuture, ToolSpec, settings::cvar_value_to_json};
use crate::{inspection::ViewerInspectionHandle, mcp::MAX_JSON_RESPONSE_BYTES, settings::SettingsStore};

pub const SPEC: ToolSpec = ToolSpec {
    name: "set_setting",
    description: "Changes local viewer state: set a local cvar setting by name through SettingsStore validation and persistence.",
    input_schema,
    handler,
    max_response_bytes: MAX_JSON_RESPONSE_BYTES,
};

fn input_schema() -> Value {
    json!({
        "type": "object",
        "required": ["name", "value"],
        "properties": {
            "name": { "type": "string" },
            "value": {}
        }
    })
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

fn apply_setting(settings: &Arc<SettingsStore>, name: &str, raw_value: &Value) -> Result<Value, String> {
    let descriptor = lookup_by_name(name).ok_or_else(|| CvarError::UnknownName(name.to_owned()).to_string())?;
    let parsed = json_to_cvar_value(descriptor.name, descriptor.domain, raw_value).map_err(|e| e.to_string())?;
    let (old_value, new_value, outcome) = settings.set_by_name(name, &parsed).map_err(|e| e.to_string())?;
    Ok(json!({
        "name": descriptor.name,
        "old_value": cvar_value_to_json(&old_value),
        "new_value": cvar_value_to_json(&new_value),
        "revision": outcome.revision.get(),
        "pending_restart": outcome.pending_restart,
    }))
}

fn handler(handle: ViewerInspectionHandle, args: Value) -> ToolFuture {
    Box::pin(async move {
        let Some(settings) = handle.settings() else {
            return Err("settings store is unavailable".to_owned());
        };
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            return Err("name is required".to_owned());
        };
        let Some(value) = args.get("value") else {
            return Err("value is required".to_owned());
        };
        let result = apply_setting(&settings, name, value)?;
        Ok(ToolContent::Json(result))
    })
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
        assert_eq!(result["name"], "render.draw_distance");
        assert_eq!(result["new_value"], 256.0);
        assert_eq!(store.get(RENDER_DRAW_DISTANCE), 256.0);

        let (reloaded, report) = SettingsStore::load_or_default(path, None);
        assert!(report.is_ok());
        assert_eq!(reloaded.get(RENDER_DRAW_DISTANCE), 256.0);
    }
}
