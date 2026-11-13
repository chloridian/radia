use std::{
    assert_matches,
    sync::{Arc, mpsc},
    time::Instant,
};

use assets::{Budget as AssetBudget, Failure, FetchResponse, LoadJob, Priority, Request, Store};
use futures::executor::block_on;
use math::{DVec3, Vec3};
use utils::AssetId;
use uuid::Uuid;
use wgpu::util::{BufferInitDescriptor, DeviceExt};
use wgpu::{
    AddressMode, Backends, BindGroupDescriptor, BindGroupEntry, BindingResource, BufferDescriptor, BufferUsages, Color, DeviceDescriptor,
    ErrorFilter, Extent3d, FilterMode, InstanceDescriptor, LoadOp, MapMode, MipmapFilterMode, Operations, Origin3d, PollType, RenderPass,
    RenderPassColorAttachment, RenderPassDepthStencilAttachment, RenderPassDescriptor, SamplerDescriptor, StoreOp, TexelCopyBufferInfo,
    TexelCopyBufferLayout, TexelCopyTextureInfo, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
};

use super::*;
use crate::geometry::{
    fixtures::{scene, scene_with_agent, scene_with_mapping, scene_with_scales, scene_with_world},
    instance::{instance_base, packed_instance},
};
use crate::material::FaceParams;
use crate::pipeline::{INSTANCE_BYTES, Pipeline, VERTEX_BYTES};
use crate::scene::{
    AgentPose, AssetChange, AssetRequest, Camera, DrawnMeshKey, Placement, ReadQuery, ReadReply, ReconcileStep, Reconciler, SceneView,
};
use world::{
    Appearance, FieldUpdate, Kind, ObjectObservation, ObjectPatch, OrderEvidence, ParentUpdate, Pose, Region, SculptFlags, Shape,
    SurfaceAppearance, UpdateSource, WorldPose,
};

fn fixture_region(world: &world::World) -> Region {
    world.region_page(None, 64).expect("fixture regions fit one page").regions[0].region
}

fn check_pages(device: &wgpu::Device, queue: &wgpu::Queue, alpha: u8, separate_pages: bool) {
    let scope = device.push_error_scope(ErrorFilter::Validation);
    let (_scene, view, camera) = scene(alpha);
    let mut geometry = GeometryRenderer::new(device, queue, TextureFormat::Rgba8UnormSrgb);
    let filler = separate_pages.then(|| {
        let (_, shared) = view.objects().next().unwrap().mesh().unwrap();
        assert_matches!(geometry.upload_mesh(device, shared, None, &mut 0, true), MeshUpload::Ready);
        let allocation = &geometry.meshes[&shared.id()].allocation;
        let vertices = allocation.buffers().vertices.size() as usize / VERTEX_BYTES - allocation.vertices().end as usize;
        let indices = allocation.buffers().indices.size() as usize / size_of::<u32>() - allocation.indices().end as usize;
        geometry.storage.allocate(device, vertices, indices, false, &geometry.memory).unwrap()
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    let depth = geometry.depth(device, [128, 128]).unwrap();
    geometry.prepare(device, &mut encoder, &view, None, camera, [128, 128], None);
    let mut meshes: Vec<_> = geometry.meshes.values().collect();
    meshes.sort_by_key(|mesh| mesh.allocation.vertices().start);
    assert_eq!(meshes.len(), 2);
    assert_eq!(Arc::ptr_eq(meshes[0].allocation.buffers(), meshes[1].allocation.buffers()), !separate_pages);
    if !separate_pages {
        assert_eq!(meshes[0].allocation.vertices().end, meshes[1].allocation.vertices().start);
        assert_eq!(meshes[0].allocation.indices().end, meshes[1].allocation.indices().start);
    }
    if alpha == 255 {
        let grouped_calls = if separate_pages { 2 } else { 1 };
        let expected_calls = if geometry.indirect_first_instance { grouped_calls } else { geometry.stats.draw_commands };
        assert_eq!(geometry.stats.draw_calls, expected_calls, "draw submissions match the available indirect features");
        geometry.draw_list.build_draw_batches(&geometry.meshes, geometry.indirect_first_instance);
        assert_eq!(geometry.draw_list.draw_batches().len(), grouped_calls, "rebuilding retained indirect groups keeps submissions");
    }
    let mut objects: Vec<_> = view.objects().collect();
    objects.sort_by_key(|object| object.detail().summary.id);
    if alpha != 255 {
        objects.reverse();
    }
    let reference: Vec<_> = objects
        .into_iter()
        .map(|object| {
            let (_, shared) = object.mesh().unwrap();
            let mesh = shared.mesh();
            let mut vertices = Vec::new();
            for vertex in mesh.vertices() {
                floats(&mut vertices, vertex.position.to_array());
                floats(&mut vertices, vertex.normal.to_array());
                floats(&mut vertices, vertex.uv.to_array());
            }
            let indices: Vec<_> = mesh.indices().iter().flat_map(|index| index.to_le_bytes()).collect();
            let Placement::Ready(transform) = object.placement() else { panic!("fixture object must be placed") };
            let params = FaceParams::new(Some(&object.materials().unwrap()[0]));
            let packed = packed_instance(
                &instance_base(transform.pose.position, transform.pose.rotation, transform.scale, false, camera.position()),
                &params,
                transform.scale,
            );
            (
                device.create_buffer_init(&BufferInitDescriptor { label: None, contents: &vertices, usage: BufferUsages::VERTEX }),
                device.create_buffer_init(&BufferInitDescriptor { label: None, contents: &indices, usage: BufferUsages::INDEX }),
                device.create_buffer_init(&BufferInitDescriptor { label: None, contents: &packed, usage: BufferUsages::VERTEX }),
                mesh.indices().len() as u32,
            )
        })
        .collect();
    let color = device.create_texture(&TextureDescriptor {
        label: None,
        size: Extent3d { width: 128, height: 128, depth_or_array_layers: 2 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format: TextureFormat::Rgba8UnormSrgb,
        usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    for layer in 0..2 {
        let color_view = color.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2),
            base_array_layer: layer,
            array_layer_count: Some(1),
            ..Default::default()
        });
        let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
            color_attachments: &[Some(RenderPassColorAttachment {
                view: &color_view,
                depth_slice: None,
                resolve_target: None,
                ops: Operations { load: LoadOp::Clear(Color::BLACK), store: StoreOp::Store },
            })],
            depth_stencil_attachment: Some(RenderPassDepthStencilAttachment {
                view: &depth,
                depth_ops: Some(Operations { load: LoadOp::Clear(0.0), store: StoreOp::Store }),
                stencil_ops: None,
            }),
            ..Default::default()
        });
        if layer == 0 {
            geometry.draw(&mut pass);
        } else {
            geometry.pipelines.bind_pipeline(&mut pass, if alpha == 255 { Pipeline::Surface } else { Pipeline::Blend });
            pass.set_bind_group(1, geometry.textures.binding(None), &[]);
            for (vertices, indices, instance, count) in &reference {
                pass.set_vertex_buffer(0, vertices.slice(..));
                pass.set_vertex_buffer(1, instance.slice(..));
                pass.set_index_buffer(indices.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..*count, 0, 0..1);
            }
        }
    }
    let readback = device.create_buffer(&BufferDescriptor {
        label: None,
        size: 2 * 128 * 128 * 4,
        usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    for layer in 0..2 {
        encoder.copy_texture_to_buffer(
            TexelCopyTextureInfo { origin: Origin3d { x: 0, y: 0, z: layer }, ..color.as_image_copy() },
            TexelCopyBufferInfo {
                buffer: &readback,
                layout: TexelCopyBufferLayout {
                    offset: u64::from(layer) * 128 * 128 * 4,
                    bytes_per_row: Some(512),
                    rows_per_image: Some(128),
                },
            },
            Extent3d { width: 128, height: 128, depth_or_array_layers: 1 },
        );
    }
    let frame = geometry.admit().unwrap();
    queue.submit([encoder.finish()]);
    geometry.submitted(frame);
    let (mapped, complete) = mpsc::channel();
    readback.slice(..).map_async(MapMode::Read, move |result| mapped.send(result).unwrap());
    device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
    complete.recv().unwrap().unwrap();
    let pixels = readback.get_mapped_range(..).unwrap();
    let (shared_pixels, reference_pixels) = pixels.split_at(128 * 128 * 4);
    let mismatched_pixels = shared_pixels
        .as_chunks::<4>()
        .0
        .iter()
        .zip(reference_pixels.as_chunks::<4>().0)
        .filter(|(shared, reference)| shared != reference)
        .count();
    assert_eq!(mismatched_pixels, 0, "batched pages must match independently drawn complete source meshes");
    let mut red = 0;
    let mut green = 0;
    for pixel in shared_pixels.as_chunks::<4>().0 {
        red += usize::from(pixel[0] > pixel[1].saturating_add(32));
        green += usize::from(pixel[1] > pixel[0].saturating_add(32));
    }
    assert!(red > 100 && green > 100, "both meshes retain distinct geometry and material colors: {red} red, {green} green pixels");
    drop(pixels);
    readback.unmap();
    let (id, old) = geometry
        .meshes
        .iter()
        .filter(|(_, mesh)| filler.as_ref().is_none_or(|filler| !Arc::ptr_eq(mesh.allocation.buffers(), filler.buffers())))
        .min_by_key(|(_, mesh)| mesh.allocation.vertices().start)
        .unwrap();
    let id = *id;
    let old_vertices = old.allocation.vertices();
    let old_indices = old.allocation.indices();
    let old_buffers = old.allocation.buffers().clone();
    let charged = geometry.memory.mesh_state().bytes;
    geometry.retired.push(geometry.meshes.remove(&id).unwrap());
    let frame = geometry.admit().unwrap();
    queue.submit([]);
    geometry.submitted(frame);
    device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
    assert_eq!(geometry.memory.mesh_state().bytes, charged);
    assert!(geometry.needs_frame(Instant::now()), "retirement must unblock preparation without freeing a whole page");
    let reused = geometry.storage.allocate(device, old_vertices.len(), old_indices.len(), false, &geometry.memory).unwrap();
    assert_eq!((reused.vertices(), reused.indices()), (old_vertices, old_indices));
    if separate_pages {
        assert!(!Arc::ptr_eq(reused.buffers(), &old_buffers), "empty pages are released before a new page allocation");
    } else {
        assert!(Arc::ptr_eq(reused.buffers(), &old_buffers), "ranges on a page with live allocations are reused");
    }
    drop(reused);
    drop(old_buffers);
    drop(filler);
    assert!(block_on(scope.pop()).is_none(), "shared ranges and draw offsets must satisfy GPU validation");
    let memory = geometry.memory.clone();
    drop(geometry);
    device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
    assert_eq!(memory.mesh_state().bytes, 0, "renderer retirement releases page accounting after GPU completion");
}

fn mesh_page_bytes(shared: &crate::scene::SharedMesh) -> usize {
    let mesh = shared.mesh();
    let bytes = mesh.vertices().len() * VERTEX_BYTES + std::mem::size_of_val(mesh.indices());
    bytes * (1024 * 1024 / bytes).max(1)
}

fn settle_scene(world: &world::World, scene: &mut Reconciler, view: &mut SceneView, mut now: Instant) -> Vec<AssetRequest> {
    let mut requests = Vec::new();
    loop {
        match scene.poll(now) {
            ReconcileStep::Asset(AssetChange::Request(request)) => requests.push(request),
            ReconcileStep::Asset(AssetChange::Release(_)) => {}
            ReconcileStep::Asset(
                AssetChange::UpdateImportance { .. } | AssetChange::UpdatePriority { .. } | AssetChange::UpdateSize { .. },
            ) => {}
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
            ReconcileStep::Idle => return requests,
            _ => panic!("fixture stopped before publishing geometry"),
        }
    }
}

fn move_object(world: &mut world::World, id: u32, order: u64, position: Vec3, color: [u8; 4]) {
    let mut observation = ObjectObservation::new(
        UpdateSource::new(fixture_region(world), u32::try_from(order).expect("fixture sequence"), OrderEvidence::Ordered(order), 0),
        uuid::Uuid::from_u128(u128::from(id)),
        std::num::NonZeroU32::new(id).expect("fixture local ID"),
        Kind::Object,
        ParentUpdate::Root,
    );
    observation.pose = Some(Pose { position: Some(position), rotation: Some(math::Quat::IDENTITY) });
    observation.appearance = FieldUpdate::Set(Appearance::Surface);
    observation.surface_appearance =
        Some(SurfaceAppearance::new(world::FaceAppearance { color, ..Default::default() }, Vec::new()).expect("fixture appearance"));
    assert_matches!(world.observe_object(observation).unwrap().outcome, world::ApplyOutcome::Updated(_));
}

fn move_mesh_object(world: &mut world::World, id: u32, order: u64, position: Vec3, color: [u8; 4]) {
    let mut observation = ObjectObservation::new(
        UpdateSource::new(fixture_region(world), u32::try_from(order).expect("fixture sequence"), OrderEvidence::Ordered(order), 0),
        Uuid::from_u128(100_000 + u128::from(id)),
        std::num::NonZeroU32::new(id).expect("fixture local ID"),
        Kind::Object,
        ParentUpdate::Root,
    );
    observation.pose = Some(Pose { position: Some(position), rotation: Some(math::Quat::IDENTITY) });
    observation.appearance = FieldUpdate::Set(Appearance::Surface);
    observation.surface_appearance =
        Some(SurfaceAppearance::new(world::FaceAppearance { color, ..Default::default() }, Vec::new()).expect("fixture appearance"));
    assert_matches!(world.observe_object(observation).unwrap().outcome, world::ApplyOutcome::Updated(_));
}

fn prepare_until_current(device: &wgpu::Device, queue: &wgpu::Queue, geometry: &mut GeometryRenderer, view: &SceneView, camera: Camera) {
    let deadline = Instant::now() + std::time::Duration::from_secs(60);
    loop {
        prepare_frame(device, queue, geometry, view, camera);
        if geometry.cache.rebuild.is_none()
            && geometry.cache.version.as_ref() == Some(&view.version())
            && geometry.draw_list.valid()
            && geometry.stats.pending_meshes == 0
            && !geometry.stats.rebuild_pending
        {
            return;
        }
        assert!(Instant::now() < deadline, "geometry preparation did not settle");
    }
}

fn prepare_cache_until_current(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    geometry: &mut GeometryRenderer,
    view: &SceneView,
    camera: Camera,
) {
    let deadline = Instant::now() + std::time::Duration::from_secs(60);
    loop {
        prepare_frame(device, queue, geometry, view, camera);
        assert_draws_resident(geometry);
        if geometry.cache.rebuild.is_none() && geometry.cache.version.as_ref() == Some(&view.version()) {
            return;
        }
        assert!(Instant::now() < deadline, "geometry cache did not observe the current scene version");
    }
}

fn update_cache_until_current(geometry: &mut GeometryRenderer, view: &SceneView, camera: Camera) {
    geometry.update_cache(view);
    geometry.note_rebuild_camera(camera);
    for _ in 0..64 {
        if geometry.cache.rebuild.is_none() && geometry.cache.version.as_ref() == Some(&view.version()) {
            return;
        }
        geometry.update_cache(view);
        geometry.note_rebuild_camera(camera);
    }
    panic!("geometry cache preparation did not settle");
}

fn page_contains_object(view: &SceneView, entity: world::Entity) -> bool {
    let mut after = None;
    loop {
        let page = view.object_page(after, 1_024);
        if page.entries.iter().any(|(_, object)| object.is_some_and(|object| object.detail().summary.entity == entity)) {
            return true;
        }
        if page.done {
            return false;
        }
        after = Some(page.cursor.expect("unfinished object page has a cursor"));
    }
}

fn mesh_scene(
    objects: &[(u32, AssetId, Vec3)],
    textures: Option<(AssetId, AssetId)>,
) -> (world::World, Reconciler, SceneView, Camera, Vec<AssetRequest>) {
    mesh_scene_with_alpha(objects, textures, 128)
}

fn mesh_scene_with_alpha(
    objects: &[(u32, AssetId, Vec3)],
    textures: Option<(AssetId, AssetId)>,
    blended_alpha: u8,
) -> (world::World, Reconciler, SceneView, Camera, Vec<AssetRequest>) {
    let count = objects.len().max(1).next_power_of_two();
    let mut world = world::World::new(world::WorldLimits {
        max_regions: 1,
        max_entities: count,
        max_entities_per_region: count,
        max_tombstones_per_region: 1,
        max_tombstones: 1,
        max_tombstone_bytes_per_region: world::WorldLimits::tombstone_bytes_for_count(1),
        max_tombstone_bytes: world::WorldLimits::tombstone_bytes_for_count(1),
        max_journal_entries: 16,
        max_journal_changes: 64,
        max_region_name_bytes: 64,
        max_recovery_targets: count,
    });
    let region = world.register_region().unwrap();
    let _metadata = world.patch_region(region, world::RegionInfo { origin: Some(DVec3::ZERO), ..Default::default() }).unwrap();
    for (id, asset, position) in objects {
        let mut observation = ObjectObservation::new(
            UpdateSource::new(region, *id, OrderEvidence::Ordered(u64::from(*id)), 0),
            Uuid::from_u128(100_000 + u128::from(*id)),
            std::num::NonZeroU32::new(*id).unwrap(),
            Kind::Object,
            ParentUpdate::Root,
        );
        observation.pose = Some(Pose { position: Some(*position), rotation: Some(math::Quat::IDENTITY) });
        observation.scale = Some(Vec3::ONE);
        observation.shape = FieldUpdate::Set(Shape::Mesh { asset_id: *asset, flags: SculptFlags::default() });
        if *id == 1
            && let Some((red, green)) = textures
        {
            observation.appearance = FieldUpdate::Set(Appearance::Surface);
            observation.surface_appearance = Some(
                SurfaceAppearance::new(
                    world::FaceAppearance { texture: Some(red), color: [255, 0, 0, 255], ..Default::default() },
                    vec![(1, world::FaceAppearance { texture: Some(green), color: [0, 255, 0, blended_alpha], ..Default::default() })],
                )
                .unwrap(),
            );
        }
        let _applied = world.observe_object(observation).unwrap();
    }
    let camera = Camera::new(DVec3::ZERO, Vec3::X, std::f64::consts::FRAC_PI_3, [128, 128]).unwrap();
    let (mut scene, mut view) = Reconciler::new(crate::scene::Budget::default());
    scene.set_camera(camera);
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    (world, scene, view, camera, requests)
}

fn avatar_mesh_scene() -> (world::World, Reconciler, SceneView, Camera) {
    let mut world = world::World::new(world::WorldLimits {
        max_regions: 1,
        max_entities: 1,
        max_entities_per_region: 1,
        max_tombstones_per_region: 1,
        max_tombstones: 1,
        max_tombstone_bytes_per_region: world::WorldLimits::tombstone_bytes_for_count(1),
        max_tombstone_bytes: world::WorldLimits::tombstone_bytes_for_count(1),
        max_journal_entries: 16,
        max_journal_changes: 64,
        max_region_name_bytes: 64,
        max_recovery_targets: 1,
    });
    let region = world.register_region().expect("avatar region registers");
    let _ =
        world.patch_region(region, world::RegionInfo { origin: Some(DVec3::ZERO), ..Default::default() }).expect("avatar region origin");
    let mut observation = ObjectObservation::new(
        UpdateSource::new(region, 1, OrderEvidence::Ordered(1), 0),
        Uuid::from_u128(1),
        std::num::NonZeroU32::new(1).expect("avatar local id"),
        Kind::Avatar,
        ParentUpdate::Root,
    );
    observation.scale = Some(Vec3::ONE);
    observation.shape = FieldUpdate::Set(Shape::Prim(world::PrimShape::default()));
    observation.appearance = FieldUpdate::Set(Appearance::Surface);
    observation.surface_appearance = Some(SurfaceAppearance::new(world::FaceAppearance::default(), Vec::new()).expect("avatar appearance"));
    let _ = world.observe_object(observation).expect("avatar introduction");
    let (mut scene, mut view) = Reconciler::new(crate::scene::Budget::default());
    let camera = Camera::new(DVec3::ZERO, Vec3::X, 1.0, [128, 128]).expect("avatar camera");
    scene.set_camera(camera);
    settle_scene(&world, &mut scene, &mut view, Instant::now());
    (world, scene, view, camera)
}

fn prepared_asset(request: Request, bytes: Vec<u8>) -> assets::Status {
    let now = Instant::now();
    let mut store = Store::new(AssetBudget::default());
    let route = store.register_route().unwrap();
    let demand = store.request(route, request, Priority::Visible, now).unwrap();
    let LoadJob::Fetch(job) = store.next_job(now).unwrap() else { panic!("fixture request unexpectedly used a cached source") };
    let input = match request {
        Request::MeshLod(block) => {
            let range = block.range();
            let start = usize::try_from(range.start).unwrap();
            let end = usize::try_from(range.end).unwrap();
            let bytes = bytes.get(start..end).expect("mesh fixture contains the requested LOD block").to_vec();
            job.complete_response(Ok(FetchResponse::Partial { bytes, range, total_length: Some(block.asset_length()) })).unwrap()
        }
        Request::MeshHeader(_) if bytes.len() > job.response_limit() => {
            let failure = job.complete_response(Err(Failure::FullAssetRequired)).expect_err("header fallback requires a full retry");
            store.complete(failure.into(), now);
            let LoadJob::Fetch(retry) = store.next_job(now).expect("full mesh retry is admitted") else {
                panic!("full mesh retry unexpectedly used a cached source");
            };
            assert!(retry.force_full_mesh());
            retry.complete(Ok(bytes)).unwrap()
        }
        _ => job.complete(Ok(bytes)).unwrap(),
    };
    let completion = store.start_decode(input).unwrap().run();
    store.complete(completion, now);
    demand.status()
}

fn vulkan_device() -> (wgpu::Device, wgpu::Queue) {
    block_on(async {
        let instance =
            wgpu::Instance::new(InstanceDescriptor { backends: Backends::VULKAN, ..InstanceDescriptor::new_without_display_handle() });
        let adapter = instance.request_adapter(&Default::default()).await.unwrap();
        adapter.request_device(&DeviceDescriptor::default()).await.unwrap()
    })
}

fn prepare_frame(device: &wgpu::Device, queue: &wgpu::Queue, geometry: &mut GeometryRenderer, view: &SceneView, camera: Camera) {
    prepare_frame_at(device, queue, geometry, view, camera, Instant::now());
}

fn assert_draws_resident(geometry: &GeometryRenderer) {
    for draw in geometry.draw_list.draws() {
        assert!(geometry.meshes.contains_key(&draw.mesh), "committed draw command lost GPU mesh residency");
        assert!(geometry.draw_list.committed_pins_mesh(draw.mesh), "committed draw command lost its mesh pin");
    }
}

fn prepare_frame_at(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    geometry: &mut GeometryRenderer,
    view: &SceneView,
    camera: Camera,
    now: Instant,
) {
    let frame = geometry.admit().unwrap();
    let mut encoder = device.create_command_encoder(&Default::default());
    geometry.prepare_at(device, &mut encoder, view, None, camera, [128, 128], crate::FrameInput::default(), now);
    queue.submit([encoder.finish()]);
    geometry.submitted(frame);
    device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
}

fn ready_mesh_scene(objects: &[(u32, AssetId, Vec3)], bytes: Vec<u8>) -> (world::World, Reconciler, SceneView, Camera) {
    let (world, mut scene, mut view, camera, requests) = mesh_scene(objects, None);
    for request in requests {
        if matches!(request.request, Request::MeshHeader(_)) {
            scene.complete_asset(request.ticket, prepared_asset(request.request, bytes.clone()));
        }
    }
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    for request in requests {
        if matches!(request.request, Request::MeshLod(_)) {
            scene.complete_asset(request.ticket, prepared_asset(request.request, bytes.clone()));
        }
    }
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());
    (world, scene, view, camera)
}

fn prepare_agent_frame(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    geometry: &mut GeometryRenderer,
    view: &SceneView,
    camera: Camera,
    agent_pose: Option<AgentPose>,
) {
    let frame = geometry.admit().expect("geometry frame lease");
    let mut encoder = device.create_command_encoder(&Default::default());
    geometry.prepare(device, &mut encoder, view, agent_pose, camera, [128, 128], None);
    queue.submit([encoder.finish()]);
    geometry.submitted(frame);
    device.poll(PollType::Wait { submission_index: None, timeout: None }).expect("submitted frame completes");
}

fn fill_mesh_pages(device: &wgpu::Device, geometry: &mut GeometryRenderer) -> Vec<crate::geometry::storage::Allocation> {
    let mut pages: Vec<(Arc<crate::geometry::storage::Buffers>, u32, u32)> = Vec::new();
    for mesh in geometry.meshes.values() {
        let buffers = mesh.allocation.buffers().clone();
        if let Some((_, vertices, indices)) = pages.iter_mut().find(|(page, _, _)| Arc::ptr_eq(page, &buffers)) {
            *vertices = (*vertices).max(mesh.allocation.vertices().end);
            *indices = (*indices).max(mesh.allocation.indices().end);
        } else {
            pages.push((buffers, mesh.allocation.vertices().end, mesh.allocation.indices().end));
        }
    }
    let mut tails: Vec<_> = pages
        .iter()
        .map(|(buffers, vertices, indices)| {
            let vertex_capacity = u32::try_from(buffers.vertices.size() / VERTEX_BYTES as u64).expect("fixture page vertices");
            let index_capacity = u32::try_from(buffers.indices.size() / size_of::<u32>() as u64).expect("fixture page indices");
            (buffers.clone(), vertex_capacity - *vertices, index_capacity - *indices)
        })
        .collect();
    tails.sort_unstable_by_key(|(_, vertices, indices)| std::cmp::Reverse(vertices.saturating_add(*indices)));
    let mut allocations = Vec::new();
    for (buffers, vertices, indices) in tails {
        if vertices == 0 || indices == 0 {
            continue;
        }
        if let Some(allocation) = geometry.storage.allocate(device, vertices as usize, indices as usize, false, &geometry.memory) {
            assert!(Arc::ptr_eq(allocation.buffers(), &buffers), "the page tail reservation matches its page");
            allocations.push(allocation);
        }
    }
    allocations
}

fn render_pixels(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    size: [u32; 2],
    depth: &wgpu::TextureView,
    draw: impl FnOnce(&mut RenderPass<'_>),
) -> Vec<[u8; 4]> {
    let color = device.create_texture(&TextureDescriptor {
        label: None,
        size: Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format: TextureFormat::Rgba8UnormSrgb,
        usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color.create_view(&Default::default());
    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
            color_attachments: &[Some(RenderPassColorAttachment {
                view: &color_view,
                depth_slice: None,
                resolve_target: None,
                ops: Operations { load: LoadOp::Clear(Color::BLACK), store: StoreOp::Store },
            })],
            depth_stencil_attachment: Some(RenderPassDepthStencilAttachment {
                view: depth,
                depth_ops: Some(Operations { load: LoadOp::Clear(0.0), store: StoreOp::Store }),
                stencil_ops: None,
            }),
            ..Default::default()
        });
        draw(&mut pass);
    }
    let bytes_per_row = size[0] * 4;
    let readback = device.create_buffer(&BufferDescriptor {
        label: None,
        size: u64::from(bytes_per_row) * u64::from(size[1]),
        usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        TexelCopyTextureInfo { origin: Origin3d::ZERO, ..color.as_image_copy() },
        TexelCopyBufferInfo {
            buffer: &readback,
            layout: TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bytes_per_row), rows_per_image: Some(size[1]) },
        },
        Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
    );
    queue.submit([encoder.finish()]);
    let (mapped, complete) = mpsc::channel();
    readback.slice(..).map_async(MapMode::Read, move |result| mapped.send(result).unwrap());
    device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
    complete.recv().unwrap().unwrap();
    let pixels = readback.get_mapped_range(..).unwrap().as_chunks::<4>().0.to_vec();
    readback.unmap();
    pixels
}

fn check_mirrored_render(device: &wgpu::Device, queue: &wgpu::Queue) {
    let scope = device.push_error_scope(ErrorFilter::Validation);
    let (_scene, view, camera) = scene_with_scales(255, [Vec3::splat(2.0), Vec3::new(-2.0, 2.0, 2.0)]);
    let size = [256, 256];
    let mut geometry = GeometryRenderer::new(device, queue, TextureFormat::Rgba8UnormSrgb);
    let mut prepare = device.create_command_encoder(&Default::default());
    let depth = geometry.depth(device, size).unwrap();
    geometry.prepare(device, &mut prepare, &view, None, camera, size, None);
    queue.submit([prepare.finish()]);
    assert_eq!(geometry.stats.instances, 2, "the public scene view submits both visible prims");
    let pixels = render_pixels(device, queue, size, &depth, |pass| geometry.draw(pass));
    let mut red = Vec::new();
    let mut green = Vec::new();
    for pixel in pixels {
        if pixel[0] > pixel[1].saturating_add(32) {
            red.push(pixel[0]);
        }
        if pixel[1] > pixel[0].saturating_add(32) {
            green.push(pixel[1]);
        }
    }
    assert!(red.len() > 100, "unmirrored prim remains visible after back-face culling: {} pixels", red.len());
    assert!(green.len() > 100, "mirrored prim remains visible after back-face culling: {} pixels", green.len());
    let average = |values: &[u8]| values.iter().map(|value| u32::from(*value)).sum::<u32>() / values.len() as u32;
    assert!(average(&red).abs_diff(average(&green)) < 30, "mirroring keeps front-face normals outward for studio lighting");
    drop(geometry);
    device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
    assert!(block_on(scope.pop()).is_none(), "production scene preparation and mirrored draw pass satisfy GPU validation");
}

