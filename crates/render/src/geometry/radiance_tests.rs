use math::{DVec3, Vec3};
use wgpu::{Device, Queue, TextureFormat};
use world::FaceAppearance;

use super::{GeometryRenderer, fixtures};
use crate::{
    Limits,
    environment::{Evaluated, Schedule, Sources, evaluate},
    scene::Camera,
};

const SIZE: [u32; 2] = [128, 128];

#[test]
#[ignore = "requires Vulkan; run with cargo test -p render --lib fullbright_over_sky_unscaled -- --ignored"]
fn fullbright_over_sky_unscaled() {
    let (device, queue) = fixtures::vulkan_device();
    for (case, objects) in [
        ("F1", vec![front(fullbright())]),
        ("F4", vec![front(fullbright()), back(fullbright())]),
        ("F5", vec![overlay(), front(fullbright())]),
    ] {
        assert_grey(center_rgb(&device, &queue, &objects), 146, case);
    }
}

#[test]
#[ignore = "requires Vulkan; run with cargo test -p render --lib fullbright_over_lit_scaled -- --ignored"]
fn fullbright_over_lit_scaled() {
    let (device, queue) = fixtures::vulkan_device();
    let lit = FaceAppearance::default();
    for (case, objects) in [
        ("F2", vec![front(fullbright()), back(lit)]),
        ("F3", vec![back(lit), front(fullbright())]),
        ("F5", vec![overlay(), front(fullbright()), back(lit)]),
    ] {
        assert_grey(center_rgb(&device, &queue, &objects), 154, case);
    }
}

fn fullbright() -> FaceAppearance {
    FaceAppearance { color: [124, 124, 124, 255], fullbright: true, ..FaceAppearance::default() }
}

fn overlay() -> (Vec3, Vec3, FaceAppearance) {
    (Vec3::new(5.0, 0.0, 0.0), Vec3::new(1.0, 30.0, 30.0), FaceAppearance { color: [124, 124, 124, 128], ..fullbright() })
}

fn front(face: FaceAppearance) -> (Vec3, Vec3, FaceAppearance) {
    (Vec3::new(10.0, 0.0, 0.0), Vec3::new(1.0, 60.0, 60.0), face)
}

fn back(face: FaceAppearance) -> (Vec3, Vec3, FaceAppearance) {
    (Vec3::new(20.0, 0.0, 0.0), Vec3::new(1.0, 120.0, 120.0), face)
}

fn environment() -> Evaluated {
    let sources = Sources {
        base: None,
        schedule: Schedule::default(),
        experience_day: None,
        experience_sky: None,
        experience_water: None,
        sky_override: None,
        water_override: None,
        local: None,
    };
    let mut environment = evaluate(&sources, 0.0, 0.0);
    environment.sky.reflection_probe_ambiance = Some(0.5);
    environment.sky.gamma = 1.0;
    environment.sky.haze.distance_multiplier = 0.0;
    environment
}

fn center_rgb(device: &Device, queue: &Queue, objects: &[(Vec3, Vec3, FaceAppearance)]) -> [u8; 3] {
    let (_scene, view) = fixtures::surface_scene(objects, None);
    let mut geometry = GeometryRenderer::new(device, queue, TextureFormat::Rgba16Float, Limits::default());
    let camera = Camera::new(DVec3::ZERO, Vec3::X, 1.0, SIZE).expect("camera is valid");
    let image = fixtures::render_frame(device, queue, &mut geometry, &view, camera, SIZE, &environment(), None);
    let center = ((SIZE[1] / 2 * SIZE[0] + SIZE[0] / 2) * 4) as usize;
    image.get(center..center + 3).expect("center pixel is in the image").try_into().expect("pixel has three color channels")
}

fn assert_grey(pixel: [u8; 3], expected: u8, case: &str) {
    assert!(pixel.iter().all(|channel| channel.abs_diff(expected) <= 1), "{case}: expected grey {expected}, got {pixel:?}");
}
