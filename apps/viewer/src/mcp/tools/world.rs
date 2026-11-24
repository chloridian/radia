use serde_json::{Value, json};
use world::ObjectDetailResult;

use super::{ToolContent, ToolFuture, ToolSpec, lookup_object_entity};
use crate::{inspection::ViewerInspectionHandle, mcp::MAX_JSON_RESPONSE_BYTES};

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 100;

pub const SPEC: ToolSpec = ToolSpec {
    name: "world",
    description: "Query active regions, bounded object lists, or object properties by UUID.",
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
                Ok(ToolContent::Json(json!({
                    "found": true,
                    "revision": revision.get(),
                    "entity": format!("{:?}", detail.summary.entity),
                    "id": detail.summary.id.to_string(),
                    "region": detail.summary.address.region().get(),
                    "local_id": detail.summary.address.local_id(),
                    "kind": format!("{:?}", detail.summary.kind),
                    "shape": detail.shape.map(|shape| format!("{shape:?}")),
                    "is_physical": detail.is_physical,
                    "attachment_point": detail.attachment_point.map(|point| format!("{point:?}")),
                })))
            }
            other => Err(format!("unknown world query: {other}")),
        }
    })
}
