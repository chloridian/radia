use rmcp::{
    ErrorData,
    handler::server::wrapper::{Json, Parameters},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use world::{ObjectDetail, ObjectDetailResult, WorldRevision};

use super::{ShapeOutput, format_shape, lookup_object_entity};
use crate::{inspection::ViewerInspectionHandle, mcp::server::Handler};

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 100;

#[derive(Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorldQueryKind {
    #[default]
    Summary,
    Regions,
    Objects,
    Object,
}

#[derive(Deserialize, JsonSchema)]
pub struct WorldArgs {
    #[schemars(description = "Query mode. Defaults to 'summary'.")]
    pub query: Option<WorldQueryKind>,
    #[schemars(description = "Maximum number of items to return (1..=100).")]
    pub limit: Option<usize>,
    #[schemars(description = "Object UUID (required when query='object').")]
    pub object_id: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct WorldRegionItemOutput {
    pub region: u64,
    pub name: Option<String>,
    pub handle: String,
}

#[derive(Serialize, JsonSchema)]
pub struct WorldObjectItemOutput {
    pub entity: String,
    pub id: String,
    pub region: u64,
    pub local_id: u32,
    pub kind: String,
}

#[derive(Serialize, JsonSchema)]
pub struct WorldObjectDetailOutput {
    pub found: bool,
    pub revision: u64,
    pub entity: String,
    pub id: String,
    pub region: u64,
    pub local_id: u32,
    pub kind: String,
    pub shape: Option<ShapeOutput>,
    pub is_physical: bool,
    pub attachment_point: Option<String>,
}

#[derive(Serialize, JsonSchema)]
#[serde(untagged)]
pub enum WorldOutput {
    Inactive { active_session: bool },
    Summary { active_session: bool, revision: u64, region_count: usize, objects_returned: usize, has_more_objects: bool },
    Regions { revision: u64, regions: Vec<WorldRegionItemOutput>, has_more: bool },
    Objects { revision: u64, objects: Vec<WorldObjectItemOutput>, has_more: bool },
    ObjectNotFound { found: bool, object_id: String },
    ObjectDetail(WorldObjectDetailOutput),
}

#[tool_router(router = world_router, vis = "pub(super)")]
impl Handler {
    #[tool(
        name = "world",
        title = "World Query",
        description = "Query active regions, bounded object lists, or object properties by UUID.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn world(&self, Parameters(args): Parameters<WorldArgs>) -> Result<Json<WorldOutput>, ErrorData> {
        let query = args.query.unwrap_or_default();
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let output = match query {
            WorldQueryKind::Summary => world_summary_data(&self.handle).await.map_err(|msg| ErrorData::internal_error(msg, None))?,
            WorldQueryKind::Regions => {
                let Some(session) = self.handle.session() else {
                    return Ok(Json(WorldOutput::Inactive { active_session: false }));
                };
                let revision =
                    session.world.revision().map_err(|e| ErrorData::internal_error(format!("world revision failed: {e}"), None))?;
                let page = session
                    .world
                    .regions(None, limit)
                    .await
                    .map_err(|e| ErrorData::internal_error(format!("regions query failed: {e}"), None))?;
                let regions = page
                    .regions
                    .into_iter()
                    .map(|region| WorldRegionItemOutput {
                        region: region.region.get(),
                        name: region.info.name,
                        handle: format!("{:?}", region.info.handle),
                    })
                    .collect();
                WorldOutput::Regions { revision: revision.get(), regions, has_more: page.next.is_some() }
            }
            WorldQueryKind::Objects => {
                let Some(session) = self.handle.session() else {
                    return Ok(Json(WorldOutput::Inactive { active_session: false }));
                };
                let revision =
                    session.world.revision().map_err(|e| ErrorData::internal_error(format!("world revision failed: {e}"), None))?;
                let page = session
                    .world
                    .objects(None, limit)
                    .await
                    .map_err(|e| ErrorData::internal_error(format!("objects query failed: {e}"), None))?;
                let objects = page
                    .objects
                    .into_iter()
                    .map(|item| WorldObjectItemOutput {
                        entity: format!("{:?}", item.entity),
                        id: item.id.to_string(),
                        region: item.address.region().get(),
                        local_id: item.address.local_id().get(),
                        kind: format!("{:?}", item.kind),
                    })
                    .collect();
                WorldOutput::Objects { revision: revision.get(), objects, has_more: page.next.is_some() }
            }
            WorldQueryKind::Object => {
                let Some(session) = self.handle.session() else {
                    return Ok(Json(WorldOutput::Inactive { active_session: false }));
                };
                let Some(id_str) = args.object_id.as_deref() else {
                    return Err(ErrorData::invalid_params("object_id is required for query=object", None));
                };
                let revision =
                    session.world.revision().map_err(|e| ErrorData::internal_error(format!("world revision failed: {e}"), None))?;
                let Some(entity) =
                    lookup_object_entity(&session.world, id_str).await.map_err(|msg| ErrorData::invalid_params(msg, None))?
                else {
                    return Ok(Json(WorldOutput::ObjectNotFound { found: false, object_id: id_str.to_owned() }));
                };
                let batch = session
                    .world
                    .details(&[entity])
                    .await
                    .map_err(|e| ErrorData::internal_error(format!("details query failed: {e}"), None))?;
                let Some(detail) = batch.details.into_iter().find_map(|entry| match entry {
                    ObjectDetailResult::Found(detail) => Some(detail),
                    _ => None,
                }) else {
                    return Ok(Json(WorldOutput::ObjectNotFound { found: false, object_id: id_str.to_owned() }));
                };
                WorldOutput::ObjectDetail(format_object_detail(revision, &detail))
            }
        };
        Ok(Json(output))
    }
}

