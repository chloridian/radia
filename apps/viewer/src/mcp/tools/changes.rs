use rmcp::{
    ErrorData,
    handler::server::wrapper::{Json, Parameters},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
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
    pub changes: Option<Vec<WorldChangeOutput>>,
}

#[derive(Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorldChangeOutput {
    EntityAdded { revision: u64, id: String, region: u64, local_id: u32 },
    EntityUpdated { revision: u64, id: String, classes: Vec<ChangeClassOutput>, reparented: bool },
    SubtreeInvalidated { revision: u64, id: String },
    SubtreeRemovalPending { revision: u64, id: String, region: u64, order: u64, block_ordinal: u32 },
    Kill { revision: u64, id: String, region: u64, local_id: u32, reason: RemovalReasonOutput },
    EntityRemoved { revision: u64, id: String, region: u64, local_id: u32, reason: RemovalReasonOutput },
    EntityRebound { revision: u64, id: String, from_region: u64, from_local_id: u32, to_region: u64, to_local_id: u32 },
    EntityTransferred { revision: u64, id: String, from_region: u64, from_local_id: u32, to_region: u64, to_local_id: u32 },
    RegionRegistered { revision: u64, region: u64 },
    RegionUpdated { revision: u64, region: u64 },
    EnvironmentUpdated { revision: u64, region: u64 },
    ParcelUpdated { revision: u64, region: u64 },
    TerrainPatchUpdated { revision: u64, region: u64, patch: [u8; 2] },
    WaterHeightUpdated { revision: u64, region: u64 },
    RegionRetired { revision: u64, region: u64, entities_removed: usize },
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
pub enum ChangeClassOutput {
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
pub enum RemovalReasonOutput {
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

fn format_record(record: &WorldChangeRecord) -> WorldChangeOutput {
    let revision = record.revision.get();
    match record.change {
        WorldChange::EntityAdded { id, address, .. } => WorldChangeOutput::EntityAdded {
            revision,
            id: id.to_string(),
            region: address.region().get(),
            local_id: address.local_id().get(),
        },
        WorldChange::EntityUpdated { id, classes, .. } => WorldChangeOutput::EntityUpdated {
            revision,
            id: id.to_string(),
            classes: change_classes_list(classes),
            reparented: classes.contains(WorldChangeClasses::RELATIONSHIP),
        },
        WorldChange::SubtreeInvalidated { id, .. } => WorldChangeOutput::SubtreeInvalidated { revision, id: id.to_string() },
        WorldChange::SubtreeRemovalPending { id, region, order, block_ordinal, .. } => {
            WorldChangeOutput::SubtreeRemovalPending { revision, id: id.to_string(), region: region.get(), order, block_ordinal }
        }
        WorldChange::EntityRemoved { id, address, reason, .. } => {
            let id = id.to_string();
            let region = address.region().get();
            let local_id = address.local_id().get();
            let reason_out = RemovalReasonOutput::from(reason);
            if reason == WorldEntityRemovalReason::Killed {
                WorldChangeOutput::Kill { revision, id, region, local_id, reason: reason_out }
            } else {
                WorldChangeOutput::EntityRemoved { revision, id, region, local_id, reason: reason_out }
            }
        }
        WorldChange::EntityRebound { id, from, to, .. } => WorldChangeOutput::EntityRebound {
            revision,
            id: id.to_string(),
            from_region: from.region().get(),
            from_local_id: from.local_id().get(),
            to_region: to.region().get(),
            to_local_id: to.local_id().get(),
        },
        WorldChange::EntityTransferred { id, from, to, .. } => WorldChangeOutput::EntityTransferred {
            revision,
            id: id.to_string(),
            from_region: from.region().get(),
            from_local_id: from.local_id().get(),
            to_region: to.region().get(),
            to_local_id: to.local_id().get(),
        },
        WorldChange::RegionRegistered(region) => WorldChangeOutput::RegionRegistered { revision, region: region.get() },
        WorldChange::RegionUpdated(region) => WorldChangeOutput::RegionUpdated { revision, region: region.get() },
        WorldChange::EnvironmentUpdated(region) => WorldChangeOutput::EnvironmentUpdated { revision, region: region.get() },
        WorldChange::ParcelUpdated(region) => WorldChangeOutput::ParcelUpdated { revision, region: region.get() },
        WorldChange::TerrainPatchUpdated { region, patch } => {
            let (x, y) = patch.coordinates();
            WorldChangeOutput::TerrainPatchUpdated { revision, region: region.get(), patch: [x, y] }
        }
        WorldChange::WaterHeightUpdated(region) => WorldChangeOutput::WaterHeightUpdated { revision, region: region.get() },
        WorldChange::RegionRetired { region, entities_removed } => {
            WorldChangeOutput::RegionRetired { revision, region: region.get(), entities_removed }
        }
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
