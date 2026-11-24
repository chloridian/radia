use rmcp::{
    ErrorData,
    handler::server::wrapper::{Json, Parameters},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{inspection::InventoryFetchStats, mcp::server::RadiaServer};

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;

#[derive(Deserialize, JsonSchema)]
pub struct InventoryArgs {
    #[schemars(description = "Maximum number of failed folders to return (1..=200).")]
    pub limit: Option<usize>,
}

#[derive(Serialize, JsonSchema)]
pub struct FailedFolderOutput {
    pub folder_id: String,
    pub owner_id: String,
    pub retry_state: &'static str,
    pub error_kind: &'static str,
    pub error: String,
}

#[derive(Serialize, JsonSchema)]
pub struct InventoryOutput {
    pub active_session: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folders_known: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_flight: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_done: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_folders: Option<Vec<FailedFolderOutput>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
}

#[tool_router(router = inventory_router, vis = "pub(super)")]
impl RadiaServer {
    #[tool(
        name = "inventory",
        title = "Inventory Fetch Status",
        description = "Return inventory folder fetch counts (pending, in flight, done, failed) and a bounded list of failed folders without item or asset data.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn inventory(&self, Parameters(args): Parameters<InventoryArgs>) -> Result<Json<InventoryOutput>, ErrorData> {
        let Some(session) = self.handle.session() else {
            return Ok(Json(InventoryOutput {
                active_session: false,
                folders_known: None,
                pending: None,
                in_flight: None,
                done: None,
                failed: None,
                background_done: None,
                failed_folders: None,
                has_more: None,
            }));
        };
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let stats = session.diagnostics.inventory.borrow().clone();
        Ok(Json(format_inventory_stats(&stats, limit)))
    }
}

fn format_inventory_stats(stats: &InventoryFetchStats, limit: usize) -> InventoryOutput {
    let failed_folders = stats
        .failed_folders
        .iter()
        .take(limit)
        .map(|folder| FailedFolderOutput {
            folder_id: folder.folder_id.to_string(),
            owner_id: folder.owner_id.to_string(),
            retry_state: folder.retry_state,
            error_kind: folder.error_kind,
            error: folder.error.clone(),
        })
        .collect();
    InventoryOutput {
        active_session: true,
        folders_known: Some(stats.folders_known),
        pending: Some(stats.folders_pending),
        in_flight: Some(stats.folders_in_flight),
        done: Some(stats.folders_done),
        failed: Some(stats.folders_failed),
        background_done: Some(stats.background_done),
        failed_folders: Some(failed_folders),
        has_more: Some(stats.failed_folders.len() > limit),
    }
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

        let output = serde_json::to_value(format_inventory_stats(&stats, 1)).expect("serialize inventory");
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