fn check_planar_projection(device: &wgpu::Device, queue: &wgpu::Queue) {
    let scope = device.push_error_scope(ErrorFilter::Validation);
    let (_scene, view, camera) = scene_with_mapping(255, [Vec3::splat(2.0), Vec3::new(2.0, -2.0, 2.0)], world::TextureMapping::Planar);
    let size = [256, 256];
    let mut geometry = GeometryRenderer::new(device, queue, TextureFormat::Rgba8UnormSrgb);
    let depth = geometry.depth(device, size).unwrap();
    let projection = geometry.pipelines.update_camera_uniform(queue, camera, size, camera.position());
    let mut objects: Vec<_> = view.objects().collect();
    objects.sort_by_key(|object| object.detail().summary.id);
    let mut instances = Vec::new();
    for object in &objects {
        let Placement::Ready(transform) = object.placement() else { panic!("fixture prim must be placed") };
        let face = object.materials().unwrap().iter().find(|material| material.slot == 0).expect("fixture has face zero");
        let mut params = FaceParams::new(Some(face));
        assert_eq!(params.params[3], 1.0, "scene planar mapping reaches the production surface parameters");
        params.color = [1.0; 4];
        params.params[0] = 1.0;
        instances.extend_from_slice(&packed_instance(
            &instance_base(transform.pose.position, transform.pose.rotation, transform.scale, false, camera.position()),
            &params,
            transform.scale,
        ));
    }
    let vertices: Vec<u8> = [Vec3::new(0.0, -0.5, -0.5), Vec3::new(0.0, 0.5, -0.5), Vec3::new(0.0, 0.5, 0.5), Vec3::new(0.0, -0.5, 0.5)]
        .into_iter()
        .flat_map(|position| position.to_array().into_iter().chain([-1.0, 0.0, 0.0]).chain([0.0, 0.0]).flat_map(f32::to_le_bytes))
        .collect();
    let indices = [0_u32, 2, 1, 0, 3, 2];
    let index_bytes: Vec<_> = indices.into_iter().flat_map(u32::to_le_bytes).collect();
    let vertex_buffer = device.create_buffer_init(&BufferInitDescriptor { label: None, contents: &vertices, usage: BufferUsages::VERTEX });
    let index_buffer = device.create_buffer_init(&BufferInitDescriptor { label: None, contents: &index_bytes, usage: BufferUsages::INDEX });
    let instance_buffer =
        device.create_buffer_init(&BufferInitDescriptor { label: None, contents: &instances, usage: BufferUsages::VERTEX });
    let texture_pixels: Vec<_> =
        (0..8).flat_map(|_| (0..8).flat_map(|x| if x < 4 { [255, 0, 0, 255] } else { [0, 0, 255, 255] })).collect();
    let texture = device.create_texture(&TextureDescriptor {
        label: None,
        size: Extent3d { width: 8, height: 8, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format: TextureFormat::Rgba8UnormSrgb,
        usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        texture.as_image_copy(),
        &texture_pixels,
        TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(32), rows_per_image: Some(8) },
        Extent3d { width: 8, height: 8, depth_or_array_layers: 1 },
    );
    let texture_view = texture.create_view(&Default::default());
    let sampler = device.create_sampler(&SamplerDescriptor {
        address_mode_u: AddressMode::ClampToEdge,
        address_mode_v: AddressMode::ClampToEdge,
        mag_filter: FilterMode::Nearest,
        min_filter: FilterMode::Nearest,
        mipmap_filter: MipmapFilterMode::Nearest,
        ..Default::default()
    });
    let texture_binding = device.create_bind_group(&BindGroupDescriptor {
        label: None,
        layout: &geometry.textures.layout,
        entries: &[
            BindGroupEntry { binding: 0, resource: BindingResource::TextureView(&texture_view) },
            BindGroupEntry { binding: 1, resource: BindingResource::Sampler(&sampler) },
        ],
    });
    let pixels = render_pixels(device, queue, size, &depth, |pass| {
        geometry.pipelines.bind_pipeline(pass, Pipeline::DoubleSided);
        pass.set_bind_group(1, &texture_binding, &[]);
        pass.set_vertex_buffer(0, vertex_buffer.slice(..));
        pass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint32);
        for index in 0..objects.len() {
            let start = index * INSTANCE_BYTES;
            pass.set_vertex_buffer(1, instance_buffer.slice(start as u64..(start + INSTANCE_BYTES) as u64));
            pass.draw_indexed(0..indices.len() as u32, 0, 0..1);
        }
    });
    for object in objects {
        let Placement::Ready(transform) = object.placement() else { panic!("fixture prim must be placed") };
        for (delta, red_expected) in [(0.2, true), (-0.2, false)] {
            let point = (transform.pose.position + DVec3::new(0.0, delta, 0.0)).as_vec3();
            let clip = projection.project_point3(point);
            let x = ((clip.x * 0.5 + 0.5) * size[0] as f32).floor() as usize;
            let y = ((1.0 - (clip.y * 0.5 + 0.5)) * size[1] as f32).floor() as usize;
            let pixel = pixels[y * size[0] as usize + x];
            assert_eq!(pixel[0] > pixel[2].saturating_add(32), red_expected, "planar texels follow signed object scale after projection");
        }
    }
    drop(geometry);
    device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
    assert!(block_on(scope.pop()).is_none(), "planar projection through the surface shader satisfies GPU validation");
}

fn assert_visible_draw_coverage(geometry: &GeometryRenderer) {
    let visible: std::collections::HashSet<_> = geometry.cache.visible.iter().copied().collect();
    let mut face_slots = std::collections::HashSet::new();
    for (index, item) in geometry.cache.items.iter() {
        for face in &item.faces {
            match (visible.contains(&index), face.slot) {
                (true, Some(slot)) => assert!(face_slots.insert(slot), "each visible face owns one instance slot"),
                (false, None) | (true, None) => {}
                (false, Some(_)) => panic!("hidden items cannot retain instance slots"),
            }
        }
    }
    let draw_slots: std::collections::HashSet<_> =
        geometry.draw_list.draws().iter().flat_map(|draw| draw.first..draw.first.saturating_add(draw.count)).collect();
    let draw_only = draw_slots.difference(&face_slots).copied().collect::<Vec<_>>();
    let faces_only = face_slots.difference(&draw_slots).copied().collect::<Vec<_>>();
    assert!(
        draw_only.is_empty() && faces_only.is_empty(),
        "draw commands cover every packed visible face and no hidden face: draw_only={draw_only:?}, faces_only={faces_only:?}"
    );
}

#[test]
#[ignore = "requires a Vulkan adapter; run explicitly with cargo nextest run -p render --run-ignored only -E 'test(gpu_retired_hidden_page_releases_for_visible_mesh)'"]
fn gpu_retired_hidden_page_releases_for_visible_mesh() {
    let (device, queue) = block_on(async {
        let instance =
            wgpu::Instance::new(InstanceDescriptor { backends: Backends::VULKAN, ..InstanceDescriptor::new_without_display_handle() });
        let adapter = instance.request_adapter(&Default::default()).await.unwrap();
        adapter.request_device(&DeviceDescriptor::default()).await.unwrap()
    });
    let (_scene, view, _) = scene(128);
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    let retired_camera = Camera::new(DVec3::new(13.0, 0.0, 0.0), Vec3::X, 1.0, [128, 128]).unwrap();
    let mut encoder = device.create_command_encoder(&Default::default());
    geometry.prepare(&device, &mut encoder, &view, None, retired_camera, [128, 128], None);
    assert_eq!(geometry.stats.instances, 1);
    assert_eq!(geometry.meshes.len(), 1);
    assert!(!geometry.draw_list.draw_batches().is_empty());

    let retired_id = *geometry.meshes.keys().next().expect("initial visible mesh");
    let retired_object = view
        .objects()
        .find(|object| object.mesh().is_some_and(|(_, mesh)| mesh.id() == retired_id))
        .expect("retired mesh object remains in the scene");
    let retired_entity = retired_object.detail().summary.entity;
    let target_object =
        view.objects().find(|object| object.mesh().is_some_and(|(_, mesh)| mesh.id() != retired_id)).expect("another cached mesh");
    let (_, target_mesh) = target_object.mesh().expect("target mesh");
    let target_id = target_mesh.id();
    let target_entity = target_object.detail().summary.entity;
    let retired_index = geometry.cache.objects[&retired_entity];
    let target_index = geometry.cache.objects[&target_entity];
    let old_page_bytes = geometry.memory.mesh_state().bytes;
    let target_page_bytes = mesh_page_bytes(target_mesh);
    let headroom = target_page_bytes.saturating_sub(old_page_bytes).saturating_add(1);
    let blocker_bytes = crate::geometry::MESH_BYTES
        .checked_sub(old_page_bytes.checked_add(headroom).expect("fixture page budget"))
        .expect("fixture meshes fit the renderer budget");
    let blocker = geometry.memory.reserve(blocker_bytes, crate::geometry::Resource::Mesh).expect("fill the unused mesh budget");

    geometry.cache.items.get_mut(retired_index).expect("cached retired item").position = DVec3::new(-100.0, 0.0, 0.0);
    geometry.cache.items.get_mut(target_index).expect("cached target item").position = DVec3::new(14.0, 0.0, 0.0);
    let hidden_camera = Camera::new(DVec3::new(1000.0, 0.0, 0.0), Vec3::X, 1.0, [128, 128]).unwrap();
    prepare_until_current(&device, &queue, &mut geometry, &view, hidden_camera);
    assert!(!geometry.draw_list.pins_mesh(retired_id), "the old drawable releases its mesh before cache eviction");
    drop(geometry.meshes.remove(&retired_id).expect("unreferenced GPU mesh"));
    geometry.cache.camera = None;
    prepare_until_current(&device, &queue, &mut geometry, &view, retired_camera);

    assert!(geometry.meshes.contains_key(&target_id), "the newly visible mesh uploads after the hidden page is released");
    assert!(!geometry.meshes.contains_key(&retired_id), "the hidden mesh retires after the replacement layout releases its pin");
    assert!(geometry.pending_meshes.is_empty());
    assert!(geometry.cache.items.get(retired_index).is_some(), "hidden face groups remain cached");
    assert!(!geometry.cache.candidates.contains(&retired_index));
    assert!(!geometry.cache.visible.contains(&retired_index));
    assert!(geometry.cache.candidates.contains(&target_index));
    assert!(geometry.cache.visible.contains(&target_index));

    let memory = geometry.memory.clone();
    drop(blocker);
    drop(geometry);
    device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
    assert_eq!(memory.mesh_state().bytes, 0);
}

