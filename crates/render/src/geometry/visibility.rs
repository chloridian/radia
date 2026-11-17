use math::{Bounds, DVec3, Frustum, Quat, Sphere, Vec3};

use super::ShadingMode;
use crate::scene::Camera;

#[derive(Clone, Copy)]
pub(super) struct WorldSphere {
    center: DVec3,
    radius: f64,
    distance: f64,
}

impl WorldSphere {
    pub(super) fn new(position: DVec3, rotation: Quat, scale: Vec3, bounds: Bounds, camera: Camera) -> Option<Self> {
        let sphere = bounds_sphere(position, rotation, scale, bounds)?;
        let center = sphere.center - camera.position();
        let relative = position - camera.position();
        if !center.is_finite()
            || center.abs().max_element() + sphere.radius > f64::from(f32::MAX) / 16.0
            || !relative.is_finite()
            || relative.abs().max_element() > f64::from(f32::MAX)
        {
            return None;
        }
        let distance = center.length() - sphere.radius;
        distance.is_finite().then_some(Self { center, radius: sphere.radius, distance })
    }

    pub(super) fn in_range(self, shading: ShadingMode, range: f64) -> bool {
        shading != ShadingMode::Surface || self.distance <= range
    }

    pub(super) fn visible(self, shading: ShadingMode, camera: Camera, frustum: &Frustum) -> bool {
        (shading != ShadingMode::Surface || self.distance <= camera.draw_distance()) && frustum.intersects_sphere(self.center, self.radius)
    }
}

pub(super) fn bounds_sphere(position: DVec3, rotation: Quat, scale: Vec3, bounds: Bounds) -> Option<Sphere> {
    let local_center = (bounds.min.as_dvec3() + bounds.max.as_dvec3()) * 0.5 * scale.as_dvec3();
    let center = position + (rotation * local_center.as_vec3()).as_dvec3();
    let radius = ((bounds.max.as_dvec3() - bounds.min.as_dvec3()) * 0.5 * scale.as_dvec3()).length();
    (center.is_finite() && radius.is_finite() && rotation.is_finite() && scale.recip().is_finite()).then_some(Sphere { center, radius })
}

#[cfg(test)]
#[path = "visibility_tests.rs"]
mod tests;
