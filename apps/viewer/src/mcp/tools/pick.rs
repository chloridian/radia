use render::PickDetail;
use rmcp::{
    ErrorData,
    handler::server::wrapper::{Json, Parameters},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::{inspection::ViewportQuery, mcp::server::Handler};

#[derive(Deserialize, JsonSchema)]
pub struct PickArgs {
    #[schemars(description = "Viewport pixel X coordinate.")]
    pub x: f64,
    #[schemars(description = "Viewport pixel Y coordinate.")]
    pub y: f64,
}

#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PickHitKind {
    Terrain,
    Object,
}

#[derive(Serialize, JsonSchema)]
pub struct PickOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_session: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<PickHitKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub point: Option<[f64; 3]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub face_range: Option<[u32; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub material_slot: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_base_texture: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_emissive_texture: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub double_sided: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blended: Option<bool>,
}

#[tool_router(router = pick_router, vis = "pub(super)")]
impl Handler {
    #[tool(
        name = "pick",
        title = "Viewport Pick",
        description = "Pick the rendered geometry at viewport pixel coordinates (x, y) and return hit point, object, face, and texture presence.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn pick(&self, Parameters(args): Parameters<PickArgs>) -> Result<Json<PickOutput>, ErrorData> {
        let Some(session) = self.handle.session() else {
            return Ok(Json(PickOutput {
                active_session: Some(false),
                hit: None,
                kind: None,
                point: None,
                object_id: None,
                region: None,
                face_range: None,
                material_slot: None,
                has_base_texture: None,
                has_emissive_texture: None,
                double_sided: None,
                blended: None,
            }));
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        session
            .try_send_viewport_query(ViewportQuery::PixelPick { x: args.x as f32, y: args.y as f32, reply: reply_tx })
            .map_err(|msg| ErrorData::internal_error(msg, None))?;
        let pick_result = reply_rx
            .await
            .map_err(|_| ErrorData::internal_error("viewport dropped pick request", None))?
            .map_err(|e| ErrorData::internal_error(format!("pick failed: {e}"), None))?;

        match pick_result {
            Some(detail) => Ok(Json(format_pick_detail(detail))),
            None => Ok(Json(PickOutput {
                active_session: None,
                hit: Some(false),
                kind: None,
                point: None,
                object_id: None,
                region: None,
                face_range: None,
                material_slot: None,
                has_base_texture: None,
                has_emissive_texture: None,
                double_sided: None,
                blended: None,
            })),
        }
    }
}

fn format_pick_detail(detail: PickDetail) -> PickOutput {
    let kind = if detail.object_id.is_some() || detail.entity.is_some() { PickHitKind::Object } else { PickHitKind::Terrain };
    PickOutput {
        active_session: None,
        hit: Some(true),
        kind: Some(kind),
        point: Some([detail.point.x, detail.point.y, detail.point.z]),
        object_id: detail.object_id.map(|id| id.to_string()),
        region: Some(detail.region.get()),
        face_range: Some([detail.face_range.start, detail.face_range.end]),
        material_slot: Some(detail.material_slot),
        has_base_texture: Some(detail.has_base_texture),
        has_emissive_texture: Some(detail.has_emissive_texture),
        double_sided: Some(detail.double_sided),
        blended: Some(detail.blended),
    }
}

#[cfg(test)]
mod tests {
    use math::DVec3;
    use render::PickDetail;
    use uuid::Uuid;
    use world::{Region, World, WorldLimits};

    use super::format_pick_detail;

    fn sample_detail(region: Region, object_id: Option<Uuid>) -> PickDetail {
        PickDetail {
            point: DVec3::new(1.0, 2.0, 3.0),
            entity: None,
            object_id,
            region,
            face_range: 0..6,
            material_slot: 0,
            has_base_texture: false,
            has_emissive_texture: false,
            double_sided: false,
            blended: false,
        }
    }

    #[test]
    fn pick_reports_terrain_and_object_hit_kinds() {
        let mut world = World::new(WorldLimits {
            max_regions: 1,
            max_entities: 1,
            max_entities_per_region: 1,
            max_tombstones_per_region: 1,
            max_tombstones: 1,
            max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(1),
            max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(1),
            max_journal_entries: 1,
            max_journal_changes: 1,
            max_region_name_bytes: 64,
            max_recovery_targets: 1,
        });
        let region = world.register_region().expect("register region");

        let terrain = serde_json::to_value(format_pick_detail(sample_detail(region, None))).expect("serialize terrain pick");
        assert_eq!(terrain["hit"], true);
        assert_eq!(terrain["kind"], "terrain");

        let object =
            serde_json::to_value(format_pick_detail(sample_detail(region, Some(Uuid::from_u128(1))))).expect("serialize object pick");
        assert_eq!(object["hit"], true);
        assert_eq!(object["kind"], "object");
    }
}
