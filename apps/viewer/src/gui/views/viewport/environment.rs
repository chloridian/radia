use std::{sync::Arc, time::Instant};

use prism::SharedString;
use prism_component::select::SelectItem;
use render::scene::{self, EnvironmentPreset, LocalChoice};

use super::FRAME_INTERVAL;
use crate::inventory::environments::{EnvironmentItems, EnvironmentMenu, EnvironmentSettingKind};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct EnvOption {
    pub(super) title: SharedString,
    pub(super) choice: Option<LocalChoice>,
}

impl SelectItem for EnvOption {
    type Value = Option<LocalChoice>;

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.choice
    }
}

pub(super) fn sky_options(items: Option<&EnvironmentMenu>) -> Vec<EnvOption> {
    let mut options = vec![EnvOption { title: "Shared".into(), choice: None }];
    for preset in EnvironmentPreset::ALL {
        options.push(EnvOption { title: preset.label().into(), choice: Some(LocalChoice::Preset(preset)) });
    }
    if let Some(menu) = items {
        for item in menu.items() {
            if item.kind() == EnvironmentSettingKind::Sky && !item.asset_id.uuid().is_nil() {
                options.push(EnvOption {
                    title: item.label().into(),
                    choice: Some(LocalChoice::Inventory {
                        owner_id: item.key.owner().id(),
                        item_id: item.key.id().uuid(),
                        asset_id: item.asset_id,
                    }),
                });
            }
        }
    }
    options
}

pub(super) fn water_options(items: Option<&EnvironmentMenu>) -> Vec<EnvOption> {
    let mut options = vec![EnvOption { title: "Shared".into(), choice: None }];
    if let Some(menu) = items {
        for item in menu.items() {
            if item.kind() == EnvironmentSettingKind::Water && !item.asset_id.uuid().is_nil() {
                options.push(EnvOption {
                    title: item.label().into(),
                    choice: Some(LocalChoice::Inventory {
                        owner_id: item.key.owner().id(),
                        item_id: item.key.id().uuid(),
                        asset_id: item.asset_id,
                    }),
                });
            }
        }
    }
    options
}

pub(super) fn menu_from_items(items: &EnvironmentItems) -> Option<&Arc<EnvironmentMenu>> {
    match items {
        EnvironmentItems::Ready(menu) => Some(menu),
        EnvironmentItems::Loading | EnvironmentItems::Failed => None,
    }
}

pub(super) fn step_index(current: usize, len: usize, delta: isize) -> Option<usize> {
    if len <= 1 {
        return None;
    }
    let current = current.min(len - 1);
    Some((current as isize + delta).rem_euclid(len as isize) as usize)
}

pub(super) fn sky_frame_deadline(camera: Option<scene::Camera>, animating: bool, now: Instant) -> Option<Instant> {
    let visible = camera.is_some_and(camera_sees_sky);
    (visible && animating).then(|| now + FRAME_INTERVAL)
}

pub(super) fn water_frame_deadline(camera: Option<scene::Camera>, height: Option<f64>, animating: bool, now: Instant) -> Option<Instant> {
    let visible = camera.zip(height).is_some_and(|(camera, height)| camera_sees_water(camera, height));
    (visible && animating).then(|| now + FRAME_INTERVAL)
}

fn camera_sees_sky(camera: scene::Camera) -> bool {
    let direction = camera.direction();
    let half_fov = camera.vertical_fov() as f32 * 0.5;
    let maximum_vertical = direction.z * half_fov.cos() + (1.0 - direction.z * direction.z).max(0.0).sqrt() * half_fov.sin();
    maximum_vertical > 0.0
}

fn camera_sees_water(camera: scene::Camera, height: f64) -> bool {
    if !height.is_finite() || camera.position().z <= height {
        return false;
    }
    let direction = camera.direction();
    let half_fov = camera.vertical_fov() as f32 * 0.5;
    let minimum_vertical = direction.z * half_fov.cos() - (1.0 - direction.z * direction.z).max(0.0).sqrt() * half_fov.sin();
    minimum_vertical < 0.0
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use math::{DVec3, Vec3};
    use render::scene::Camera;

    use super::{FRAME_INTERVAL, sky_frame_deadline, step_index, water_frame_deadline};

    #[test]
    fn step_wraps_through_options() {
        assert_eq!(step_index(0, 0, 1), None);
        assert_eq!(step_index(0, 1, 1), None);
        assert_eq!(step_index(0, 3, -1), Some(2));
        assert_eq!(step_index(2, 3, 1), Some(0));
        assert_eq!(step_index(1, 3, 1), Some(2));
    }

    #[test]
    fn sky_redraw_needs_visible_animation() {
        let now = Instant::now();
        let sky = Camera::new(DVec3::ZERO, Vec3::Z, 1.0, [1280, 720]).unwrap();
        let ground = Camera::new(DVec3::ZERO, Vec3::NEG_Z, 1.0, [1280, 720]).unwrap();
        assert_eq!(sky_frame_deadline(Some(sky), true, now), Some(now + FRAME_INTERVAL));
        assert_eq!(sky_frame_deadline(Some(sky), false, now), None);
        assert_eq!(sky_frame_deadline(Some(ground), true, now), None);
        assert_eq!(sky_frame_deadline(None, true, now), None);
    }

    #[test]
    fn water_redraw_needs_visible_waves() {
        let now = Instant::now();
        let water = Camera::new(DVec3::new(0.0, 0.0, 10.0), Vec3::NEG_Z, 1.0, [1280, 720]).unwrap();
        let sky = Camera::new(DVec3::new(0.0, 0.0, 10.0), Vec3::Z, 1.0, [1280, 720]).unwrap();
        let below = Camera::new(DVec3::new(0.0, 0.0, -1.0), Vec3::NEG_Z, 1.0, [1280, 720]).unwrap();
        assert_eq!(water_frame_deadline(Some(water), Some(0.0), true, now), Some(now + FRAME_INTERVAL));
        assert_eq!(water_frame_deadline(Some(water), Some(0.0), false, now), None);
        assert_eq!(water_frame_deadline(Some(sky), Some(0.0), true, now), None);
        assert_eq!(water_frame_deadline(Some(below), Some(0.0), true, now), None);
        assert_eq!(water_frame_deadline(Some(water), None, true, now), None);
    }
}
