use math::{Bounds, DVec3, Frustum, Quat, Vec3};

use super::{RANGE_MARGIN, WorldSphere};
use crate::{geometry::ShadingMode, pipeline::projection, scene::Camera};

const SIZE: [u32; 2] = [1000, 500];

#[test]
fn range_extends_draw_distance_for_surfaces_only() {
    let camera =
        Camera::new(DVec3::ZERO, Vec3::X, 1.0, SIZE).expect("fixture camera").with_quality(32.0, 1.0).expect("fixture draw distance");
    let point = Bounds { min: Vec3::ZERO, max: Vec3::ZERO };
    let behind = |distance: f64| sphere(DVec3::new(-distance, 0.0, 0.0), point, camera);

    assert!(behind(32.0 + RANGE_MARGIN).in_range(ShadingMode::Surface, camera.draw_distance()));
    assert!(!behind(32.0 + RANGE_MARGIN + 0.01).in_range(ShadingMode::Surface, camera.draw_distance()));
    assert!(behind(10_000.0).in_range(ShadingMode::Terrain, camera.draw_distance()));
}

#[test]
fn negative_scale_stays_visible() {
    let camera = Camera::new(DVec3::ZERO, Vec3::X, 1.0, SIZE).expect("fixture camera");
    let bounds = Bounds { min: Vec3::new(1.8, -0.2, -0.2), max: Vec3::new(2.2, 0.2, 0.2) };
    let frustum = Frustum::from_clip_matrix(projection(camera, SIZE));

    assert!(
        WorldSphere::new(
            DVec3::new(10.0, -7.5, 0.0),
            Quat::from_rotation_z(-std::f32::consts::FRAC_PI_2),
            Vec3::new(-1.0, 1.0, 1.0),
            bounds,
            camera,
        )
        .expect("finite transformed bounds")
        .visible(ShadingMode::Surface, camera, &frustum)
    );
}

fn sphere(position: DVec3, bounds: Bounds, camera: Camera) -> WorldSphere {
    WorldSphere::new(position, Quat::IDENTITY, Vec3::ONE, bounds, camera).expect("finite bounds")
}
