use assets::{DayCycle, EnvironmentDescription, EnvironmentSettings, Sky, Water};
use rmcp::{ErrorData, handler::server::wrapper::Json, tool, tool_router};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use world::{EnvironmentAssignment, EnvironmentRead, EnvironmentSource};

use crate::{
    inspection::{EnvironmentOverrideSummary, ViewportQuery},
    mcp::server::Handler,
};

#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentSourceKindOutput {
    Inherit,
    Asset,
    Inline,
}

#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentEffectiveSourceOutput {
    LocalOverride,
    Parcel,
    Region,
    Default,
}

#[derive(Serialize, JsonSchema)]
pub struct LocalOverrideOutput {
    pub active: bool,
    pub sky: bool,
    pub water: bool,
}

#[derive(Serialize, JsonSchema)]
pub struct EnvironmentAssignmentOutput {
    pub source_kind: EnvironmentSourceKindOutput,
    pub version: Option<u32>,
    pub day_length_seconds: Option<u32>,
    pub day_offset_seconds: Option<i32>,
    pub altitudes: Option<[f32; 3]>,
    pub settings: Value,
}

#[derive(Serialize, JsonSchema)]
pub struct ParcelEnvironmentOutput {
    pub parcel_local_id: u32,
    pub assignment: Option<EnvironmentAssignmentOutput>,
}

#[derive(Serialize, JsonSchema)]
pub struct EnvironmentOutput {
    pub active_session: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hydration_revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<EnvironmentEffectiveSourceOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_override: Option<LocalOverrideOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region_assignment: Option<EnvironmentAssignmentOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parcel: Option<ParcelEnvironmentOutput>,
}

#[tool_router(router = environment_router, vis = "pub(super)")]
impl Handler {
    #[tool(
        name = "environment",
        title = "Region Environment",
        description = "Return active sky, water, and day-cycle values and source (region, parcel, local override) for the agent's region without asset UUIDs.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn environment(&self) -> Result<Json<EnvironmentOutput>, ErrorData> {
        let Some(session) = self.handle.session() else {
            return Ok(Json(EnvironmentOutput {
                active_session: false,
                region: None,
                hydration_revision: None,
                source: None,
                local_override: None,
                region_assignment: None,
                parcel: None,
            }));
        };
        let primary_region = session.diagnostics.network.borrow().iter().find(|entry| entry.is_primary).map(|entry| entry.region);
        let region = match primary_region {
            Some(region) => region,
            None => {
                let page = session
                    .world
                    .regions(None, 1)
                    .await
                    .map_err(|e| ErrorData::internal_error(format!("regions query failed: {e}"), None))?;
                let Some(first) = page.regions.first() else {
                    return Ok(Json(EnvironmentOutput {
                        active_session: true,
                        region: None,
                        hydration_revision: None,
                        source: None,
                        local_override: None,
                        region_assignment: None,
                        parcel: None,
                    }));
                };
                first.region
            }
        };
        let Some(read) = session
            .world
            .environment(region)
            .await
            .map_err(|e| ErrorData::internal_error(format!("environment query failed: {e}"), None))?
        else {
            return Ok(Json(EnvironmentOutput {
                active_session: true,
                region: Some(region.get()),
                hydration_revision: None,
                source: None,
                local_override: None,
                region_assignment: None,
                parcel: None,
            }));
        };
        let overrides = {
            let (reply_tx, reply_rx) = oneshot::channel();
            if session.try_send_viewport_query(ViewportQuery::EnvironmentOverride { reply: reply_tx }).is_ok() {
                reply_rx.await.unwrap_or_default()
            } else {
                EnvironmentOverrideSummary::default()
            }
        };
        Ok(Json(format_environment(&read, overrides)))
    }
}

fn format_sky(sky: &Sky) -> Value {
    json!({
        "sun_rotation": sky.sun.rotation,
        "sun_scale": sky.sun.scale,
        "moon_rotation": sky.moon.rotation,
        "moon_scale": sky.moon.scale,
        "sunlight": sky.sunlight,
        "moon_brightness": sky.moon_brightness,
        "star_brightness": sky.star_brightness,
        "gamma": sky.gamma,
        "glow": sky.glow,
        "max_y": sky.max_y,
        "dome_offset": sky.dome_offset,
        "dome_radius": sky.dome_radius,
        "reflection_probe_ambiance": sky.reflection_probe_ambiance,
        "clouds": {
            "color": sky.clouds.color,
            "density": sky.clouds.density,
            "scale": sky.clouds.scale,
            "scroll_rate": sky.clouds.scroll_rate,
            "shadow": sky.clouds.shadow,
            "variance": sky.clouds.variance,
        },
        "haze": {
            "ambient": sky.haze.ambient,
            "blue_density": sky.haze.blue_density,
            "blue_horizon": sky.haze.blue_horizon,
            "density_multiplier": sky.haze.density_multiplier,
            "distance_multiplier": sky.haze.distance_multiplier,
            "density": sky.haze.density,
            "horizon": sky.haze.horizon,
        },
        "atmosphere": {
            "planet_radius": sky.atmosphere.planet_radius,
            "bottom_radius": sky.atmosphere.bottom_radius,
            "top_radius": sky.atmosphere.top_radius,
            "sun_arc": sky.atmosphere.sun_arc,
            "moisture": sky.atmosphere.moisture,
            "droplet_radius": sky.atmosphere.droplet_radius,
            "ice": sky.atmosphere.ice,
        },
    })
}

