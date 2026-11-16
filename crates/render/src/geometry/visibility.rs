use math::{Bounds, DVec3, Frustum, Mat4, Quat, Sphere, Vec3};

use super::ShadingMode;
use crate::scene::Camera;

const RETENTION_MARGIN_PIXELS: f32 = 16.0;
const PREPARATION_MARGIN_PIXELS: f32 = 64.0;

pub(super) struct VisibilityBands {
    pub(super) strict: Frustum,
    pub(super) retained: Frustum,
    pub(super) preparation: Frustum,
}

#[derive(Clone, Copy)]
pub(super) struct WorldSphere {
    center: DVec3,
    radius: f64,
    distance: f64,
}

impl VisibilityBands {
    pub(super) fn new(projection: Mat4, size: [u32; 2]) -> Self {
        Self {
            strict: Frustum::from_clip_matrix(projection),
            retained: side_margin(projection, size, RETENTION_MARGIN_PIXELS),
            preparation: side_margin(projection, size, PREPARATION_MARGIN_PIXELS),
        }
    }
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

fn side_margin(projection: Mat4, size: [u32; 2], margin: f32) -> Frustum {
    let width = size[0].max(1) as f32;
    let height = size[1].max(1) as f32;
    let scale = Vec3::new(1.0 / (1.0 + 2.0 * margin / width), 1.0 / (1.0 + 2.0 * margin / height), 1.0);
    Frustum::from_clip_matrix(Mat4::from_scale(scale) * projection)
}

#[cfg(test)]
#[path = "visibility_tests.rs"]
mod tests;