#[test]
#[ignore = "requires a Vulkan adapter; run explicitly with cargo nextest run -p render --run-ignored only -E 'test(gpu_appearance_replacement_rechecks_visibility)'"]
fn gpu_appearance_replacement_rechecks_visibility() {
    let (device, queue) = block_on(async {
        let instance =
            wgpu::Instance::new(InstanceDescriptor { backends: Backends::VULKAN, ..InstanceDescriptor::new_without_display_handle() });
        let adapter = instance.request_adapter(&Default::default()).await.unwrap();
        adapter.request_device(&DeviceDescriptor::default()).await.unwrap()
    });
    let (mut world, mut scene, mut view, camera) = scene_with_world(255, [Vec3::splat(2.0); 2], Default::default());
    let entity = view
        .objects()
        .find(|object| object.detail().summary.id == uuid::Uuid::from_u128(1))
        .expect("fixture object")
        .detail()
        .summary
        .entity;
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    prepare_until_current(&device, &queue, &mut geometry, &view, camera);
    let index = geometry.cache.objects[&entity];
    let item_id = geometry.cache.items.get(index).expect("cached fixture object").id;
    assert!(geometry.cache.candidates.contains(&index));
    assert!(geometry.cache.visible.contains(&index));

    move_object(&mut world, 1, 100, Vec3::new(10.0, 100.0, 0.0), [0, 0, 255, 255]);
    scene.notify_world_change();
    settle_scene(&world, &mut scene, &mut view, Instant::now());
    prepare_until_current(&device, &queue, &mut geometry, &view, camera);
    let index = geometry.cache.objects[&entity];
    assert_eq!(geometry.cache.items.get(index).map(|item| item.id), Some(item_id));
    assert!(!geometry.cache.candidates.contains(&index));
    assert!(!geometry.cache.visible.contains(&index));
    assert_visible_draw_coverage(&geometry);

    move_object(&mut world, 1, 200, Vec3::new(10.0, -2.0, 0.0), [255, 255, 0, 255]);
    scene.notify_world_change();
    settle_scene(&world, &mut scene, &mut view, Instant::now());
    prepare_until_current(&device, &queue, &mut geometry, &view, camera);
    let index = geometry.cache.objects[&entity];
    assert_eq!(geometry.cache.items.get(index).map(|item| item.id), Some(item_id));
    assert!(geometry.cache.candidates.contains(&index));
    assert!(geometry.cache.visible.contains(&index));
    assert_visible_draw_coverage(&geometry);
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo test -p render gpu_same_mesh_appearance_rebuilds_after_eviction -- --ignored"]
fn gpu_same_mesh_appearance_rebuilds_after_eviction() {
    let (device, queue) = vulkan_device();
    let (mut world, mut scene, mut view, camera) = scene_with_world(255, [Vec3::splat(2.0); 2], Default::default());
    let entity =
        view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(1)).expect("fixture object").detail().summary.entity;
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    prepare_until_current(&device, &queue, &mut geometry, &view, camera);
    let index = geometry.cache.objects[&entity];
    let mesh_id = geometry.cache.items.get(index).expect("cached object").mesh.id();

    move_object(&mut world, 1, 100, Vec3::new(10.0, 100.0, 0.0), [255, 0, 0, 255]);
    move_object(&mut world, 2, 101, Vec3::new(10.0, -100.0, 0.0), [0, 255, 0, 255]);
    scene.notify_world_change();
    settle_scene(&world, &mut scene, &mut view, Instant::now());
    prepare_until_current(&device, &queue, &mut geometry, &view, camera);
    assert_eq!(geometry.draw_list.visible_items(), 0, "the committed layout releases both objects");
    assert!(!geometry.draw_list.pins_mesh(mesh_id), "the committed layout releases its mesh pin before eviction");
    drop(geometry.meshes.remove(&mesh_id).expect("resident fixture mesh"));

    let color = [24, 96, 224, 255];
    move_object(&mut world, 1, 102, Vec3::new(10.0, -2.0, 0.0), color);
    scene.notify_world_change();
    settle_scene(&world, &mut scene, &mut view, Instant::now());
    prepare_until_current(&device, &queue, &mut geometry, &view, camera);

    let index = geometry.cache.objects.get(&entity).copied().expect("rebuilt object index");
    let item = geometry.cache.items.get(index).expect("rebuilt object");
    let object = view.object(entity).expect("updated scene object");
    let expected = FaceParams::new(Some(&object.materials().expect("updated material")[0]));
    assert_eq!(item.mesh.id(), mesh_id);
    assert!(item.replacement.is_none(), "same-mesh appearance updates rebuild immediately");
    assert!(geometry.meshes.contains_key(&mesh_id), "the evicted mesh is uploaded again");
    assert_eq!(item.faces.first().map(|face| face.base_params.color), Some(expected.color));
    assert_eq!(geometry.stats.pending_meshes, 0);

    let depth = geometry.depth(&device, [128, 128]).expect("rebuilt depth target");
    let actual = render_pixels(&device, &queue, [128, 128], &depth, |pass| geometry.draw(pass));
    let blue = actual.iter().filter(|pixel| pixel[2] > pixel[0].saturating_add(32) && pixel[2] > pixel[1].saturating_add(32)).count();
    assert!(blue > 100, "the updated material appears in the rendered pixels: {blue} blue pixels");

    let mut clean = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    clean.memory = Arc::new(crate::geometry::Accounting::default());
    prepare_until_current(&device, &queue, &mut clean, &view, camera);
    let clean_depth = clean.depth(&device, [128, 128]).expect("clean depth target");
    let expected = render_pixels(&device, &queue, [128, 128], &clean_depth, |pass| clean.draw(pass));
    assert_eq!(actual, expected, "eviction recovery renders the same pixels as a clean build");
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo test -p render gpu_removed_pending_replacement_reconciles_snapshot -- --ignored"]
fn gpu_removed_pending_replacement_reconciles_snapshot() {
    let (device, queue) = vulkan_device();
    let (mut world, mut scene, mut view, camera) = scene_with_world(255, [Vec3::splat(2.0); 2], Default::default());
    let entity =
        view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(1)).expect("replacement target").detail().summary.entity;
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    prepare_until_current(&device, &queue, &mut geometry, &view, camera);
    let original_mesh = geometry.cache.items.get(geometry.cache.objects[&entity]).expect("original item").mesh.id();
    let target_index = geometry.cache.objects[&entity];
    move_object(&mut world, 2, 10, Vec3::new(1000.0, 0.0, 0.0), [0, 255, 0, 255]);
    scene.notify_world_change();
    settle_scene(&world, &mut scene, &mut view, Instant::now());
    prepare_until_current(&device, &queue, &mut geometry, &view, camera);
    let stale_version = geometry.cache.version.clone().expect("prepared view version");
    let fillers = fill_mesh_pages(&device, &mut geometry);
    assert!(!fillers.is_empty(), "fixture mesh pages have spare capacity to reserve");
    let blocker_bytes = crate::geometry::MESH_BYTES - geometry.memory.mesh_state().bytes;
    let blocker = geometry.memory.reserve(blocker_bytes, crate::geometry::Resource::Mesh).expect("fill mesh budget");

    let mut patch = world::ObjectPatch::new(
        UpdateSource::new(fixture_region(&world), 20, OrderEvidence::Ordered(20), 0),
        std::num::NonZeroU32::new(1).expect("target local id"),
    );
    patch.shape = FieldUpdate::Set(Shape::Prim(world::PrimShape { path_scale_x: 40, ..Default::default() }));
    let _ = world.patch_object(patch).expect("replacement shape applies");
    scene.notify_world_change();
    settle_scene(&world, &mut scene, &mut view, Instant::now());
    let replacement_mesh =
        view.object(entity).and_then(|object| object.mesh().map(|(_, mesh)| mesh.id())).expect("new mesh geometry published");
    assert_ne!(replacement_mesh, original_mesh);
    assert!(!geometry.meshes.contains_key(&replacement_mesh));
    assert_eq!(geometry.cache.items.get(geometry.cache.objects[&entity]).map(|item| item.mesh.id()), Some(original_mesh));
    let view_changes = view.changes_since(&stale_version).expect("same session changes").collect::<Vec<_>>();
    assert!(view_changes.iter().all(|change| !change.full), "the mesh update remains incremental");
    prepare_frame(&device, &queue, &mut geometry, &view, camera);
    let held = geometry.cache.items.get(geometry.cache.objects[&entity]).expect("held item");
    assert_eq!(held.mesh.id(), original_mesh);
    assert_eq!(held.replacement.as_ref().map(|replacement| replacement.item.mesh.id()), Some(replacement_mesh));
    assert_eq!(geometry.stats.instances, 1, "the old item remains drawn while allocation is blocked");

    let _ = world
        .remove_object(
            UpdateSource::new(fixture_region(&world), 30, OrderEvidence::Ordered(30), 0),
            std::num::NonZeroU32::new(1).expect("target local id"),
        )
        .expect("target removal applies");
    scene.notify_world_change();
    settle_scene(&world, &mut scene, &mut view, Instant::now());
    for order in 31..=95 {
        move_object(&mut world, 2, order, Vec3::new(1000.0, 0.0, 0.0), [order as u8, 96, 224, 255]);
        scene.notify_world_change();
        settle_scene(&world, &mut scene, &mut view, Instant::now());
    }
    let changes = view.changes_since(&stale_version).expect("same session changes").collect::<Vec<_>>();
    assert!(changes.iter().any(|change| change.reconcile_all), "evicted removal history requests snapshot reconciliation");
    assert!(changes.iter().flat_map(|change| &change.objects).all(|changed| *changed != entity), "the removal event aged out of history");
    assert_eq!(view.object_count(), 1, "the surviving scene snapshot contains the unrelated object");

    prepare_frame(&device, &queue, &mut geometry, &view, camera);
    assert!(!geometry.cache.visible.contains(&target_index), "the removed object leaves visibility immediately");
    assert_eq!(geometry.stats.instances, 0, "the removed object is no longer drawn during bounded cleanup");
    assert!(geometry.draw_list.draw_batches().is_empty(), "hidden instance slots have no executable draw batches");

    prepare_until_current(&device, &queue, &mut geometry, &view, camera);
    assert!(!geometry.cache.objects.contains_key(&entity));
    assert!(geometry.cache.objects.is_empty(), "the only remaining object is outside the view");
    assert!(geometry.cache.items.iter().all(|(_, item)| item.entity != Some(entity)));
    assert!(geometry.cache.items.iter().all(|(_, item)| item.replacement.is_none()));
    assert!(!geometry.cache.mesh_info.contains_key(&replacement_mesh));
    assert_eq!(geometry.stats.instances, 0);
    assert_visible_draw_coverage(&geometry);

    drop(blocker);
    drop(fillers);
    let memory = geometry.memory.clone();
    drop(geometry);
    device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
    assert_eq!(memory.mesh_state().bytes, 0);
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --run-ignored only -E 'test(gpu_history_repeated_overflow_converges_and_matches_clean_build)'"]
fn gpu_history_repeated_overflow_converges_and_matches_clean_build() {
    let asset = AssetId::new(Uuid::from_u128(9801));
    let bytes = test_support::mesh::asset_with_lod_faces(&[(3, 1)]);
    let object_count = 1_024u32;
    let object_count_usize = object_count as usize;
    let objects = (1..=object_count).map(|id| (id, asset, Vec3::new(10.0, 0.0, 0.0))).collect::<Vec<_>>();
    let (mut world, mut scene, mut view, camera) = ready_mesh_scene(&objects, bytes);
    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    prepare_until_current(&device, &queue, &mut geometry, &view, camera);
    let old_entity = view
        .objects()
        .find(|object| object.detail().summary.id == Uuid::from_u128(100_001))
        .expect("first fixture object")
        .detail()
        .summary
        .entity;
    let old_index = geometry.cache.objects[&old_entity];
    let checkpoint = geometry.cache.version.clone().expect("prepared scene checkpoint");

    for order in 1_000..1_065 {
        move_mesh_object(&mut world, 1, order, Vec3::new(10.0, 0.0, 0.0), [order as u8, 64, 192, 255]);
        scene.notify_world_change();
        settle_scene(&world, &mut scene, &mut view, Instant::now());
    }
    let changes = view.changes_since(&checkpoint).expect("same scene session").collect::<Vec<_>>();
    assert!(changes.iter().any(|change| change.reconcile_all), "history loss requests a full derived-cache reconciliation");

    prepare_frame(&device, &queue, &mut geometry, &view, camera);
    assert!(geometry.cache.rebuild.is_some(), "the first bounded page leaves a large rebuild pending");
    assert!(geometry.needs_frame(Instant::now()), "pending geometry schedules another frame");
    assert!(!geometry.cache.visible.is_empty(), "valid old drawables remain available during catch-up");

    let _ = world
        .remove_object(
            UpdateSource::new(fixture_region(&world), 1_100, OrderEvidence::Ordered(1_100), 0),
            std::num::NonZeroU32::new(1).expect("removed local ID"),
        )
        .expect("old object removal applies");
    let mut reused = ObjectObservation::new(
        UpdateSource::new(fixture_region(&world), 1_101, OrderEvidence::Ordered(1_101), 0),
        Uuid::from_u128(100_001),
        std::num::NonZeroU32::new(object_count + 1).expect("reused local ID"),
        Kind::Object,
        ParentUpdate::Root,
    );
    reused.pose = Some(Pose { position: Some(Vec3::new(10.0, 0.0, 0.0)), rotation: Some(math::Quat::IDENTITY) });
    reused.scale = Some(Vec3::ONE);
    reused.shape = FieldUpdate::Set(Shape::Prim(world::PrimShape::default()));
    reused.appearance = FieldUpdate::Set(Appearance::Surface);
    reused.surface_appearance = Some(SurfaceAppearance::new(world::FaceAppearance::default(), Vec::new()).expect("reused appearance"));
    let introduced = world.observe_object(reused).expect("same UUID can be reused after removal");
    let world::ApplyOutcome::Introduced(new_entity) = introduced.outcome else { panic!("reused UUID must create a new entity") };
    scene.notify_world_change();
    settle_scene(&world, &mut scene, &mut view, Instant::now());
    assert_ne!(old_entity, new_entity, "entity identity changes when a removed UUID is reused");
    assert!(view.object_is_live(new_entity));
    assert!(view.object(new_entity).is_some_and(|object| object.mesh().is_some()), "the reused entity is geometry-ready");
    assert_eq!(view.object_count(), object_count_usize);
    assert!(page_contains_object(&view, new_entity));

    let removed_entity = view
        .objects()
        .find(|object| object.detail().summary.id == Uuid::from_u128(100_200))
        .expect("last fixture object")
        .detail()
        .summary
        .entity;
    let _ = world
        .remove_object(
            UpdateSource::new(fixture_region(&world), 1_102, OrderEvidence::Ordered(1_102), 0),
            std::num::NonZeroU32::new(200).expect("removed local ID"),
        )
        .expect("fixture slot removal applies");
    scene.notify_world_change();
    settle_scene(&world, &mut scene, &mut view, Instant::now());
    assert_eq!(view.object_count(), object_count_usize - 1);

    let visibility_before_insert = view.visibility_version();
    let mut added = ObjectObservation::new(
        UpdateSource::new(fixture_region(&world), 1_103, OrderEvidence::Ordered(1_103), 0),
        Uuid::from_u128(200_902),
        std::num::NonZeroU32::new(object_count + 2).expect("new local ID"),
        Kind::Object,
        ParentUpdate::Root,
    );
    added.pose = Some(Pose { position: Some(Vec3::new(10.0, 0.0, 0.0)), rotation: Some(math::Quat::IDENTITY) });
    added.scale = Some(Vec3::ONE);
    added.shape = FieldUpdate::Set(Shape::Prim(world::PrimShape::default()));
    added.appearance = FieldUpdate::Set(Appearance::Surface);
    added.surface_appearance = Some(SurfaceAppearance::new(world::FaceAppearance::default(), Vec::new()).expect("added appearance"));
    let introduced = world.observe_object(added).expect("new object applies");
    let world::ApplyOutcome::Introduced(added_entity) = introduced.outcome else { panic!("new UUID must create an entity") };
    scene.notify_world_change();
    settle_scene(&world, &mut scene, &mut view, Instant::now());
    assert_eq!(view.visibility_version(), visibility_before_insert, "new membership leaves existing drawables valid");
    assert_eq!(view.object_count(), object_count_usize);
    assert!(page_contains_object(&view, added_entity));

    for order in 2_000..2_065 {
        move_mesh_object(&mut world, 150, order, Vec3::new(10.0, 0.0, 0.0), [order as u8, 128, 64, 255]);
        scene.notify_world_change();
        settle_scene(&world, &mut scene, &mut view, Instant::now());
    }
    prepare_frame(&device, &queue, &mut geometry, &view, camera);
    assert!(geometry.cache.rebuild.is_some(), "a second overflow leaves bounded recovery active");
    assert!(!geometry.cache.visible.contains(&old_index), "the removed entity disappears from draw visibility immediately");
    let far_camera = Camera::new(DVec3::new(10_000.0, 0.0, 0.0), Vec3::X, 1.0, [128, 128]).expect("rebuild camera");
    for camera in [far_camera, camera, far_camera, camera] {
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        assert!(geometry.cache.rebuild.is_some(), "camera churn does not restart the pending cache pass");
    }

    let deadline = Instant::now() + std::time::Duration::from_secs(60);
    for order in 10_000..10_130 {
        assert!(Instant::now() < deadline, "recovery did not stay responsive during updates");
        move_mesh_object(&mut world, 150, order, Vec3::new(10.0, 0.0, 0.0), [order as u8, 128, 64, 255]);
        scene.notify_world_change();
        settle_scene(&world, &mut scene, &mut view, Instant::now());
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
    }
    let changes = view.changes_since(&checkpoint).expect("same scene session").collect::<Vec<_>>();
    assert!(changes.iter().any(|change| change.reconcile_all), "the retained history no longer covers the original checkpoint");
    prepare_until_current(&device, &queue, &mut geometry, &view, camera);
    assert!(geometry.cache.rebuild.is_none(), "quiet-source recovery completes");
    assert_eq!(geometry.cache.version.as_ref(), Some(&view.version()));
    assert!(!geometry.cache.visible.is_empty(), "returning the camera restores the current scene visibility");
    assert!(!geometry.cache.objects.contains_key(&old_entity));
    let reused_index = geometry.cache.index((2, 100_001, 0)).expect("reused UUID item");
    assert_eq!(geometry.cache.objects.get(&new_entity), Some(&reused_index));
    assert!(geometry.cache.objects.contains_key(&added_entity));
    assert!(!geometry.cache.objects.contains_key(&removed_entity));
    assert_eq!(geometry.cache.objects.len(), object_count_usize);

    let mut clean = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    clean.memory = Arc::new(crate::geometry::Accounting::default());
    prepare_until_current(&device, &queue, &mut clean, &view, camera);
    assert!(clean.cache.rebuild.is_none(), "clean derived-cache build completes");
    let signature = |renderer: &GeometryRenderer| {
        let mut items = renderer
            .cache
            .items
            .iter()
            .map(|(_, item)| {
                (
                    item.id,
                    item.entity,
                    item.mesh.id(),
                    item.position,
                    item.rotation,
                    item.scale,
                    item.faces.iter().map(|face| (face.order, face.indices.clone())).collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        items.sort_unstable_by_key(|item| item.0);
        items
    };
    assert_eq!(signature(&geometry), signature(&clean), "incremental catch-up matches a clean SceneView rebuild");
    assert_visible_draw_coverage(&geometry);
    assert_visible_draw_coverage(&clean);
    let size = [128, 128];
    let incremental_depth = geometry.depth(&device, size).expect("incremental depth target");
    let clean_depth = clean.depth(&device, size).expect("clean depth target");
    let incremental_pixels = render_pixels(&device, &queue, size, &incremental_depth, |pass| geometry.draw(pass));
    let clean_pixels = render_pixels(&device, &queue, size, &clean_depth, |pass| clean.draw(pass));
    assert_eq!(incremental_pixels, clean_pixels, "rendered fixture output matches the clean rebuild");
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --run-ignored only -E 'test(gpu_retirement_filters_draws_during_history_rebuild)'"]
fn gpu_retirement_filters_draws_during_history_rebuild() {
    let asset = AssetId::new(Uuid::from_u128(9802));
    let bytes = test_support::mesh::asset_with_lod_faces(&[(3, 1)]);
    let objects = (1..=900).map(|id| (id, asset, Vec3::new(10.0, 0.0, 0.0))).collect::<Vec<_>>();
    let (mut world, mut scene, mut view, camera) = ready_mesh_scene(&objects, bytes);
    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    prepare_until_current(&device, &queue, &mut geometry, &view, camera);
    assert!(!geometry.cache.visible.is_empty());
    for order in 1_000..1_065 {
        move_mesh_object(&mut world, 1, order, Vec3::new(10.0, 0.0, 0.0), [order as u8, 64, 192, 255]);
        scene.notify_world_change();
        settle_scene(&world, &mut scene, &mut view, Instant::now());
    }
    prepare_frame(&device, &queue, &mut geometry, &view, camera);
    assert!(geometry.cache.rebuild.is_some());

    let _ = world.retire_region(fixture_region(&world)).expect("region retirement applies");
    scene.notify_world_change();
    settle_scene(&world, &mut scene, &mut view, Instant::now());
    prepare_frame(&device, &queue, &mut geometry, &view, camera);
    let page_deadline = Instant::now() + std::time::Duration::from_secs(60);
    while geometry.stats.rebuild_sweep_slots == 0 {
        assert!(Instant::now() < page_deadline, "filtered pages did not advance recovery to stale-item cleanup");
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
    }

    assert!(geometry.cache.rebuild.is_some(), "stale-item reclamation continues in bounded pages");
    assert!(geometry.stats.rebuild_sweep_slots > 0, "filtered object pages advance recovery into stale-item cleanup");
    assert_eq!(view.object_count(), 0, "the authoritative scene projection filters the retired region");
    assert!(geometry.cache.visible.is_empty(), "retired region content is removed from visibility immediately");
    assert_eq!(geometry.stats.instances, 0);
    assert_eq!(geometry.stats.draw_commands, 0, "retired commands leave the prepared indirect list immediately");
    assert!(geometry.draw_list.draw_batches().is_empty(), "retired slots are omitted while layout cleanup continues");
    assert!(!geometry.draw_list.draws().is_empty(), "the previous draw list remains available during cleanup");
    let size = [128, 128];
    let depth = geometry.depth(&device, size).expect("retirement depth target");
    let pixels = render_pixels(&device, &queue, size, &depth, |pass| geometry.draw(pass));
    let background = render_pixels(&device, &queue, size, &depth, |pass| {
        geometry.sky.draw(pass);
        geometry.void_water.draw(pass);
    });
    let differences = pixels.iter().zip(&background).enumerate().filter(|(_, (pixel, background))| pixel != background).collect::<Vec<_>>();
    assert!(
        differences.is_empty(),
        "retired faces add {} pixels over the scene background; first difference {:?}, water {}, batches {}",
        differences.len(),
        differences.first(),
        geometry.void_water.active,
        geometry.draw_list.draw_batches().len()
    );
    prepare_until_current(&device, &queue, &mut geometry, &view, camera);
    assert!(geometry.cache.rebuild.is_none(), "filtered object pages allow retirement recovery to finish");
    assert_eq!(geometry.cache.version.as_ref(), Some(&view.version()));
    assert!(geometry.cache.objects.is_empty(), "retired object cache entries are removed");
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --run-ignored only -E 'test(gpu_large_face_mesh_info_rebuild_respects_work_allowance)'"]
fn gpu_large_face_mesh_info_rebuild_respects_work_allowance() {
    let asset = AssetId::new(Uuid::from_u128(9803));
    let bytes = test_support::mesh::asset_with_lod_geometry(&[(3, 1, 65_535, 131_079)]);
    let objects = (1..=4).map(|id| (id, asset, Vec3::new(10.0, 0.0, 0.0))).collect::<Vec<_>>();
    let (_world, _scene, view, camera) = ready_mesh_scene(&objects, bytes);
    let mesh = view.objects().next().expect("large mesh fixture object").mesh().expect("large mesh is ready").1.clone();
    let expected_info_bytes = mesh.mesh().indices().len() * (std::mem::size_of::<u32>() + std::mem::size_of::<Vec3>());
    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    let mut total_info_bytes = 0;
    let deadline = Instant::now() + std::time::Duration::from_secs(60);
    loop {
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        let stats = geometry.stats;
        total_info_bytes += stats.rebuild_mesh_info_bytes;
        assert!(stats.rebuild_mesh_info_bytes <= 8 * 1024 * 1024, "one face cannot exceed the per-frame byte allowance");
        if geometry.cache.rebuild.is_none() && geometry.cache.version.as_ref() == Some(&view.version()) && stats.pending_meshes == 0 {
            break;
        }
        assert!(Instant::now() < deadline, "large shared-mesh cache did not converge");
    }
    assert_eq!(total_info_bytes, expected_info_bytes, "the single large face is processed exactly once");
    assert_eq!(geometry.cache.mesh_info[&mesh.id()].centers.len(), mesh.mesh().faces().len());
    assert_eq!(geometry.cache.items.iter().count(), objects.len());
}

#[test]
#[ignore = "requires a Vulkan adapter; run explicitly with cargo nextest run -p render --run-ignored only -E 'test(gpu_replacement_visibility_tracks_its_own_bounds)'"]
fn gpu_replacement_visibility_tracks_its_own_bounds() {
    let first_asset = AssetId::new(Uuid::from_u128(801));
    let second_asset = AssetId::new(Uuid::from_u128(802));
    let first_bytes = test_support::mesh::asset_with_lod_faces(&[(3, 1)]);
    let second_bytes = test_support::mesh::asset_with_lod_bounds(3, [-0.01, 14.0, -1.0], [0.01, 16.0, 1.0]);
    let (mut world, mut scene, mut view, first_camera, requests) = mesh_scene(&[(1, first_asset, Vec3::new(2.0, 0.0, 0.0))], None);
    let first_header = requests
        .iter()
        .find(|request| matches!(request.request, Request::MeshHeader(asset) if asset == first_asset))
        .copied()
        .expect("held mesh header request");
    scene.complete_asset(first_header.ticket, prepared_asset(first_header.request, first_bytes.clone()));
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    let first_high = requests
        .iter()
        .find(|request| {
            request.request.resource().id == first_asset
                && matches!(request.request, Request::MeshLod(lod) if lod.lod() == assets::mesh::MeshLod::High)
        })
        .copied()
        .expect("held high LOD request");
    scene.complete_asset(first_high.ticket, prepared_asset(first_high.request, first_bytes));
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());

    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    prepare_frame(&device, &queue, &mut geometry, &view, first_camera);
    let entity = view
        .objects()
        .find(|object| object.detail().summary.id == Uuid::from_u128(100_001))
        .expect("held mesh object")
        .detail()
        .summary
        .entity;
    let item_index = geometry.cache.objects[&entity];
    let held_mesh = geometry.cache.items.get(item_index).expect("held mesh item").mesh.id();
    assert!(geometry.meshes.contains_key(&held_mesh));
    assert!(geometry.cache.candidates.contains(&item_index));
    let initial_visible_union = geometry.cache.candidates.len() + geometry.cache.replacement_visible.len();
    assert_eq!(initial_visible_union, 1, "the held item is the only visible candidate");

    let fillers = fill_mesh_pages(&device, &mut geometry);
    assert!(!fillers.is_empty(), "fixture mesh page has capacity to reserve");
    let blocker_bytes = crate::geometry::MESH_BYTES - geometry.memory.mesh_state().bytes;
    let blocker = geometry.memory.reserve(blocker_bytes, crate::geometry::Resource::Mesh).expect("fill mesh budget");
    let mut patch = world::ObjectPatch::new(
        UpdateSource::new(fixture_region(&world), 2, OrderEvidence::Ordered(2), 0),
        std::num::NonZeroU32::new(1).expect("target local ID"),
    );
    patch.shape = FieldUpdate::Set(Shape::Mesh { asset_id: second_asset, flags: SculptFlags::default() });
    let _ = world.patch_object(patch).expect("replacement mesh applies");
    scene.notify_world_change();
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    let second_header = requests
        .iter()
        .find(|request| matches!(request.request, Request::MeshHeader(asset) if asset == second_asset))
        .copied()
        .expect("replacement mesh header request");
    scene.complete_asset(second_header.ticket, prepared_asset(second_header.request, second_bytes.clone()));
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    let second_high = requests
        .iter()
        .find(|request| {
            request.request.resource().id == second_asset
                && matches!(request.request, Request::MeshLod(lod) if lod.lod() == assets::mesh::MeshLod::High)
        })
        .copied()
        .expect("replacement high LOD request");
    scene.complete_asset(second_high.ticket, prepared_asset(second_high.request, second_bytes));
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());
    let expected_replacement_id =
        view.object(entity).and_then(|object| object.mesh().map(|(_, mesh)| mesh.id())).expect("replacement mesh published");

    let second_camera = Camera::new(DVec3::ZERO, Vec3::new(2.0, 15.0, 0.0), 1.0, [128, 128]).expect("replacement camera");
    scene.set_camera(second_camera);
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());
    prepare_cache_until_current(&device, &queue, &mut geometry, &view, second_camera);
    let held = geometry.cache.items.get(item_index).expect("held item with replacement");
    let replacement_id = held.replacement.as_ref().expect("pending replacement").item.mesh.id();
    assert_eq!(replacement_id, expected_replacement_id);
    assert!(geometry.cache.candidates.is_empty(), "the held bounds are outside the new frustum");
    assert!(geometry.cache.replacement_visible.contains(&item_index), "the replacement bounds enter the new frustum");
    assert_eq!(
        geometry.cache.candidates.len() + geometry.cache.replacement_visible.len(),
        initial_visible_union,
        "moving visibility from held bounds to replacement bounds keeps the union size"
    );
    assert_eq!(geometry.stats.instances, 0, "an off-screen held mesh contributes no instances");
    assert_eq!(geometry.stats.pending_meshes, 1, "replacement demand survives the unchanged visible union");

    drop(blocker);
    prepare_frame(&device, &queue, &mut geometry, &view, second_camera);
    assert!(geometry.meshes.contains_key(&replacement_id), "the visible replacement uploads after budget is released");
    prepare_frame(&device, &queue, &mut geometry, &view, second_camera);
    let current = geometry.cache.items.get(item_index).expect("promoted replacement item");
    assert_eq!(current.mesh.id(), replacement_id);
    assert!(current.replacement.is_none());
    assert!(geometry.cache.visible.contains(&item_index));
    assert_eq!(geometry.stats.instances, 1);
    assert_visible_draw_coverage(&geometry);

    let memory = geometry.memory.clone();
    drop(geometry);
    drop(fillers);
    device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
    assert_eq!(memory.mesh_state().bytes, 0);
}

#[test]
#[ignore = "requires a Vulkan adapter; run explicitly with cargo nextest run -p render --run-ignored ignored-only gpu_mesh_pages"]
fn gpu_mesh_pages() {
    let (device, queue) = block_on(async {
        let instance =
            wgpu::Instance::new(InstanceDescriptor { backends: Backends::VULKAN, ..InstanceDescriptor::new_without_display_handle() });
        let adapter = instance.request_adapter(&Default::default()).await.unwrap();
        adapter
            .request_device(&DeviceDescriptor {
                required_features: adapter.features() & wgpu::Features::INDIRECT_FIRST_INSTANCE,
                ..Default::default()
            })
            .await
            .unwrap()
    });
    for (alpha, separate_pages) in [(255, false), (128, true)] {
        check_pages(&device, &queue, alpha, separate_pages);
    }
}

#[test]
#[ignore = "requires a Vulkan adapter; run explicitly with cargo test -p render gpu_prim_transforms -- --ignored"]
fn gpu_prim_transforms() {
    let (device, queue) = block_on(async {
        let instance =
            wgpu::Instance::new(InstanceDescriptor { backends: Backends::VULKAN, ..InstanceDescriptor::new_without_display_handle() });
        let adapter = instance.request_adapter(&Default::default()).await.unwrap();
        adapter.request_device(&DeviceDescriptor::default()).await.unwrap()
    });
    check_mirrored_render(&device, &queue);
    check_planar_projection(&device, &queue);
}

#[test]
#[ignore = "requires a Vulkan adapter; run explicitly with cargo test -p render gpu_skip_rebuilds_scene -- --ignored"]
fn gpu_skip_rebuilds_scene() {
    let (device, queue) = block_on(async {
        let instance =
            wgpu::Instance::new(InstanceDescriptor { backends: Backends::VULKAN, ..InstanceDescriptor::new_without_display_handle() });
        let adapter = instance.request_adapter(&Default::default()).await.unwrap();
        adapter.request_device(&DeviceDescriptor::default()).await.unwrap()
    });
    let (_scene, view, camera) = scene(255);
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    let mut encoder = device.create_command_encoder(&Default::default());
    geometry.prepare(&device, &mut encoder, &view, None, camera, [128, 128], None);
    assert!(!geometry.draw_list.draws().is_empty());
    geometry.pending_meshes.insert(geometry.cache.mesh_order.first().expect("visible mesh demand").mesh);
    assert!(!geometry.cache.mesh_representatives.is_empty());
    assert!(!geometry.cache.mesh_compositions.is_empty());
    assert!(!geometry.cache.mesh_order.is_empty());
    assert!(!geometry.cache.mesh_ready.is_empty());
    geometry.skip();
    assert!(geometry.pending_meshes.is_empty());
    assert!(geometry.cache.mesh_order.is_empty());
    assert!(geometry.cache.mesh_order_by_id.is_empty());
    assert!(geometry.cache.mesh_representatives.is_empty());
    assert!(geometry.cache.mesh_compositions.is_empty());
    assert!(geometry.cache.mesh_order_by_id.is_empty());
    assert!(geometry.cache.mesh_ready.is_empty());
    let mut encoder = device.create_command_encoder(&Default::default());
    geometry.prepare(&device, &mut encoder, &view, None, camera, [128, 128], None);
    assert!(!geometry.draw_list.draws().is_empty());
}

#[test]
#[ignore = "requires a Vulkan adapter; run explicitly with cargo test -p render gpu_hidden_items_leave_no_instances_in_draws -- --ignored"]
fn gpu_hidden_items_leave_no_instances_in_draws() {
    let (device, queue) = block_on(async {
        let instance =
            wgpu::Instance::new(InstanceDescriptor { backends: Backends::VULKAN, ..InstanceDescriptor::new_without_display_handle() });
        let adapter = instance.request_adapter(&Default::default()).await.unwrap();
        adapter.request_device(&DeviceDescriptor::default()).await.unwrap()
    });
    let (_scene, view, _) = scene(128);
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    let hidden = Camera::new(DVec3::new(13.0, 0.0, 0.0), Vec3::X, 1.0, [128, 128]).unwrap();
    prepare_until_current(&device, &queue, &mut geometry, &view, hidden);
    assert_eq!(geometry.stats.instances, 1);
    assert!(geometry.draw_list.has_blended());

    let visible = Camera::new(DVec3::ZERO, Vec3::X, 1.0, [128, 128]).unwrap();
    prepare_until_current(&device, &queue, &mut geometry, &view, visible);
    assert_eq!(geometry.stats.instances, 2);
    assert_visible_draw_coverage(&geometry);

    prepare_until_current(&device, &queue, &mut geometry, &view, hidden);
    assert_eq!(geometry.stats.instances, 1);
    assert_visible_draw_coverage(&geometry);
}

#[test]
#[ignore = "requires a Vulkan adapter; run explicitly with cargo nextest run -p render --run-ignored only -E 'test(gpu_agent_pose_override_patches_draw_and_pick)'"]
fn gpu_agent_pose_override_patches_draw_and_pick() {
    let (device, queue) = block_on(async {
        let instance =
            wgpu::Instance::new(InstanceDescriptor { backends: Backends::VULKAN, ..InstanceDescriptor::new_without_display_handle() });
        let adapter = instance.request_adapter(&Default::default()).await.unwrap();
        adapter.request_device(&DeviceDescriptor::default()).await.unwrap()
    });
    let (_scene, view, _) = scene_with_agent();
    let size = [128, 128];
    let agent_id = uuid::Uuid::from_u128(1);
    let agent = view.objects().find(|object| object.detail().summary.id == agent_id).expect("fixture avatar");
    let entity = agent.detail().summary.entity;
    let Placement::Ready(simulator_transform) = agent.placement() else { panic!("fixture avatar must have a pose") };
    let camera = Camera::new(DVec3::new(0.0, -2.0, 0.0), Vec3::X, 1.0, size).unwrap();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    let mut encoder = device.create_command_encoder(&Default::default());
    geometry.prepare(&device, &mut encoder, &view, None, camera, size, None);
    assert_eq!(geometry.stats.layouts, 1);
    let old_ray = camera.ray([64.0, 64.0], size);
    assert!(geometry.pick(camera, old_ray).unwrap().is_some(), "the simulator pose is pickable");

    let predicted_pose = WorldPose { position: simulator_transform.pose.position + DVec3::Y * 2.0, ..simulator_transform.pose };
    let mut encoder = device.create_command_encoder(&Default::default());
    geometry.prepare(&device, &mut encoder, &view, Some(AgentPose::new(agent_id, predicted_pose)), camera, size, None);
    assert_eq!(geometry.stats.layouts, 0, "a pose-only update does not rebuild the draw-list layout");
    assert!(geometry.stats.faces_patched > 0, "visible avatar faces use the pose-patch path");
    let index = geometry.cache.objects[&entity];
    let item = geometry.cache.items.get(index).expect("cached avatar");
    assert_eq!(item.position, predicted_pose.position);
    let slot = item.faces.iter().find_map(|face| face.slot).expect("visible avatar face slot");
    assert_eq!(geometry.draw_list.instance_transform(slot), Some(item.transform.as_slice()));
    assert_eq!(geometry.pick(camera, old_ray).unwrap(), None, "picking no longer sees the old avatar position");
    let moved_ray = camera.ray([41.0, 64.0], size);
    let moved_hit = geometry.pick(camera, moved_ray).unwrap().expect("predicted avatar position is pickable");
    assert!(moved_hit.distance(predicted_pose.position) < 1.0, "picked point follows the predicted pose: {moved_hit:?}");

    let mut encoder = device.create_command_encoder(&Default::default());
    geometry.prepare(&device, &mut encoder, &view, None, camera, size, None);
    assert_eq!(geometry.stats.layouts, 0, "removing an override restores the item through a pose patch");
    assert!(geometry.stats.faces_patched > 0);
    let item = geometry.cache.items.get(index).expect("cached avatar");
    assert_eq!(item.position, simulator_transform.pose.position);
    let slot = item.faces.iter().find_map(|face| face.slot).expect("visible avatar face slot");
    assert_eq!(geometry.draw_list.instance_transform(slot), Some(item.transform.as_slice()));
    assert!(geometry.pick(camera, old_ray).unwrap().is_some(), "picking returns to the simulator pose");
    assert_eq!(geometry.pick(camera, moved_ray).unwrap(), None, "the restored avatar is absent from the predicted ray");
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo test -p render gpu_avatar_mesh_ready_late_uses_agent_pose -- --ignored"]
fn gpu_avatar_mesh_ready_late_uses_agent_pose() {
    let (device, queue) = vulkan_device();
    let (mut world, mut scene, mut view, camera) = avatar_mesh_scene();
    let agent_id = Uuid::from_u128(1);
    let agent = view.objects().next().expect("avatar");
    let entity = agent.detail().summary.entity;
    let predicted_pose = WorldPose { position: DVec3::new(10.0, 0.0, 0.0), rotation: math::Quat::IDENTITY };
    let agent_pose = Some(AgentPose::new(agent_id, predicted_pose));
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    prepare_agent_frame(&device, &queue, &mut geometry, &view, camera, agent_pose);
    assert!(!geometry.cache.objects.contains_key(&entity), "the unresolved avatar has no render item");

    let mut observation = ObjectObservation::new(
        UpdateSource::new(fixture_region(&world), 10, OrderEvidence::Ordered(10), 0),
        agent_id,
        std::num::NonZeroU32::new(1).expect("avatar local id"),
        Kind::Avatar,
        ParentUpdate::Root,
    );
    observation.pose = Some(Pose { position: Some(Vec3::new(10.0, -2.0, 0.0)), rotation: Some(math::Quat::IDENTITY) });
    let applied = world.observe_object(observation).expect("avatar pose update");
    assert_matches!(applied.outcome, world::ApplyOutcome::Updated(_));
    scene.notify_world_change();
    settle_scene(&world, &mut scene, &mut view, Instant::now());

    prepare_agent_frame(&device, &queue, &mut geometry, &view, camera, agent_pose);
    assert_eq!(geometry.stats.instances, 1);
    let index = geometry.cache.objects[&entity];
    let item = geometry.cache.items.get(index).expect("late avatar render item");
    assert!(item.replacement.is_none());
    assert_eq!(item.position, predicted_pose.position);
    let slot = item.faces.iter().find_map(|face| face.slot).expect("visible avatar face slot");
    assert_eq!(geometry.draw_list.instance_transform(slot), Some(item.transform.as_slice()));
    assert_visible_draw_coverage(&geometry);
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo test -p render gpu_mesh_faces_and_alpha -- --ignored"]
fn gpu_mesh_faces_and_alpha() {
    let mesh_asset = AssetId::new(Uuid::from_u128(801));
    let other_asset = AssetId::new(Uuid::from_u128(804));
    let red = AssetId::new(Uuid::from_u128(802));
    let green = AssetId::new(Uuid::from_u128(803));
    let (mut world, mut scene, mut view, _, requests) =
        mesh_scene(&[(1, mesh_asset, Vec3::new(5.0, 0.0, 0.0)), (2, other_asset, Vec3::new(3.0, 0.5, 0.0))], Some((red, green)));
    let camera = Camera::new(DVec3::ZERO, Vec3::X, 25_f64.to_radians(), [128, 128]).expect("fixture camera");
    scene.set_camera(camera);
    let header = requests
        .iter()
        .find(|request| matches!(request.request, Request::MeshHeader(asset) if asset == mesh_asset))
        .copied()
        .expect("mesh header request");
    let other_header = requests
        .iter()
        .find(|request| matches!(request.request, Request::MeshHeader(asset) if asset == other_asset))
        .copied()
        .expect("other mesh header request");
    for request in requests.iter().filter(|request| request.request.resource().kind == assets::Kind::Texture) {
        let texture = if request.request.resource().id == red { test_support::texture::RGB_J2C } else { test_support::texture::GRAY_J2C };
        let status = prepared_asset(request.request, texture.to_vec());
        assert_matches!(status, assets::Status::Ready(_));
        scene.complete_asset(request.ticket, status);
    }
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());
    let bytes = test_support::mesh::asset_with_lod_faces(&[(2, 2), (3, 1)]);
    scene.complete_asset(header.ticket, prepared_asset(header.request, bytes.clone()));
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    let other_header = requests
        .iter()
        .find(|request| matches!(request.request, Request::MeshHeader(asset) if asset == other_asset))
        .copied()
        .unwrap_or(other_header);
    let high = requests
        .iter()
        .find(|request| matches!(request.request, Request::MeshLod(lod) if lod.lod() == assets::mesh::MeshLod::High))
        .copied()
        .expect("selected high LOD request");
    let medium = requests
        .iter()
        .find(|request| matches!(request.request, Request::MeshLod(lod) if lod.lod() == assets::mesh::MeshLod::Medium))
        .copied()
        .expect("fallback medium LOD request");
    scene.complete_asset(medium.ticket, prepared_asset(medium.request, bytes.clone()));
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    for request in requests.into_iter().filter(|request| request.request.resource().kind == assets::Kind::Texture) {
        let texture = if request.request.resource().id == red { test_support::texture::RGB_J2C } else { test_support::texture::GRAY_J2C };
        let status = prepared_asset(request.request, texture.to_vec());
        assert_matches!(status, assets::Status::Ready(_));
        scene.complete_asset(request.ticket, status);
    }
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());
    let object = view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(100_001)).expect("mesh object");
    assert_eq!(object.mesh().map(|(key, _)| key), Some(DrawnMeshKey::Asset { asset: mesh_asset, lod: assets::mesh::MeshLod::Medium }));
    let faces = object.materials().expect("fallback materials");
    let red_face = faces.iter().find(|face| face.slot == 0).expect("first fallback face material");
    let green_face = faces.iter().find(|face| face.slot == 1).expect("second fallback face material");
    assert_eq!(red_face.base_color_texture.as_ref().expect("red texture").state().asset, red);
    assert_eq!(green_face.base_color_texture.as_ref().expect("green texture").state().asset, green);
    assert_eq!(red_face.alpha, crate::material::AlphaBehavior::Opaque);
    assert_eq!(green_face.alpha, crate::material::AlphaBehavior::Blend);

    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    let depth = geometry.depth(&device, [128, 128]).expect("depth target");
    let first = Instant::now() - crate::geometry::first_appearance::ASSET_WAIT;
    let frame = geometry.admit().unwrap();
    let mut encoder = device.create_command_encoder(&Default::default());
    geometry.prepare_at(&device, &mut encoder, &view, None, camera, [128, 128], crate::FrameInput::default(), first);
    queue.submit([encoder.finish()]);
    geometry.submitted(frame);
    device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
    assert_eq!(geometry.stats.instances, 0, "first appearance waits for the matching textures to reach the GPU");
    prepare_frame(&device, &queue, &mut geometry, &view, camera);
    assert_eq!(geometry.stats.instances, 1, "the complete fallback appears by the optional asset deadline");

    scene.complete_asset(high.ticket, prepared_asset(high.request, bytes));
    let other_bytes = test_support::mesh::asset_with_lod_geometry(&[(3, 3, 65_535, 65_535)]);
    scene.complete_asset(other_header.ticket, prepared_asset(other_header.request, other_bytes.clone()));
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    let other_high = requests
        .iter()
        .find(|request| {
            request.request.resource().id == other_asset
                && matches!(request.request, Request::MeshLod(lod) if lod.lod() == assets::mesh::MeshLod::High)
        })
        .copied()
        .expect("other selected high LOD request");
    scene.complete_asset(other_high.ticket, prepared_asset(other_high.request, other_bytes));
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());
    let target = view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(100_001)).expect("replacement target");
    assert_eq!(target.mesh().map(|(key, _)| key), Some(DrawnMeshKey::Asset { asset: mesh_asset, lod: assets::mesh::MeshLod::High }));
    assert_eq!(target.materials().expect("selected high material").len(), 1, "new mesh materials use only the published face slots");

    let frame = geometry.admit().unwrap();
    let mut encoder = device.create_command_encoder(&Default::default());
    geometry.prepare(&device, &mut encoder, &view, None, camera, [128, 128], None);
    assert_eq!(
        geometry.stats.pending_meshes, 1,
        "the larger new mesh uses this frame's upload budget: instances={}, uploaded={}",
        geometry.stats.instances, geometry.stats.uploaded_bytes
    );
    assert_eq!(geometry.stats.instances, 2, "the old multi-face item stays drawn while the replacement waits");
    queue.submit([encoder.finish()]);
    let pixels = render_pixels(&device, &queue, [128, 128], &depth, |pass| geometry.draw(pass));
    geometry.submitted(frame);
    let red_pixels = pixels.iter().filter(|pixel| pixel[0] > pixel[1].saturating_add(24)).count();
    let green_pixels = pixels.iter().filter(|pixel| pixel[1] > pixel[0].saturating_add(24)).count();
    let max_red = pixels.iter().filter(|pixel| pixel[0] > pixel[1].saturating_add(24)).map(|pixel| pixel[0]).max().unwrap_or_default();
    let max_green = pixels.iter().filter(|pixel| pixel[1] > pixel[0].saturating_add(24)).map(|pixel| pixel[1]).max().unwrap_or_default();
    assert!(red_pixels > 50, "fallback opaque face renders with red material: {red_pixels} pixels");
    assert!(green_pixels > 50, "fallback blended face renders with green material: {green_pixels} pixels");
    assert!(max_green < max_red, "fallback face alpha blends against the black clear color: green={max_green}, red={max_red}");

    prepare_frame(&device, &queue, &mut geometry, &view, camera);
    assert_eq!(geometry.stats.pending_meshes, 0, "the replacement becomes resident on the next frame");
    assert_eq!(geometry.stats.instances, 2);

    let _removed = world
        .remove_object(
            UpdateSource::new(fixture_region(&world), 10, OrderEvidence::Ordered(10), 0),
            std::num::NonZeroU32::new(2).expect("fixture local ID"),
        )
        .expect("competing object removal applies");
    scene.notify_world_change();
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());
    prepare_frame(&device, &queue, &mut geometry, &view, camera);
    assert_eq!(geometry.stats.instances, 1, "only the mesh target remains visible");

    let mut patch = world::ObjectPatch::new(
        UpdateSource::new(fixture_region(&world), 11, OrderEvidence::Ordered(11), 0),
        std::num::NonZeroU32::new(1).expect("fixture local ID"),
    );
    patch.shape = FieldUpdate::Set(Shape::Mesh { asset_id: mesh_asset, flags: SculptFlags { invert: true, mirror: true } });
    let _applied = world.patch_object(patch).expect("mesh flags update applies");
    scene.notify_world_change();
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());
    let object = view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(100_001)).expect("flagged mesh target");
    assert_eq!(
        object
            .materials()
            .and_then(|materials| materials.iter().find(|face| face.slot == 0))
            .and_then(|face| face.base_color_texture.as_ref())
            .map(|texture| texture.state().asset),
        Some(red),
        "mesh flags preserve the object's existing face material"
    );
    let frame = geometry.admit().expect("geometry frame lease");
    let mut encoder = device.create_command_encoder(&Default::default());
    geometry.prepare(&device, &mut encoder, &view, None, camera, [128, 128], None);
    assert!(geometry.stats.layouts > 0, "mirror and invert flag changes rebuild the draw layout");
    assert_eq!(geometry.stats.instances, 1);
    queue.submit([encoder.finish()]);
    let hit = (0..128).step_by(8).any(|y| {
        (0..128).step_by(8).any(|x| geometry.pick(camera, camera.ray([f64::from(x), f64::from(y)], [128, 128])).unwrap().is_some())
    });
    geometry.submitted(frame);
    assert!(hit, "a mirrored and inverted mesh remains pickable after winding and normal updates");
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo test -p render gpu_large_mesh_first_upload_exceeds_upload_target -- --ignored"]
fn gpu_large_mesh_first_upload_exceeds_upload_target() {
    let asset = AssetId::new(Uuid::from_u128(810));
    let (world, mut scene, mut view, camera, requests) = mesh_scene(&[(1, asset, Vec3::new(2.0, 0.0, 0.0))], None);
    let bytes = test_support::mesh::asset_with_lod_geometry(&[(3, 3, 65_535, 65_535)]);
    let header = requests
        .iter()
        .find(|request| matches!(request.request, Request::MeshHeader(id) if id == asset))
        .copied()
        .expect("mesh header request");
    scene.complete_asset(header.ticket, prepared_asset(header.request, bytes.clone()));
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    let high = requests
        .iter()
        .find(|request| matches!(request.request, Request::MeshLod(lod) if lod.lod() == assets::mesh::MeshLod::High))
        .copied()
        .expect("selected high LOD request");
    scene.complete_asset(high.ticket, prepared_asset(high.request, bytes));
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());

    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    let mut largest_upload = 0;
    for _ in 0..64 {
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        largest_upload = largest_upload.max(geometry.stats.uploaded_bytes);
        if geometry.cache.rebuild.is_none()
            && geometry.cache.version.as_ref() == Some(&view.version())
            && geometry.draw_list.valid()
            && geometry.stats.pending_meshes == 0
            && !geometry.stats.rebuild_pending
        {
            break;
        }
    }
    assert!(largest_upload > 4 * 1024 * 1024, "one visible mesh may exceed the per-turn upload target");
    assert_eq!(geometry.stats.pending_meshes, 0);
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo test -p render gpu_lod_keeps_fallback_during_unrelated_retirement -- --ignored"]
fn gpu_lod_keeps_fallback_during_unrelated_retirement() {
    let target = AssetId::new(Uuid::from_u128(830));
    let unrelated = AssetId::new(Uuid::from_u128(831));
    let target_bytes = test_support::mesh::asset_with_lod_geometry(&[(2, 8, 65_535, 65_535), (3, 8, 65_535, 131_079)]);
    let unrelated_bytes = test_support::mesh::asset_with_lod_geometry(&[(2, 8, 65_535, 196_605)]);
    let (mut world, mut scene, mut view, camera, requests) =
        mesh_scene(&[(1, target, Vec3::new(2.0, 0.0, 0.0)), (2, unrelated, Vec3::new(4.0, 0.0, 0.0))], None);
    for request in requests {
        let Request::MeshHeader(asset) = request.request else { continue };
        let bytes = if asset == target { target_bytes.clone() } else { unrelated_bytes.clone() };
        scene.complete_asset(request.ticket, prepared_asset(request.request, bytes));
    }
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    let mut high = None;
    for request in requests {
        let Request::MeshLod(lod) = request.request else { continue };
        let bytes = if request.request.resource().id == target { target_bytes.clone() } else { unrelated_bytes.clone() };
        if request.request.resource().id == target && lod.lod() == assets::mesh::MeshLod::High {
            high = Some(request);
        } else if lod.lod() == assets::mesh::MeshLod::Medium {
            scene.complete_asset(request.ticket, prepared_asset(request.request, bytes));
        }
    }
    let high = high.expect("selected high LOD request remains pending");
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());

    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    prepare_until_current(&device, &queue, &mut geometry, &view, camera);
    let target_entity = view
        .objects()
        .find(|object| object.detail().summary.id == Uuid::from_u128(100_001))
        .expect("replacement target")
        .detail()
        .summary
        .entity;
    let unrelated_entity = view
        .objects()
        .find(|object| object.detail().summary.id == Uuid::from_u128(100_002))
        .expect("unrelated mesh object")
        .detail()
        .summary
        .entity;
    let fallback_id = geometry.cache.items.get(geometry.cache.objects[&target_entity]).expect("fallback item").mesh.id();
    let unrelated_id = geometry.cache.items.get(geometry.cache.objects[&unrelated_entity]).expect("unrelated item").mesh.id();
    assert!(geometry.meshes.contains_key(&unrelated_id));

    let _ = world
        .remove_object(
            UpdateSource::new(fixture_region(&world), 30, OrderEvidence::Ordered(30), 0),
            std::num::NonZeroU32::new(2).expect("unrelated local id"),
        )
        .expect("unrelated object removal applies");
    scene.notify_world_change();
    settle_scene(&world, &mut scene, &mut view, Instant::now());
    prepare_until_current(&device, &queue, &mut geometry, &view, camera);
    assert!(!geometry.cache.objects.contains_key(&unrelated_entity));
    assert!(geometry.meshes.contains_key(&unrelated_id), "unrelated GPU allocation remains available for eviction");
    let blocker_bytes = crate::geometry::MESH_BYTES - geometry.memory.mesh_state().bytes;
    let blocker = geometry.memory.reserve(blocker_bytes, crate::geometry::Resource::Mesh).expect("fill mesh budget");

    scene.complete_asset(high.ticket, prepared_asset(high.request, target_bytes));
    settle_scene(&world, &mut scene, &mut view, Instant::now());
    let replacement_id = view.object(target_entity).and_then(|object| object.mesh().map(|(_, mesh)| mesh.id())).expect("high LOD geometry");
    prepare_cache_until_current(&device, &queue, &mut geometry, &view, camera);
    let held = geometry.cache.items.get(geometry.cache.objects[&target_entity]).expect("fallback item");
    assert_eq!(held.mesh.id(), fallback_id);
    assert_eq!(held.replacement.as_ref().map(|replacement| replacement.item.mesh.id()), Some(replacement_id));
    assert!(geometry.meshes.contains_key(&fallback_id), "the visible fallback stays resident");
    assert!(!geometry.meshes.contains_key(&unrelated_id), "the unrelated allocation was retired to make room");
    assert_eq!(geometry.stats.mesh_budget_blocked, 1);
    assert_eq!(geometry.stats.instances, 1);

    prepare_frame(&device, &queue, &mut geometry, &view, camera);
    prepare_frame(&device, &queue, &mut geometry, &view, camera);
    let current = geometry.cache.items.get(geometry.cache.objects[&target_entity]).expect("replacement item");
    assert_eq!(current.mesh.id(), replacement_id);
    assert!(current.replacement.is_none());
    assert_eq!(geometry.stats.mesh_budget_blocked, 0);
    assert_eq!(geometry.stats.instances, 1);
    assert!(geometry.stats.mesh_bytes <= crate::geometry::MESH_BYTES);

    let memory = geometry.memory.clone();
    drop(blocker);
    drop(geometry);
    device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
    assert_eq!(memory.mesh_state().bytes, 0);
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo test -p render gpu_lod_keeps_fallback -- --ignored"]
fn gpu_lod_keeps_fallback() {
    let target = AssetId::new(Uuid::from_u128(820));
    let other_a = AssetId::new(Uuid::from_u128(821));
    let other_b = AssetId::new(Uuid::from_u128(822));
    let target_bytes = test_support::mesh::asset_with_lod_geometry(&[(2, 8, 65_535, 65_535), (3, 8, 65_535, 131_079)]);
    let other_a_bytes = test_support::mesh::asset_with_lod_geometry(&[(2, 8, 65_535, 196_605)]);
    let other_b_bytes = test_support::mesh::asset_with_lod_geometry(&[(2, 4, 65_535, 65_535)]);
    let (mut world, mut scene, mut view, camera, requests) = mesh_scene(
        &[(1, target, Vec3::new(2.0, 0.0, 0.0)), (2, other_a, Vec3::new(4.0, 0.0, 0.0)), (3, other_b, Vec3::new(6.0, 0.0, 0.0))],
        None,
    );
    for request in requests {
        let Request::MeshHeader(asset) = request.request else { continue };
        let bytes = if asset == target {
            target_bytes.clone()
        } else if asset == other_a {
            other_a_bytes.clone()
        } else {
            other_b_bytes.clone()
        };
        scene.complete_asset(request.ticket, prepared_asset(request.request, bytes));
    }
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    let mut high = None;
    for request in requests {
        let Request::MeshLod(lod) = request.request else { continue };
        let asset = request.request.resource().id;
        if asset == target && lod.lod() == assets::mesh::MeshLod::High {
            high = Some(request);
            continue;
        }
        if lod.lod() == assets::mesh::MeshLod::Medium {
            let bytes = if asset == target {
                target_bytes.clone()
            } else if asset == other_a {
                other_a_bytes.clone()
            } else {
                other_b_bytes.clone()
            };
            scene.complete_asset(request.ticket, prepared_asset(request.request, bytes));
        }
    }
    let high = high.expect("selected high LOD request remains pending");
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());
    assert_eq!(
        view.objects()
            .find(|object| object.detail().summary.id == Uuid::from_u128(100_001))
            .and_then(|object| object.mesh().map(|(key, _)| key)),
        Some(DrawnMeshKey::Asset { asset: target, lod: assets::mesh::MeshLod::Medium })
    );

    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    for _ in 0..512 {
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        if geometry.cache.rebuild.is_none()
            && geometry.cache.version.as_ref() == Some(&view.version())
            && geometry.draw_list.valid()
            && geometry.meshes.len() == 3
            && geometry.stats.pending_meshes == 0
        {
            break;
        }
    }
    assert_eq!(geometry.stats.pending_meshes, 0);
    assert!(
        geometry.stats.mesh_bytes > 48 * 1024 * 1024,
        "resident bytes {} across {} meshes; version {:?}, expected {:?}, rebuilding {}, rebuild objects {}, mesh info entries {}, mesh info bytes {}, demands {} visible/{} preparation, ready {}, active {}, visits {}, uploaded {}",
        geometry.stats.mesh_bytes,
        geometry.meshes.len(),
        geometry.cache.version,
        view.version(),
        geometry.cache.rebuild.is_some(),
        geometry.stats.rebuild_objects,
        geometry.stats.rebuild_mesh_info_entries,
        geometry.stats.rebuild_mesh_info_bytes,
        geometry.cache.mesh_order.len(),
        geometry.cache.preparation_mesh_order.len(),
        geometry.cache.mesh_ready.len(),
        geometry.mesh_admission.active,
        geometry.mesh_admission.visits_this_frame,
        geometry.stats.uploaded_bytes
    );
    assert!(geometry.stats.mesh_bytes < 49 * 1024 * 1024);
    assert_eq!(geometry.stats.instances, 3);
    let target_entity = view
        .objects()
        .find(|object| object.detail().summary.id == Uuid::from_u128(100_001))
        .expect("replacement target")
        .detail()
        .summary
        .entity;
    let other_a_entity = view
        .objects()
        .find(|object| object.detail().summary.id == Uuid::from_u128(100_002))
        .expect("capacity freeing object")
        .detail()
        .summary
        .entity;
    let fallback_id = geometry.cache.items.get(geometry.cache.objects[&target_entity]).expect("fallback item").mesh.id();
    let other_a_id = geometry.cache.items.get(geometry.cache.objects[&other_a_entity]).expect("capacity freeing item").mesh.id();

    scene.complete_asset(high.ticket, prepared_asset(high.request, target_bytes));
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());
    let replacement_id = view.object(target_entity).and_then(|object| object.mesh().map(|(_, mesh)| mesh.id())).expect("high LOD geometry");
    prepare_cache_until_current(&device, &queue, &mut geometry, &view, camera);
    let held = geometry.cache.items.get(geometry.cache.objects[&target_entity]).expect("fallback item");
    assert_eq!(held.mesh.id(), fallback_id);
    assert_eq!(held.replacement.as_ref().map(|replacement| replacement.item.mesh.id()), Some(replacement_id));
    assert_eq!(geometry.stats.mesh_budget_blocked, 1);
    assert_eq!(geometry.stats.pending_meshes, 1);
    assert_eq!(geometry.stats.instances, 3);
    assert_eq!(geometry.cache.visible.len(), 3);
    assert!(geometry.meshes.contains_key(&fallback_id), "the displayed fallback stays resident under pressure");
    assert!(geometry.memory.mesh_state().bytes <= crate::geometry::MESH_BYTES);
    assert!(!geometry.needs_frame(Instant::now()), "a blocked replacement does not schedule repeated frames");

    for _ in 0..3 {
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        let held = geometry.cache.items.get(geometry.cache.objects[&target_entity]).expect("fallback item");
        assert_eq!(held.mesh.id(), fallback_id);
        assert_eq!(held.replacement.as_ref().map(|replacement| replacement.item.mesh.id()), Some(replacement_id));
        assert_eq!(geometry.stats.mesh_budget_blocked, 1);
        assert_eq!(geometry.stats.pending_meshes, 1);
        assert_eq!(geometry.stats.instances, 3);
        assert_eq!(geometry.cache.visible.len(), 3);
        assert!(geometry.meshes.contains_key(&fallback_id), "the displayed fallback stays resident under pressure");
        assert!(geometry.memory.mesh_state().bytes <= crate::geometry::MESH_BYTES);
        assert!(!geometry.needs_frame(Instant::now()), "the blocked upload stays dormant until mesh capacity changes");
    }

    let _applied = world
        .remove_object(
            UpdateSource::new(fixture_region(&world), 30, OrderEvidence::Ordered(30), 0),
            std::num::NonZeroU32::new(2).expect("capacity freeing local id"),
        )
        .expect("capacity freeing object removal applies");
    scene.notify_world_change();
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());

    for _ in 0..512 {
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        if geometry.cache.version.as_ref() == Some(&view.version())
            && geometry.cache.items.get(geometry.cache.objects[&target_entity]).is_some_and(|item| item.replacement.is_none())
        {
            break;
        }
    }
    let promoted = geometry.cache.items.get(geometry.cache.objects[&target_entity]).expect("promoted target");
    assert_eq!(promoted.mesh.id(), replacement_id);
    assert!(promoted.replacement.is_none(), "the resident replacement eventually becomes the displayed mesh");
    assert!(!geometry.cache.objects.contains_key(&other_a_entity));
    assert!(!geometry.meshes.contains_key(&other_a_id), "the removed object's allocation frees mesh capacity");
    assert_eq!(geometry.stats.mesh_budget_blocked, 0);
    assert_eq!(geometry.stats.pending_meshes, 0);
    assert_eq!(geometry.stats.instances, 2);
    assert!(geometry.memory.mesh_state().bytes <= crate::geometry::MESH_BYTES);

    assert!(
        !geometry.cache.mesh_order.iter().any(|demand| demand.mesh == fallback_id),
        "promoted fallback no longer reserves upload demand"
    );

    let memory = geometry.memory.clone();
    drop(geometry);
    device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
    assert_eq!(memory.mesh_state().bytes, 0, "renderer retirement releases every mesh page");
}

