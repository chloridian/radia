use rmcp::{ErrorData, handler::server::wrapper::Json, tool, tool_router};
use schemars::JsonSchema;
use serde::Serialize;
use world::{ParcelFlags, ParcelRead};

use crate::{
    inspection::{RegionSimStats, ViewerInspectionHandle},
    mcp::server::Handler,
};

#[derive(Serialize, JsonSchema)]
pub struct RegionIdentityOutput {
    pub id: u64,
    pub region_id: Option<String>,
    pub name: Option<String>,
    pub handle: Option<u64>,
    pub origin: Option<[f64; 3]>,
}

#[derive(Serialize, JsonSchema)]
pub struct ParcelFlagsOutput {
    pub bits: u32,
    pub allow_fly: bool,
    pub allow_build: bool,
    pub allow_other_scripts: bool,
    pub allow_group_scripts: bool,
    pub allow_damage: bool,
    pub restrict_push: bool,
    pub sound_local: bool,
    pub group_access: bool,
    pub use_access_list: bool,
    pub use_ban_list: bool,
    pub use_pass_list: bool,
    pub deny_anonymous: bool,
    pub deny_age_unverified: bool,
}

#[derive(Serialize, JsonSchema)]
pub struct CurrentParcelOutput {
    pub local_id: u32,
    pub name: String,
    pub area: u32,
    pub group_owned: bool,
    pub flags: ParcelFlagsOutput,
}

#[derive(Serialize, JsonSchema)]
pub struct RegionFlagsOutput {
    pub low: Option<u32>,
    pub extended: Option<u64>,
}

#[derive(Serialize, JsonSchema)]
pub struct EffectiveRestrictionsOutput {
    pub allow_fly: Option<bool>,
    pub allow_build: Option<bool>,
    pub allow_scripts: Option<bool>,
    pub allow_group_scripts: Option<bool>,
    pub restrict_push: Option<bool>,
    pub allow_damage: Option<bool>,
    pub sound_local: Option<bool>,
    pub avatars_hidden_outside: Option<bool>,
    pub allow_direct_teleport: Option<bool>,
    pub deny_anonymous: Option<bool>,
    pub deny_identified: Option<bool>,
    pub deny_transacted: Option<bool>,
    pub deny_age_unverified: Option<bool>,
}

#[derive(Serialize, JsonSchema)]
pub struct ParcelOutput {
    pub current: Option<CurrentParcelOutput>,
    pub region_flags: RegionFlagsOutput,
    pub effective_restrictions: EffectiveRestrictionsOutput,
}

#[derive(Serialize, JsonSchema)]
pub struct OtherSimStatOutput {
    pub id: u32,
    pub value: f32,
}

#[derive(Serialize, JsonSchema)]
pub struct SimStatsOutput {
    pub region_x: u32,
    pub region_y: u32,
    pub region_flags: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region_flags_extended: Option<u64>,
    pub object_capacity: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_dilation: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sim_fps: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub physics_fps: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_updates_per_sec: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_ms: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net_ms: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub other_ms: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub physics_ms: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_ms: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_ms: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_ms: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_objects: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_objects: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main_agents: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_agents: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_scripts: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_lsl_ips: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_packets_per_sec: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub out_packets_per_sec: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_downloads: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_uploads: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub virtual_size_kb: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resident_size_kb: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_local_uploads: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unacked_bytes: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub physics_pinned_tasks: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub physics_lod_tasks: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub physics_step_ms: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub physics_shape_ms: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub physics_other_ms: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub physics_memory_mb: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_eps: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spare_ms: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sleep_ms: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io_pump_ms: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scripts_run_pct: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ai_step_ms: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_silhouette_steps: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub characters_updated_pct: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub other: Vec<OtherSimStatOutput>,
}

#[derive(Serialize, JsonSchema)]
pub struct RegionOutput {
    pub active_session: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<RegionIdentityOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parcel: Option<ParcelOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sim_stats: Option<SimStatsOutput>,
}

#[tool_router(router = region_router, vis = "pub(super)")]
impl Handler {
    #[tool(
        name = "region",
        title = "Region & Parcel Stats",
        description = "Return the agent's current region name, handle, latest simulator stats (SimStats), and current parcel name, area, and flags.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn region(&self) -> Result<Json<RegionOutput>, ErrorData> {
        let output = region_data(&self.handle).await.map_err(|msg| ErrorData::internal_error(msg, None))?;
        Ok(Json(output))
    }
}

