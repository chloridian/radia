use std::{
    collections::BTreeSet,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use assets::{EnvironmentDescription, EnvironmentSettings};
use utils::AssetId;
use world::Region;

use super::{
    Evaluated, Local, Schedule, SkyState, Sources, TextureBlend, Transition, WaterState, evaluate,
    property_transition::PropertyTransitions, star_brightness,
};
use crate::scene::{ExperienceSetting, LocalChoice, LocalEnvironment, SceneView, ViewerEnvironment};

const DEFAULT_TRANSITION: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositionIdentity {
    region: Region,
    environment: Option<u64>,
    experience_settings: [Option<u64>; 3],
}

#[derive(Clone, Debug, PartialEq)]
enum ActiveLocal {
    Sky { choice: LocalChoice, water: WaterState },
    Water { choice: LocalChoice, sky: SkyState },
    DayCycle { choice: LocalChoice },
    Split { sky: Option<LocalChoice>, water: Option<LocalChoice> },
}

impl ActiveLocal {
    fn matches(
        &self,
        local: Option<&LocalEnvironment>,
        local_sky: Option<&LocalEnvironment>,
        local_water: Option<&LocalEnvironment>,
    ) -> bool {
        match (self, local, local_sky, local_water) {
            (Self::Sky { choice, .. }, Some(local), None, None) => {
                choice == local.choice() && matches!(&local.environment().settings, EnvironmentSettings::Sky(_))
            }
            (Self::Water { choice, .. }, Some(local), None, None) => {
                choice == local.choice() && matches!(&local.environment().settings, EnvironmentSettings::Water(_))
            }
            (Self::DayCycle { choice }, Some(local), None, None) => {
                choice == local.choice() && matches!(&local.environment().settings, EnvironmentSettings::DayCycle(_))
            }
            (Self::Split { sky, water }, None, local_sky, local_water) => {
                let sky_matches = match (sky, local_sky) {
                    (Some(choice), Some(local)) => {
                        choice == local.choice() && matches!(&local.environment().settings, EnvironmentSettings::Sky(_))
                    }
                    (None, None) => true,
                    _ => false,
                };
                let water_matches = match (water, local_water) {
                    (Some(choice), Some(local)) => {
                        choice == local.choice() && matches!(&local.environment().settings, EnvironmentSettings::Water(_))
                    }
                    (None, None) => true,
                    _ => false,
                };
                sky_matches && water_matches
            }
            _ => false,
        }
    }

    fn transition_duration(&self, previous: Option<&Self>) -> Duration {
        match self {
            Self::Sky { choice, .. } | Self::Water { choice, .. } | Self::DayCycle { choice } => match choice {
                LocalChoice::Preset(_) => Duration::ZERO,
                LocalChoice::Inventory { .. } => DEFAULT_TRANSITION,
            },
            Self::Split { sky, water } => {
                let (prev_sky, prev_water) = match previous {
                    Some(Self::Split { sky, water }) => (sky.as_ref(), water.as_ref()),
                    _ => (None, None),
                };
                let sky_changed = sky.as_ref() != prev_sky;
                let water_changed = water.as_ref() != prev_water;
                if sky_changed && matches!(sky, Some(LocalChoice::Preset(_))) && !water_changed {
                    Duration::ZERO
                } else {
                    DEFAULT_TRANSITION
                }
            }
        }
    }
}

#[derive(Default)]
pub struct ActiveEnvironment {
    transition: Transition,
    property_transitions: PropertyTransitions,
    transition_identity: u64,
    composition: Option<CompositionIdentity>,
    viewer_version: u64,
    altitude: Option<f64>,
    evaluated: Option<Evaluated>,
    next_deadline: Option<Instant>,
    cloud_scroll: [f64; 2],
    cloud_rate: [f32; 2],
    cloud_time: Option<SystemTime>,
    animation_origin: Option<Instant>,
    animation_time: f32,
    water_scroll: [f32; 4],
    local: Option<ActiveLocal>,
    local_exit_transition: bool,
}

impl ActiveEnvironment {
    pub fn update(&mut self, view: &SceneView, agent: Option<(Region, f64)>, wall_time: SystemTime, now: Instant) -> bool {
        self.advance_cloud_scroll(wall_time);
        let animation_origin = *self.animation_origin.get_or_insert(now);
        self.animation_time = now.duration_since(animation_origin).as_secs_f32();
        let Some((region, altitude)) = agent.filter(|(_, altitude)| altitude.is_finite()) else {
            return self.clear_environment();
        };
        let viewer = view.viewer_environment();
        let viewer_version = viewer.map_or(0, |viewer| viewer.version());
        let viewer_version_changed = self.viewer_version != viewer_version;
        self.viewer_version = viewer_version;
        let local = viewer.and_then(|viewer| viewer.local());
        let local_sky = viewer.and_then(|viewer| viewer.local_sky());
        let local_water = viewer.and_then(|viewer| viewer.local_water());
        let has_local = local.is_some() || local_sky.is_some() || local_water.is_some();
        let local_changed = match (&self.local, has_local) {
            (None, false) => false,
            (Some(active), true) => !active.matches(local, local_sky, local_water),
            _ => true,
        };
        let leaving_local = local_changed && !has_local;
        let identity = composition_identity(view, region);
        let viewer_composition_changed =
            self.composition.is_some_and(|previous| previous.experience_settings != identity.experience_settings);
        let changed = self.composition != Some(identity) || self.altitude != Some(altitude);
        let deadline = self.next_deadline.is_some_and(|deadline| now >= deadline);
        if !changed
            && !local_changed
            && !viewer_version_changed
            && !deadline
            && self.evaluated.as_ref().is_none_or(|value| value.cycle_length.is_none())
        {
            self.water_scroll = [self.animation_time * 0.5, 0.0, 0.0, 0.0];
            return false;
        }
        if self.composition != Some(identity) || local_changed {
            self.transition_identity = self.transition_identity.checked_add(1).expect("environment transition identities exhausted");
            self.composition = Some(identity);
        }
        if local_changed {
            self.local_exit_transition = leaving_local;
        }
        self.altitude = Some(altitude);
        let utc = wall_time.duration_since(UNIX_EPOCH).map_or(0.0, |duration| duration.as_secs_f64());
        let previous_local = self.local.clone();
        if local_changed {
            self.local = if let Some(local) = local {
                let choice = local.choice().clone();
                let shared = self.evaluated.as_ref().cloned().unwrap_or_else(|| {
                    let sources = compose_sources(view, region, viewer, None, true);
                    evaluate(&sources, utc, altitude)
                });
                Some(match &local.environment().settings {
                    EnvironmentSettings::Sky(_) => ActiveLocal::Sky { choice, water: shared.water },
                    EnvironmentSettings::Water(_) => ActiveLocal::Water { choice, sky: shared.sky },
                    EnvironmentSettings::DayCycle(_) => ActiveLocal::DayCycle { choice },
                })
            } else if local_sky.is_some() || local_water.is_some() {
                Some(ActiveLocal::Split {
                    sky: local_sky.map(|local| local.choice().clone()),
                    water: local_water.map(|local| local.choice().clone()),
                })
            } else {
                None
            };
        }
        let include_properties = self.local.is_some();
        let sources = compose_sources(view, region, viewer, self.local.as_ref(), include_properties);
        let has_properties = viewer.is_some_and(|viewer| {
            viewer.sky_properties().values().any(|property| property.region() == region)
                || viewer.water_properties().values().any(|property| property.region() == region)
        });
        let target = evaluate(&sources, utc, altitude);
        let has_environment = sources.base.is_some()
            || sources.experience_day.is_some()
            || sources.experience_sky.is_some()
            || sources.experience_water.is_some()
            || sources.sky_override.is_some()
            || sources.water_override.is_some()
            || has_properties
            || viewer.is_some() && self.property_transitions.active_for_region(region, now)
            || sources.local.is_some();
        if !has_environment {
            let changed = self.clear_environment();
            self.composition = Some(identity);
            self.altitude = Some(altitude);
            return changed;
        }
        let duration = match &self.local {
            Some(active) => active.transition_duration(previous_local.as_ref()),
            None if leaving_local => DEFAULT_TRANSITION,
            None => viewer.and_then(|viewer| viewer.transition_for_region(region)).unwrap_or_else(|| {
                viewer
                    .filter(|_| viewer_composition_changed)
                    .and_then(|viewer| viewer.clear_transition_for_region(region))
                    .unwrap_or(DEFAULT_TRANSITION)
            }),
        };
        let (target, property_next) = if self.local.is_none() && self.local_exit_transition {
            self.property_transitions.apply(viewer, region, &target, now)
        } else {
            (target, None)
        };
        let (baseline, mut next) = self.transition.update(self.transition_identity, target, duration, now);
        let (evaluated, property_next) = if self.local.is_none() && !self.local_exit_transition {
            self.property_transitions.apply(viewer, region, &baseline, now)
        } else {
            (baseline, property_next)
        };
        self.local_exit_transition &= self.transition.is_blending();
        next = match (next, property_next) {
            (Some(next), Some(property_next)) => Some(next.min(property_next)),
            (Some(next), None) => Some(next),
            (None, Some(property_next)) => Some(property_next),
            (None, None) => None,
        };
        let output_changed = self.evaluated.as_ref() != Some(&evaluated);
        self.cloud_rate = evaluated.sky.clouds.scroll_rate;
        self.water_scroll = [self.animation_time * 0.5, 0.0, 0.0, 0.0];
        self.evaluated = Some(evaluated);
        self.next_deadline = next;
        output_changed
    }

    pub fn evaluated(&self) -> Option<&Evaluated> {
        self.evaluated.as_ref()
    }

    pub fn texture_assets(&self) -> BTreeSet<AssetId> {
        let Some(environment) = &self.evaluated else { return BTreeSet::new() };
        let mut assets = BTreeSet::new();
        for texture in [
            environment.sky.sun.texture,
            environment.sky.moon.texture,
            environment.sky.clouds.texture,
            environment.sky.bloom,
            environment.sky.rainbow,
            environment.sky.halo,
            environment.water.normal_map,
        ] {
            assets.extend([texture.current, texture.next].into_iter().flatten());
        }
        assets
    }

    pub fn cloud_scroll(&self) -> [f32; 2] {
        [self.cloud_scroll[0] as f32, self.cloud_scroll[1] as f32]
    }

    pub fn animation_time(&self) -> f32 {
        self.animation_time
    }

    pub fn water_scroll(&self) -> [f32; 4] {
        self.water_scroll
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.next_deadline
    }

    pub fn clouds_animate(&self, view: &SceneView) -> bool {
        self.evaluated.as_ref().is_some_and(|evaluated| {
            evaluated.sky.clouds.scale >= 0.001
                && texture_available(view, evaluated.sky.clouds.texture)
                && self.cloud_rate.iter().any(|rate| *rate != 0.0)
        })
    }

    pub fn stars_animate(&self, view: &SceneView) -> bool {
        self.evaluated.as_ref().is_some_and(|evaluated| {
            star_brightness(evaluated.sky.star_brightness) >= 0.001 && texture_available(view, evaluated.sky.bloom)
        })
    }

    pub fn water_animate(&self, view: &SceneView) -> bool {
        self.evaluated.as_ref().is_some_and(|evaluated| {
            texture_available(view, evaluated.water.normal_map)
                && evaluated.water.normal_scale.iter().any(|scale| *scale != 0.0)
                && evaluated.water.waves.iter().flatten().any(|component| *component != 0.0)
        })
    }

    fn advance_cloud_scroll(&mut self, now: SystemTime) {
        if let Some(previous) = self.cloud_time
            && let Ok(delta) = now.duration_since(previous)
        {
            let seconds = delta.as_secs_f64();
            for (index, (offset, rate)) in self.cloud_scroll.iter_mut().zip(self.cloud_rate).enumerate() {
                let direction = if index == 0 { -1.0 } else { 1.0 };
                *offset = if rate == 0.0 { 0.0 } else { (*offset + direction * f64::from(rate) * seconds / 100.0).rem_euclid(1.0) };
            }
        }
        self.cloud_time = Some(now);
    }

    fn clear_environment(&mut self) -> bool {
        let changed = self.evaluated.take().is_some();
        self.transition = Transition::default();
        self.property_transitions = PropertyTransitions::default();
        self.composition = None;
        self.viewer_version = 0;
        self.altitude = None;
        self.next_deadline = None;
        self.cloud_scroll = [0.0; 2];
        self.cloud_rate = [0.0; 2];
        self.water_scroll = [0.0; 4];
        self.local = None;
        self.local_exit_transition = false;
        changed
    }
}

fn texture_available(view: &SceneView, texture: TextureBlend) -> bool {
    [texture.current, texture.next].into_iter().flatten().any(|id| view.environment_texture(id).is_some())
}

struct ExperienceSources<'a> {
    day: Option<&'a EnvironmentDescription>,
    sky: Option<&'a EnvironmentDescription>,
    water: Option<&'a EnvironmentDescription>,
}

