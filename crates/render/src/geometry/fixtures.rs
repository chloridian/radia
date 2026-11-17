use std::assert_matches;
use std::{num::NonZeroU32, sync::mpsc, time::Instant};

use futures::executor::block_on;
use math::{DVec3, Quat, Vec3};
use uuid::Uuid;
use wgpu::{
    Backends, BufferDescriptor, BufferUsages, Device, DeviceDescriptor, Extent3d, InstanceDescriptor, LoadOp, MapMode, Operations,
    PollType, Queue, RenderPassColorAttachment, RenderPassDepthStencilAttachment, RenderPassDescriptor, StoreOp, TexelCopyBufferInfo,
    TexelCopyBufferLayout, TexelCopyTextureInfo, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages, TextureViewDescriptor,
};
use world::{
    Appearance, FaceAppearance, FieldUpdate, Kind, ObjectObservation, OrderEvidence, ParentUpdate, Pose, PrimShape, Shape,
    SurfaceAppearance, UpdateSource, World, WorldLimits,
};

use super::GeometryRenderer;
use crate::{
    FrameInput,
    environment::Evaluated,
    pipeline::{PostProcess, SCENE_CLEAR, camera_view, projection},
    scene::{Budget, Camera, ReadQuery, ReadReply, ReconcileStep, Reconciler, SceneView},
    texture::CompressionQuality,
};

pub(super) fn vulkan_device() -> (Device, Queue) {
    block_on(async {
        let instance =
            wgpu::Instance::new(InstanceDescriptor { backends: Backends::VULKAN, ..InstanceDescriptor::new_without_display_handle() });
        let adapter = instance.request_adapter(&Default::default()).await.expect("Vulkan adapter is available");
        adapter.request_device(&DeviceDescriptor::default()).await.expect("Vulkan device is available")
    })
}

pub(super) fn scene(alpha: u8) -> (Reconciler, SceneView, Camera) {
    scene_with_scales(alpha, [Vec3::splat(2.0); 2])
}

pub(super) fn scene_with_scales(alpha: u8, scales: [Vec3; 2]) -> (Reconciler, SceneView, Camera) {
    scene_with_mapping(alpha, scales, world::TextureMapping::Default)
}

pub(super) fn scene_with_mapping(alpha: u8, scales: [Vec3; 2], mapping: world::TextureMapping) -> (Reconciler, SceneView, Camera) {
    let (_, scene, view, camera) = scene_with_world(alpha, scales, mapping);
    (scene, view, camera)
}

pub(super) fn scene_with_agent() -> (Reconciler, SceneView, Camera) {
    let (_, scene, view, camera) =
        scene_with_world_with_agent(255, [Vec3::splat(2.0); 2], world::TextureMapping::Default, Some(Uuid::from_u128(1)), false);
    (scene, view, camera)
}

pub(super) fn water_scene(alpha: u8) -> (Reconciler, SceneView, Camera) {
    let (_, scene, view, camera) = scene_with_world_with_agent(alpha, [Vec3::splat(2.0); 2], world::TextureMapping::Default, None, true);
    (scene, view, camera)
}

pub(super) fn scene_with_world(alpha: u8, scales: [Vec3; 2], mapping: world::TextureMapping) -> (World, Reconciler, SceneView, Camera) {
    scene_with_world_with_agent(alpha, scales, mapping, None, false)
}

pub(super) fn object_scene(count: u32) -> (World, Reconciler, SceneView) {
    object_scene_with(count, Camera::new(DVec3::ZERO, Vec3::X, 1.0, [128, 128]).unwrap(), |_| {
        (Vec3::new(10.0, 0.0, 0.0), PrimShape::default())
    })
}

