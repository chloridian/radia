use cvars::{CvarSnapshot, CvarValue, descriptors};
use serde_json::{Value, json};

use super::{ToolContent, ToolFuture, ToolSpec};
use crate::{inspection::ViewerInspectionHandle, mcp::MAX_JSON_RESPONSE_BYTES};

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;

pub const SPEC: ToolSpec = ToolSpec {
    name: "settings",
    description: "Return current cvar values and provenance.",
    input_schema,
    handler,
    max_response_bytes: MAX_JSON_RESPONSE_BYTES,
};

fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "prefix": { "type": "string" },
            "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT }
        }
    })
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

fn format_settings_snapshot(snapshot: &CvarSnapshot, prefix: Option<&str>, limit: usize) -> Value {
    let mut matched = descriptors().iter().filter(|desc| prefix.is_none_or(|prefix| desc.name.starts_with(prefix)));
    let mut entries = Vec::new();
    for desc in matched.by_ref().take(limit) {
        let provenance = snapshot.provenance(desc.id);
        entries.push(json!({
            "name": desc.name,
            "value": cvar_value_to_json(&provenance.value),
            "source": format!("{:?}", provenance.source),
            "unit": desc.unit,
            "apply": format!("{:?}", desc.apply),
            "persistence": format!("{:?}", desc.persistence),
            "pending_restart": provenance.pending_restart_value.as_ref().map(cvar_value_to_json),
        }));
    }
    let has_more = matched.next().is_some();
    json!({
        "revision": snapshot.revision().get(),
        "settings": entries,
        "has_more": has_more,
    })
}

fn handler(handle: ViewerInspectionHandle, args: Value) -> ToolFuture {
    Box::pin(async move {
        let Some(settings) = handle.settings() else {
            return Ok(ToolContent::Json(json!({ "available": false })));
        };
        let prefix = args.get("prefix").and_then(Value::as_str);
        let limit =
            args.get("limit").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let snapshot = settings.snapshot();
        Ok(ToolContent::Json(format_settings_snapshot(&snapshot, prefix, limit)))
    })
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
        let items = output["settings"].as_array().expect("settings array");
        assert_eq!(items.len(), 2);
        assert_eq!(output["has_more"], true);
    }
}