fn assert_visible_assets_match_source(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    geometry: &GeometryRenderer,
    view: &SceneView,
    camera: Camera,
    depth: &wgpu::TextureView,
    visible: &std::collections::HashSet<world::Entity>,
    phase: &str,
) {
    let mut references = Vec::new();
    for object in view.objects().filter(|object| visible.contains(&object.detail().summary.entity)) {
        let (_, shared) = object.mesh().expect("visible mesh asset is ready");
        let mesh = shared.mesh();
        let mut vertices = Vec::with_capacity(mesh.vertices().len() * VERTEX_BYTES);
        for vertex in mesh.vertices() {
            floats(&mut vertices, vertex.position.to_array());
            floats(&mut vertices, vertex.normal.to_array());
            floats(&mut vertices, vertex.uv.to_array());
        }
        let indices: Vec<_> = mesh.indices().iter().flat_map(|index| index.to_le_bytes()).collect();
        let Placement::Ready(transform) = object.placement() else { panic!("mesh fixture object must be placed") };
        let material = object.materials().expect("mesh fixture materials").first().expect("mesh fixture face material");
        let params = FaceParams::new(Some(material));
        let instance = packed_instance(
            &instance_base(transform.pose.position, transform.pose.rotation, transform.scale, false, camera.position()),
            &params,
            transform.scale,
        );
        references.push((
            device.create_buffer_init(&BufferInitDescriptor { label: None, contents: &vertices, usage: BufferUsages::VERTEX }),
            device.create_buffer_init(&BufferInitDescriptor { label: None, contents: &indices, usage: BufferUsages::INDEX }),
            device.create_buffer_init(&BufferInitDescriptor { label: None, contents: &instance, usage: BufferUsages::VERTEX }),
            mesh.indices().len() as u32,
        ));
    }
    assert_eq!(references.len(), visible.len(), "{phase}: every visible asset has one independent reference draw");
    let actual = render_pixels(device, queue, [128, 128], depth, |pass| geometry.draw(pass));
    let expected = render_pixels(device, queue, [128, 128], depth, |pass| {
        geometry.pipelines.bind_pipeline(pass, Pipeline::Surface);
        pass.set_bind_group(1, geometry.textures.binding(None), &[]);
        for (vertices, indices, instance, count) in &references {
            pass.set_vertex_buffer(0, vertices.slice(..));
            pass.set_vertex_buffer(1, instance.slice(..));
            pass.set_index_buffer(indices.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..*count, 0, 0..1);
        }
    });
    let lit = actual.iter().filter(|pixel| pixel[0] > 16 || pixel[1] > 16 || pixel[2] > 16).count();
    assert!(lit > 100, "{phase}: source triangles render, {lit} pixels");
    assert_eq!(actual, expected, "{phase}: shared-page draws match complete decoded meshes");
}

fn complete_mesh_requests(
    world: &world::World,
    scene: &mut Reconciler,
    view: &mut SceneView,
    assets: &std::collections::HashMap<AssetId, Vec<u8>>,
    mut requests: Vec<AssetRequest>,
) {
    for _ in 0..8 {
        if requests.is_empty() {
            return;
        }
        for request in requests.drain(..) {
            if matches!(request.request, Request::MeshHeader(_) | Request::MeshLod(_)) {
                let bytes = assets.get(&request.request.resource().id).expect("fixture asset bytes").clone();
                scene.complete_asset(request.ticket, prepared_asset(request.request, bytes));
            }
        }
        requests = settle_scene(world, scene, view, Instant::now());
    }
    assert!(requests.is_empty(), "mesh fixture requests settle within the bounded chain");
}

fn complete_mesh_requests_collect_textures(
    world: &world::World,
    scene: &mut Reconciler,
    view: &mut SceneView,
    assets: &std::collections::HashMap<AssetId, Vec<u8>>,
    mut requests: Vec<AssetRequest>,
) -> Vec<AssetRequest> {
    let mut textures = Vec::new();
    for _ in 0..8 {
        if requests.is_empty() {
            return textures;
        }
        for request in requests.drain(..) {
            if matches!(request.request, Request::MeshHeader(_) | Request::MeshLod(_)) {
                let bytes = assets.get(&request.request.resource().id).expect("fixture mesh bytes").clone();
                scene.complete_asset(request.ticket, prepared_asset(request.request, bytes));
            } else if matches!(request.request, Request::Texture { .. }) {
                textures.push(request);
            }
        }
        requests = settle_scene(world, scene, view, Instant::now());
    }
    assert!(requests.is_empty(), "mesh fixture requests settle within the bounded chain");
    textures
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo test -p render gpu_asset_reuse_preserves_triangles -- --ignored"]
fn gpu_asset_reuse_preserves_triangles() {
    let (device, queue) = vulkan_device();
    let a = AssetId::new(Uuid::from_u128(840));
    let b = AssetId::new(Uuid::from_u128(841));
    let c = AssetId::new(Uuid::from_u128(842));
    let point = |degrees: f32| {
        let radians = degrees.to_radians();
        Vec3::new(3.0 * radians.cos(), 3.0 * radians.sin(), 0.0)
    };
    let assets = std::collections::HashMap::from([
        (a, test_support::mesh::asset_with_lod_bounds(3, [-0.01, -0.8, -0.5], [0.01, 0.1, 0.5])),
        (b, test_support::mesh::asset_with_lod_bounds(3, [-0.01, 0.0, -0.5], [0.01, 0.8, 0.5])),
        (c, test_support::mesh::asset_with_lod_bounds(3, [-0.01, -0.5, -0.5], [0.01, 0.5, 0.5])),
    ]);
    let (world, mut scene, mut view, _, mut requests) = mesh_scene(&[(1, a, point(-30.0)), (2, b, point(0.0)), (3, c, point(30.0))], None);
    let first_camera =
        Camera::new(DVec3::ZERO, Vec3::new(15_f32.to_radians().cos(), -15_f32.to_radians().sin(), 0.0), 40_f64.to_radians(), [128, 128])
            .expect("first fixture camera");
    let second_camera =
        Camera::new(DVec3::ZERO, Vec3::new(15_f32.to_radians().cos(), 15_f32.to_radians().sin(), 0.0), 40_f64.to_radians(), [128, 128])
            .expect("second fixture camera");
    scene.set_camera(first_camera);
    requests.extend(settle_scene(&world, &mut scene, &mut view, Instant::now()));
    complete_mesh_requests(&world, &mut scene, &mut view, &assets, requests);
    let a_entity =
        view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(100_001)).expect("asset A").detail().summary.entity;
    let b_entity =
        view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(100_002)).expect("asset B").detail().summary.entity;
    let c_entity =
        view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(100_003)).expect("asset C").detail().summary.entity;
    assert!(view.object(a_entity).and_then(|object| object.mesh()).is_some());
    assert!(view.object(b_entity).and_then(|object| object.mesh()).is_some());

    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    let depth = geometry.depth(&device, [128, 128]).expect("depth target");
    prepare_frame(&device, &queue, &mut geometry, &view, first_camera);
    let first_visible = std::collections::HashSet::from([a_entity, b_entity]);
    let actual_visible: std::collections::HashSet<_> =
        geometry.cache.visible.iter().filter_map(|index| geometry.cache.items.get(*index).and_then(|item| item.entity)).collect();
    assert_eq!(actual_visible, first_visible, "the initial camera admits the two intended assets");
    let a_id = geometry.cache.items.get(geometry.cache.objects[&a_entity]).expect("asset A item").mesh.id();
    let b_id = geometry.cache.items.get(geometry.cache.objects[&b_entity]).expect("asset B item").mesh.id();
    assert!(Arc::ptr_eq(geometry.meshes[&a_id].allocation.buffers(), geometry.meshes[&b_id].allocation.buffers()));
    assert_visible_assets_match_source(&device, &queue, &geometry, &view, first_camera, &depth, &first_visible, "initial camera");

    let fillers = fill_mesh_pages(&device, &mut geometry);
    assert_eq!(fillers.len(), 1, "the two source assets share one saturated page");
    let blocker_bytes = crate::geometry::MESH_BYTES - geometry.memory.mesh_state().bytes;
    let blocker = geometry.memory.reserve(blocker_bytes, crate::geometry::Resource::Mesh).expect("hold page-growth budget");

    scene.set_camera(second_camera);
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    complete_mesh_requests(&world, &mut scene, &mut view, &assets, requests);
    assert!(view.object(c_entity).and_then(|object| object.mesh()).is_some(), "the revealed asset has decoded geometry");
    prepare_cache_until_current(&device, &queue, &mut geometry, &view, second_camera);
    let second_visible = std::collections::HashSet::from([b_entity, c_entity]);
    let c_index = geometry.cache.objects[&c_entity];
    assert!(geometry.cache.candidates.contains(&c_index), "the revealed asset bounds enter the camera frustum");
    let actual_visible: std::collections::HashSet<_> =
        geometry.cache.visible.iter().filter_map(|index| geometry.cache.items.get(*index).and_then(|item| item.entity)).collect();
    assert_eq!(
        actual_visible,
        std::collections::HashSet::from([b_entity]),
        "the revealed asset stays whole while its page range is retired"
    );
    let c_id = view.object(c_entity).and_then(|object| object.mesh().map(|(_, mesh)| mesh.id())).expect("asset C mesh");
    assert!(!geometry.meshes.contains_key(&a_id), "the hidden allocation is retired to admit the revealed mesh");
    assert!(geometry.meshes.contains_key(&b_id));
    assert!(!geometry.meshes.contains_key(&c_id), "the first pressure frame waits for safe range reuse");
    assert_eq!(geometry.stats.pending_meshes, 1);
    assert_eq!(geometry.stats.instances, 1);
    assert!(geometry.needs_frame(Instant::now()), "retirement schedules a follow-up upload");
    assert_visible_assets_match_source(
        &device,
        &queue,
        &geometry,
        &view,
        second_camera,
        &depth,
        &std::collections::HashSet::from([b_entity]),
        "revealed asset pending",
    );
    prepare_until_current(&device, &queue, &mut geometry, &view, second_camera);
    assert!(geometry.meshes.contains_key(&b_id));
    assert!(geometry.meshes.contains_key(&c_id));
    assert!(Arc::ptr_eq(geometry.meshes[&b_id].allocation.buffers(), geometry.meshes[&c_id].allocation.buffers()));
    assert_eq!(geometry.stats.pending_meshes, 0);
    assert_eq!(geometry.stats.instances, 2);
    let actual_visible: std::collections::HashSet<_> =
        geometry.cache.visible.iter().filter_map(|index| geometry.cache.items.get(*index).and_then(|item| item.entity)).collect();
    assert_eq!(actual_visible, second_visible, "camera movement reveals the adjacent asset after whole upload");
    assert_visible_assets_match_source(&device, &queue, &geometry, &view, second_camera, &depth, &second_visible, "first reuse");

    scene.set_camera(first_camera);
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    complete_mesh_requests(&world, &mut scene, &mut view, &assets, requests);
    let deadline = Instant::now() + std::time::Duration::from_secs(60);
    while geometry.meshes.contains_key(&c_id)
        || geometry.stats.pending_meshes != 1
        || geometry
            .cache
            .visible
            .iter()
            .filter_map(|index| geometry.cache.items.get(*index).and_then(|item| item.entity))
            .collect::<std::collections::HashSet<_>>()
            != std::collections::HashSet::from([b_entity])
    {
        prepare_frame(&device, &queue, &mut geometry, &view, first_camera);
        assert_draws_resident(&geometry);
        assert!(Instant::now() < deadline, "reverse camera demand did not retire the hidden mesh");
    }
    assert!(!geometry.meshes.contains_key(&c_id), "the second hidden allocation is retired on the reverse camera move");
    assert!(geometry.meshes.contains_key(&b_id));
    assert_eq!(geometry.stats.pending_meshes, 1);
    assert_eq!(geometry.stats.instances, 1);
    assert!(geometry.needs_frame(Instant::now()));
    assert_visible_assets_match_source(
        &device,
        &queue,
        &geometry,
        &view,
        first_camera,
        &depth,
        &std::collections::HashSet::from([b_entity]),
        "reverse asset pending",
    );
    prepare_until_current(&device, &queue, &mut geometry, &view, first_camera);
    assert!(geometry.meshes.contains_key(&a_id));
    assert!(geometry.meshes.contains_key(&b_id));
    assert!(Arc::ptr_eq(geometry.meshes[&a_id].allocation.buffers(), geometry.meshes[&b_id].allocation.buffers()));
    assert_eq!(geometry.stats.pending_meshes, 0);
    assert_eq!(geometry.stats.instances, 2);
    assert_visible_assets_match_source(&device, &queue, &geometry, &view, first_camera, &depth, &first_visible, "reverse reuse");

    drop(blocker);
    drop(fillers);
    let memory = geometry.memory.clone();
    drop(geometry);
    device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
    assert_eq!(memory.mesh_state().bytes, 0);
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo test -p render --lib gpu_invisible_face_allows_mesh -- --ignored"]
fn gpu_invisible_face_allows_mesh() {
    let asset = AssetId::new(Uuid::from_u128(864));
    let visible_texture = AssetId::new(Uuid::from_u128(865));
    let hidden_texture = AssetId::new(Uuid::from_u128(866));
    let bytes = test_support::mesh::asset_with_lod_faces(&[(2, 2), (3, 2)]);
    let (world, mut scene, mut view, _, mut requests) =
        mesh_scene_with_alpha(&[(1, asset, Vec3::new(5.0, 0.0, 0.0))], Some((visible_texture, hidden_texture)), 0);
    let camera = Camera::new(DVec3::ZERO, Vec3::X, 25_f64.to_radians(), [128, 128]).expect("fixture camera");
    scene.set_camera(camera);
    requests.extend(settle_scene(&world, &mut scene, &mut view, Instant::now()));
    let visible_texture_requests: Vec<_> = requests
        .iter()
        .filter(|request| request.request.resource().id == visible_texture && request.request.resource().kind == assets::Kind::Texture)
        .copied()
        .collect();
    assert!(!visible_texture_requests.is_empty(), "the opaque face requests its texture");
    let mesh_assets = std::collections::HashMap::from([(asset, bytes)]);
    complete_mesh_requests(&world, &mut scene, &mut view, &mesh_assets, requests);
    for request in visible_texture_requests {
        scene.complete_asset(request.ticket, prepared_asset(request.request, test_support::texture::RGB_J2C.to_vec()));
    }
    let _requests = settle_scene(&world, &mut scene, &mut view, Instant::now());

    let object = view.objects().next().expect("mesh object");
    let entity = object.detail().summary.entity;
    let materials = object.materials().expect("mesh face materials");
    let opaque = materials.iter().find(|face| face.slot == 0).expect("opaque face material");
    let invisible = materials.iter().find(|face| face.slot == 1).expect("invisible face material");
    assert_eq!(opaque.alpha, crate::material::AlphaBehavior::Opaque);
    assert_eq!(invisible.alpha, crate::material::AlphaBehavior::Blend);
    assert_eq!(invisible.base_color_factor()[3], 0.0);
    assert!(matches!(
        invisible.base_color_texture.as_ref().expect("invisible face texture").state().resolution,
        crate::material::Resolution::Pending
    ));
    let (_, shared) = object.mesh().expect("uploaded mesh geometry");
    assert_eq!(shared.mesh().faces().len(), 2, "both authored face slots remain in the selected LOD");

    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    let depth = geometry.depth(&device, [128, 128]).expect("depth target");
    let started = Instant::now();
    prepare_frame_at(&device, &queue, &mut geometry, &view, camera, started);
    assert!(geometry.meshes.contains_key(&shared.id()), "the two-face LOD has been uploaded to the GPU");
    assert_eq!(geometry.stats.instances, 0, "the opaque texture must reach GPU residency before it can appear");
    let job = geometry.next_texture_job().expect("opaque face texture upload job");
    geometry.complete_texture(job.run(crate::texture::CompressionQuality::Fast));
    let early = started + std::time::Duration::from_millis(10);
    for _ in 0..3 {
        prepare_frame_at(&device, &queue, &mut geometry, &view, camera, early);
        if geometry.stats.instances == 1 {
            break;
        }
    }
    assert_eq!(geometry.stats.instances, 1, "the opaque face appears once its own texture is resident, before 250 ms");
    let opaque_image = opaque.base_color_texture.as_ref().and_then(|binding| binding.image()).expect("resident opaque texture");
    assert!(geometry.textures.is_resident(opaque_image.id()));
    assert!(
        invisible.base_color_texture.as_ref().is_some_and(|binding| binding.image().is_none()),
        "the invisible face texture remains pending"
    );

    let item = geometry.cache.items.get(geometry.cache.objects[&entity]).expect("visible mesh item");
    assert_eq!(item.faces.iter().map(|face| face.order.3).collect::<Vec<_>>(), [0]);
    assert_eq!(geometry.draw_list.draws().len(), 1, "only the opaque face has a draw command");
    assert_eq!(geometry.draw_list.draws()[0].indices, shared.mesh().faces()[0].indices);
    assert!(geometry.cache.visible.contains(&geometry.cache.objects[&entity]), "the whole mesh object became visible");

    let local_center = |face_index: usize| {
        let mesh = shared.mesh();
        let face = mesh.faces().get(face_index).expect("fixture face slot");
        let start = usize::try_from(face.indices.start).expect("fixture face index start");
        let end = usize::try_from(face.indices.end).expect("fixture face index end");
        let indices = mesh.indices().get(start..end).expect("fixture face index range");
        let mut center = Vec3::ZERO;
        for index in indices {
            center += mesh.vertices().get(*index as usize).expect("fixture face vertex").position;
        }
        center / indices.len() as f32
    };
    let ray_to_face = |center: Vec3| {
        let target = item.position + (item.rotation * (item.scale * center)).as_dvec3();
        (target - camera.position()).normalize()
    };
    assert!(geometry.pick(camera, ray_to_face(local_center(0))).unwrap().is_some(), "the opaque face remains pickable");
    assert_eq!(geometry.pick(camera, ray_to_face(local_center(1))).unwrap(), None, "the invisible face cannot be picked");

    let pixels = render_pixels(&device, &queue, [128, 128], &depth, |pass| geometry.draw(pass));
    let red_pixels = pixels.iter().filter(|pixel| pixel[0] > pixel[1].saturating_add(24)).count();
    assert!(red_pixels > 50, "the opaque face is rendered while the hidden texture is still pending: {red_pixels} pixels");

    let late = started + crate::geometry::first_appearance::ASSET_WAIT + std::time::Duration::from_millis(1);
    prepare_frame_at(&device, &queue, &mut geometry, &view, camera, late);
    assert_eq!(geometry.stats.instances, 1, "the visible object remains available after the optional asset deadline");
}