fn format_water(water: &Water) -> Value {
    json!({
        "fog_color": water.fog_color,
        "fog_density": water.fog_density,
        "underwater_fog": water.underwater_fog,
        "blur": water.blur,
        "fresnel_offset": water.fresnel_offset,
        "fresnel_scale": water.fresnel_scale,
        "normal_scale": water.normal_scale,
        "scale_above": water.scale_above,
        "scale_below": water.scale_below,
        "waves": water.waves,
    })
}

fn format_day_cycle(day: &DayCycle) -> Value {
    json!({
        "water_keyframe_count": day.water().len(),
        "sky_track_keyframe_counts": day.skies().iter().map(Vec::len).collect::<Vec<_>>(),
        "water_positions": day.water().iter().take(16).map(|frame| frame.position()).collect::<Vec<_>>(),
    })
}

fn format_assignment(assignment: &EnvironmentAssignment) -> EnvironmentAssignmentOutput {
    let (source_kind, settings) = match &assignment.source {
        EnvironmentSource::Inherit => (EnvironmentSourceKindOutput::Inherit, Value::Null),
        EnvironmentSource::Asset(_) => (EnvironmentSourceKindOutput::Asset, Value::Null),
        EnvironmentSource::Inline(bytes) => {
            let decoded = EnvironmentDescription::decode(bytes).ok().map(|description| match &description.settings {
                EnvironmentSettings::Sky(sky) => json!({ "kind": "sky", "sky": format_sky(sky) }),
                EnvironmentSettings::Water(water) => json!({ "kind": "water", "water": format_water(water) }),
                EnvironmentSettings::DayCycle(day) => json!({ "kind": "day_cycle", "day_cycle": format_day_cycle(day) }),
            });
            (EnvironmentSourceKindOutput::Inline, decoded.unwrap_or(Value::Null))
        }
    };
    EnvironmentAssignmentOutput {
        source_kind,
        version: assignment.version,
        day_length_seconds: assignment.day_length_seconds,
        day_offset_seconds: assignment.day_offset_seconds,
        altitudes: assignment.altitudes,
        settings,
    }
}

fn format_environment(read: &EnvironmentRead, overrides: EnvironmentOverrideSummary) -> EnvironmentOutput {
    let parcel_active =
        read.parcel.as_ref().and_then(|parcel| parcel.assignment.as_ref()).is_some_and(|a| !matches!(a.source, EnvironmentSource::Inherit));
    let effective_source = if overrides.local_override {
        EnvironmentEffectiveSourceOutput::LocalOverride
    } else if parcel_active {
        EnvironmentEffectiveSourceOutput::Parcel
    } else if read.region_assignment.is_some() {
        EnvironmentEffectiveSourceOutput::Region
    } else {
        EnvironmentEffectiveSourceOutput::Default
    };
    EnvironmentOutput {
        active_session: true,
        region: Some(read.region.get()),
        hydration_revision: Some(read.hydration_revision.get()),
        source: Some(effective_source),
        local_override: Some(LocalOverrideOutput {
            active: overrides.local_override,
            sky: overrides.local_sky,
            water: overrides.local_water,
        }),
        region_assignment: read.region_assignment.as_ref().map(format_assignment),
        parcel: read.parcel.as_ref().map(|parcel| ParcelEnvironmentOutput {
            parcel_local_id: parcel.parcel.get(),
            assignment: parcel.assignment.as_ref().map(format_assignment),
        }),
    }
}

#[cfg(test)]
mod tests {
    use utils::AssetId;
    use uuid::Uuid;
    use world::{EnvironmentAssignment, EnvironmentRead, EnvironmentSource, World, WorldLimits};

    use super::{EnvironmentEffectiveSourceOutput, EnvironmentSourceKindOutput, format_environment};
    use crate::inspection::EnvironmentOverrideSummary;

    #[test]
    fn environment_output_omits_asset_uuids() {
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
        let secret_asset = Uuid::from_u128(0xdead_beef_cafe_babe_0123_4567_89ab_cdef);
        let read = EnvironmentRead {
            region,
            hydration_revision: world.revision(),
            region_assignment: Some(EnvironmentAssignment {
                source: EnvironmentSource::Asset(AssetId::new(secret_asset)),
                version: Some(2),
                day_length_seconds: Some(14400),
                day_offset_seconds: Some(0),
                altitudes: Some([1000.0, 2000.0, 3000.0]),
            }),
            parcel: None,
        };
        let output = format_environment(&read, EnvironmentOverrideSummary::default());
        assert_eq!(output.source, Some(EnvironmentEffectiveSourceOutput::Region));
        let assignment = output.region_assignment.as_ref().expect("region assignment");
        assert_eq!(assignment.source_kind, EnvironmentSourceKindOutput::Asset);
        assert_eq!(assignment.day_length_seconds, Some(14400));
        let serialized = serde_json::to_string(&output).expect("serialize environment");
        assert!(!serialized.contains(&secret_asset.to_string()));
    }
}
