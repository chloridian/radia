use std::sync::mpsc;

use math::{DVec3, Vec3};
use wgpu::{
    BufferDescriptor, BufferUsages, Extent3d, LoadOp, MapMode, Operations, PollType, RenderPassColorAttachment,
    RenderPassDepthStencilAttachment, RenderPassDescriptor, StoreOp, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo,
    TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
};
use world::FaceAppearance;

use super::{GeometryRenderer, fixtures};
use crate::{
    Limits,
    pipeline::SCENE_CLEAR,
    scene::{Camera, SceneView},
};

const SIZE: [u32; 2] = [128, 128];
const RGB_TOLERANCE: u8 = 2;
const FRONT: (Vec3, Vec3) = (Vec3::new(10.0, 0.0, 0.0), Vec3::new(1.0, 30.0, 30.0));
const BACK: (Vec3, Vec3) = (Vec3::new(20.0, 0.0, 0.0), Vec3::new(1.0, 60.0, 60.0));

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo test -p render --lib blend_preserves_scene_alpha -- --ignored"]
fn blend_preserves_scene_alpha() {
    let (device, queue) = fixtures::vulkan_device();
    let water_camera = Camera::new(DVec3::new(10.0, -1.35, -5.0), Vec3::Z, 1.0, SIZE).expect("water camera is valid");
    let water = [0, 125, 130, 255].map(|alpha| {
        let (_scene, view, _) = fixtures::water_scene(alpha);
        render_center(&device, &queue, &view, water_camera, Some(20.0))
    });
    let water_background = water[0];
    let water_opaque = water[3];

    for (pixel, alpha) in [(water[1], 125), (water[2], 130)] {
        assert_eq!(pixel[3], 0, "opacity {alpha}/255 preserves water classification");
        assert_blended_rgb(pixel, water_background, water_opaque, alpha);
    }

    let blended = FaceAppearance { color: [255, 255, 255, 125], ..FaceAppearance::default() };
    let opaque_background =
        center_alpha(&device, &queue, &[(FRONT.0, FRONT.1, blended), (BACK.0, BACK.1, FaceAppearance::default())], forward(), None);
    assert_eq!(opaque_background, 255, "transparent surface preserves opaque background classification");
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo test -p render --lib scene_alpha_tracks_fullbright -- --ignored"]
fn scene_alpha_tracks_fullbright() {
    let (device, queue) = fixtures::vulkan_device();
    let lit = FaceAppearance::default();
    let fullbright = FaceAppearance { fullbright: true, ..FaceAppearance::default() };
    let upward = Camera::new(DVec3::new(10.0, 10.0, -5.0), Vec3::Z, 1.0, SIZE).expect("upward camera is valid");
    let overhead = (Vec3::new(10.0, 10.0, 5.0), Vec3::new(30.0, 30.0, 1.0), fullbright);
    for (case, objects, camera, water, expected) in [
        ("lit opaque", vec![(FRONT.0, FRONT.1, lit)], forward(), None, 255),
        ("fullbright over clear", vec![(FRONT.0, FRONT.1, fullbright)], forward(), None, 128),
        ("fullbright over lit", vec![(FRONT.0, FRONT.1, fullbright), (BACK.0, BACK.1, lit)], forward(), None, 255),
        ("fullbright over water", vec![overhead], upward, Some(20.0), 128),
        ("region water", Vec::new(), upward, Some(20.0), 0),
    ] {
        let alpha = center_alpha(&device, &queue, &objects, camera, water);
        assert!(alpha.abs_diff(expected) <= 1, "{case}: expected alpha {expected}, got {alpha}");
    }
}

fn forward() -> Camera {
    Camera::new(DVec3::ZERO, Vec3::X, 1.0, SIZE).expect("forward camera is valid")
}

fn center_alpha(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    objects: &[(Vec3, Vec3, FaceAppearance)],
    camera: Camera,
    water_height: Option<f32>,
) -> u8 {
    let (_scene, view) = fixtures::surface_scene(objects, water_height);
    render_center(device, queue, &view, camera, water_height.map(f64::from))[3]
}

fn render_center(device: &wgpu::Device, queue: &wgpu::Queue, view: &SceneView, camera: Camera, water_height: Option<f64>) -> [u8; 4] {
    let mut geometry = GeometryRenderer::new(device, queue, TextureFormat::Rgba8Unorm, Limits::default());
    let depth = geometry.depth(device, SIZE).expect("depth target is admitted");
    let frame = geometry.admit().expect("frame is admitted");
    let mut encoder = device.create_command_encoder(&Default::default());
    geometry.prepare(device, &mut encoder, view, None, camera, SIZE, water_height);
    let color = device.create_texture(&TextureDescriptor {
        label: Some("Surface alpha regression"),
        size: Extent3d { width: SIZE[0], height: SIZE[1], depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format: TextureFormat::Rgba8Unorm,
        usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color.create_view(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
            label: Some("Surface alpha regression"),
            color_attachments: &[Some(RenderPassColorAttachment {
                view: &color_view,
                depth_slice: None,
                resolve_target: None,
                ops: Operations { load: LoadOp::Clear(SCENE_CLEAR), store: StoreOp::Store },
            })],
            depth_stencil_attachment: Some(RenderPassDepthStencilAttachment {
                view: &depth,
                depth_ops: Some(Operations { load: LoadOp::Clear(0.0), store: StoreOp::Store }),
                stencil_ops: None,
            }),
            ..Default::default()
        });
        geometry.draw(&mut pass);
    }
    let bytes_per_row = SIZE[0] * 4;
    let readback = device.create_buffer(&BufferDescriptor {
        label: Some("Surface alpha regression readback"),
        size: u64::from(bytes_per_row) * u64::from(SIZE[1]),
        usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        TexelCopyTextureInfo { texture: &color, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        TexelCopyBufferInfo {
            buffer: &readback,
            layout: TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bytes_per_row), rows_per_image: Some(SIZE[1]) },
        },
        Extent3d { width: SIZE[0], height: SIZE[1], depth_or_array_layers: 1 },
    );
    queue.submit([encoder.finish()]);
    geometry.submitted(frame);
    let (send, receive) = mpsc::channel();
    readback.slice(..).map_async(MapMode::Read, move |result| {
        send.send(result).expect("readback receiver remains alive until completion");
    });
    device.poll(PollType::Wait { submission_index: None, timeout: None }).expect("render completes");
    receive.recv().expect("readback result arrives").expect("readback maps");
    let mapped = readback.slice(..).get_mapped_range().expect("readback range is mapped");
    let center = ((SIZE[1] / 2 * SIZE[0] + SIZE[0] / 2) * 4) as usize;
    let pixel: [u8; 4] =
        mapped.get(center..center + 4).expect("center pixel is in the readback").try_into().expect("pixel has four channels");
    drop(mapped);
    readback.unmap();
    pixel
}

fn assert_blended_rgb(pixel: [u8; 4], background: [u8; 4], surface: [u8; 4], alpha: u8) {
    let opacity = f32::from(alpha) / f32::from(u8::MAX);
    for channel in 0..3 {
        let expected = (f32::from(background[channel]) * (1.0 - opacity) + f32::from(surface[channel]) * opacity).round() as u8;
        assert!(
            pixel[channel].abs_diff(expected) <= RGB_TOLERANCE,
            "opacity {alpha}/255 channel {channel}: expected {expected}, got {}",
            pixel[channel]
        );
    }
}