#[test]
#[ignore = "requires a Vulkan adapter"]
fn gpu_ready_texture_wakes_appearance() {
    let asset = AssetId::new(Uuid::from_u128(862));
    let texture = AssetId::new(Uuid::from_u128(863));
    let data = test_support::mesh::asset_with_lod_bounds(3, [-0.5; 3], [0.5; 3]);
    let (world, mut scene, mut view, camera, requests) = mesh_scene(&[(1, asset, Vec3::new(2.0, 0.0, 0.0))], Some((texture, texture)));
    let texture_requests: Vec<_> =
        requests.iter().filter(|request| request.request.resource().kind == assets::Kind::Texture).copied().collect();
    assert!(!texture_requests.is_empty());
    complete_mesh_requests(&world, &mut scene, &mut view, &std::collections::HashMap::from([(asset, data)]), requests);
    for request in texture_requests {
        scene.complete_asset(request.ticket, prepared_asset(request.request, test_support::texture::GRAY_4096_J2C.to_vec()));
    }
    let _requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    let entity = view.objects().next().expect("gated object").detail().summary.entity;
    geometry.request_frame_textures(&view, &[entity], &[entity]);
    let job = geometry.next_texture_job().expect("gated visible demand must override the shared background demand");
    let now = Instant::now();
    prepare_frame_at(&device, &queue, &mut geometry, &view, camera, now);
    assert_eq!(geometry.stats.instances, 0);
    geometry.complete_texture(job.run(crate::texture::CompressionQuality::Fast));
    let moved = Camera::new(camera.position(), Vec3::new(1.0, 0.01, 0.0), camera.vertical_fov(), [128, 128]).unwrap();
    let early = now + std::time::Duration::from_millis(1);
    let frame = geometry.admit().unwrap();
    let mut encoder = device.create_command_encoder(&Default::default());
    geometry.prepare_at(&device, &mut encoder, &view, None, moved, [128, 128], crate::FrameInput::default(), early);
    assert_eq!(geometry.stats.instances, 0, "residency is established after this frame's visibility decision");
    assert!(geometry.needs_frame(early), "new texture residency schedules eligibility before the timeout");
    queue.submit([encoder.finish()]);
    geometry.submitted(frame);
    device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
    prepare_frame_at(&device, &queue, &mut geometry, &view, moved, early);
    assert_eq!(geometry.stats.instances, 1, "ready assets release the gate before 250 ms");
}

#[test]
#[ignore = "requires a Vulkan adapter"]
fn gpu_first_appearance_wait_is_bounded() {
    let asset = AssetId::new(Uuid::from_u128(860));
    let texture = AssetId::new(Uuid::from_u128(861));
    let data = test_support::mesh::asset_with_lod_bounds(3, [-0.5; 3], [0.5; 3]);
    let (mut world, mut scene, mut view, camera, requests) = mesh_scene(&[(1, asset, Vec3::new(2.0, 0.0, 0.0))], Some((texture, texture)));
    complete_mesh_requests(&world, &mut scene, &mut view, &std::collections::HashMap::from([(asset, data)]), requests);
    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    let now = Instant::now();
    let wait = crate::geometry::first_appearance::ASSET_WAIT;
    let away = Camera::new(camera.position(), -camera.direction(), camera.vertical_fov(), [128, 128]).unwrap();
    for (elapsed, current, expected) in [
        (std::time::Duration::ZERO, camera, 0),
        (wait / 2, away, 0),
        (wait - std::time::Duration::from_nanos(1), camera, 0),
        (wait, camera, 1),
    ] {
        let frame = geometry.admit().unwrap();
        let mut encoder = device.create_command_encoder(&Default::default());
        geometry.prepare_at(&device, &mut encoder, &view, None, current, [128, 128], crate::FrameInput::default(), now + elapsed);
        queue.submit([encoder.finish()]);
        geometry.submitted(frame);
        device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
        assert_eq!(
            geometry.stats.instances,
            expected,
            "appearance at {elapsed:?}; layout pending={}, required={}, progress items={}, faces={}, rebuild_pending={}",
            geometry.draw_list.layout_pending(),
            geometry.layout_required,
            geometry.stats.layout_items_visited,
            geometry.stats.layout_faces_visited,
            geometry.stats.rebuild_pending
        );
    }
    assert_eq!(geometry.meshes.len(), 1, "complete geometry remains resident throughout the optional texture wait");
    assert_eq!(geometry.redraw_deadline(), None, "fallback appearance settles without an idle redraw loop");
    let entity = view.objects().next().unwrap().detail().summary.entity;
    let key = crate::geometry::first_appearance::AppearanceKey { entity, asset };
    assert!(geometry.first_appearances.shown(key));
    let _removed = world
        .remove_object(UpdateSource::new(fixture_region(&world), 2, OrderEvidence::Ordered(2), 0), std::num::NonZeroU32::new(1).unwrap())
        .unwrap();
    scene.notify_world_change();
    let _requests = settle_scene(&world, &mut scene, &mut view, now + wait);
    prepare_frame(&device, &queue, &mut geometry, &view, camera);
    assert!(!geometry.first_appearances.shown(key), "production scene cleanup forgets removed appearance history");
}

#[test]
#[ignore = "requires a Vulkan adapter"]
fn gpu_camera_rebase_keeps_committed_draw() {
    const OBJECTS: u32 = 1_025;
    let mesh_asset = AssetId::new(Uuid::from_u128(9_811));
    let neighbor_mesh_asset = AssetId::new(Uuid::from_u128(9_812));
    let mut world = world::World::new(world::WorldLimits {
        max_regions: 2,
        max_entities: 2_048,
        max_entities_per_region: 2_048,
        max_tombstones_per_region: 1,
        max_tombstones: 2,
        max_tombstone_bytes_per_region: world::WorldLimits::tombstone_bytes_for_count(1),
        max_tombstone_bytes: world::WorldLimits::tombstone_bytes_for_count(2),
        max_journal_entries: 32,
        max_journal_changes: 128,
        max_region_name_bytes: 64,
        max_recovery_targets: 2_048,
    });
    let region = world.register_region().expect("first terrain region registers");
    let neighbor = world.register_region().expect("adjacent terrain region registers");
    let _ =
        world.patch_region(region, world::RegionInfo { origin: Some(DVec3::ZERO), ..Default::default() }).expect("first origin applies");
    let _ = world
        .patch_region(neighbor, world::RegionInfo { origin: Some(DVec3::new(0.0, 256.0, 0.0)), ..Default::default() })
        .expect("neighbor origin applies");
    let first_textures = std::array::from_fn(|index| Uuid::from_u128(98_110 + index as u128));
    let neighbor_textures = std::array::from_fn(|index| Uuid::from_u128(98_120 + index as u128));
    let first_texture_assets = first_textures.map(AssetId::new);
    let neighbor_texture_assets = neighbor_textures.map(AssetId::new);
    for (region, textures, starts, ranges, sequence) in
        [(region, first_textures, [0.0; 4], [2.0; 4], 1), (neighbor, neighbor_textures, [1.5; 4], [1.0; 4], 1)]
    {
        let settings = world::TerrainSettings::new(textures, starts, ranges).expect("regional terrain settings");
        let _ = world
            .observe_terrain_settings(UpdateSource::new(region, sequence, OrderEvidence::Ordered(u64::from(sequence)), 0), settings)
            .expect("regional terrain settings apply");
    }
    for id in 1..=OBJECTS {
        let mut observation = ObjectObservation::new(
            UpdateSource::new(region, id, OrderEvidence::Ordered(u64::from(id)), 0),
            Uuid::from_u128(100_000 + u128::from(id)),
            std::num::NonZeroU32::new(id).expect("first region local ID"),
            Kind::Object,
            ParentUpdate::Root,
        );
        observation.pose = Some(Pose { position: Some(Vec3::new(250.0, 255.0, 0.0)), rotation: Some(math::Quat::IDENTITY) });
        observation.scale = Some(Vec3::ONE);
        observation.shape = FieldUpdate::Set(Shape::Mesh { asset_id: mesh_asset, flags: SculptFlags::default() });
        let _ = world.observe_object(observation).expect("first region object applies");
    }
    for (region, patch_x, patch_y, height, sequence) in [(region, 15, 15, 0.0, 2_000), (neighbor, 15, 0, 1.5, 2), (neighbor, 14, 0, 1.5, 3)]
    {
        let patch =
            world::TerrainPatch::new(patch_x, patch_y, Box::new([height; world::TERRAIN_PATCH_SAMPLES])).expect("regional terrain patch");
        let _ = world
            .observe_terrain_patch(UpdateSource::new(region, sequence, OrderEvidence::Ordered(u64::from(sequence)), 0), patch)
            .expect("regional terrain patch applies");
    }
    let before = Camera::new(DVec3::new(256.5, 256.0, 20.0), Vec3::new(-1.0, 0.0, -1.0), 1.2, [128, 128])
        .expect("camera before origin rebase")
        .with_quality(512.0, 4.0)
        .expect("fixture draw distance");
    let after = Camera::new(DVec3::new(255.5, 256.0, 20.0), Vec3::new(-1.0, 0.0, -1.0), 1.2, [128, 128])
        .expect("camera after origin rebase")
        .with_quality(512.0, 4.0)
        .expect("fixture draw distance");
    let (mut scene, mut view) = Reconciler::new(crate::scene::Budget::default());
    scene.set_camera(before);
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    let mut texture_requests =
        requests.iter().filter(|request| matches!(request.request, Request::Texture { .. })).copied().collect::<Vec<_>>();
    for request in texture_requests.iter().filter(|request| first_texture_assets.contains(&request.request.resource().id)) {
        scene.complete_asset(request.ticket, prepared_asset(request.request, test_support::texture::RGB_J2C.to_vec()));
    }
    let asset_bytes = test_support::mesh::asset_with_lod_geometry(&[(3, 1, 3, 3)]);
    texture_requests.extend(complete_mesh_requests_collect_textures(
        &world,
        &mut scene,
        &mut view,
        &std::collections::HashMap::from([(mesh_asset, asset_bytes)]),
        requests,
    ));
    for request in texture_requests.iter().filter(|request| first_texture_assets.contains(&request.request.resource().id)) {
        scene.complete_asset(request.ticket, prepared_asset(request.request, test_support::texture::RGB_J2C.to_vec()));
    }
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());
    assert_eq!(
        texture_requests
            .iter()
            .filter(|request| first_texture_assets.contains(&request.request.resource().id))
            .map(|request| request.request.resource().id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        4,
        "the first region requests all four distinct terrain textures"
    );
    assert_eq!(
        view.region(region)
            .expect("first region summary")
            .terrain_textures()
            .iter()
            .flatten()
            .filter(|texture| texture.image().is_some())
            .count(),
        4,
        "the first region retains all four completed terrain textures"
    );
    assert_eq!(
        view.region(region)
            .expect("first region summary")
            .terrain_textures()
            .iter()
            .flatten()
            .map(assets::TextureHandle::asset)
            .collect::<std::collections::HashSet<_>>(),
        std::collections::HashSet::from(first_texture_assets),
        "the first region keeps its own terrain texture bindings"
    );
    for texture in view.region(region).expect("first region summary").terrain_textures().iter().flatten() {
        texture.set_usage(assets::TextureUsage::MainAndRender);
    }
    assert_eq!(view.objects().count(), OBJECTS as usize);
    let surfaces = view.surfaces().collect::<Vec<_>>();
    assert_eq!(surfaces.len(), 3, "both adjacent regions publish terrain surfaces");
    assert!(surfaces.iter().any(|surface| surface.region == region && surface.position == DVec3::new(240.0, 240.0, 0.0)));
    assert!(surfaces.iter().any(|surface| surface.region == neighbor && surface.position == DVec3::new(240.0, 256.0, 0.0)));
    assert!(surfaces.iter().any(|surface| surface.region == neighbor && surface.position == DVec3::new(224.0, 256.0, 0.0)));
    let first_composition = surfaces
        .iter()
        .find(|surface| surface.region == region)
        .and_then(|surface| surface.terrain_composition.as_deref())
        .expect("first regional terrain composition");
    let neighbor_composition = surfaces
        .iter()
        .find(|surface| surface.region == neighbor)
        .and_then(|surface| surface.terrain_composition.as_deref())
        .expect("neighbor regional terrain composition");
    assert_ne!(first_composition, neighbor_composition, "each regional origin and terrain settings shape its own composition");

    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    prepare_until_current(&device, &queue, &mut geometry, &view, before);
    let upload_terrain_textures = |geometry: &mut GeometryRenderer, view: &SceneView, camera: Camera, regions: &[Region]| {
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        loop {
            prepare_frame(&device, &queue, geometry, view, camera);
            let mut converted = false;
            while let Some(job) = geometry.next_texture_job() {
                geometry.complete_texture(job.run(crate::texture::CompressionQuality::Fast));
                converted = true;
            }
            let complete = regions.iter().all(|region| {
                view.region(*region).is_some_and(|data| {
                    let textures = data.terrain_textures();
                    textures.iter().all(Option::is_some)
                        && textures.iter().flatten().all(|texture| texture.image().is_some() && geometry.textures.is_resident(texture.id()))
                })
            });
            if complete && !converted && !geometry.textures.uploading() {
                return;
            }
            let progress = regions
                .iter()
                .map(|region| {
                    let textures = view.region(*region).map(|data| {
                        data.terrain_textures()
                            .iter()
                            .flatten()
                            .map(|texture| (texture.asset(), texture.image().is_some(), geometry.textures.is_resident(texture.id())))
                            .collect::<Vec<_>>()
                    });
                    (*region, textures)
                })
                .collect::<Vec<_>>();
            assert!(
                Instant::now() < deadline,
                "regional terrain textures did not become resident: progress={progress:?}, uploading={}, pending={}, uploaded={}",
                geometry.textures.uploading(),
                geometry.stats.pending_textures,
                geometry.textures.uploaded
            );
        }
    };
    upload_terrain_textures(&mut geometry, &view, before, &[region]);
    assert!(geometry.draw_list.valid(), "the initial object and terrain layout is committed");
    assert!(geometry.cache.visible.len() >= OBJECTS as usize, "nearby region objects and terrain are part of the active drawable");
    let terrain_regions = geometry.draw_list.draws().iter().filter_map(|draw| draw.terrain).collect::<std::collections::HashSet<_>>();
    assert_eq!(terrain_regions, std::collections::HashSet::from([region, neighbor]), "terrain draws retain each region binding");
    geometry.depth(&device, [128, 128]).expect("fixture depth target");
    let assert_positions = |renderer: &GeometryRenderer, view: &SceneView| {
        let expected = view
            .objects()
            .map(|object| {
                let position = match object.placement() {
                    crate::scene::Placement::Ready(transform) => transform.pose.position,
                    placement => panic!("published fixture object has unresolved placement: {placement:?}"),
                };
                (object.detail().summary.entity, position)
            })
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(renderer.cache.objects.len(), expected.len(), "renderer object membership matches SceneView");
        for (entity, position) in expected {
            let index = renderer.cache.objects.get(&entity).copied().expect("SceneView object has a cached item");
            assert_eq!(renderer.cache.items.get(index).expect("cached object item").position, position);
        }
    };
    assert_positions(&geometry, &view);

    prepare_frame(&device, &queue, &mut geometry, &view, after);
    assert!(geometry.draw_list.layout_pending(), "the replacement layout exceeds one bounded layout turn");
    assert!(geometry.draw_list.valid(), "the previous drawable stays active during replacement");
    assert!(geometry.needs_frame(Instant::now()), "a pending replacement layout schedules its next useful frame");
    assert_positions(&geometry, &view);

    let mut clean = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    prepare_until_current(&device, &queue, &mut clean, &view, after);
    upload_terrain_textures(&mut clean, &view, after, &[region]);
    let clean_depth = clean.depth(&device, [128, 128]).expect("clean depth target");
    let expected = render_pixels(&device, &queue, [128, 128], &clean_depth, |pass| clean.draw(pass));
    let expected_terrain_pixels = expected.iter().filter(|pixel| **pixel != [0, 0, 0, 255]).count();
    assert!(expected_terrain_pixels > 100, "the clean near view contains visible terrain and objects");
    let assert_matches_clean = |renderer: &GeometryRenderer, expected: &[[u8; 4]], phase: &str| {
        let depth = renderer.depth.as_ref().expect("active depth target");
        let actual = render_pixels(&device, &queue, [128, 128], &depth.view, |pass| renderer.draw(pass));
        let first_mismatch = actual
            .iter()
            .zip(expected.iter())
            .enumerate()
            .find_map(|(index, (actual, expected))| (actual != expected).then_some((index, actual, expected)));
        let mismatch_count = actual.iter().zip(expected).filter(|(actual, expected)| actual != expected).count();
        let total_channel_error = actual
            .iter()
            .zip(expected)
            .flat_map(|(actual, expected)| {
                actual.iter().zip(expected.iter()).map(|(actual, expected)| u64::from(actual.abs_diff(*expected)))
            })
            .sum::<u64>();
        let large_mismatch_count = actual
            .iter()
            .zip(expected)
            .filter(|(actual, expected)| actual.iter().zip(expected.iter()).any(|(actual, expected)| actual.abs_diff(*expected) > 1))
            .count();
        assert!(
            total_channel_error <= 32 && large_mismatch_count == 0,
            "{phase}: rebase output differs from a clean build at {mismatch_count} pixels, total channel error={total_channel_error}, {large_mismatch_count} exceed one channel value, first mismatch={first_mismatch:?}"
        );
    };
    assert_matches_clean(&geometry, &expected, "first rebase frame");
    let before_origin = DVec3::new(256.0, 256.0, 0.0);
    assert_eq!(geometry.active_origin, before_origin, "the committed drawable keeps its old frame while replacement is pending");

    let next_origin = DVec3::new(0.0, 256.0, 0.0);
    let mut completed = false;
    let mut turns = Vec::new();
    for _ in 0..16 {
        prepare_frame(&device, &queue, &mut geometry, &view, after);
        assert_positions(&geometry, &view);
        assert_matches_clean(&geometry, &expected, "rebase continuation");
        turns.push((
            geometry.stats.layouts,
            geometry.stats.layout_items_visited,
            geometry.layout_required,
            geometry.draw_list.layout_pending(),
            geometry.draw_list.layout_building(),
            geometry.stats.rebuild_pending,
            geometry.cache.rebuild.is_some(),
        ));
        completed |= geometry.active_origin == next_origin;
        if geometry.draw_list.layout_building() {
            assert_eq!(geometry.active_origin, before_origin, "the active frame stays old while the replacement builds");
        }
        if geometry.active_origin == next_origin && !geometry.layout_required {
            break;
        }
    }
    assert!(
        completed,
        "the bounded replacement layout eventually commits: layouts={}, visited={}, pending={}, building={}, required={}, cache_origin={:?}, active_origin={:?}, turns={turns:?}",
        geometry.stats.layouts,
        geometry.stats.layout_items_visited,
        geometry.draw_list.layout_pending(),
        geometry.draw_list.layout_building(),
        geometry.layout_required,
        geometry.cache.origin,
        geometry.active_origin
    );
    assert!(!geometry.layout_required, "the replacement request is satisfied");
    assert_eq!(geometry.cache.origin, next_origin);
    assert_eq!(geometry.active_origin, geometry.cache.origin, "the frame advances with the committed replacement");

    let partial_pixels = {
        let depth = geometry.depth.as_ref().expect("active partial scene depth");
        render_pixels(&device, &queue, [128, 128], &depth.view, |pass| geometry.draw(pass))
    };
    assert_matches_clean(&geometry, &expected, "partially textured adjacent regions");
    let neighbor_requests =
        texture_requests.iter().filter(|request| neighbor_texture_assets.contains(&request.request.resource().id)).collect::<Vec<_>>();
    assert_eq!(
        neighbor_requests.iter().map(|request| request.request.resource().id).collect::<std::collections::HashSet<_>>().len(),
        4,
        "the neighboring region requests its own terrain textures"
    );
    for request in neighbor_requests {
        scene.complete_asset(request.ticket, prepared_asset(request.request, test_support::texture::GRAY_J2C.to_vec()));
    }
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());
    let neighbor_handles = view.region(neighbor).expect("neighbor summary").terrain_textures();
    assert_eq!(neighbor_handles.iter().flatten().filter(|texture| texture.image().is_some()).count(), 4);
    assert_eq!(
        neighbor_handles.iter().flatten().map(assets::TextureHandle::asset).collect::<std::collections::HashSet<_>>(),
        std::collections::HashSet::from(neighbor_texture_assets),
        "the neighbor keeps its distinct terrain texture bindings"
    );
    for texture in neighbor_handles.iter().flatten() {
        texture.set_usage(assets::TextureUsage::MainAndRender);
    }
    prepare_until_current(&device, &queue, &mut geometry, &view, after);
    upload_terrain_textures(&mut geometry, &view, after, &[region, neighbor]);
    let neighbor_pixels = {
        let depth = geometry.depth.as_ref().expect("active textured scene depth");
        render_pixels(&device, &queue, [128, 128], &depth.view, |pass| geometry.draw(pass))
    };
    let texture_arrival_pixels = neighbor_pixels.iter().zip(&partial_pixels).filter(|(after, before)| after != before).count();
    assert!(texture_arrival_pixels > 0, "neighbor terrain texture arrival changes rendered terrain pixels");

    let mut clean_terrain = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    prepare_until_current(&device, &queue, &mut clean_terrain, &view, after);
    upload_terrain_textures(&mut clean_terrain, &view, after, &[region, neighbor]);
    let clean_terrain_depth = clean_terrain.depth(&device, [128, 128]).expect("clean textured terrain depth");
    let expected_terrain = render_pixels(&device, &queue, [128, 128], &clean_terrain_depth, |pass| clean_terrain.draw(pass));
    assert_matches_clean(&geometry, &expected_terrain, "neighbor terrain texture arrival");

    let neighbor_height = view.terrain_height_at(DVec3::new(238.0, 260.0, 0.0)).expect("neighbor object terrain height") as f32;
    let neighbor_position = DVec3::new(238.0, 260.0, f64::from(neighbor_height + 3.0));
    let mut observation = ObjectObservation::new(
        UpdateSource::new(neighbor, 4, OrderEvidence::Ordered(4), 0),
        Uuid::from_u128(200_001),
        std::num::NonZeroU32::new(1).expect("neighbor local ID"),
        Kind::Object,
        ParentUpdate::Root,
    );
    observation.pose = Some(Pose {
        position: Some(Vec3::new(238.0, 4.0, neighbor_height + 3.0)),
        rotation: Some(math::Quat::from_rotation_z(std::f32::consts::PI)),
    });
    observation.scale = Some(Vec3::splat(2.0));
    observation.shape = FieldUpdate::Set(Shape::Mesh { asset_id: neighbor_mesh_asset, flags: SculptFlags::default() });
    observation.appearance = FieldUpdate::Set(Appearance::Surface);
    observation.surface_appearance = Some(
        SurfaceAppearance::new(world::FaceAppearance { color: [0, 255, 255, 255], ..Default::default() }, Vec::new())
            .expect("neighbor object material"),
    );
    let _ = world.observe_object(observation).expect("neighbor object arrives");
    scene.notify_world_change();
    let neighbor_object_requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    assert_eq!(view.objects().count(), OBJECTS as usize + 1, "the neighboring object publishes independently of its mesh");
    let neighbor_object =
        view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(200_001)).expect("neighbor object summary");
    let neighbor_entity = neighbor_object.detail().summary.entity;
    assert_eq!(neighbor_object.detail().summary.address.region(), neighbor);
    assert!(neighbor_object.mesh().is_none(), "the neighboring object waits for its mesh asset");
    assert_matches!(neighbor_object.placement(), crate::scene::Placement::Ready(transform) if transform.pose.position == neighbor_position);
    prepare_until_current(&device, &queue, &mut geometry, &view, after);
    let pending_object_pixels = {
        let depth = geometry.depth.as_ref().expect("neighbor object pending depth");
        render_pixels(&device, &queue, [128, 128], &depth.view, |pass| geometry.draw(pass))
    };
    assert_matches_clean(&geometry, &expected_terrain, "neighbor object mesh pending");

    let late_texture_requests = complete_mesh_requests_collect_textures(
        &world,
        &mut scene,
        &mut view,
        &std::collections::HashMap::from([(neighbor_mesh_asset, test_support::mesh::asset_with_lod_geometry(&[(3, 1, 3, 3)]))]),
        neighbor_object_requests,
    );
    assert!(late_texture_requests.is_empty(), "the untextured neighbor mesh adds no material texture requests");
    let published_neighbor = view.object(neighbor_entity).expect("neighbor object remains live");
    assert!(published_neighbor.mesh().is_some(), "the neighbor mesh arrives through the scene asset path");
    let neighbor_material = published_neighbor.materials().expect("neighbor object material").first().expect("neighbor face material");
    assert_eq!(neighbor_material.base_color_factor(), [0.0, 1.0, 1.0, 1.0]);
    prepare_until_current(&device, &queue, &mut geometry, &view, after);
    assert_positions(&geometry, &view);
    assert!(!geometry.layout_required && !geometry.draw_list.layout_pending(), "the neighbor object draw commits before pixel checks");
    let neighbor_item = geometry.cache.objects.get(&neighbor_entity).copied().expect("neighbor object cache membership");
    assert!(geometry.cache.visible.contains(&neighbor_item), "the neighbor object is an active visible candidate");
    assert_eq!(geometry.cache.items.get(neighbor_item).expect("neighbor cached item").position, neighbor_position);
    let object_pixels = {
        let depth = geometry.depth.as_ref().expect("neighbor object visible depth");
        render_pixels(&device, &queue, [128, 128], &depth.view, |pass| geometry.draw(pass))
    };
    let object_arrival_pixels = object_pixels.iter().zip(&pending_object_pixels).filter(|(after, before)| after != before).count();
    let cyan_pixels = object_pixels.iter().filter(|pixel| pixel[1] > pixel[0] + 30 && pixel[2] > pixel[0] + 30).count();
    let neighbor_item = geometry.cache.items.get(neighbor_item).expect("neighbor cached item");
    assert!(
        object_arrival_pixels > 0,
        "neighbor mesh and material arrival changes rendered pixels: delta={object_arrival_pixels}, cyan={cyan_pixels}, instances={}, faces={}, slots={:?}, position={:?}, scale={:?}, mesh_indices={}, bounds={:?}",
        geometry.stats.instances,
        neighbor_item.faces.len(),
        neighbor_item.faces.iter().map(|face| face.slot).collect::<Vec<_>>(),
        neighbor_item.position,
        neighbor_item.scale,
        neighbor_item.mesh.mesh().indices().len(),
        neighbor_item.info.bounds
    );
    assert!(cyan_pixels > 0, "the late neighbor material produces visible cyan pixels");

    let mut clean_full = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    prepare_until_current(&device, &queue, &mut clean_full, &view, after);
    upload_terrain_textures(&mut clean_full, &view, after, &[region, neighbor]);
    let clean_full_depth = clean_full.depth(&device, [128, 128]).expect("clean full-scene depth");
    let expected_full = render_pixels(&device, &queue, [128, 128], &clean_full_depth, |pass| clean_full.draw(pass));
    assert_matches_clean(&geometry, &expected_full, "neighbor object and material arrival");

    let mut clean_before = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    prepare_until_current(&device, &queue, &mut clean_before, &view, before);
    upload_terrain_textures(&mut clean_before, &view, before, &[region, neighbor]);
    let clean_before_depth = clean_before.depth(&device, [128, 128]).expect("clean far depth target");
    let expected_before = render_pixels(&device, &queue, [128, 128], &clean_before_depth, |pass| clean_before.draw(pass));
    prepare_frame(&device, &queue, &mut geometry, &view, before);
    assert_positions(&geometry, &view);
    assert!(geometry.draw_list.layout_pending(), "the reverse replacement also spans bounded layout turns");
    assert_eq!(geometry.active_origin, next_origin);
    assert_matches_clean(&geometry, &expected_before, "first reverse rebase frame");
    let mut reverse_completed = false;
    for _ in 0..16 {
        prepare_frame(&device, &queue, &mut geometry, &view, before);
        assert_positions(&geometry, &view);
        assert_matches_clean(&geometry, &expected_before, "reverse rebase continuation");
        reverse_completed |= geometry.active_origin == before_origin;
        if geometry.draw_list.layout_building() {
            assert_eq!(geometry.active_origin, next_origin, "the reverse active frame stays old while its replacement builds");
        }
        if geometry.active_origin == before_origin && !geometry.layout_required {
            break;
        }
    }
    assert!(reverse_completed, "the reverse bounded replacement layout eventually commits");
    assert!(!geometry.layout_required, "the reverse replacement request is satisfied");
    assert_eq!(geometry.cache.origin, before_origin);
    assert_eq!(geometry.active_origin, geometry.cache.origin);

    let memory = geometry.memory.clone();
    drop(clean_before);
    drop(clean_full);
    drop(clean_terrain);
    drop(clean);
    drop(geometry);
    device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
    assert_eq!(memory.mesh_state().bytes, 0);
}

