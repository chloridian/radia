use serde_json::{Value, json};
use world::{ObjectDetail, ObjectDetailResult, WorldRevision};

use super::{ToolAnnotationsSpec, ToolContent, ToolFuture, ToolSpec, format_shape, lookup_object_entity};
use crate::{inspection::ViewerInspectionHandle, mcp::MAX_JSON_RESPONSE_BYTES};

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 100;

pub const SPEC: ToolSpec = ToolSpec {
    name: "world",
    title: "World Query",
    description: "Query active regions, bounded object lists, or object properties by UUID.",
    annotations: ToolAnnotationsSpec::read_only(),
    input_schema,
    handler,
    max_response_bytes: MAX_JSON_RESPONSE_BYTES,
};

fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "enum": ["summary", "regions", "objects", "object"] },
            "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT },
            "object_id": { "type": "string" }
        }
    })
}

fn handler(handle: ViewerInspectionHandle, args: Value) -> ToolFuture {
    Box::pin(async move {
        let Some(session) = handle.session() else {
            return Ok(ToolContent::Json(json!({ "active_session": false })));
        };
        let query = args.get("query").and_then(Value::as_str).unwrap_or("summary");
        let limit =
            args.get("limit").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let revision = session.world.revision().map_err(|e| format!("world revision failed: {e}"))?;

        match query {
            "summary" => {
                let regions = session.world.regions(None, 32).await.map_err(|e| format!("regions query failed: {e}"))?;
                let objects = session.world.objects(None, limit).await.map_err(|e| format!("objects query failed: {e}"))?;
                Ok(ToolContent::Json(json!({
                    "active_session": true,
                    "revision": revision.get(),
                    "region_count": regions.regions.len(),
                    "objects_returned": objects.objects.len(),
                    "has_more_objects": objects.next.is_some(),
                })))
            }
            "regions" => {
                let page = session.world.regions(None, limit).await.map_err(|e| format!("regions query failed: {e}"))?;
                let regions: Vec<Value> = page
                    .regions
                    .into_iter()
                    .map(|region| {
                        json!({
                            "region": region.region.get(),
                            "name": region.info.name,
                            "handle": format!("{:?}", region.info.handle),
                        })
                    })
                    .collect();
                Ok(ToolContent::Json(json!({
                    "revision": revision.get(),
                    "regions": regions,
                    "has_more": page.next.is_some(),
                })))
            }
            "objects" => {
                let page = session.world.objects(None, limit).await.map_err(|e| format!("objects query failed: {e}"))?;
                let objects: Vec<Value> = page
                    .objects
                    .into_iter()
                    .map(|item| {
                        json!({
                            "entity": format!("{:?}", item.entity),
                            "id": item.id.to_string(),
                            "region": item.address.region().get(),
                            "local_id": item.address.local_id(),
                            "kind": format!("{:?}", item.kind),
                        })
                    })
                    .collect();
                Ok(ToolContent::Json(json!({
                    "revision": revision.get(),
                    "objects": objects,
                    "has_more": page.next.is_some(),
                })))
            }
            "object" => {
                let Some(id_str) = args.get("object_id").and_then(Value::as_str) else {
                    return Err("object_id is required for query=object".to_owned());
                };
                let Some(entity) = lookup_object_entity(&session.world, id_str).await? else {
                    return Ok(ToolContent::Json(json!({ "found": false, "object_id": id_str })));
                };
                let batch = session.world.details(&[entity]).await.map_err(|e| format!("details query failed: {e}"))?;
                let Some(detail) = batch.details.into_iter().find_map(|entry| match entry {
                    ObjectDetailResult::Found(detail) => Some(detail),
                    _ => None,
                }) else {
                    return Ok(ToolContent::Json(json!({ "found": false, "object_id": id_str })));
                };
                Ok(ToolContent::Json(format_object_detail(revision, &detail)))
            }
            other => Err(format!("unknown world query: {other}")),
        }
    })
}

fn format_object_detail(revision: WorldRevision, detail: &ObjectDetail) -> Value {
    json!({
        "found": true,
        "revision": revision.get(),
        "entity": format!("{:?}", detail.summary.entity),
        "id": detail.summary.id.to_string(),
        "region": detail.summary.address.region().get(),
        "local_id": detail.summary.address.local_id(),
        "kind": format!("{:?}", detail.summary.kind),
        "shape": detail.shape.as_ref().map(format_shape),
        "is_physical": detail.is_physical,
        "attachment_point": detail.attachment_point.map(|point| format!("{point:?}")),
    })
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
        let mesh_output = format_object_detail(world.revision(), &mesh_detail);
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
        let sculpt_output = format_object_detail(world.revision(), &sculpt_detail);
        assert_eq!(sculpt_output["shape"]["kind"], "sculpt");
        assert_eq!(sculpt_output["shape"]["sculpt_type"], "torus");
        assert_eq!(sculpt_output["shape"]["flags"]["mirror"], true);
        assert!(!sculpt_output.to_string().contains(&secret_sculpt.to_string()));
    }
}