pub(super) fn object_scene_with(count: u32, camera: Camera, object: impl Fn(u32) -> (Vec3, PrimShape)) -> (World, Reconciler, SceneView) {
    let capacity = usize::try_from(count).unwrap().next_power_of_two();
    let mut world = World::new(WorldLimits {
        max_regions: 1,
        max_entities: capacity,
        max_entities_per_region: capacity,
        max_tombstones_per_region: 1,
        max_tombstones: 1,
        max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(1),
        max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(1),
        max_journal_entries: 16,
        max_journal_changes: 64,
        max_region_name_bytes: 64,
        max_recovery_targets: capacity,
    });
    let region = world.register_region().unwrap();
    let _ = world.patch_region(region, world::RegionInfo { origin: Some(DVec3::ZERO), ..Default::default() }).unwrap();
    for id in 1..=count {
        let mut observation = ObjectObservation::new(
            UpdateSource::new(region, id, OrderEvidence::Ordered(u64::from(id)), 0),
            Uuid::from_u128(u128::from(id)),
            NonZeroU32::new(id).unwrap(),
            Kind::Object,
            ParentUpdate::Root,
        );
        let (position, shape) = object(id);
        observation.pose = Some(Pose { position: Some(position), rotation: Some(Quat::IDENTITY) });
        observation.scale = Some(Vec3::ONE);
        observation.shape = FieldUpdate::Set(Shape::Prim(shape));
        observation.appearance = FieldUpdate::Set(Appearance::Surface);
        observation.surface_appearance = Some(SurfaceAppearance::new(FaceAppearance::default(), Vec::new()).unwrap());
        assert_matches!(world.observe_object(observation).unwrap().outcome, world::ApplyOutcome::Introduced(_));
    }
    let (mut scene, mut view) = Reconciler::new(Budget::default());
    scene.set_camera(camera);
    settle(&world, &mut scene, &mut view);
    assert_eq!(view.object_count(), usize::try_from(count).unwrap());
    (world, scene, view)
}

pub(super) fn surface_scene(objects: &[(Vec3, Vec3, FaceAppearance)], water_height: Option<f32>) -> (Reconciler, SceneView) {
    let capacity = objects.len().max(1).next_power_of_two();
    let mut world = World::new(WorldLimits {
        max_regions: 1,
        max_entities: capacity,
        max_entities_per_region: capacity,
        max_tombstones_per_region: 1,
        max_tombstones: 1,
        max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(1),
        max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(1),
        max_journal_entries: 16,
        max_journal_changes: 64,
        max_region_name_bytes: 64,
        max_recovery_targets: capacity,
    });
    let region = world.register_region().unwrap();
    let _ = world.patch_region(region, world::RegionInfo { origin: Some(DVec3::ZERO), ..Default::default() }).unwrap();
    for (id, (position, scale, face)) in (1..).zip(objects) {
        let mut object = ObjectObservation::new(
            UpdateSource::new(region, id, OrderEvidence::Ordered(u64::from(id)), 0),
            Uuid::from_u128(u128::from(id)),
            NonZeroU32::new(id).unwrap(),
            Kind::Object,
            ParentUpdate::Root,
        );
        object.pose = Some(Pose { position: Some(*position), rotation: Some(Quat::IDENTITY) });
        object.scale = Some(*scale);
        object.shape = FieldUpdate::Set(Shape::Prim(PrimShape::default()));
        object.appearance = FieldUpdate::Set(Appearance::Surface);
        object.surface_appearance = Some(SurfaceAppearance::new(*face, Vec::new()).unwrap());
        assert_matches!(world.observe_object(object).unwrap().outcome, world::ApplyOutcome::Introduced(_));
    }
    if let Some(height) = water_height {
        let sequence = u32::try_from(objects.len()).unwrap() + 1;
        assert_matches!(
            world
                .observe_water_height(UpdateSource::new(region, sequence, OrderEvidence::Ordered(u64::from(sequence)), 0), height)
                .unwrap()
                .outcome,
            world::RegionOutcome::Patched { .. }
        );
    }
    let (mut scene, mut view) = Reconciler::new(Budget::default());
    scene.set_camera(Camera::new(DVec3::ZERO, Vec3::X, 1.0, [128, 128]).unwrap());
    settle(&world, &mut scene, &mut view);
    assert_eq!(view.object_count(), objects.len());
    (scene, view)
}