pub(in crate::mcp) async fn world_summary_data(handle: &ViewerInspectionHandle) -> Result<WorldOutput, String> {
    let Some(session) = handle.session() else {
        return Ok(WorldOutput::Inactive { active_session: false });
    };
    let revision = session.world.revision().map_err(|e| format!("world revision failed: {e}"))?;
    let regions = session.world.regions(None, 32).await.map_err(|e| format!("regions query failed: {e}"))?;
    let objects = session.world.objects(None, DEFAULT_LIMIT).await.map_err(|e| format!("objects query failed: {e}"))?;
    Ok(WorldOutput::Summary {
        active_session: true,
        revision: revision.get(),
        region_count: regions.regions.len(),
        objects_returned: objects.objects.len(),
        has_more_objects: objects.next.is_some(),
    })
}

fn format_object_detail(revision: WorldRevision, detail: &ObjectDetail) -> WorldObjectDetailOutput {
    WorldObjectDetailOutput {
        found: true,
        revision: revision.get(),
        entity: format!("{:?}", detail.summary.entity),
        id: detail.summary.id.to_string(),
        region: detail.summary.address.region().get(),
        local_id: detail.summary.address.local_id().get(),
        kind: format!("{:?}", detail.summary.kind),
        shape: detail.shape.as_ref().map(format_shape),
        is_physical: detail.is_physical,
        attachment_point: detail.attachment_point.map(|point| format!("{point:?}")),
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use utils::AssetId;
    use uuid::Uuid;
    use world::{
        FieldUpdate, Kind, ObjectObservation, OrderEvidence, ParentUpdate, SculptFlags, SculptType, Shape, UpdateSource, World, WorldLimits,
    };

    use super::format_object_detail;

    #[test]
    fn world_object_output_omits_shape_asset_uuids() {
        let mut world = World::new(WorldLimits {
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
        let region = world.register_region().expect("register region");
        let secret_mesh = Uuid::from_u128(0xdead_beef_cafe_babe_0123_4567_89ab_cdef);
        let secret_sculpt = Uuid::from_u128(0xfeed_face_dead_c0de_9876_5432_10fe_dcba);
        let local_id = NonZeroU32::new(1).expect("nonzero local id");
        let source = UpdateSource::new(region, 1, OrderEvidence::Ordered(1), 0);
        let mesh_id = Uuid::from_u128(1);
        let mut observation = ObjectObservation::new(source, mesh_id, local_id, Kind::Object, ParentUpdate::Root);
        observation.shape =
            FieldUpdate::Set(Shape::Mesh { asset_id: AssetId::new(secret_mesh), flags: SculptFlags { invert: true, mirror: false } });
        drop(world.observe_object(observation).expect("observe mesh"));
        let entity = world.entity(mesh_id).expect("mesh entity");
        let mesh_detail = world.object_detail(entity).expect("mesh detail");
        let mesh_output = serde_json::to_value(format_object_detail(world.revision(), &mesh_detail)).expect("serialize mesh");
        assert_eq!(mesh_output["shape"]["kind"], "mesh");
        assert_eq!(mesh_output["shape"]["flags"]["invert"], true);
        assert!(!mesh_output.to_string().contains(&secret_mesh.to_string()));

        let sculpt_local_id = NonZeroU32::new(2).expect("nonzero local id");
        let sculpt_source = UpdateSource::new(region, 2, OrderEvidence::Ordered(2), 0);
        let sculpt_id = Uuid::from_u128(2);
        let mut sculpt_obs = ObjectObservation::new(sculpt_source, sculpt_id, sculpt_local_id, Kind::Object, ParentUpdate::Root);
        sculpt_obs.shape = FieldUpdate::Set(Shape::Sculpt {
            asset_id: AssetId::new(secret_sculpt),
            sculpt_type: SculptType::Torus,
            flags: SculptFlags { invert: false, mirror: true },
        });
        drop(world.observe_object(sculpt_obs).expect("observe sculpt"));
        let sculpt_entity = world.entity(sculpt_id).expect("sculpt entity");
        let sculpt_detail = world.object_detail(sculpt_entity).expect("sculpt detail");
        let sculpt_output = serde_json::to_value(format_object_detail(world.revision(), &sculpt_detail)).expect("serialize sculpt");
        assert_eq!(sculpt_output["shape"]["kind"], "sculpt");
        assert_eq!(sculpt_output["shape"]["sculpt_type"], "torus");
        assert_eq!(sculpt_output["shape"]["flags"]["mirror"], true);
        assert!(!sculpt_output.to_string().contains(&secret_sculpt.to_string()));
    }
}