#[test]
#[ignore = "requires a Vulkan adapter"]
fn gpu_blended_rotation_uploads_instances() {
    let (device, queue) = vulkan_device();
    let (_scene, view, _) = scene(128);
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.instances = device.create_buffer(&BufferDescriptor {
        label: None,
        size: crate::geometry::INSTANCE_BUFFER_BYTES as u64,
        usage: BufferUsages::VERTEX | BufferUsages::COPY_DST | BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let camera = |x| Camera::new(DVec3::new(x, -30.0, 0.0), Vec3::Y, 1.0, [128, 128]).unwrap();
    prepare_frame(&device, &queue, &mut geometry, &view, camera(0.0));
    assert!(geometry.draw_list.has_blended());
    let instances = geometry.stats.instances;
    let slots = geometry.draw_list.draws().iter().map(|draw| draw.first + draw.count).max().unwrap();
    let bytes = u64::from(slots) * crate::pipeline::INSTANCE_BYTES as u64;
    let expected_instances = |geometry: &GeometryRenderer| {
        let mut expected = vec![0; bytes as usize];
        for (_, item) in geometry.cache.items.iter() {
            for face in &item.faces {
                let Some(slot) = face.slot else { continue };
                let start = slot as usize * crate::pipeline::INSTANCE_BYTES;
                let end = start + crate::pipeline::INSTANCE_BYTES;
                expected[start..end].copy_from_slice(&packed_instance(&item.transform, &face.params, item.scale));
            }
        }
        expected
    };
    let read_instances = |buffer: &wgpu::Buffer| {
        let readback = device.create_buffer(&BufferDescriptor {
            label: None,
            size: bytes,
            usage: BufferUsages::MAP_READ | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        encoder.copy_buffer_to_buffer(buffer, 0, &readback, 0, bytes);
        queue.submit([encoder.finish()]);
        let (sent, received) = std::sync::mpsc::channel();
        readback.slice(..).map_async(MapMode::Read, move |result| sent.send(result).unwrap());
        device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
        received.recv().unwrap().unwrap();
        let result = readback.get_mapped_range(..).unwrap().to_vec();
        readback.unmap();
        result
    };
    let before = read_instances(&geometry.instances);
    assert_eq!(before, expected_instances(&geometry), "the initial upload matches active transforms and materials");
    prepare_frame(&device, &queue, &mut geometry, &view, camera(26.0));
    assert_eq!(geometry.stats.instances, instances);
    assert_eq!(geometry.stats.layouts, 0);
    assert_eq!(geometry.stats.blended_sorts, 1);
    let expected = expected_instances(&geometry);
    let actual = read_instances(&geometry.instances);
    assert_ne!(expected, before, "the camera change reorders blended transforms or materials");
    assert_eq!(actual, expected, "camera sorting must upload the newly ordered transforms and materials");
    assert!(geometry.stats.draw_instance_write_calls > 0, "the reordered blended instances reach the GPU buffer");
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo test -p render gpu_warmed_rotation_preserves_triangles -- --ignored"]
fn gpu_warmed_rotation_preserves_triangles() {
    let (device, queue, indirect) = block_on(async {
        let instance =
            wgpu::Instance::new(InstanceDescriptor { backends: Backends::VULKAN, ..InstanceDescriptor::new_without_display_handle() });
        let adapter = instance.request_adapter(&Default::default()).await.unwrap();
        let indirect = adapter.features() & wgpu::Features::INDIRECT_FIRST_INSTANCE;
        let (device, queue) =
            adapter.request_device(&DeviceDescriptor { required_features: indirect, ..Default::default() }).await.unwrap();
        (device, queue, indirect.contains(wgpu::Features::INDIRECT_FIRST_INSTANCE))
    });
    let scope = device.push_error_scope(ErrorFilter::Validation);
    let a = AssetId::new(Uuid::from_u128(850));
    let b = AssetId::new(Uuid::from_u128(851));
    let c = AssetId::new(Uuid::from_u128(852));
    let point = |degrees: f32| {
        let radians = degrees.to_radians();
        Vec3::new(3.0 * radians.cos(), 3.0 * radians.sin(), 0.0)
    };
    let assets = std::collections::HashMap::from([
        (a, test_support::mesh::asset_with_lod_bounds(3, [-0.01, -0.8, -0.5], [0.01, 0.1, 0.5])),
        (b, test_support::mesh::asset_with_lod_bounds(3, [-0.01, -0.4, -0.4], [0.01, 0.4, 0.4])),
        (c, test_support::mesh::asset_with_lod_bounds(3, [-0.01, -0.5, -0.8], [0.01, 0.5, 0.2])),
    ]);
    let (world, mut scene, mut view, _, mut requests) = mesh_scene(&[(1, a, point(0.0)), (2, b, point(31.0)), (3, c, point(-15.0))], None);
    let load_camera = Camera::new(DVec3::ZERO, Vec3::X, 120_f64.to_radians(), [128, 128])
        .expect("wide loading camera")
        .with_quality(512.0, 4.0)
        .expect("high detail loading camera");
    let away_camera =
        Camera::new(DVec3::ZERO, Vec3::new(5_f32.to_radians().cos(), -5_f32.to_radians().sin(), 0.0), 40_f64.to_radians(), [128, 128])
            .expect("away camera");
    let back_camera =
        Camera::new(DVec3::ZERO, Vec3::new(5_f32.to_radians().cos(), 5_f32.to_radians().sin(), 0.0), 40_f64.to_radians(), [128, 128])
            .expect("back camera");
    scene.set_camera(load_camera);
    requests.extend(settle_scene(&world, &mut scene, &mut view, Instant::now()));
    complete_mesh_requests(&world, &mut scene, &mut view, &assets, requests);
    let entities: Vec<_> = view.objects().map(|object| object.detail().summary.entity).collect();
    assert_eq!(entities.len(), 3);
    let a_entity =
        view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(100_001)).expect("asset A").detail().summary.entity;
    let b_entity =
        view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(100_002)).expect("asset B").detail().summary.entity;
    let c_entity =
        view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(100_003)).expect("asset C").detail().summary.entity;
    for (entity, asset) in [(a_entity, a), (b_entity, b), (c_entity, c)] {
        assert_eq!(
            view.object(entity).and_then(|object| object.mesh().map(|(key, _)| key)),
            Some(DrawnMeshKey::Asset { asset, lod: assets::mesh::MeshLod::High }),
            "all assets are fully loaded at high LOD before camera rotation"
        );
    }

    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    assert_eq!(geometry.indirect_first_instance, indirect);
    let depth = geometry.depth(&device, [128, 128]).expect("depth target");
    prepare_frame(&device, &queue, &mut geometry, &view, load_camera);
    let all_visible = std::collections::HashSet::from([a_entity, b_entity, c_entity]);
    let actual_visible: std::collections::HashSet<_> =
        geometry.cache.visible.iter().filter_map(|index| geometry.cache.items.get(*index).and_then(|item| item.entity)).collect();
    assert_eq!(actual_visible, all_visible, "all meshes are resident and drawn before movement");
    assert_eq!(geometry.meshes.len(), 3);
    assert!(geometry.stats.uploaded_bytes > 0, "the warm-up frame uploads the complete mesh set");
    let ids = [a_entity, b_entity, c_entity].map(|entity| geometry.cache.items.get(geometry.cache.objects[&entity]).unwrap().mesh.id());
    let ranges = ids.map(|id| {
        let allocation = &geometry.meshes[&id].allocation;
        (allocation.vertices(), allocation.indices())
    });
    let page = Arc::as_ptr(geometry.meshes[&ids[0]].allocation.buffers()) as usize;
    assert!(ids.iter().all(|id| Arc::as_ptr(geometry.meshes[id].allocation.buffers()) as usize == page));
    let memory = geometry.memory.mesh_state();
    assert_eq!(geometry.draw_list.draw_batches().len(), 1, "the warm meshes use one shared opaque page batch");
    assert_eq!(geometry.draw_list.draw_batches()[0].count, 3);
    assert_eq!(geometry.stats.draw_commands, 3);
    assert_eq!(geometry.stats.draw_calls, if indirect { 1 } else { 3 });
    assert_visible_assets_match_source(&device, &queue, &geometry, &view, load_camera, &depth, &all_visible, "fully warm scene");

    scene.set_camera(away_camera);
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    complete_mesh_requests(&world, &mut scene, &mut view, &assets, requests);
    let away_visible = std::collections::HashSet::from([a_entity, c_entity]);
    let visible_entities = |geometry: &GeometryRenderer| {
        geometry
            .cache
            .visible
            .iter()
            .filter_map(|index| geometry.cache.items.get(*index).and_then(|item| item.entity))
            .collect::<std::collections::HashSet<_>>()
    };
    let mut away_committed = false;
    let mut away_turns = Vec::new();
    for _ in 0..16 {
        prepare_frame(&device, &queue, &mut geometry, &view, away_camera);
        assert_eq!(visible_entities(&geometry), away_visible, "the small turn culls the resident edge mesh");
        assert_visible_assets_match_source(&device, &queue, &geometry, &view, away_camera, &depth, &away_visible, "resident edge hidden");
        away_turns.push((
            geometry.stats.layouts,
            geometry.stats.layout_items_visited,
            geometry.layout_required,
            geometry.draw_list.layout_pending(),
        ));
        away_committed = !geometry.layout_required && !geometry.draw_list.layout_pending();
        if away_committed {
            break;
        }
        assert!(geometry.draw_list.valid(), "the previous drawable remains active while camera visibility is rebuilt");
        assert!(geometry.needs_frame(Instant::now()), "a pending bounded layout requests its next useful frame");
    }
    assert!(away_committed, "camera visibility commits over bounded turns: {away_turns:?}");
    assert_eq!(geometry.draw_list.draw_batches().len(), 1);
    assert_eq!(geometry.draw_list.draw_batches()[0].count, 2, "the opaque page batch drops the hidden command");
    assert_eq!(geometry.stats.draw_commands, 2);
    assert_eq!(geometry.stats.draw_calls, if indirect { 1 } else { 2 });
    assert_eq!(geometry.stats.uploaded_bytes, 0);
    assert_eq!(geometry.stats.pending_meshes, 0);
    assert_eq!(geometry.stats.mesh_budget_blocked, 0);
    let away_memory = geometry.memory.mesh_state();
    assert_eq!(away_memory.bytes, memory.bytes, "camera-only visibility changes allocate no page memory");
    assert_eq!(away_memory.revision, memory.revision, "camera-only visibility changes do not retire a mesh");
    assert_eq!(geometry.meshes.len(), 3, "all GPU meshes remain resident while visibility changes");
    assert!(geometry.retired.is_empty(), "camera-only visibility changes do not evict a mesh");
    for (id, (vertices, indices)) in ids.iter().zip(ranges.iter()) {
        let allocation = &geometry.meshes[id].allocation;
        assert_eq!((allocation.vertices(), allocation.indices()), (vertices.clone(), indices.clone()));
        assert_eq!(Arc::as_ptr(allocation.buffers()) as usize, page);
    }
    scene.set_camera(back_camera);
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    complete_mesh_requests(&world, &mut scene, &mut view, &assets, requests);
    let back_visible = all_visible;
    let mut back_committed = false;
    let mut back_turns = Vec::new();
    for _ in 0..16 {
        prepare_frame(&device, &queue, &mut geometry, &view, back_camera);
        assert_eq!(visible_entities(&geometry), back_visible, "turning back makes the same resident mesh visible again");
        assert_visible_assets_match_source(
            &device,
            &queue,
            &geometry,
            &view,
            back_camera,
            &depth,
            &back_visible,
            "resident edge reentered",
        );
        back_turns.push((
            geometry.stats.layouts,
            geometry.stats.layout_items_visited,
            geometry.layout_required,
            geometry.draw_list.layout_pending(),
        ));
        back_committed = !geometry.layout_required && !geometry.draw_list.layout_pending();
        if back_committed {
            break;
        }
        assert!(geometry.draw_list.valid(), "the away drawable remains active while camera visibility is rebuilt");
        assert!(geometry.needs_frame(Instant::now()), "a pending bounded layout requests its next useful frame");
    }
    assert!(back_committed, "camera visibility reentry commits over bounded turns: {back_turns:?}");
    assert_eq!(geometry.draw_list.draw_batches().len(), 1);
    assert_eq!(geometry.draw_list.draw_batches()[0].count, 3, "the reentered command returns to the page batch");
    assert_eq!(geometry.stats.draw_commands, 3);
    assert_eq!(geometry.stats.draw_calls, if indirect { 1 } else { 3 });
    assert_eq!(geometry.stats.uploaded_bytes, 0);
    assert_eq!(geometry.stats.pending_meshes, 0);
    assert_eq!(geometry.stats.mesh_budget_blocked, 0);
    let back_memory = geometry.memory.mesh_state();
    assert_eq!(back_memory.bytes, memory.bytes);
    assert_eq!(back_memory.revision, memory.revision);
    assert_eq!(geometry.meshes.len(), 3);
    assert!(geometry.retired.is_empty());
    for (id, (vertices, indices)) in ids.iter().zip(ranges.iter()) {
        let allocation = &geometry.meshes[id].allocation;
        assert_eq!((allocation.vertices(), allocation.indices()), (vertices.clone(), indices.clone()));
        assert_eq!(Arc::as_ptr(allocation.buffers()) as usize, page);
    }
    let memory = geometry.memory.clone();
    drop(geometry);
    device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
    assert_eq!(memory.mesh_state().bytes, 0);
    assert!(block_on(scope.pop()).is_none(), "warmed page batching and camera reentry satisfy GPU validation");
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --run-ignored only -E 'test(gpu_side_jitter_keeps_layout)'"]
fn gpu_side_jitter_keeps_layout() {
    use crate::geometry::visibility::{VisibilityBands, WorldSphere};

    const SIZE: [u32; 2] = [128, 128];
    let (_scene, view, _) = scene(255);
    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    geometry.update_cache(&view);
    let target = view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(1)).expect("edge target");
    let target_entity = target.detail().summary.entity;
    let target_index = geometry.cache.objects[&target_entity];
    let other = view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(2)).expect("hidden object");
    let other_index = geometry.cache.objects[&other.detail().summary.entity];
    geometry.cache.items.get_mut(other_index).unwrap().position = DVec3::new(10.0, 100.0, 0.0);
    let item = geometry.cache.items.get(target_index).expect("cached edge target");
    let (position, rotation, scale, bounds, shading) = (item.position, item.rotation, item.scale, item.info.bounds, item.shading);
    let mut last_strict = None;
    let mut edge = None;
    for step in 0..3000 {
        let y = f64::from(step) * 0.01;
        let camera = Camera::new(DVec3::new(0.0, y, 0.0), Vec3::X, 1.0, SIZE).expect("edge camera");
        let bands = VisibilityBands::new(crate::pipeline::projection(camera, SIZE), SIZE);
        let sphere = WorldSphere::new(position, rotation, scale, bounds, camera).expect("finite target bounds");
        if sphere.visible(shading, camera, &bands.strict) {
            last_strict = Some(camera);
        } else if let Some(visible_camera) = last_strict
            && sphere.visible(shading, camera, &bands.retained)
        {
            edge = Some((visible_camera, camera));
            break;
        }
    }
    let (strict_camera, exited_camera) = edge.expect("sphere crosses the side plane inside the retention band");

    prepare_frame(&device, &queue, &mut geometry, &view, strict_camera);
    assert!(geometry.cache.candidates.contains(&target_index));
    assert!(geometry.cache.visible.contains(&target_index));
    assert_eq!(geometry.stats.instances, 1);

    for camera in [exited_camera, strict_camera, exited_camera] {
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        assert!(geometry.cache.candidates.contains(&target_index), "the prior candidate stays admitted through side-plane jitter");
        assert!(geometry.cache.visible.contains(&target_index));
        assert_eq!(geometry.stats.layouts, 0, "membership and draw layout stay stable across the margin");
        assert_eq!(geometry.stats.instances, 1);
    }
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --run-ignored only -E 'test(gpu_prep_resident_before_entry)'"]
fn gpu_prep_resident_before_entry() {
    use crate::geometry::first_appearance::AppearanceKey;

    let asset = AssetId::new(Uuid::from_u128(9201));
    let bytes = test_support::mesh::asset(&[3], 1);
    let (_world, _scene, view, camera) = ready_mesh_scene(&[(1, asset, Vec3::new(10.0, 0.0, 0.0))], bytes);
    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    geometry.update_cache(&view);
    let object = view.objects().next().expect("target object");
    let entity = object.detail().summary.entity;
    let index = geometry.cache.objects[&entity];
    geometry.cache.items.get_mut(index).expect("cached target").position = DVec3::new(10.0, -9.0, 0.0);
    let mesh_id = geometry.cache.items.get(index).expect("cached target").mesh.id();

    prepare_frame(&device, &queue, &mut geometry, &view, camera);

    assert!(geometry.cache.preparation_candidates.contains(&index));
    assert!(!geometry.cache.candidates.contains(&index));
    assert!(!geometry.cache.visible.contains(&index));
    assert!(geometry.cache.preparation_mesh_order.iter().any(|demand| demand.mesh == mesh_id));
    assert!(!geometry.cache.mesh_order.iter().any(|demand| demand.mesh == mesh_id));
    assert!(geometry.meshes.contains_key(&mesh_id));
    assert!(!geometry.cache.mesh_ready.contains(&mesh_id));
    assert_eq!(geometry.stats.instances, 0);
    assert_eq!(geometry.stats.draw_commands, 0);
    assert_eq!(geometry.pick(camera, DVec3::new(10.0, -9.0, 0.0).normalize()).expect("prep-only pick"), None);
    assert!(!geometry.first_appearances.shown(AppearanceKey { entity, asset }));
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --run-ignored only -E 'test(gpu_pending_priority_tracks_camera)'"]
fn gpu_pending_priority_tracks_camera() {
    let first_asset = AssetId::new(Uuid::from_u128(9301));
    let second_asset = AssetId::new(Uuid::from_u128(9302));
    let bytes = test_support::mesh::asset(&[3], 1);
    let (world, mut scene, mut view, _, mut requests) =
        mesh_scene(&[(1, first_asset, Vec3::new(10.0, 100.0, 0.0)), (2, second_asset, Vec3::new(10.0, 108.0, 0.0))], None);
    let load_camera = Camera::new(DVec3::new(0.0, 104.0, 0.0), Vec3::X, std::f64::consts::FRAC_PI_3, [128, 128]).unwrap();
    scene.set_camera(load_camera);
    requests.extend(settle_scene(&world, &mut scene, &mut view, Instant::now()));
    let assets = std::collections::HashMap::from([(first_asset, bytes.clone()), (second_asset, bytes)]);
    complete_mesh_requests(&world, &mut scene, &mut view, &assets, requests);
    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    geometry.update_cache(&view);
    let first_entity =
        view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(100_001)).unwrap().detail().summary.entity;
    let second_entity =
        view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(100_002)).unwrap().detail().summary.entity;
    let first_mesh = geometry.cache.items.get(geometry.cache.objects[&first_entity]).unwrap().mesh.id();
    let second_mesh = geometry.cache.items.get(geometry.cache.objects[&second_entity]).unwrap().mesh.id();
    assert_ne!(first_mesh, second_mesh);
    let blocker = geometry.memory.reserve(crate::geometry::MESH_BYTES, crate::geometry::Resource::Mesh).expect("fill mesh budget");
    let left_camera = Camera::new(DVec3::new(0.0, 102.0, 0.0), Vec3::X, std::f64::consts::FRAC_PI_3, [128, 128]).unwrap();
    let right_camera = Camera::new(DVec3::new(0.0, 106.0, 0.0), Vec3::X, std::f64::consts::FRAC_PI_3, [128, 128]).unwrap();

    prepare_frame(&device, &queue, &mut geometry, &view, left_camera);
    assert_eq!(geometry.cache.mesh_order.iter().map(|demand| demand.mesh).collect::<Vec<_>>(), vec![first_mesh, second_mesh]);
    assert_eq!(geometry.stats.pending_meshes, 2);
    prepare_frame(&device, &queue, &mut geometry, &view, left_camera);
    assert!(!geometry.needs_frame(Instant::now()), "a stalled visible upload stops requesting frames without progress");
    let candidates = geometry.cache.candidates.clone();

    prepare_frame(&device, &queue, &mut geometry, &view, right_camera);

    assert_eq!(geometry.cache.candidates, candidates, "both draw memberships stay unchanged");
    assert_eq!(
        geometry.cache.mesh_order.iter().map(|demand| demand.mesh).collect::<Vec<_>>(),
        vec![second_mesh, first_mesh],
        "pending order follows the moved camera"
    );
    assert_eq!(geometry.stats.pending_meshes, 2);
    assert_eq!(geometry.stats.layouts, 0);
    assert!(!geometry.needs_frame(Instant::now()));
    drop(blocker);
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --run-ignored only -E 'test(gpu_prep_pressure_keeps_residents)'"]
fn gpu_prep_pressure_keeps_residents() {
    use crate::geometry::first_appearance::AppearanceKey;

    let display_asset = AssetId::new(Uuid::from_u128(9401));
    let hidden_asset = AssetId::new(Uuid::from_u128(9402));
    let target_asset = AssetId::new(Uuid::from_u128(9403));
    let bytes = test_support::mesh::asset_with_lod_geometry(&[(3, 1, 20_000, 20_000)]);
    let (_world, _scene, view, camera) = ready_mesh_scene(
        &[
            (1, display_asset, Vec3::new(10.0, 0.0, 0.0)),
            (2, hidden_asset, Vec3::new(10.0, 3.0, 0.0)),
            (3, target_asset, Vec3::new(10.0, 0.0, 0.0)),
        ],
        bytes,
    );
    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    geometry.update_cache(&view);
    let display = view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(100_001)).unwrap();
    let hidden = view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(100_002)).unwrap();
    let target = view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(100_003)).unwrap();
    let display_index = geometry.cache.objects[&display.detail().summary.entity];
    let hidden_index = geometry.cache.objects[&hidden.detail().summary.entity];
    let target_index = geometry.cache.objects[&target.detail().summary.entity];
    let display_id = geometry.cache.items.get(display_index).unwrap().mesh.id();
    let hidden_id = geometry.cache.items.get(hidden_index).unwrap().mesh.id();
    let target_id = geometry.cache.items.get(target_index).unwrap().mesh.id();
    geometry.cache.items.get_mut(target_index).unwrap().position = DVec3::new(10.0, -30.0, 0.0);
    prepare_frame(&device, &queue, &mut geometry, &view, camera);
    assert!(geometry.meshes.contains_key(&display_id));
    assert!(geometry.meshes.contains_key(&hidden_id));
    assert!(!geometry.meshes.contains_key(&target_id));

    geometry.cache.items.get_mut(hidden_index).unwrap().position = DVec3::new(10.0, 30.0, 0.0);
    geometry.cache.items.get_mut(target_index).unwrap().position = DVec3::new(10.0, -9.0, 0.0);
    let remaining = crate::geometry::MESH_BYTES - geometry.memory.mesh_state().bytes;
    let blocker = geometry.memory.reserve(remaining, crate::geometry::Resource::Mesh).expect("fill mesh budget");
    let moved_camera = Camera::new(DVec3::new(0.0, 0.1, 0.0), Vec3::X, std::f64::consts::FRAC_PI_3, [128, 128]).unwrap();

    prepare_frame(&device, &queue, &mut geometry, &view, moved_camera);

    assert!(geometry.cache.preparation_candidates.contains(&target_index));
    assert!(!geometry.cache.candidates.contains(&target_index));
    assert!(geometry.cache.preparation_mesh_order.iter().any(|demand| demand.mesh == target_id));
    assert!(!geometry.cache.mesh_order.iter().any(|demand| demand.mesh == target_id));
    assert!(geometry.meshes.contains_key(&display_id), "the displayed mesh stays resident");
    assert!(geometry.meshes.contains_key(&hidden_id), "preparation pressure does not evict an unused mesh");
    assert!(!geometry.meshes.contains_key(&target_id));
    assert!(geometry.retired.is_empty());
    assert_eq!(geometry.stats.instances, 1);
    assert_eq!(geometry.stats.pending_meshes, 0);
    assert_eq!(geometry.stats.mesh_bytes, crate::geometry::MESH_BYTES);
    assert!(geometry.needs_frame(Instant::now()), "the changed memory state permits one follow-up");

    prepare_frame(&device, &queue, &mut geometry, &view, moved_camera);
    assert!(!geometry.needs_frame(Instant::now()), "a capacity-blocked prep does not keep waking the renderer");
    assert!(geometry.meshes.contains_key(&hidden_id));

    drop(blocker);
    assert!(geometry.needs_frame(Instant::now()), "released memory schedules preparation again");
    prepare_frame(&device, &queue, &mut geometry, &view, moved_camera);
    assert!(geometry.meshes.contains_key(&target_id));
    assert_eq!(geometry.stats.instances, 1);
    assert!(!geometry.first_appearances.shown(AppearanceKey { entity: target.detail().summary.entity, asset: target_asset }));
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --run-ignored only -E 'test(gpu_prep_cleanup_on_lifecycle)'"]
fn gpu_prep_cleanup_on_lifecycle() {
    let first_asset = AssetId::new(Uuid::from_u128(9501));
    let second_asset = AssetId::new(Uuid::from_u128(9502));
    let bytes = test_support::mesh::asset(&[3], 1);
    let (mut world, mut scene, mut view, camera) =
        ready_mesh_scene(&[(1, first_asset, Vec3::new(10.0, 0.0, 0.0)), (2, second_asset, Vec3::new(10.0, 0.0, 0.0))], bytes);
    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    geometry.update_cache(&view);
    let target = view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(100_002)).unwrap();
    let target_entity = target.detail().summary.entity;
    let target_index = geometry.cache.objects[&target_entity];
    let target_mesh = geometry.cache.items.get(target_index).unwrap().mesh.id();
    geometry.cache.items.get_mut(target_index).unwrap().position = DVec3::new(10.0, -9.0, 0.0);
    prepare_frame(&device, &queue, &mut geometry, &view, camera);
    assert!(geometry.cache.preparation_mesh_order.iter().any(|demand| demand.mesh == target_mesh));

    let _ = world
        .remove_object(UpdateSource::new(fixture_region(&world), 10, OrderEvidence::Ordered(10), 0), std::num::NonZeroU32::new(2).unwrap())
        .expect("target removal applies");
    scene.notify_world_change();
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());
    prepare_frame(&device, &queue, &mut geometry, &view, camera);
    assert!(!geometry.cache.objects.contains_key(&target_entity));
    assert!(!geometry.cache.preparation_candidates.contains(&target_index));
    assert!(!geometry.cache.preparation_mesh_order.iter().any(|demand| demand.mesh == target_mesh));

    let remaining = view.objects().next().expect("remaining region object");
    let remaining_entity = remaining.detail().summary.entity;
    let remaining_index = geometry.cache.objects[&remaining_entity];
    let remaining_mesh = geometry.cache.items.get(remaining_index).unwrap().mesh.id();
    geometry.cache.items.get_mut(remaining_index).unwrap().position = DVec3::new(10.0, -9.0, 0.0);
    let moved_camera = Camera::new(DVec3::new(0.0, 0.1, 0.0), Vec3::X, std::f64::consts::FRAC_PI_3, [128, 128]).unwrap();
    prepare_frame(&device, &queue, &mut geometry, &view, moved_camera);
    assert!(geometry.cache.candidates.is_empty());
    assert!(geometry.cache.preparation_candidates.contains(&remaining_index));
    assert!(geometry.cache.preparation_mesh_order.iter().any(|demand| demand.mesh == remaining_mesh));

    let _ = world.retire_region(fixture_region(&world)).expect("region retirement applies");
    scene.notify_world_change();
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());
    prepare_frame(&device, &queue, &mut geometry, &view, moved_camera);
    assert!(geometry.cache.items.iter().next().is_none());
    assert!(geometry.cache.objects.is_empty());
    assert!(geometry.cache.candidates.is_empty());
    assert!(geometry.cache.preparation_candidates.is_empty());
    assert!(geometry.cache.preparation_replacements.is_empty());
    assert!(geometry.cache.mesh_order.is_empty());
    assert!(geometry.cache.preparation_mesh_order.is_empty());
    assert_eq!(geometry.stats.instances, 0);
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --run-ignored only -E 'test(gpu_large_prep_respects_upload_limit)'"]
fn gpu_large_prep_respects_upload_limit() {
    let asset = AssetId::new(Uuid::from_u128(9601));
    let bytes = test_support::mesh::asset_with_lod_geometry(&[(3, 3, 65_535, 65_535)]);
    let (_world, _scene, view, camera) = ready_mesh_scene(&[(1, asset, Vec3::new(10.0, 0.0, 0.0))], bytes);
    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    update_cache_until_current(&mut geometry, &view, camera);
    let object = view.objects().next().expect("target object");
    let entity = object.detail().summary.entity;
    let index = geometry.cache.objects[&entity];
    geometry.cache.items.get_mut(index).unwrap().position = DVec3::new(10.0, -9.0, 0.0);
    let mesh_id = geometry.cache.items.get(index).unwrap().mesh.id();

    prepare_until_current(&device, &queue, &mut geometry, &view, camera);

    assert!(geometry.cache.preparation_mesh_order.iter().any(|demand| demand.mesh == mesh_id));
    assert!(!geometry.cache.mesh_order.iter().any(|demand| demand.mesh == mesh_id));
    assert!(!geometry.meshes.contains_key(&mesh_id), "preparation cannot use the large first-upload exception");
    assert_eq!(geometry.stats.uploaded_bytes, 0);
    assert_eq!(geometry.stats.pending_meshes, 0);
    assert!(!geometry.needs_frame(Instant::now()), "an over-budget prep does not schedule an empty retry");
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --run-ignored only -E 'test(gpu_pending_visible_mesh_precedes_prep)'"]
fn gpu_pending_visible_mesh_precedes_prep() {
    let visible_asset = AssetId::new(Uuid::from_u128(9701));
    let preparation_asset = AssetId::new(Uuid::from_u128(9702));
    let visible_bytes = test_support::mesh::asset_with_lod_geometry(&[(3, 3, 65_535, 65_535)]);
    let preparation_bytes = test_support::mesh::asset(&[3], 1);
    let (world, mut scene, mut view, camera, requests) =
        mesh_scene(&[(1, visible_asset, Vec3::new(10.0, 0.0, 0.0)), (2, preparation_asset, Vec3::new(10.0, 0.0, 0.0))], None);
    let assets = std::collections::HashMap::from([(visible_asset, visible_bytes), (preparation_asset, preparation_bytes)]);
    complete_mesh_requests(&world, &mut scene, &mut view, &assets, requests);

    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    update_cache_until_current(&mut geometry, &view, camera);
    let visible_entity =
        view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(100_001)).unwrap().detail().summary.entity;
    let preparation_entity =
        view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(100_002)).unwrap().detail().summary.entity;
    let visible_index = geometry.cache.objects[&visible_entity];
    let preparation_index = geometry.cache.objects[&preparation_entity];
    geometry.cache.items.get_mut(preparation_index).unwrap().position = DVec3::new(10.0, -9.0, 0.0);
    let visible_mesh = geometry.cache.items.get(visible_index).unwrap().mesh.id();
    let preparation_mesh = geometry.cache.items.get(preparation_index).unwrap().mesh.id();
    let preparation_page = view.object(preparation_entity).unwrap().mesh().unwrap().1.clone();
    let available = mesh_page_bytes(&preparation_page);
    let blocker = geometry
        .memory
        .reserve(crate::geometry::MESH_BYTES - available, crate::geometry::Resource::Mesh)
        .expect("leave one preparation page available");

    prepare_frame(&device, &queue, &mut geometry, &view, camera);

    assert!(geometry.cache.candidates.contains(&visible_index));
    assert!(!geometry.cache.candidates.contains(&preparation_index));
    assert!(geometry.cache.preparation_candidates.contains(&preparation_index));
    assert_eq!(geometry.cache.mesh_order.iter().map(|demand| demand.mesh).collect::<Vec<_>>(), vec![visible_mesh]);
    assert!(geometry.cache.preparation_mesh_order.iter().any(|demand| demand.mesh == preparation_mesh));
    assert_eq!(geometry.stats.pending_meshes, 1);
    assert!(!geometry.meshes.contains_key(&visible_mesh));
    assert!(!geometry.meshes.contains_key(&preparation_mesh), "available prep capacity stays behind pending visible demand");
    assert_eq!(geometry.stats.uploaded_bytes, 0);
    assert_eq!(geometry.stats.mesh_bytes, crate::geometry::MESH_BYTES - available);
    assert!(geometry.needs_frame(Instant::now()), "the memory change schedules one follow-up");
    prepare_frame(&device, &queue, &mut geometry, &view, camera);
    assert!(!geometry.meshes.contains_key(&preparation_mesh));
    assert!(!geometry.needs_frame(Instant::now()), "blocked visible demand and deferred preparation do not spin");
    drop(blocker);
}