fn experience_sources(viewer: Option<&ViewerEnvironment>, region: Region) -> ExperienceSources<'_> {
    let mut sources = ExperienceSources { day: None, sky: None, water: None };
    let Some(viewer) = viewer else { return sources };
    for setting in [viewer.day(), viewer.sky(), viewer.water()].into_iter().flatten() {
        if setting.region() != region {
            continue;
        }
        let target = match &setting.environment().settings {
            EnvironmentSettings::DayCycle(_) => &mut sources.day,
            EnvironmentSettings::Sky(_) => &mut sources.sky,
            EnvironmentSettings::Water(_) => &mut sources.water,
        };
        *target = Some(setting.environment());
    }
    sources
}

fn compose_sources<'a>(
    view: &'a SceneView,
    region: Region,
    viewer: Option<&'a ViewerEnvironment>,
    active_local: Option<&'a ActiveLocal>,
    include_properties: bool,
) -> Sources<'a> {
    let base = view.environment(region);
    let experience = experience_sources(viewer, region);
    let mut schedule = base.map_or_else(Schedule::default, |environment| environment.schedule());
    let local = viewer.and_then(|viewer| viewer.local());
    if matches!(active_local, Some(ActiveLocal::DayCycle { .. })) {
        let default = Schedule::default();
        schedule = Schedule { altitudes: schedule.altitudes, ..default };
    }
    let local = match (local, active_local) {
        (Some(local), Some(ActiveLocal::Sky { water, .. })) => match &local.environment().settings {
            EnvironmentSettings::Sky(sky) => Some(Local::Sky { sky, water }),
            _ => None,
        },
        (Some(local), Some(ActiveLocal::Water { sky, .. })) => match &local.environment().settings {
            EnvironmentSettings::Water(water) => Some(Local::Water { water, sky }),
            _ => None,
        },
        (Some(local), Some(ActiveLocal::DayCycle { .. })) => match &local.environment().settings {
            EnvironmentSettings::DayCycle(day) => Some(Local::DayCycle(day)),
            _ => None,
        },
        (None, Some(ActiveLocal::Split { .. })) => {
            let sky = viewer.and_then(|viewer| viewer.local_sky()).and_then(|local| match &local.environment().settings {
                EnvironmentSettings::Sky(sky) => Some(sky.as_ref()),
                _ => None,
            });
            let water = viewer.and_then(|viewer| viewer.local_water()).and_then(|local| match &local.environment().settings {
                EnvironmentSettings::Water(water) => Some(water.as_ref()),
                _ => None,
            });
            (sky.is_some() || water.is_some()).then_some(Local::Split { sky, water })
        }
        _ => None,
    };
    Sources {
        base: base.map(|environment| environment.description()),
        schedule,
        experience_day: experience.day,
        experience_sky: experience.sky,
        experience_water: experience.water,
        sky_override: viewer.filter(|_| include_properties).and_then(|viewer| viewer.sky_overrides_for_region(region)),
        water_override: viewer.filter(|_| include_properties).and_then(|viewer| viewer.water_overrides_for_region(region)),
        local,
    }
}

fn composition_identity(view: &SceneView, region: Region) -> CompositionIdentity {
    let viewer = view.viewer_environment();
    let setting_sequence =
        |setting: Option<&ExperienceSetting>| setting.filter(|setting| setting.region() == region).map(|setting| setting.sequence());
    CompositionIdentity {
        region,
        environment: view.environment(region).map(|environment| environment.identity()),
        experience_settings: viewer
            .map_or([None; 3], |viewer| [setting_sequence(viewer.day()), setting_sequence(viewer.sky()), setting_sequence(viewer.water())]),
    }
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