fn scene_with_world_with_agent(
    alpha: u8,
    scales: [Vec3; 2],
    mapping: world::TextureMapping,
    agent_id: Option<Uuid>,
    with_water: bool,
) -> (World, Reconciler, SceneView, Camera) {
    let mut world = World::new(WorldLimits {
        max_regions: 1,
        max_entities: 2,
        max_entities_per_region: 2,
        max_tombstones_per_region: 1,
        max_tombstones: 1,
        max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(1),
        max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(1),
        max_journal_entries: 16,
        max_journal_changes: 64,
        max_region_name_bytes: 64,
        max_recovery_targets: 4,
    });
    let region = world.register_region().unwrap();
    let _ = world.patch_region(region, world::RegionInfo { origin: Some(DVec3::ZERO), ..Default::default() }).unwrap();
    for ((id, y, mut color, shape), scale) in [
        (1, -2.0, [255, 0, 0, 255], PrimShape::default()),
        (2, 2.0, [0, 255, 0, 255], PrimShape { path_scale_x: 150, ..PrimShape::default() }),
    ]
    .into_iter()
    .zip(scales)
    {
        color[3] = alpha;
        let object_id = Uuid::from_u128(u128::from(id));
        let kind = if Some(object_id) == agent_id { Kind::Avatar } else { Kind::Object };
        let mut object = ObjectObservation::new(
            UpdateSource::new(region, id, OrderEvidence::Ordered(u64::from(id)), 0),
            object_id,
            NonZeroU32::new(id).unwrap(),
            kind,
            ParentUpdate::Root,
        );
        let position = if alpha == 255 { Vec3::new(10.0, y, 0.0) } else { Vec3::new(if id == 1 { 10.0 } else { 14.0 }, y * 0.35, 0.0) };
        object.pose = Some(Pose { position: Some(position), rotation: Some(Quat::IDENTITY) });
        object.scale = Some(scale);
        object.shape = FieldUpdate::Set(Shape::Prim(shape));
        object.appearance = FieldUpdate::Set(Appearance::Surface);
        object.surface_appearance =
            Some(SurfaceAppearance::new(FaceAppearance { color, mapping, ..Default::default() }, Vec::new()).unwrap());
        assert_matches!(world.observe_object(object).unwrap().outcome, world::ApplyOutcome::Introduced(_));
    }
    if with_water {
        assert_matches!(
            world.observe_water_height(UpdateSource::new(region, 3, OrderEvidence::Ordered(3), 0), 20.0).unwrap().outcome,
            world::RegionOutcome::Patched { .. }
        );
    }
    let (mut scene, mut view) = Reconciler::new(Budget::default());
    let camera = Camera::new(DVec3::ZERO, Vec3::X, 1.0, [128, 128]).unwrap();
    scene.set_camera(camera);
    settle(&world, &mut scene, &mut view);
    assert_eq!(view.object_count(), 2);
    assert!(view.objects().all(|object| object.mesh().is_some() && object.materials().is_some()));
    (world, scene, view, camera)
}

