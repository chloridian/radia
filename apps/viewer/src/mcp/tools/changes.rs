use rmcp::{
    ErrorData,
    handler::server::wrapper::{Json, Parameters},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use world::{MAX_WORLD_CHANGE_PAGE_ITEMS, WorldChange, WorldChangeClasses, WorldChangeCursor, WorldChangeRecord, WorldEntityRemovalReason};

use crate::mcp::server::Handler;

const DEFAULT_LIMIT: usize = 50;

#[derive(Deserialize, JsonSchema)]
pub struct ChangesArgs {
    #[schemars(description = "Opaque cursor token from a previous changes response.")]
    pub cursor: Option<String>,
    #[schemars(description = "Maximum number of change records to return (1..=MAX_WORLD_CHANGE_PAGE_ITEMS).")]
    pub limit: Option<usize>,
}

#[derive(Serialize, JsonSchema)]
pub struct ChangesOutput {
    pub active_session: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changes: Option<Vec<Value>>,
}

#[tool_router(router = changes_router, vis = "pub(super)")]
impl Handler {
    #[tool(
        name = "changes",
        title = "World Changes",
        description = "Return a bounded page of the World change stream using an optional cursor and returning the next cursor.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn changes(&self, Parameters(args): Parameters<ChangesArgs>) -> Result<Json<ChangesOutput>, ErrorData> {
        let Some(session) = self.handle.session() else {
            return Ok(Json(ChangesOutput {
                active_session: false,
                observed_revision: None,
                completed_revision: None,
                next_cursor: None,
                has_more: None,
                changes: None,
            }));
        };
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_WORLD_CHANGE_PAGE_ITEMS);
        let cursor = match args.cursor.as_deref() {
            Some(token) => WorldChangeCursor::decode(token).ok_or_else(|| ErrorData::invalid_params("invalid cursor", None))?,
            None => {
                session.world.barrier().await.map_err(|e| ErrorData::internal_error(format!("world barrier failed: {e}"), None))?.changes
            }
        };
        let page = session
            .world
            .changes(cursor, limit)
            .await
            .map_err(|e| ErrorData::internal_error(format!("changes query failed: {e}"), None))?;
        let changes = page.records.iter().map(format_record).collect();
        Ok(Json(ChangesOutput {
            active_session: true,
            observed_revision: Some(page.observed_revision.get()),
            completed_revision: Some(page.completed_revision.get()),
            next_cursor: Some(page.cursor.encode()),
            has_more: Some(page.has_more),
            changes: Some(changes),
        }))
    }
}

#[derive(Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ChangeClassOutput {
    Pose,
    Motion,
    Scale,
    Relationship,
    Shape,
    Appearance,
    Media,
    PbrMaterialOverrides,
}

#[derive(Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum RemovalReasonOutput {
    Killed,
    AddressReplaced,
    ParentCascade,
    RegionRetired,
}

impl From<WorldEntityRemovalReason> for RemovalReasonOutput {
    fn from(reason: WorldEntityRemovalReason) -> Self {
        match reason {
            WorldEntityRemovalReason::Killed => Self::Killed,
            WorldEntityRemovalReason::AddressReplaced => Self::AddressReplaced,
            WorldEntityRemovalReason::ParentCascade => Self::ParentCascade,
            WorldEntityRemovalReason::RegionRetired => Self::RegionRetired,
        }
    }
}

fn change_classes_list(classes: WorldChangeClasses) -> Vec<ChangeClassOutput> {
    let mut list = Vec::new();
    if classes.contains(WorldChangeClasses::POSE) {
        list.push(ChangeClassOutput::Pose);
    }
    if classes.contains(WorldChangeClasses::MOTION) {
        list.push(ChangeClassOutput::Motion);
    }
    if classes.contains(WorldChangeClasses::SCALE) {
        list.push(ChangeClassOutput::Scale);
    }
    if classes.contains(WorldChangeClasses::RELATIONSHIP) {
        list.push(ChangeClassOutput::Relationship);
    }
    if classes.contains(WorldChangeClasses::SHAPE) {
        list.push(ChangeClassOutput::Shape);
    }
    if classes.contains(WorldChangeClasses::APPEARANCE) {
        list.push(ChangeClassOutput::Appearance);
    }
    if classes.contains(WorldChangeClasses::MEDIA) {
        list.push(ChangeClassOutput::Media);
    }
    if classes.contains(WorldChangeClasses::PBR_MATERIAL_OVERRIDES) {
        list.push(ChangeClassOutput::PbrMaterialOverrides);
    }
    list
}