pub(in crate::mcp) async fn region_data(handle: &ViewerInspectionHandle) -> Result<RegionOutput, String> {
    let Some(session) = handle.session() else {
        return Ok(RegionOutput { active_session: false, region: None, parcel: None, sim_stats: None });
    };

    let agent_region = (*session.agent_motion.borrow())
        .map(|sample| sample.region)
        .or_else(|| session.diagnostics.network.borrow().iter().find(|entry| entry.is_primary).map(|entry| entry.region));
    let Some(region) = agent_region else {
        return Ok(RegionOutput { active_session: true, region: None, parcel: None, sim_stats: None });
    };

    let regions = session.world.regions(None, 64).await.map_err(|e| format!("regions query failed: {e}"))?;
    let region_entry = regions.regions.into_iter().find(|entry| entry.region == region);
    let parcel = session.world.parcel_state(region).await.map_err(|e| format!("parcel query failed: {e}"))?;
    let sim_stats = session.diagnostics.sim_stats.borrow().get(&region).cloned();

    Ok(RegionOutput {
        active_session: true,
        region: Some(RegionIdentityOutput {
            id: region.get(),
            region_id: region_entry.as_ref().and_then(|entry| entry.info.region_id).map(|id| id.to_string()),
            name: region_entry.as_ref().and_then(|entry| entry.info.name.clone()),
            handle: region_entry.as_ref().and_then(|entry| entry.info.handle).map(|handle| handle.get()),
            origin: region_entry.as_ref().and_then(|entry| entry.info.resolved_origin()).map(|origin| [origin.x, origin.y, origin.z]),
        }),
        parcel: parcel.as_ref().map(ParcelOutput::from),
        sim_stats: sim_stats.as_ref().map(SimStatsOutput::from),
    })
}

impl From<ParcelFlags> for ParcelFlagsOutput {
    fn from(flags: ParcelFlags) -> Self {
        Self {
            bits: flags.bits(),
            allow_fly: flags.allow_fly(),
            allow_build: flags.allow_build(),
            allow_other_scripts: flags.allow_other_scripts(),
            allow_group_scripts: flags.allow_group_scripts(),
            allow_damage: flags.allow_damage(),
            restrict_push: flags.restrict_push(),
            sound_local: flags.sound_local(),
            group_access: flags.group_access(),
            use_access_list: flags.use_access_list(),
            use_ban_list: flags.use_ban_list(),
            use_pass_list: flags.use_pass_list(),
            deny_anonymous: flags.deny_anonymous(),
            deny_age_unverified: flags.deny_age_unverified(),
        }
    }
}

impl From<&ParcelRead> for ParcelOutput {
    fn from(parcel: &ParcelRead) -> Self {
        let current = parcel.current.as_ref().map(|current| {
            let props = &current.properties;
            CurrentParcelOutput {
                local_id: props.local_id.get(),
                name: props.name.to_string(),
                area: props.area,
                group_owned: props.group_owned,
                flags: ParcelFlagsOutput::from(props.flags),
            }
        });
        let restrictions = parcel.effective_restrictions;
        Self {
            current,
            region_flags: RegionFlagsOutput { low: parcel.region_flags.low, extended: parcel.region_flags.extended },
            effective_restrictions: EffectiveRestrictionsOutput {
                allow_fly: restrictions.allow_fly,
                allow_build: restrictions.allow_build,
                allow_scripts: restrictions.allow_scripts,
                allow_group_scripts: restrictions.allow_group_scripts,
                restrict_push: restrictions.restrict_push,
                allow_damage: restrictions.allow_damage,
                sound_local: restrictions.sound_local,
                avatars_hidden_outside: restrictions.avatars_hidden_outside,
                allow_direct_teleport: restrictions.allow_direct_teleport,
                deny_anonymous: restrictions.deny_anonymous,
                deny_identified: restrictions.deny_identified,
                deny_transacted: restrictions.deny_transacted,
                deny_age_unverified: restrictions.deny_age_unverified,
            },
        }
    }
}

