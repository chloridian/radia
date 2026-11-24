use assets::{DayCycle, EnvironmentDescription, EnvironmentSettings, Sky, Water};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use world::{EnvironmentAssignment, EnvironmentRead, EnvironmentSource};

use super::{ToolContent, ToolFuture, ToolSpec};
use crate::{
    inspection::{EnvironmentOverrideSummary, ViewerInspectionHandle, ViewportQuery},
    mcp::MAX_JSON_RESPONSE_BYTES,
};

pub const SPEC: ToolSpec = ToolSpec {
    name: "environment",
    description: "Return active sky, water, and day-cycle values and source (region, parcel, local override) for the agent's region without asset UUIDs.",
    input_schema,
    handler,
    max_response_bytes: MAX_JSON_RESPONSE_BYTES,
};

fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {}
    })
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

fn format_assignment(assignment: &EnvironmentAssignment) -> Value {
    let (source_kind, settings) = match &assignment.source {
        EnvironmentSource::Inherit => ("inherit", Value::Null),
        EnvironmentSource::Asset(_) => ("asset", Value::Null),
        EnvironmentSource::Inline(bytes) => {
            let decoded = EnvironmentDescription::decode(bytes).ok().map(|description| match &description.settings {
                EnvironmentSettings::Sky(sky) => json!({ "kind": "sky", "sky": format_sky(sky) }),
                EnvironmentSettings::Water(water) => json!({ "kind": "water", "water": format_water(water) }),
                EnvironmentSettings::DayCycle(day) => json!({ "kind": "day_cycle", "day_cycle": format_day_cycle(day) }),
            });
            ("inline", decoded.unwrap_or(Value::Null))
        }
    };
    json!({
        "source_kind": source_kind,
        "version": assignment.version,
        "day_length_seconds": assignment.day_length_seconds,
        "day_offset_seconds": assignment.day_offset_seconds,
        "altitudes": assignment.altitudes,
        "settings": settings,
    })
}

fn format_environment(read: &EnvironmentRead, overrides: EnvironmentOverrideSummary) -> Value {
    let parcel_active =
        read.parcel.as_ref().and_then(|parcel| parcel.assignment.as_ref()).is_some_and(|a| !matches!(a.source, EnvironmentSource::Inherit));
    let effective_source = if overrides.local_override {
        "local_override"
    } else if parcel_active {
        "parcel"
    } else if read.region_assignment.is_some() {
        "region"
    } else {
        "default"
    };
    json!({
        "active_session": true,
        "region": read.region.get(),
        "hydration_revision": read.hydration_revision.get(),
        "source": effective_source,
        "local_override": {
            "active": overrides.local_override,
            "sky": overrides.local_sky,
            "water": overrides.local_water,
        },
        "region_assignment": read.region_assignment.as_ref().map(format_assignment),
        "parcel": read.parcel.as_ref().map(|parcel| {
            json!({
                "parcel_local_id": parcel.parcel.get(),
                "assignment": parcel.assignment.as_ref().map(format_assignment),
            })
        }),
    })
}

fn handler(handle: ViewerInspectionHandle, _args: Value) -> ToolFuture {
    Box::pin(async move {
        let Some(session) = handle.session() else {
            return Ok(ToolContent::Json(json!({ "active_session": false })));
        };
        let primary_region = session.diagnostics.network.borrow().iter().find(|entry| entry.is_primary).map(|entry| entry.region);
        let region = match primary_region {
            Some(region) => region,
            None => {
                let page = session.world.regions(None, 1).await.map_err(|e| format!("regions query failed: {e}"))?;
                let Some(first) = page.regions.first() else {
                    return Ok(ToolContent::Json(json!({ "active_session": true, "region": null })));
                };
                first.region
            }
        };
        let Some(read) = session.world.environment(region).await.map_err(|e| format!("environment query failed: {e}"))? else {
            return Ok(ToolContent::Json(json!({ "active_session": true, "region": region.get(), "environment": null })));
        };
        let overrides = {
            let (reply_tx, reply_rx) = oneshot::channel();
            if session.try_send_viewport_query(ViewportQuery::EnvironmentOverride { reply: reply_tx }).is_ok() {
                reply_rx.await.unwrap_or_default()
            } else {
                EnvironmentOverrideSummary::default()
            }
        };
        Ok(ToolContent::Json(format_environment(&read, overrides)))
    })
}

#[cfg(test)]
mod tests {
    use utils::AssetId;
    use uuid::Uuid;
    use world::{EnvironmentAssignment, EnvironmentRead, EnvironmentSource, World, WorldLimits};

    use super::format_environment;
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
        assert_eq!(output["source"], "region");
        assert_eq!(output["region_assignment"]["source_kind"], "asset");
        assert_eq!(output["region_assignment"]["day_length_seconds"], 14400);
        assert!(!output.to_string().contains(&secret_asset.to_string()));
    }
}