#[test]
#[ignore = "requires a Vulkan adapter"]
fn gpu_visible_upload_wins() {
    let mesh_asset = AssetId::new(Uuid::from_u128(9801));
    let old_texture = AssetId::new(Uuid::from_u128(9802));
    let new_texture = AssetId::new(Uuid::from_u128(9803));
    let mesh = test_support::mesh::asset_with_lod_faces(&[(2, 1), (3, 1)]);
    let (mut world, mut scene, mut view, _, requests) =
        mesh_scene(&[(1, mesh_asset, Vec3::new(5.0, 0.0, 0.0)), (2, mesh_asset, Vec3::new(-5.0, 0.0, 0.0))], None);
    let old_camera = Camera::new(DVec3::ZERO, Vec3::X, 10_f64.to_radians(), [2048, 2048]).expect("fixture camera");
    let new_camera = Camera::new(DVec3::ZERO, -Vec3::X, 10_f64.to_radians(), [2048, 2048]).expect("fixture camera");
    scene.set_camera(new_camera);
    let mut requests = requests;
    requests.extend(settle_scene(&world, &mut scene, &mut view, Instant::now()));
    complete_mesh_requests(&world, &mut scene, &mut view, &std::collections::HashMap::from([(mesh_asset, mesh)]), requests);
    assert!(
        view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(100_002)).and_then(|object| object.mesh()).is_some(),
        "the return camera object has prepared mesh geometry"
    );

    let set_texture = |world: &mut world::World, id: u32, sequence: u32, texture: AssetId| {
        let mut observation = ObjectObservation::new(
            UpdateSource::new(fixture_region(world), sequence, OrderEvidence::Ordered(u64::from(sequence)), 0),
            Uuid::from_u128(100_000 + u128::from(id)),
            std::num::NonZeroU32::new(id).expect("fixture local id"),
            Kind::Object,
            ParentUpdate::Root,
        );
        observation.appearance = FieldUpdate::Set(Appearance::Surface);
        observation.surface_appearance = Some(
            SurfaceAppearance::new(world::FaceAppearance { texture: Some(texture), ..Default::default() }, Vec::new())
                .expect("fixture appearance"),
        );
        assert_matches!(world.observe_object(observation).unwrap().outcome, world::ApplyOutcome::Updated(_));
    };

    set_texture(&mut world, 1, 10, old_texture);
    scene.notify_world_change();
    scene.set_camera(old_camera);
    let old_requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    let old_texture_requests: Vec<_> = old_requests
        .into_iter()
        .filter(|request| request.request.resource().kind == assets::Kind::Texture && request.request.resource().id == old_texture)
        .collect();
    assert!(!old_texture_requests.is_empty(), "the initial camera requests the old visible texture");
    for request in old_texture_requests {
        scene.complete_asset(request.ticket, prepared_asset(request.request, test_support::texture::GRAY_4096_J2C.to_vec()));
    }
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());
    scene.set_camera(new_camera);
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());

    let texture_id = |view: &SceneView, object_id| {
        view.objects()
            .find(|object| object.detail().summary.id == Uuid::from_u128(object_id))
            .and_then(|object| object.materials())
            .and_then(|faces| {
                faces
                    .iter()
                    .find_map(|face| face.base_color_texture.as_ref().and_then(|binding| binding.image()).map(|texture| texture.id()))
            })
            .expect("fixture object texture is ready")
    };
    let old_id = texture_id(&view, 100_001);

    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    prepare_frame_at(&device, &queue, &mut geometry, &view, new_camera, Instant::now());
    assert_eq!(geometry.stats.instances, 1, "the new camera shows its mesh before the texture changes");

    set_texture(&mut world, 2, 11, new_texture);
    scene.notify_world_change();
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    let new_texture_requests: Vec<_> = requests
        .into_iter()
        .filter(|request| request.request.resource().kind == assets::Kind::Texture && request.request.resource().id == new_texture)
        .collect();
    assert!(!new_texture_requests.is_empty(), "the visible mesh requests its new texture");
    for request in new_texture_requests {
        scene.complete_asset(request.ticket, prepared_asset(request.request, test_support::texture::GRAY_4096_J2C.to_vec()));
    }
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());
    let new_id = texture_id(&view, 100_002);
    assert!(old_id < new_id, "the older texture ID sorts before the newly visible texture");

    prepare_frame_at(&device, &queue, &mut geometry, &view, new_camera, Instant::now());
    let job = geometry.next_texture_job().expect("new visible texture upload job");
    geometry.complete_texture(job.run(crate::texture::CompressionQuality::Fast));
    assert!(geometry.next_texture_job().is_none());
    prepare_frame_at(&device, &queue, &mut geometry, &view, new_camera, Instant::now());
    assert!(!geometry.textures.is_resident(new_id), "the large new texture upload is still in flight");

    prepare_frame_at(&device, &queue, &mut geometry, &view, old_camera, Instant::now());
    let job = geometry.next_texture_job().expect("old visible texture upload job");
    geometry.complete_texture(job.run(crate::texture::CompressionQuality::Fast));
    assert!(geometry.next_texture_job().is_none());
    prepare_frame_at(&device, &queue, &mut geometry, &view, old_camera, Instant::now());
    assert!(!geometry.textures.is_resident(old_id), "the large old texture remains in flight");
    assert!(!geometry.textures.is_resident(new_id), "the offscreen upload remains incomplete");

    prepare_frame_at(&device, &queue, &mut geometry, &view, new_camera, Instant::now());
    assert!(!geometry.textures.is_resident(new_id), "the returned texture still needs another upload frame");
    prepare_frame_at(&device, &queue, &mut geometry, &view, new_camera, Instant::now());
    assert!(geometry.textures.is_resident(new_id), "the newly visible texture completes ahead of the offscreen upload");
    assert!(!geometry.textures.is_resident(old_id), "the large offscreen upload remains incomplete");
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --run-ignored only -E 'test(gpu_mesh_admission_continues_with_bounded_ordered_work)'"]
fn gpu_mesh_admission_continues_with_bounded_ordered_work() {
    const EXTRA_DEMANDS: usize = 30;
    let (_scene, view, camera) = scene(255);
    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    for _ in 0..8 {
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        if geometry.draw_list.valid() && geometry.cache.visible.len() == 2 {
            break;
        }
    }
    assert!(geometry.draw_list.valid(), "the fixture establishes an active drawable");
    assert_eq!(geometry.cache.visible.len(), 2);
    let active_commands = geometry.draw_list.draws().len();
    let active_instances = geometry.draw_list.visible_items();
    let extra_meshes = crate::scene::distinct_meshes_for_test(EXTRA_DEMANDS);
    geometry.cache.clear_mesh_demands();
    for object in view.objects() {
        let (_, mesh) = object.mesh().expect("fixture mesh");
        crate::geometry::cache::add_mesh_demand_for_test(&mut geometry.cache, &mut geometry.stats, mesh, (1.0, 0.0));
    }
    for mesh in &extra_meshes {
        crate::geometry::cache::add_mesh_demand_for_test(&mut geometry.cache, &mut geometry.stats, mesh, (0.0, 0.0));
    }
    geometry.prepared.as_mut().expect("active drawable preparation").more_meshes = true;
    let total_demands = 2 + EXTRA_DEMANDS;
    assert_eq!(geometry.cache.mesh_order.len(), total_demands);
    prepare_frame(&device, &queue, &mut geometry, &view, camera);
    assert!(geometry.mesh_admission.active, "the visible demand list continues after the per-turn admission limit");
    assert!(!geometry.mesh_admission.visible_ready, "unvisited or upload-blocked demands cannot publish partial visibility");
    assert_eq!(geometry.mesh_admission.visits_this_frame, crate::geometry::frame::MESH_DEMANDS_PER_FRAME);
    assert!(geometry.pending_meshes.len() <= crate::geometry::frame::MESH_DEMANDS_PER_FRAME);
    assert_eq!(geometry.meshes.len(), 2 + super::MAX_MESH_UPLOADS, "a full upload turn still visits only one ordered slice");
    assert_eq!(geometry.pending_meshes.len(), 6, "only visited demands blocked by upload capacity enter the pending set");
    let ordered = geometry.cache.mesh_order.iter().map(|demand| demand.mesh).collect::<Vec<_>>();
    assert!(
        ordered[..2 + super::MAX_MESH_UPLOADS].iter().all(|mesh| geometry.meshes.contains_key(mesh)),
        "admission follows priority order"
    );
    assert!(
        !ordered[2 + super::MAX_MESH_UPLOADS..].iter().any(|mesh| geometry.meshes.contains_key(mesh)),
        "lower-priority demands wait for their turn"
    );
    assert_eq!(geometry.draw_list.draws().len(), active_commands, "partial admissions retain active draw commands");
    assert_eq!(geometry.draw_list.visible_items(), active_instances, "partial admissions retain active instances");
    assert_eq!(geometry.cache.visible.len(), 2, "partial admissions do not replace the current visible scene");

    let previous_revision = geometry.cache.mesh_demand_revision;
    let changed_mesh = extra_meshes.last().expect("extra demand");
    crate::geometry::cache::add_mesh_demand_for_test(&mut geometry.cache, &mut geometry.stats, changed_mesh, (2.0, 0.0));
    assert!(geometry.cache.mesh_demand_revision > previous_revision, "a changed priority advances the demand revision");
    assert_eq!(geometry.cache.mesh_order.first().map(|demand| demand.mesh), Some(changed_mesh.id()));
    let previous_resident = geometry.meshes.len();
    prepare_frame(&device, &queue, &mut geometry, &view, camera);
    assert!(geometry.mesh_admission.active, "a changed ordered demand restarts admission");
    assert_eq!(geometry.mesh_admission.revision, geometry.cache.mesh_demand_revision);
    assert!(geometry.mesh_admission.visits_this_frame <= crate::geometry::frame::MESH_DEMANDS_PER_FRAME);
    assert_eq!(geometry.draw_list.draws().len(), active_commands, "changed demand order preserves active commands");
    assert_eq!(geometry.draw_list.visible_items(), active_instances, "changed demand order preserves active instances");
    assert!(geometry.meshes.len() >= previous_resident);

    let mut resident = geometry.meshes.len();
    let mut complete = false;
    for _ in 0..8 {
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        assert!(geometry.mesh_admission.visits_this_frame <= crate::geometry::frame::MESH_DEMANDS_PER_FRAME);
        assert!(geometry.pending_meshes.len() <= crate::geometry::frame::MESH_DEMANDS_PER_FRAME);
        assert!(geometry.meshes.len() >= resident, "the bounded continuation makes monotonic resident progress");
        resident = geometry.meshes.len();
        if !geometry.mesh_admission.active && geometry.meshes.len() == total_demands {
            complete = true;
            break;
        }
    }
    assert!(complete, "all distinct visible meshes eventually become resident");
    assert_eq!(geometry.cache.visible.len(), 2);
    assert_eq!(geometry.draw_list.visible_items(), active_instances);
    assert!(geometry.draw_list.valid(), "admission preserves the committed drawable");
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --run-ignored only -E 'test(gpu_pending_layout_pins_meshes)' "]
fn gpu_pending_layout_pins_meshes() {
    const OBJECTS: u32 = 1_025;
    let old_asset = AssetId::new(Uuid::from_u128(9_901));
    let spare_asset = AssetId::new(Uuid::from_u128(9_902));
    let mut objects = (1..=OBJECTS).map(|id| (id, old_asset, Vec3::new(10.0, 0.0, 0.0))).collect::<Vec<_>>();
    objects.push((OBJECTS + 1, spare_asset, Vec3::new(10.0, 0.0, 0.0)));
    let (mut world, mut scene, mut view, camera) =
        ready_mesh_scene(&objects, test_support::mesh::asset_with_lod_geometry(&[(3, 1, 32_800, 3)]));
    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    prepare_until_current(&device, &queue, &mut geometry, &view, camera);
    assert_eq!(geometry.cache.visible.len(), OBJECTS as usize + 1);
    assert_eq!(geometry.draw_list.visible_items(), OBJECTS as usize + 1);

    let depth = geometry.depth(&device, [128, 128]).expect("fixture depth target");
    let old_pixels = render_pixels(&device, &queue, [128, 128], &depth, |pass| geometry.draw(pass));
    let old_shared = view.objects().next().and_then(|object| object.mesh().map(|(_, mesh)| mesh.clone())).expect("old mesh");
    let old_mesh = old_shared.id();
    let spare_mesh = view
        .objects()
        .find(|object| object.detail().summary.id == Uuid::from_u128(100_000 + u128::from(OBJECTS + 1)))
        .and_then(|object| object.mesh().map(|(_, mesh)| mesh.clone()))
        .expect("unreferenced large mesh");
    let spare_id = spare_mesh.id();
    let old_bytes = mesh_page_bytes(&old_shared);
    let spare_bytes = mesh_page_bytes(&spare_mesh);
    let _ = world
        .remove_object(
            UpdateSource::new(fixture_region(&world), OBJECTS + 10, OrderEvidence::Ordered(u64::from(OBJECTS) + 10), 0),
            std::num::NonZeroU32::new(OBJECTS + 1).expect("unreferenced object local ID"),
        )
        .expect("unreferenced object removal applies");
    scene.notify_world_change();
    settle_scene(&world, &mut scene, &mut view, Instant::now());
    let removal_deadline = Instant::now() + std::time::Duration::from_secs(60);
    loop {
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        if geometry.cache.rebuild.is_none()
            && geometry.cache.version.as_ref() == Some(&view.version())
            && geometry.cache.visible.len() == OBJECTS as usize
            && geometry.draw_list.visible_items() == OBJECTS as usize
            && !geometry.draw_list.layout_pending()
        {
            break;
        }
        assert!(Instant::now() < removal_deadline, "unreferenced object retirement did not settle");
    }
    assert!(geometry.meshes.contains_key(&spare_id), "unreferenced geometry remains available for eviction");
    assert!(geometry.draw_list.pins_mesh(old_mesh));
    assert!(!geometry.draw_list.pins_mesh(spare_id));
    assert_eq!(geometry.memory.mesh_state().bytes, old_bytes + spare_bytes);
    assert_eq!(
        render_pixels(&device, &queue, [128, 128], &depth, |pass| geometry.draw(pass)),
        old_pixels,
        "removing an overlapping unreferenced draw preserves the old committed pixels"
    );
    let new_mesh = crate::scene::distinct_meshes_for_test(1).pop().expect("replacement mesh");
    let new_page_bytes = mesh_page_bytes(&new_mesh);
    assert!(spare_bytes >= new_page_bytes, "retiring the unreferenced page frees enough room for the replacement page");
    let blocker_bytes = crate::geometry::MESH_BYTES - geometry.memory.mesh_state().bytes;
    let blocker = geometry.memory.reserve(blocker_bytes, crate::geometry::Resource::Mesh).expect("fill mesh budget");
    geometry.cache.clear_mesh_demands();
    crate::geometry::cache::add_mesh_demand_for_test(&mut geometry.cache, &mut geometry.stats, &new_mesh, (1.0, 0.0));
    assert!(geometry.draw_list.request_layout(&geometry.cache.visible, camera, view.visibility_version(), false, &geometry.cache.items,));

    prepare_frame(&device, &queue, &mut geometry, &view, camera);

    assert!(geometry.draw_list.layout_pending(), "the replacement layout exceeds the 1,024-item turn limit");
    assert!(geometry.draw_list.valid(), "the committed list remains drawable while replacement layout is pending");
    assert_eq!(geometry.draw_list.visible_items(), OBJECTS as usize);
    assert!(geometry.draw_list.pins_mesh(old_mesh), "the active draw list pins its command mesh while a layout is pending");
    assert!(geometry.meshes.contains_key(&old_mesh), "a pending replacement cannot evict a committed mesh");
    assert!(!geometry.meshes.contains_key(&spare_mesh.id()), "admission evicts an unreferenced resident before the committed one");
    assert_eq!(
        render_pixels(&device, &queue, [128, 128], &depth, |pass| geometry.draw(pass)),
        old_pixels,
        "the active draw list retains its pixels while the replacement is staged"
    );

    let deadline = Instant::now() + std::time::Duration::from_secs(60);
    let mut rebuilt_committed_batches = false;
    while geometry.draw_list.layout_pending() || geometry.draw_list.visible_items() != 0 {
        assert!(Instant::now() < deadline, "the bounded layout replacement did not settle");
        if geometry.draw_list.visible_items() == OBJECTS as usize {
            assert!(geometry.meshes.contains_key(&old_mesh), "staged retirement keeps the active list's mesh resident");
        }
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        if geometry.stats.draw_batch_commands_built > 0 && geometry.draw_list.visible_items() == OBJECTS as usize {
            rebuilt_committed_batches = true;
            assert!(geometry.meshes.contains_key(&old_mesh), "batch rebuilding resolves the committed mesh command");
            let _pixels = render_pixels(&device, &queue, [128, 128], &depth, |pass| geometry.draw(pass));
        }
    }
    assert!(rebuilt_committed_batches, "the active draw list's batches rebuild after unrelated mesh retirement");
    assert_eq!(geometry.draw_list.visible_items(), 0, "the blocked replacement first commits an empty ready subset");
    assert!(!geometry.draw_list.pins_mesh(old_mesh));

    let replacement_id = new_mesh.id();
    let retry_deadline = Instant::now() + std::time::Duration::from_secs(60);
    while !geometry.meshes.contains_key(&replacement_id) {
        assert!(Instant::now() < retry_deadline, "released draw-list capacity did not admit the waiting mesh");
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
    }
    assert!(!geometry.draw_list.pins_mesh(old_mesh), "the committed-list pin releases after the replacement commits");
    assert!(geometry.memory.mesh_state().bytes <= crate::geometry::MESH_BYTES);

    drop(blocker);
    let memory = geometry.memory.clone();
    drop(geometry);
    device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
    assert_eq!(memory.mesh_state().bytes, 0);
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --all-features --locked --run-ignored only -E 'test(gpu_mesh_residency_survives_replacement_eviction)'"]
fn gpu_mesh_residency_survives_replacement_eviction() {
    const OBJECTS: u32 = 1_025;
    let old_asset = AssetId::new(Uuid::from_u128(9_930));
    let spare_asset = AssetId::new(Uuid::from_u128(9_931));
    let replacement_asset = AssetId::new(Uuid::from_u128(9_932));
    let bytes = test_support::mesh::asset_with_lod_geometry(&[(3, 8, 65_535, 196_605)]);
    let replacement_bytes =
        test_support::mesh::asset_with_lod_geometry_bounds(&[(3, 8, 65_535, 196_605)], [-0.01, 199.0, -1.0], [0.01, 201.0, 1.0]);
    let objects = (1..=OBJECTS)
        .map(|id| (id, old_asset, Vec3::new(300.0, 0.0, 0.0)))
        .chain([(OBJECTS + 1, spare_asset, Vec3::new(300.0, 0.0, 0.0))])
        .collect::<Vec<_>>();
    let (mut world, mut scene, mut view, original_camera, requests) = mesh_scene(&objects, None);
    let mut appearance = ObjectPatch::new(
        UpdateSource::new(fixture_region(&world), OBJECTS + 20, OrderEvidence::Ordered(u64::from(OBJECTS) + 20), 0),
        std::num::NonZeroU32::new(1).expect("blended object local ID"),
    );
    appearance.appearance = FieldUpdate::Set(Appearance::Surface);
    appearance.surface_appearance = Some(
        SurfaceAppearance::new(world::FaceAppearance { color: [255, 0, 0, 128], ..Default::default() }, Vec::new())
            .expect("valid blended appearance"),
    );
    let _ = world.patch_object(appearance).expect("blended appearance applies");
    scene.notify_world_change();
    let assets =
        std::collections::HashMap::from([(old_asset, bytes.clone()), (spare_asset, bytes), (replacement_asset, replacement_bytes)]);
    complete_mesh_requests(&world, &mut scene, &mut view, &assets, requests);

    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    prepare_until_current(&device, &queue, &mut geometry, &view, original_camera);
    assert_eq!(geometry.meshes.len(), 2, "the initial source scene uploads both visible meshes");
    assert!(geometry.draw_list.has_blended(), "the committed source scene includes a blended mesh command");

    let entity = view
        .objects()
        .find(|object| object.detail().summary.id == Uuid::from_u128(100_001))
        .expect("replacement target")
        .detail()
        .summary
        .entity;
    let old_index = geometry.cache.objects[&entity];
    let old_mesh = geometry.cache.items.get(old_index).expect("old item").mesh.id();
    let spare_mesh = view
        .objects()
        .find(|object| object.detail().summary.id == Uuid::from_u128(100_000 + u128::from(OBJECTS + 1)))
        .and_then(|object| object.mesh().map(|(_, mesh)| mesh.id()))
        .expect("spare mesh");
    let old_bytes =
        mesh_upload_bytes(view.object(entity).and_then(|object| object.mesh().map(|(_, mesh)| mesh)).expect("old source mesh"), None)
            .expect("old mesh upload size");
    let spare_bytes = mesh_upload_bytes(
        view.objects()
            .find(|object| object.detail().summary.id == Uuid::from_u128(100_000 + u128::from(OBJECTS + 1)))
            .and_then(|object| object.mesh().map(|(_, mesh)| mesh))
            .expect("spare source mesh"),
        None,
    )
    .expect("spare mesh upload size");

    let mut replacement = ObjectPatch::new(
        UpdateSource::new(fixture_region(&world), OBJECTS + 21, OrderEvidence::Ordered(u64::from(OBJECTS) + 21), 0),
        std::num::NonZeroU32::new(1).expect("replacement target local ID"),
    );
    replacement.shape = FieldUpdate::Set(Shape::Mesh { asset_id: replacement_asset, flags: SculptFlags::default() });
    let _ = world.patch_object(replacement).expect("mesh replacement applies to the live object");
    scene.notify_world_change();
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    complete_mesh_requests(&world, &mut scene, &mut view, &assets, requests);
    let replacement_object = view.object(entity).expect("replacement remains live");
    let replacement_mesh = replacement_object.mesh().map(|(_, mesh)| mesh.id()).expect("replacement is CPU-ready");
    let replacement_bytes =
        mesh_upload_bytes(replacement_object.mesh().expect("replacement mesh").1, None).expect("replacement upload size");
    assert!(old_bytes + spare_bytes <= crate::geometry::MESH_BYTES, "the current draw list fits its mesh budget");
    assert!(
        old_bytes + spare_bytes + replacement_bytes > crate::geometry::MESH_BYTES,
        "the replacement cannot be admitted while both committed meshes are pinned"
    );

    let staging_camera =
        Camera::new(DVec3::new(256.0, 0.0, 0.0), Vec3::X, std::f64::consts::FRAC_PI_3, [128, 128]).expect("staged layout camera");
    scene.set_camera(staging_camera);
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());
    assert!(geometry.draw_list.committed_pins_mesh(old_mesh));
    assert!(geometry.draw_list.committed_pins_mesh(spare_mesh));
    let mut staged_draws = 0;
    for _ in 0..32 {
        prepare_frame(&device, &queue, &mut geometry, &view, staging_camera);
        assert_draws_resident(&geometry);
        staged_draws = geometry.draw_list.staged_draw_count();
        if staged_draws > 0 {
            break;
        }
    }
    assert!(staged_draws > 0, "the pending layout emits partial staged draw commands");
    assert!(geometry.draw_list.layout_pending() && geometry.draw_list.layout_building(), "the large staged layout remains in progress");
    assert!(geometry.draw_list.committed_pins_mesh(old_mesh));
    assert!(geometry.draw_list.committed_pins_mesh(spare_mesh));

    for order in 1_000_000..1_000_065 {
        move_mesh_object(&mut world, 1, order, Vec3::new(300.0, if order % 2 == 0 { 0.1 } else { -0.1 }, 0.0), [64, 96, 192, 255]);
        scene.notify_world_change();
        let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());
    }
    prepare_frame(&device, &queue, &mut geometry, &view, staging_camera);
    assert_draws_resident(&geometry);
    assert!(geometry.cache.rebuild.is_some(), "pose history overflow starts cache recovery");
    assert!(geometry.draw_list.layout_pending() && !geometry.draw_list.layout_building(), "recovery retires the canceled staged list");
    assert!(geometry.draw_list.committed_pins_mesh(old_mesh));
    assert!(geometry.draw_list.committed_pins_mesh(spare_mesh));

    let removed_entity = view
        .objects()
        .find(|object| object.detail().summary.id == Uuid::from_u128(100_002))
        .expect("object removed during recovery")
        .detail()
        .summary
        .entity;
    let _ = world
        .remove_object(
            UpdateSource::new(fixture_region(&world), 2_000_000, OrderEvidence::Ordered(2_000_000), 0),
            std::num::NonZeroU32::new(2).expect("removed object local ID"),
        )
        .expect("object removal applies during recovery");
    scene.notify_world_change();
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());
    assert!(!view.object_is_live(removed_entity), "the removed object leaves the published scene immediately");
    prepare_frame(&device, &queue, &mut geometry, &view, staging_camera);
    assert_draws_resident(&geometry);

    let replacement_camera =
        Camera::new(DVec3::new(256.0, 200.0, 0.0), Vec3::X, std::f64::consts::FRAC_PI_3, [128, 128]).expect("replacement scene camera");
    scene.set_camera(replacement_camera);
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());

    let deadline = Instant::now() + std::time::Duration::from_secs(60);
    loop {
        prepare_frame(&device, &queue, &mut geometry, &view, replacement_camera);
        assert_draws_resident(&geometry);
        if geometry.meshes.contains_key(&replacement_mesh) {
            break;
        }
        assert!(Instant::now() < deadline, "replacement admission did not recover after the old list retired");
    }

    loop {
        prepare_frame(&device, &queue, &mut geometry, &view, replacement_camera);
        assert_draws_resident(&geometry);
        let item = geometry.cache.items.get(geometry.cache.objects[&entity]).expect("promoted replacement item");
        if item.mesh.id() == replacement_mesh
            && item.replacement.is_none()
            && geometry.cache.version.as_ref() == Some(&view.version())
            && geometry.draw_list.valid()
            && !geometry.draw_list.layout_pending()
            && !geometry.layout_required
            && !geometry.mesh_admission.active
            && geometry.cache.visible.iter().any(|index| {
                geometry.cache.items.get(*index).is_some_and(|item| item.entity == Some(entity) && item.mesh.id() == replacement_mesh)
            })
            && geometry.draw_list.draws().iter().any(|draw| draw.mesh == replacement_mesh)
            && geometry.draw_list.draws().iter().all(|draw| draw.mesh == replacement_mesh)
        {
            break;
        }
        assert!(Instant::now() < deadline, "replacement promotion did not converge");
    }
    assert!(geometry.cache.visible.contains(&geometry.cache.objects[&entity]));
    assert!(!geometry.cache.objects.contains_key(&removed_entity), "the removed object never returns from recovery");
    assert!(geometry.meshes.contains_key(&replacement_mesh));
    assert!(geometry.memory.mesh_state().bytes <= crate::geometry::MESH_BYTES);

    let mut clean = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    clean.memory = Arc::new(crate::geometry::Accounting::default());
    prepare_until_current(&device, &queue, &mut clean, &view, replacement_camera);
    let size = [128, 128];
    let actual_depth = geometry.depth(&device, size).expect("incremental depth target");
    let clean_depth = clean.depth(&device, size).expect("clean depth target");
    let actual = render_pixels(&device, &queue, size, &actual_depth, |pass| geometry.draw(pass));
    let expected = render_pixels(&device, &queue, size, &clean_depth, |pass| clean.draw(pass));
    assert_eq!(actual, expected, "replacement promotion and eviction match a clean build");

    let memory = geometry.memory.clone();
    let clean_memory = clean.memory.clone();
    drop(geometry);
    drop(clean);
    device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
    assert_eq!(memory.mesh_state().bytes, 0);
    assert_eq!(clean_memory.mesh_state().bytes, 0);
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --all-features --locked --run-ignored only -E 'test(blended_group_reuse_stays_resident)'"]
fn blended_group_reuse_stays_resident() {
    const OBJECTS: u32 = 1_025;
    let old_asset = AssetId::new(Uuid::from_u128(9_940));
    let reused_asset = AssetId::new(Uuid::from_u128(9_941));
    let old_bytes = test_support::mesh::asset_with_lod_geometry(&[(3, 1, 3, 3)]);
    let reused_bytes = old_bytes.clone();
    let hidden_id = OBJECTS + 1;
    let objects = (1..=OBJECTS)
        .map(|id| (id, old_asset, Vec3::new(10.0, 0.0, 0.0)))
        .chain([(hidden_id, reused_asset, Vec3::new(10.0, 0.0, 0.0))])
        .collect::<Vec<_>>();
    let (mut world, mut scene, mut view, camera, requests) = mesh_scene(&objects, None);
    let assets = std::collections::HashMap::from([(old_asset, old_bytes.clone()), (reused_asset, reused_bytes)]);
    let mut pending = requests;
    let mut hidden_requests = Vec::new();
    for _ in 0..8 {
        let mut old_requests = Vec::new();
        for request in pending.drain(..) {
            if request.request.resource().id == old_asset {
                scene.complete_asset(request.ticket, prepared_asset(request.request, old_bytes.clone()));
            } else {
                hidden_requests.push(request);
            }
        }
        pending = settle_scene(&world, &mut scene, &mut view, Instant::now());
        for request in pending.drain(..) {
            if request.request.resource().id == old_asset {
                old_requests.push(request);
            } else {
                hidden_requests.push(request);
            }
        }
        pending = old_requests;
        if pending.is_empty() {
            break;
        }
    }
    assert!(pending.is_empty(), "the visible asset completes within the bounded chain");

    let mut appearance = ObjectPatch::new(
        UpdateSource::new(fixture_region(&world), OBJECTS + 10, OrderEvidence::Ordered(u64::from(OBJECTS) + 10), 0),
        std::num::NonZeroU32::new(1).expect("blended object local ID"),
    );
    appearance.appearance = FieldUpdate::Set(Appearance::Surface);
    appearance.surface_appearance = Some(
        SurfaceAppearance::new(world::FaceAppearance { color: [255, 0, 0, 128], ..Default::default() }, Vec::new())
            .expect("valid blended appearance"),
    );
    let _ = world.patch_object(appearance).expect("blended appearance applies");
    scene.notify_world_change();
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    complete_mesh_requests(&world, &mut scene, &mut view, &assets, requests);

    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    prepare_until_current(&device, &queue, &mut geometry, &view, camera);
    assert!(geometry.draw_list.has_blended(), "the committed list contains a blended face");

    let target = view
        .objects()
        .find(|object| object.detail().summary.id == Uuid::from_u128(100_001))
        .expect("blended target")
        .detail()
        .summary
        .entity;
    let old_index = geometry.cache.objects[&target];
    let old_group = geometry.cache.items.get(old_index).expect("blended item").faces[0].group;

    let mut pose = ObjectPatch::new(
        UpdateSource::new(fixture_region(&world), OBJECTS + 11, OrderEvidence::Ordered(u64::from(OBJECTS) + 11), 0),
        std::num::NonZeroU32::new(2).expect("opaque object local ID"),
    );
    pose.pose = Some(Pose { position: Some(Vec3::new(10.1, 0.0, 0.0)), rotation: Some(math::Quat::IDENTITY) });
    let _ = world.patch_object(pose).expect("opaque object pose updates");
    scene.notify_world_change();
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    complete_mesh_requests(&world, &mut scene, &mut view, &assets, requests);

    let visible = geometry.cache.visible.clone();
    assert!(geometry.draw_list.request_layout(&visible, camera, view.visibility_version(), false, &geometry.cache.items));
    for _ in 0..16 {
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        if geometry.draw_list.staged_draw_count() == 1 {
            break;
        }
    }
    assert_eq!(geometry.draw_list.staged_draw_count(), 1, "the partial staged build has emitted only its opaque group");
    assert!(geometry.draw_list.layout_pending() && geometry.draw_list.layout_building());

    let mut replacement_appearance = ObjectPatch::new(
        UpdateSource::new(fixture_region(&world), OBJECTS + 12, OrderEvidence::Ordered(u64::from(OBJECTS) + 12), 0),
        std::num::NonZeroU32::new(1).expect("replacement object local ID"),
    );
    replacement_appearance.appearance = FieldUpdate::Set(Appearance::Surface);
    replacement_appearance.surface_appearance = Some(
        SurfaceAppearance::new(world::FaceAppearance { color: [255, 0, 0, 255], ..Default::default() }, Vec::new())
            .expect("valid opaque appearance"),
    );
    let _ = world.patch_object(replacement_appearance).expect("blended object becomes opaque");
    let reorder_camera =
        Camera::new(DVec3::new(0.25, 0.0, 0.0), Vec3::X, std::f64::consts::FRAC_PI_3, [128, 128]).expect("first reorder camera");
    scene.set_camera(reorder_camera);
    scene.notify_world_change();
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());
    prepare_frame(&device, &queue, &mut geometry, &view, reorder_camera);
    assert_eq!(geometry.cache.objects.get(&target), Some(&old_index), "the updated item reuses its slab index");
    assert!(!geometry.cache.items.get(old_index).expect("updated item").faces[0].blended);
    assert!(geometry.draw_list.face_key(old_group).is_none(), "reordering releases the removed face's draw group");

    complete_mesh_requests(&world, &mut scene, &mut view, &assets, hidden_requests);
    move_mesh_object(&mut world, hidden_id, u64::from(OBJECTS + 20), Vec3::new(10.0, 1_000.0, 0.0), [255, 255, 255, 255]);
    scene.notify_world_change();
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());
    update_cache_until_current(&mut geometry, &view, reorder_camera);

    let hidden_entity = view
        .objects()
        .find(|object| object.detail().summary.id == Uuid::from_u128(100_000 + u128::from(hidden_id)))
        .expect("decoded hidden object")
        .detail()
        .summary
        .entity;
    assert!(view.object(hidden_entity).and_then(|object| object.mesh()).is_some(), "the hidden mesh decoded");
    assert!(view.object(hidden_entity).is_some_and(|object| object.mesh_matches_geometry()), "the hidden mesh geometry is ready");
    let hidden_index = geometry.cache.objects[&hidden_entity];
    let hidden_group = geometry.cache.items.get(hidden_index).expect("hidden cached item").faces[0].group;
    let hidden_mesh = geometry.cache.items.get(hidden_index).expect("hidden cached item").mesh.id();
    assert_eq!(hidden_group, old_group, "the new cached face reuses the stale blended group ID");
    assert!(!geometry.cache.visible.contains(&hidden_index), "the second asset remains outside the camera");
    assert!(!geometry.meshes.contains_key(&hidden_mesh), "the hidden asset has no GPU allocation");

    let final_camera =
        Camera::new(DVec3::new(0.5, 0.0, 0.0), Vec3::X, std::f64::consts::FRAC_PI_3, [128, 128]).expect("second reorder camera");
    scene.set_camera(final_camera);
    let _ = settle_scene(&world, &mut scene, &mut view, Instant::now());
    prepare_frame(&device, &queue, &mut geometry, &view, final_camera);

    let deadline = Instant::now() + std::time::Duration::from_secs(60);
    loop {
        prepare_frame(&device, &queue, &mut geometry, &view, final_camera);
        assert_draws_resident(&geometry);
        if geometry.cache.rebuild.is_none()
            && geometry.cache.version.as_ref() == Some(&view.version())
            && geometry.draw_list.valid()
            && !geometry.draw_list.layout_pending()
            && !geometry.layout_required
        {
            break;
        }
        assert!(Instant::now() < deadline, "the updated blended layout did not settle");
    }

    let mut clean = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    clean.memory = Arc::new(crate::geometry::Accounting::default());
    prepare_until_current(&device, &queue, &mut clean, &view, final_camera);
    let size = [128, 128];
    let actual_depth = geometry.depth(&device, size).expect("incremental depth target");
    let clean_depth = clean.depth(&device, size).expect("clean depth target");
    let actual = render_pixels(&device, &queue, size, &actual_depth, |pass| geometry.draw(pass));
    let expected = render_pixels(&device, &queue, size, &clean_depth, |pass| clean.draw(pass));
    assert_eq!(actual, expected, "staged appearance updates match a clean layout");
    assert!(actual.iter().any(|pixel| pixel[0] > 16 || pixel[1] > 16 || pixel[2] > 16), "the fixture still renders geometry");

    let memory = geometry.memory.clone();
    let clean_memory = clean.memory.clone();
    drop(geometry);
    drop(clean);
    device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
    assert_eq!(memory.mesh_state().bytes, 0);
    assert_eq!(clean_memory.mesh_state().bytes, 0);
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --run-ignored only -E 'test(gpu_overbudget_visible_subset)'"]
fn gpu_overbudget_visible_subset() {
    let assets = [AssetId::new(Uuid::from_u128(9_911)), AssetId::new(Uuid::from_u128(9_912)), AssetId::new(Uuid::from_u128(9_913))];
    let bytes = test_support::mesh::asset_with_lod_geometry(&[(3, 8, 65_535, 196_605)]);
    let asset_bytes = std::collections::HashMap::from([(assets[0], bytes.clone()), (assets[1], bytes.clone()), (assets[2], bytes)]);
    let objects =
        [(1, assets[0], Vec3::new(2.0, 0.0, 0.0)), (2, assets[1], Vec3::new(4.0, 0.0, 0.0)), (3, assets[2], Vec3::new(6.0, 0.0, 0.0))];
    let (mut world, mut scene, mut view, camera, requests) = mesh_scene(&objects, None);
    complete_mesh_requests(&world, &mut scene, &mut view, &asset_bytes, requests);
    let production_mesh_bytes = view
        .objects()
        .map(|object| mesh_upload_bytes(object.mesh().expect("loaded mesh").1, None).expect("valid mesh upload size"))
        .collect::<Vec<_>>();
    assert!(production_mesh_bytes[0] + production_mesh_bytes[1] < crate::geometry::MESH_BYTES);
    assert!(production_mesh_bytes.iter().sum::<usize>() > crate::geometry::MESH_BYTES);

    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    let settle_deadline = Instant::now() + std::time::Duration::from_secs(60);
    loop {
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        assert!(
            geometry.mesh_admission.visible_ready || geometry.needs_frame(Instant::now()),
            "capacity-only exhaustion must complete readiness instead of leaving the initial scene blank"
        );
        if geometry.cache.rebuild.is_none()
            && geometry.cache.version.as_ref() == Some(&view.version())
            && geometry.draw_list.valid()
            && !geometry.draw_list.layout_pending()
            && geometry.mesh_admission.visible_ready
            && !geometry.mesh_admission.active
            && !geometry.needs_frame(Instant::now())
        {
            break;
        }
        assert!(Instant::now() < settle_deadline, "over-budget visible admission did not publish a ready subset");
    }

    let ordered = geometry.cache.mesh_order.iter().map(|demand| demand.mesh).collect::<Vec<_>>();
    assert_eq!(ordered.len(), 3);
    assert!(geometry.meshes.contains_key(&ordered[0]), "nearest, highest-priority demand is admitted first");
    assert!(geometry.meshes.contains_key(&ordered[1]), "the next priority demand fits below the 64 MiB limit");
    assert!(!geometry.meshes.contains_key(&ordered[2]), "the lower-priority demand remains capacity-blocked");
    assert_eq!(geometry.cache.visible.len(), 2, "the ready subset replaces the empty initial list");
    assert_eq!(geometry.draw_list.visible_items(), 2);
    assert_eq!(geometry.stats.mesh_budget_blocked, 1);
    assert_eq!(geometry.stats.omitted_faces, 8);
    assert_eq!(geometry.stats.omission_reason, Some(crate::geometry::OmissionReason::MeshMemoryBudget));
    assert!(geometry.memory.mesh_state().bytes <= crate::geometry::MESH_BYTES);
    assert!(!geometry.needs_frame(Instant::now()), "permanently blocked demand does not schedule a redraw loop");

    let visits = geometry.mesh_admission.visits_this_frame;
    for _ in 0..3 {
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        assert_eq!(geometry.mesh_admission.visits_this_frame, visits, "a dormant blocked demand is not retried without change");
        assert!(!geometry.needs_frame(Instant::now()));
    }

    let _ = world
        .remove_object(
            UpdateSource::new(fixture_region(&world), 20, OrderEvidence::Ordered(20), 0),
            std::num::NonZeroU32::new(1).expect("first mesh object local id"),
        )
        .expect("first mesh object removal applies");
    scene.notify_world_change();
    settle_scene(&world, &mut scene, &mut view, Instant::now());
    let retry_deadline = Instant::now() + std::time::Duration::from_secs(60);
    loop {
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        if geometry.cache.rebuild.is_none()
            && geometry.cache.version.as_ref() == Some(&view.version())
            && geometry.draw_list.valid()
            && !geometry.draw_list.layout_pending()
            && !geometry.mesh_admission.active
            && geometry.stats.pending_meshes == 0
            && !geometry.needs_frame(Instant::now())
        {
            break;
        }
        assert!(Instant::now() < retry_deadline, "released capacity did not retry the previously blocked demand");
    }
    assert_eq!(geometry.cache.visible.len(), 2);
    assert_eq!(geometry.draw_list.visible_items(), 2);
    assert_eq!(geometry.stats.mesh_budget_blocked, 0);
    assert!(geometry.meshes.contains_key(&ordered[2]), "the previously omitted mesh is admitted after demand changes");

    let mut clean = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    clean.memory = Arc::new(crate::geometry::Accounting::default());
    prepare_until_current(&device, &queue, &mut clean, &view, camera);
    let size = [128, 128];
    let actual_depth = geometry.depth(&device, size).expect("incremental depth target");
    let clean_depth = clean.depth(&device, size).expect("clean depth target");
    let actual = render_pixels(&device, &queue, size, &actual_depth, |pass| geometry.draw(pass));
    let expected = render_pixels(&device, &queue, size, &clean_depth, |pass| clean.draw(pass));
    assert_eq!(actual, expected, "capacity recovery matches a clean geometry build");

    let actual_memory = geometry.memory.clone();
    let clean_memory = clean.memory.clone();
    drop(geometry);
    drop(clean);
    device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
    assert_eq!(actual_memory.mesh_state().bytes, 0);
    assert_eq!(clean_memory.mesh_state().bytes, 0);
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --all-features --locked --run-ignored only -E 'test(gpu_draws_during_scene_churn)'"]
fn gpu_draws_during_scene_churn() {
    draws_during_scene_churn(false, false);
}

#[test]
#[ignore = "requires a Vulkan adapter"]
fn gpu_draws_during_history_churn() {
    draws_during_scene_churn(true, false);
}

#[test]
#[ignore = "requires a Vulkan adapter"]
fn gpu_draws_during_membership_churn() {
    draws_during_scene_churn(false, true);
}

fn draws_during_scene_churn(overflow: bool, membership: bool) {
    const OBJECTS: u32 = 1_100;
    const UPDATES: u64 = 120;
    let first_asset = AssetId::new(Uuid::from_u128(9_921));
    let second_asset = AssetId::new(Uuid::from_u128(9_922));
    let bytes = test_support::mesh::asset_with_lod_faces(&[(3, 1)]);
    let objects = (1..=OBJECTS).map(|id| (id, first_asset, Vec3::new(10.0, 0.0, 0.0))).collect::<Vec<_>>();
    let (mut world, mut scene, mut view, camera) = ready_mesh_scene(&objects, bytes.clone());
    assert_eq!(view.object_count(), OBJECTS as usize);
    assert!(view.objects().all(|object| object.mesh().is_some()), "every fixture object has valid geometry before churn");

    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(crate::geometry::Accounting::default());
    let depth = geometry.depth(&device, [128, 128]).expect("fixture depth target");
    let assets = std::collections::HashMap::from([(first_asset, bytes.clone()), (second_asset, bytes)]);

    for update in 0..UPDATES {
        let order = 1_000_000 + update * 100;
        let mut observation = ObjectObservation::new(
            UpdateSource::new(
                fixture_region(&world),
                u32::try_from(order).expect("fixture update sequence"),
                OrderEvidence::Ordered(order),
                0,
            ),
            Uuid::from_u128(100_001),
            std::num::NonZeroU32::new(1).expect("fixture object local ID"),
            Kind::Object,
            ParentUpdate::Root,
        );
        observation.pose = Some(Pose {
            position: Some(Vec3::new(10.0, if update % 2 == 0 { 0.1 } else { -0.1 }, 0.0)),
            rotation: Some(math::Quat::IDENTITY),
        });
        observation.scale = Some(Vec3::ONE);
        if update == 0 || update == UPDATES / 2 {
            let asset = if update == 0 { second_asset } else { first_asset };
            observation.shape = FieldUpdate::Set(Shape::Mesh { asset_id: asset, flags: SculptFlags::default() });
        }
        assert_matches!(world.observe_object(observation).unwrap().outcome, world::ApplyOutcome::Updated(_));
        if membership {
            let mut added = ObjectObservation::new(
                UpdateSource::new(fixture_region(&world), (order + 1) as u32, OrderEvidence::Ordered(order + 1), 0),
                Uuid::from_u128(200_000 + u128::from(update)),
                std::num::NonZeroU32::new(OBJECTS + update as u32 + 1).unwrap(),
                Kind::Object,
                ParentUpdate::Root,
            );
            added.pose = Some(Pose { position: Some(Vec3::new(10.0, 0.0, 0.0)), rotation: Some(math::Quat::IDENTITY) });
            added.scale = Some(Vec3::ONE);
            added.shape = FieldUpdate::Set(Shape::Mesh { asset_id: first_asset, flags: SculptFlags::default() });
            assert_matches!(world.observe_object(added).unwrap().outcome, world::ApplyOutcome::Introduced(_));
        }
        scene.notify_world_change();
        let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
        complete_mesh_requests(&world, &mut scene, &mut view, &assets, requests);
        if overflow {
            for extra in 1..=65 {
                move_mesh_object(&mut world, 2, order + extra, Vec3::new(10.0, 0.0, 0.0), [extra as u8, 64, 192, 255]);
                scene.notify_world_change();
                settle_scene(&world, &mut scene, &mut view, Instant::now());
            }
        }
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
    }

    let active = render_pixels(&device, &queue, [128, 128], &depth, |pass| geometry.draw(pass));
    let lit = active.iter().filter(|pixel| pixel[0] > 16 || pixel[1] > 16 || pixel[2] > 16).count();
    assert!(
        lit > 100,
        "valid object geometry renders while pose and mesh asset updates continue: {lit} pixels, instances={}, commands={}, rebuild_objects={}, cache_rebuild={}, version_current={}, layout_pending={}, layout_required={}, layout_items={}, mesh_admission={}",
        geometry.stats.instances,
        geometry.stats.draw_commands,
        geometry.stats.rebuild_objects,
        geometry.cache.rebuild.is_some(),
        geometry.cache.version.as_ref() == Some(&view.version()),
        geometry.draw_list.layout_pending(),
        geometry.layout_required,
        geometry.stats.layout_items_visited,
        geometry.mesh_admission.active
    );

    prepare_until_current(&device, &queue, &mut geometry, &view, camera);
    let mut clean = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    clean.memory = Arc::new(crate::geometry::Accounting::default());
    prepare_until_current(&device, &queue, &mut clean, &view, camera);
    let signature = |renderer: &GeometryRenderer| {
        let mut items = renderer
            .cache
            .items
            .iter()
            .map(|(_, item)| {
                (
                    item.id,
                    item.entity,
                    item.mesh.id(),
                    item.position,
                    item.rotation,
                    item.scale,
                    item.faces.iter().map(|face| (face.order, face.indices.clone())).collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        items.sort_unstable_by_key(|item| item.0);
        items
    };
    assert_eq!(signature(&geometry), signature(&clean), "incremental preparation settles to the clean cache");
    let incremental_depth = geometry.depth(&device, [128, 128]).expect("incremental depth target");
    let clean_depth = clean.depth(&device, [128, 128]).expect("clean depth target");
    let incremental = render_pixels(&device, &queue, [128, 128], &incremental_depth, |pass| geometry.draw(pass));
    let expected = render_pixels(&device, &queue, [128, 128], &clean_depth, |pass| clean.draw(pass));
    let mismatch_count = incremental.iter().zip(&expected).filter(|(actual, expected)| actual != expected).count();
    let first_mismatch = incremental
        .iter()
        .zip(&expected)
        .enumerate()
        .find_map(|(index, (actual, expected))| (actual != expected).then_some((index, actual, expected)));
    assert_eq!(
        incremental, expected,
        "quiet incremental rendering matches a clean build: {mismatch_count} pixels differ, first={first_mismatch:?}"
    );
}