fn format_record(record: &WorldChangeRecord) -> Value {
    let revision = record.revision.get();
    match record.change {
        WorldChange::EntityAdded { id, address, .. } => json!({
            "revision": revision,
            "kind": "entity_added",
            "id": id.to_string(),
            "region": address.region().get(),
            "local_id": address.local_id(),
        }),
        WorldChange::EntityUpdated { id, classes, .. } => json!({
            "revision": revision,
            "kind": "entity_updated",
            "id": id.to_string(),
            "classes": change_classes_list(classes),
            "reparented": classes.contains(WorldChangeClasses::RELATIONSHIP),
        }),
        WorldChange::SubtreeInvalidated { id, .. } => json!({
            "revision": revision,
            "kind": "subtree_invalidated",
            "id": id.to_string(),
        }),
        WorldChange::SubtreeRemovalPending { id, region, order, block_ordinal, .. } => json!({
            "revision": revision,
            "kind": "subtree_removal_pending",
            "id": id.to_string(),
            "region": region.get(),
            "order": order,
            "block_ordinal": block_ordinal,
        }),
        WorldChange::EntityRemoved { id, address, reason, .. } => json!({
            "revision": revision,
            "kind": if reason == WorldEntityRemovalReason::Killed { "kill" } else { "entity_removed" },
            "id": id.to_string(),
            "region": address.region().get(),
            "local_id": address.local_id(),
            "reason": RemovalReasonOutput::from(reason),
        }),
        WorldChange::EntityRebound { id, from, to, .. } => json!({
            "revision": revision,
            "kind": "entity_rebound",
            "id": id.to_string(),
            "from_region": from.region().get(),
            "from_local_id": from.local_id(),
            "to_region": to.region().get(),
            "to_local_id": to.local_id(),
        }),
        WorldChange::EntityTransferred { id, from, to, .. } => json!({
            "revision": revision,
            "kind": "entity_transferred",
            "id": id.to_string(),
            "from_region": from.region().get(),
            "from_local_id": from.local_id(),
            "to_region": to.region().get(),
            "to_local_id": to.local_id(),
        }),
        WorldChange::RegionRegistered(region) => json!({
            "revision": revision,
            "kind": "region_registered",
            "region": region.get(),
        }),
        WorldChange::RegionUpdated(region) => json!({
            "revision": revision,
            "kind": "region_updated",
            "region": region.get(),
        }),
        WorldChange::EnvironmentUpdated(region) => json!({
            "revision": revision,
            "kind": "environment_updated",
            "region": region.get(),
        }),
        WorldChange::ParcelUpdated(region) => json!({
            "revision": revision,
            "kind": "parcel_updated",
            "region": region.get(),
        }),
        WorldChange::TerrainPatchUpdated { region, patch } => {
            let (x, y) = patch.coordinates();
            json!({
                "revision": revision,
                "kind": "terrain_patch_updated",
                "region": region.get(),
                "patch": [x, y],
            })
        }
        WorldChange::WaterHeightUpdated(region) => json!({
            "revision": revision,
            "kind": "water_height_updated",
            "region": region.get(),
        }),
        WorldChange::RegionRetired { region, entities_removed } => json!({
            "revision": revision,
            "kind": "region_retired",
            "region": region.get(),
            "entities_removed": entities_removed,
        }),
    }
}

#[cfg(test)]
mod tests {
    use world::{World, WorldChangeCursor, WorldLimits};

    #[test]
    fn change_cursor_roundtrips_and_rejects_malformed() {
        let world = World::new(WorldLimits {
            max_regions: 2,
            max_entities: 8,
            max_entities_per_region: 8,
            max_tombstones_per_region: 8,
            max_tombstones: 8,
            max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(8),
            max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(8),
            max_journal_entries: 8,
            max_journal_changes: 32,
            max_region_name_bytes: 64,
            max_recovery_targets: 4,
        });
        let cursor = world.read_barrier().changes;
        let encoded = cursor.encode();
        assert_eq!(WorldChangeCursor::decode(&encoded), Some(cursor));
        assert_eq!(WorldChangeCursor::decode("1:0:1"), None);
        assert_eq!(WorldChangeCursor::decode("not-a-cursor"), None);
        assert_eq!(WorldChangeCursor::decode("1:2:3:4"), None);
    }
}