pub(super) fn settle(world: &World, scene: &mut Reconciler, view: &mut SceneView) {
    let mut now = Instant::now();
    loop {
        match scene.poll(now) {
            ReconcileStep::Read(job) => {
                let result = match job.query() {
                    ReadQuery::Barrier => Ok(ReadReply::Barrier(world.read_barrier())),
                    ReadQuery::Objects(cursor) => world.object_list_page(*cursor, 256).map(ReadReply::Objects),
                    ReadQuery::Regions(cursor) => world.region_page(*cursor, 64).map(ReadReply::Regions),
                    ReadQuery::Details(entities) => world.object_detail_batch(entities).map(ReadReply::Details),
                    ReadQuery::Changes(cursor) => world.change_page(*cursor, 256).map(ReadReply::Changes),
                    ReadQuery::Terrain(region, cursor) => world.terrain_page(*region, *cursor, 8).map(ReadReply::Terrain),
                    ReadQuery::Environment(region) => Ok(ReadReply::Environment(world.environment(*region))),
                };
                scene.complete_read(job, result.map_err(crate::scene::ReadFailure::World), now);
            }
            ReconcileStep::Build(job) => scene.complete_mesh(job.run()),
            ReconcileStep::Publish(batch) => {
                view.apply(batch).unwrap();
            }
            ReconcileStep::Continue => {}
            ReconcileStep::Wait(until) => now = until,
            ReconcileStep::Idle => return,
            _ => panic!("fixture stopped before publishing geometry"),
        }
    }
}

pub(super) fn render_frame(
    device: &Device,
    queue: &Queue,
    geometry: &mut GeometryRenderer,
    view: &SceneView,
    camera: Camera,
    size: [u32; 2],
    environment: &Evaluated,
    water_height: Option<f64>,
) -> Vec<u8> {
    let mut postprocess = PostProcess::new(device, queue, TextureFormat::Rgba8UnormSrgb);
    postprocess.resize(device, size);
    let depth = geometry.depth(device, size).expect("depth target is admitted");
    postprocess.set_depth_view(device, Some(&depth));
    let frame =
        FrameInput { water_height, environment: Some(environment), cloud_scroll: [0.0; 2], animation_time: 1.0, water_scroll: [0.0; 4] };
    let mut encoder = device.create_command_encoder(&Default::default());
    let admission = geometry.admit().expect("frame is admitted");
    geometry.prepare_with_environment(device, &mut encoder, view, None, camera, size, frame);
    while let Some(job) = geometry.next_texture_job() {
        geometry.complete_texture(job.run(CompressionQuality::Fast));
    }
    geometry.prepare_with_environment(device, &mut encoder, view, None, camera, size, frame);
    let output = device.create_texture(&TextureDescriptor {
        label: Some("Scene frame output"),
        size: Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format: TextureFormat::Rgba8UnormSrgb,
        usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let output_view = output.create_view(&TextureViewDescriptor::default());
    {
        let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
            label: Some("Scene frame"),
            color_attachments: &[Some(RenderPassColorAttachment {
                view: postprocess.scene_view(),
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
    postprocess.render(
        queue,
        &mut encoder,
        &output_view,
        Some(environment),
        1.0 / 60.0,
        Some(camera.position()),
        water_height,
        projection(camera, size).inverse(),
        camera_view(camera),
        0,
        0,
    );
    let bytes_per_row = size[0] * 4;
    let padded = bytes_per_row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let readback = device.create_buffer(&BufferDescriptor {
        label: Some("Scene frame readback"),
        size: u64::from(padded) * u64::from(size[1]),
        usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        TexelCopyTextureInfo { texture: &output, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        TexelCopyBufferInfo {
            buffer: &readback,
            layout: TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(size[1]) },
        },
        Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
    );
    queue.submit([encoder.finish()]);
    geometry.submitted(admission);
    let (send, receive) = mpsc::channel();
    readback.slice(..).map_async(MapMode::Read, move |result| {
        send.send(result).expect("readback receiver remains alive until completion");
    });
    device.poll(PollType::Wait { submission_index: None, timeout: None }).expect("render completes");
    receive.recv().expect("readback result arrives").expect("readback maps");
    let mapped = readback.slice(..).get_mapped_range().expect("readback range is mapped");
    let mut pixels = Vec::with_capacity(size[0] as usize * size[1] as usize * 4);
    for row in mapped.chunks_exact(padded as usize) {
        pixels.extend_from_slice(&row[..bytes_per_row as usize]);
    }
    drop(mapped);
    readback.unmap();
    pixels
}