impl From<&RegionSimStats> for SimStatsOutput {
    fn from(stats: &RegionSimStats) -> Self {
        Self {
            region_x: stats.region_x,
            region_y: stats.region_y,
            region_flags: stats.region_flags,
            region_flags_extended: stats.region_flags_extended,
            object_capacity: stats.object_capacity,
            pid: stats.pid,
            time_dilation: stats.time_dilation,
            sim_fps: stats.sim_fps,
            physics_fps: stats.physics_fps,
            agent_updates_per_sec: stats.agent_updates_per_sec,
            frame_ms: stats.frame_ms,
            net_ms: stats.net_ms,
            other_ms: stats.other_ms,
            physics_ms: stats.physics_ms,
            agent_ms: stats.agent_ms,
            image_ms: stats.image_ms,
            script_ms: stats.script_ms,
            total_objects: stats.total_objects,
            active_objects: stats.active_objects,
            main_agents: stats.main_agents,
            child_agents: stats.child_agents,
            active_scripts: stats.active_scripts,
            script_lsl_ips: stats.script_lsl_ips,
            in_packets_per_sec: stats.in_packets_per_sec,
            out_packets_per_sec: stats.out_packets_per_sec,
            pending_downloads: stats.pending_downloads,
            pending_uploads: stats.pending_uploads,
            virtual_size_kb: stats.virtual_size_kb,
            resident_size_kb: stats.resident_size_kb,
            pending_local_uploads: stats.pending_local_uploads,
            unacked_bytes: stats.unacked_bytes,
            physics_pinned_tasks: stats.physics_pinned_tasks,
            physics_lod_tasks: stats.physics_lod_tasks,
            physics_step_ms: stats.physics_step_ms,
            physics_shape_ms: stats.physics_shape_ms,
            physics_other_ms: stats.physics_other_ms,
            physics_memory_mb: stats.physics_memory_mb,
            script_eps: stats.script_eps,
            spare_ms: stats.spare_ms,
            sleep_ms: stats.sleep_ms,
            io_pump_ms: stats.io_pump_ms,
            scripts_run_pct: stats.scripts_run_pct,
            ai_step_ms: stats.ai_step_ms,
            skipped_silhouette_steps: stats.skipped_silhouette_steps,
            characters_updated_pct: stats.characters_updated_pct,
            other: stats.other_stats.iter().map(|&(id, value)| OtherSimStatOutput { id, value }).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use protocol::messages::region::sim_stats::{SimStatEntry, SimStats, SimStatsRegion, SimStatsRegionInfo};

    use super::*;

    #[test]
    fn decodes_and_formats_sim_stats() {
        let message = SimStats {
            region: SimStatsRegion { region_x: 1000, region_y: 2000, region_flags: 0x01, object_capacity: 20_000 },
            stats: vec![
                SimStatEntry { stat_id: 0, stat_value: 0.99 },
                SimStatEntry { stat_id: 1, stat_value: 45.0 },
                SimStatEntry { stat_id: 2, stat_value: 44.8 },
                SimStatEntry { stat_id: 10, stat_value: 3.25 },
                SimStatEntry { stat_id: 11, stat_value: 5120.0 },
                SimStatEntry { stat_id: 13, stat_value: 12.0 },
                SimStatEntry { stat_id: 14, stat_value: 3.0 },
                SimStatEntry { stat_id: 4, stat_value: f32::NAN },
                SimStatEntry { stat_id: 999, stat_value: 7.5 },
            ],
            pid: Some(1234),
            region_info: vec![SimStatsRegionInfo { region_flags_extended: Some(0x100) }],
        };

        let decoded = RegionSimStats::from_message(&message);
        assert_eq!(decoded.sim_fps, Some(45.0));
        assert_eq!(decoded.time_dilation, Some(0.99));
        assert_eq!(decoded.physics_fps, Some(44.8));
        assert_eq!(decoded.script_ms, Some(3.25));
        assert_eq!(decoded.total_objects, Some(5120.0));
        assert_eq!(decoded.main_agents, Some(12.0));
        assert_eq!(decoded.child_agents, Some(3.0));
        assert_eq!(decoded.frame_ms, None);
        assert_eq!(decoded.other_stats, vec![(999, 7.5)]);

        let json = serde_json::to_value(SimStatsOutput::from(&decoded)).expect("serialize sim stats");
        assert_eq!(json["object_capacity"], 20_000);
        assert_eq!(json["sim_fps"], 45.0);
        assert!(json.get("frame_ms").is_none());
        assert_eq!(json["other"][0]["id"], 999);
    }
}
