use serde_json::{Value, json};

use super::{ToolAnnotationsSpec, ToolContent, ToolFuture, ToolSpec};
use crate::{
    inspection::{InventoryFetchStats, ViewerInspectionHandle},
    mcp::MAX_JSON_RESPONSE_BYTES,
};

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;

pub const SPEC: ToolSpec = ToolSpec {
    name: "inventory",
    title: "Inventory Fetch Status",
    description: "Return inventory folder fetch counts (pending, in flight, done, failed) and a bounded list of failed folders without item or asset data.",
    annotations: ToolAnnotationsSpec::read_only(),
    input_schema,
    handler,
    max_response_bytes: MAX_JSON_RESPONSE_BYTES,
};

fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT }
        }
    })
}

fn format_inventory_stats(stats: &InventoryFetchStats, limit: usize) -> Value {
    let failed_folders = stats
        .failed_folders
        .iter()
        .take(limit)
        .map(|folder| {
            json!({
                "folder_id": folder.folder_id.to_string(),
                "owner_id": folder.owner_id.to_string(),
                "retry_state": folder.retry_state,
                "error_kind": folder.error_kind,
                "error": folder.error,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "active_session": true,
        "folders_known": stats.folders_known,
        "pending": stats.folders_pending,
        "in_flight": stats.folders_in_flight,
        "done": stats.folders_done,
        "failed": stats.folders_failed,
        "background_done": stats.background_done,
        "failed_folders": failed_folders,
        "has_more": stats.failed_folders.len() > limit,
    })
}

fn handler(handle: ViewerInspectionHandle, args: Value) -> ToolFuture {
    Box::pin(async move {
        let Some(session) = handle.session() else {
            return Ok(ToolContent::Json(json!({ "active_session": false })));
        };
        let limit =
            args.get("limit").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let stats = session.diagnostics.inventory.borrow().clone();
        Ok(ToolContent::Json(format_inventory_stats(&stats, limit)))
    })
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::format_inventory_stats;
    use crate::inspection::{FailedInventoryFolder, InventoryFetchStats};

    #[test]
    fn inventory_output_bounds_failures_and_omits_items() {
        let stats = InventoryFetchStats {
            folders_known: 10,
            folders_pending: 2,
            folders_in_flight: 1,
            folders_done: 5,
            folders_failed: 2,
            background_done: false,
            failed_folders: vec![
                FailedInventoryFolder {
                    folder_id: Uuid::from_u128(1),
                    owner_id: Uuid::from_u128(10),
                    retry_state: "non_retryable",
                    error_kind: "invalid_response",
                    error: "inventory server returned an invalid folder response: inventory response contains malformed LLSD".to_owned(),
                },
                FailedInventoryFolder {
                    folder_id: Uuid::from_u128(2),
                    owner_id: Uuid::from_u128(10),
                    retry_state: "exhausted",
                    error_kind: "retry_exhausted",
                    error: "inventory folder fetch retries were exhausted: request timed out".to_owned(),
                },
            ],
        };

        let output = format_inventory_stats(&stats, 1);
        assert_eq!(output["pending"], 2);
        assert_eq!(output["in_flight"], 1);
        assert_eq!(output["done"], 5);
        assert_eq!(output["failed"], 2);
        assert_eq!(output["has_more"], true);

        let failed = output["failed_folders"].as_array().expect("failed_folders array");
        assert_eq!(failed.len(), 1);
        let entry = failed[0].as_object().expect("failed folder object");
        assert_eq!(entry.len(), 5);
        assert!(entry.contains_key("folder_id"));
        assert!(entry.contains_key("owner_id"));
        assert!(entry.contains_key("retry_state"));
        assert!(entry.contains_key("error_kind"));
        assert!(entry.contains_key("error"));
        assert!(!entry.contains_key("items"));
        assert!(!entry.contains_key("item_name"));
        assert!(!entry.contains_key("asset_id"));

        let text = output.to_string();
        assert!(text.contains("malformed LLSD"));
        assert!(!text.contains("asset_id"));
        assert!(!text.contains("items"));
    }
}
