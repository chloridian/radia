use std::{
    assert_matches,
    cmp::Reverse,
    collections::{BTreeSet, HashMap, HashSet},
    f64::consts::{FRAC_PI_3, TAU},
    num::NonZeroU32,
    sync::{Arc, atomic, mpsc},
    time::{Duration, Instant},
};

use assets::{Failure, FetchResponse, LoadJob, Priority, Request, Store, mesh::MeshLod};
use futures::executor::block_on;
use math::{DVec3, Vec3};
use test_support::{mesh, texture::RGB_J2C};
use utils::AssetId;
use uuid::Uuid;
use wgpu::{
    Backends, BufferDescriptor, BufferUsages, Color, DeviceDescriptor, ErrorFilter, Extent3d, InstanceDescriptor, LoadOp, MapMode,
    Operations, Origin3d, PollType, RenderPass, RenderPassColorAttachment, RenderPassDepthStencilAttachment, RenderPassDescriptor, StoreOp,
    TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
    util::{BufferInitDescriptor, DeviceExt},
};
use world::{
    Appearance, FieldUpdate, Kind, ObjectObservation, OrderEvidence, ParentUpdate, Pose, Region, SculptFlags, Shape, SurfaceAppearance,
    UpdateSource, WorldPose,
};

use super::*;
use crate::{
    geometry::{
        Accounting, INSTANCE_BUFFER_BYTES, MESH_BYTES, OmissionReason, Resource, cache, fixtures,
        fixtures::{scene, scene_with_agent, scene_with_world, vulkan_device},
        frame::MESH_DEMANDS_PER_FRAME,
        instance::{InstanceBuffers, SLOT_BYTES, instance_base, packed_instance},
        storage::Buffers,
    },
    material::{AlphaBehavior, FaceParams, Resolution},
    pipeline::{INSTANCE_BYTES, Pipeline, VERTEX_BYTES},
    scene::{
        AgentPose, AssetChange, AssetRequest, Budget, Camera, DrawnMeshKey, Placement, ReadFailure, ReadQuery, ReadReply, ReconcileStep,
        Reconciler, SceneView, distinct_meshes_for_test,
    },
    texture::{CompressionQuality, MaterialTextures},
};

fn fixture_region(world: &world::World) -> Region {
    world.region_page(None, 64).expect("fixture regions fit one page").regions[0].region
}

fn check_pages(device: &Device, queue: &wgpu::Queue, alpha: u8, separate_pages: bool) {
    let scope = device.push_error_scope(ErrorFilter::Validation);
    let (_scene, view, camera) = scene(alpha);
    let mut geometry = GeometryRenderer::new(device, queue, TextureFormat::Rgba8UnormSrgb);
    let filler = separate_pages.then(|| {
        let (_, shared) = view.objects().next().unwrap().mesh().unwrap();
        assert_matches!(geometry.upload_mesh(device, shared, None, &mut 0, None), MeshUpload::Ready);
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
        assert_eq!(geometry.main_view.batches().len(), grouped_calls, "culled commands share one submission per page");
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
                packed,
                mesh.indices().len() as u32,
            )
        })
        .collect();
    let first_reference = reference_faces(queue, &geometry, reference.iter().map(|(_, _, packed, _)| packed));
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
            pass.set_bind_group(1, geometry.textures.material_binding(MaterialTextures::default()), &[]);
            pass.set_vertex_buffer(1, geometry.instances.face_indices.slice(..));
            for (face, (vertices, indices, _, count)) in (first_reference..).zip(&reference) {
                pass.set_vertex_buffer(0, vertices.slice(..));
                pass.set_index_buffer(indices.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..*count, 0, face..face + 1);
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
                scene.complete_read(job, result.map_err(ReadFailure::World), now);
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
        Uuid::from_u128(u128::from(id)),
        NonZeroU32::new(id).expect("fixture local ID"),
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
        NonZeroU32::new(id).expect("fixture local ID"),
        Kind::Object,
        ParentUpdate::Root,
    );
    observation.pose = Some(Pose { position: Some(position), rotation: Some(math::Quat::IDENTITY) });
    observation.appearance = FieldUpdate::Set(Appearance::Surface);
    observation.surface_appearance =
        Some(SurfaceAppearance::new(world::FaceAppearance { color, ..Default::default() }, Vec::new()).expect("fixture appearance"));
    assert_matches!(world.observe_object(observation).unwrap().outcome, world::ApplyOutcome::Updated(_));
}

fn prepare_until_current(device: &Device, queue: &wgpu::Queue, geometry: &mut GeometryRenderer, view: &SceneView, camera: Camera) {
    let deadline = Instant::now() + Duration::from_secs(60);
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

fn prepare_cache_until_current(device: &Device, queue: &wgpu::Queue, geometry: &mut GeometryRenderer, view: &SceneView, camera: Camera) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        prepare_frame(device, queue, geometry, view, camera);
        assert_draws_resident(geometry);
        if geometry.cache.rebuild.is_none() && geometry.cache.version.as_ref() == Some(&view.version()) {
            return;
        }
        assert!(Instant::now() < deadline, "geometry cache did not observe the current scene version");
    }
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
            NonZeroU32::new(*id).unwrap(),
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
    let camera = Camera::new(DVec3::ZERO, Vec3::X, FRAC_PI_3, [128, 128]).unwrap();
    let (mut scene, mut view) = Reconciler::new(Budget::default());
    scene.set_camera(camera);
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    (world, scene, view, camera, requests)
}

fn prepared_asset(request: Request, bytes: Vec<u8>) -> assets::Status {
    let now = Instant::now();
    let mut store = Store::new(assets::Budget::default());
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

fn prepare_frame(device: &Device, queue: &wgpu::Queue, geometry: &mut GeometryRenderer, view: &SceneView, camera: Camera) {
    prepare_frame_at(device, queue, geometry, view, camera, Instant::now());
}

fn assert_draws_resident(geometry: &GeometryRenderer) {
    for draw in geometry.draw_list.draws().iter().filter(|draw| geometry.draw_list.committed_pins_mesh(draw.mesh)) {
        assert!(geometry.meshes.contains_key(&draw.mesh), "a live committed draw command lost GPU mesh residency");
    }
}

fn committed_world_position(geometry: &GeometryRenderer, slot: u32) -> Option<DVec3> {
    let bytes = geometry.draw_list.instance_transform(slot)?;
    let axis = |index: usize| {
        let start = 48 + index * 4;
        Some(f64::from(f32::from_le_bytes(bytes.get(start..start + 4)?.try_into().ok()?)))
    };
    Some(DVec3::new(axis(0)?, axis(1)?, axis(2)?) + geometry.active_origin)
}

fn hidden_slot(geometry: &GeometryRenderer, slot: u32) -> bool {
    geometry.draw_list.instance_transform(slot).is_some_and(|bytes| bytes.iter().all(|byte| *byte == 0))
}

fn assert_committed_positions(geometry: &GeometryRenderer, turn: u32) {
    const LAGGING: f64 = 16.0;
    const EXACT: f64 = 1e-3;
    let tolerance = if geometry.draw_list.layout_pending() || geometry.layout_required { LAGGING } else { EXACT };
    let cell = |position: DVec3| {
        [(position.x / LAGGING).floor() as i64, (position.y / LAGGING).floor() as i64, (position.z / LAGGING).floor() as i64]
    };
    let mut cells = HashMap::<[i64; 3], Vec<DVec3>>::new();
    for (_, item) in geometry.cache.items.iter() {
        cells.entry(cell(item.position)).or_default().push(item.position);
    }
    let mut slot = 0;
    while let Some(world) = committed_world_position(geometry, slot) {
        if !hidden_slot(geometry, slot) {
            let [x, y, z] = cell(world);
            let near = (-1..=1)
                .flat_map(|dx| (-1..=1).flat_map(move |dy| (-1..=1).map(move |dz| [x + dx, y + dy, z + dz])))
                .filter_map(|key| cells.get(&key))
                .flatten()
                .any(|position| (world - *position).abs().max_element() <= tolerance);
            assert!(
                near,
                "turn {turn}: committed slot {slot} at {world:?} matches no item within {tolerance}; active_origin={:?} cache_origin={:?} rebuild={} layout_pending={} layout_required={}",
                geometry.active_origin,
                geometry.cache.origin,
                geometry.cache.rebuild.is_some(),
                geometry.draw_list.layout_pending(),
                geometry.layout_required,
            );
        }
        slot += 1;
    }
}

fn prepare_frame_at(device: &Device, queue: &wgpu::Queue, geometry: &mut GeometryRenderer, view: &SceneView, camera: Camera, now: Instant) {
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
    device: &Device,
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

fn fill_mesh_pages(device: &Device, geometry: &mut GeometryRenderer) -> Vec<Allocation> {
    let mut pages: Vec<(Arc<Buffers>, u32, u32)> = Vec::new();
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
    tails.sort_unstable_by_key(|(_, vertices, indices)| Reverse(vertices.saturating_add(*indices)));
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

fn reference_faces<'a>(
    queue: &wgpu::Queue,
    geometry: &GeometryRenderer,
    records: impl IntoIterator<Item = &'a [u8; INSTANCE_BYTES]>,
) -> u32 {
    let bytes: Vec<u8> = records.into_iter().flatten().copied().collect();
    let count = bytes.len() / INSTANCE_BYTES;
    let first = geometry.instances.bytes() / INSTANCE_BYTES - count;
    queue.write_buffer(&geometry.instances.instances, (first * INSTANCE_BYTES) as u64, &bytes);
    u32::try_from(first).expect("face slots fit u32")
}

fn render_pixels(
    device: &Device,
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

fn assert_visible_draw_coverage(geometry: &GeometryRenderer) {
    let visible: HashSet<_> = geometry.cache.visible.iter().copied().collect();
    let mut face_slots = HashSet::new();
    for (index, item) in geometry.cache.items.iter() {
        for face in &item.faces {
            match (visible.contains(&index), face.slot) {
                (true, Some(slot)) => assert!(face_slots.insert(slot), "each visible face owns one instance slot"),
                (false, None) | (true, None) => {}
                (false, Some(_)) => panic!("hidden items cannot retain instance slots"),
            }
        }
    }
    let draw_slots: HashSet<_> =
        geometry.draw_list.draws().iter().flat_map(|draw| draw.first..draw.first.saturating_add(draw.count)).collect();
    let draw_only = draw_slots.difference(&face_slots).copied().collect::<Vec<_>>();
    let faces_only = face_slots.difference(&draw_slots).copied().collect::<Vec<_>>();
    assert!(
        draw_only.is_empty() && faces_only.is_empty(),
        "draw commands cover every packed visible face and no hidden face: draw_only={draw_only:?}, faces_only={faces_only:?}"
    );
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo test -p render gpu_removed_pending_reconciles_snapshot -- --ignored"]
fn gpu_removed_pending_reconciles_snapshot() {
    let (device, queue) = vulkan_device();
    let (mut world, mut scene, mut view, camera) = scene_with_world(255, [Vec3::splat(2.0); 2], Default::default());
    let entity =
        view.objects().find(|object| object.detail().summary.id == Uuid::from_u128(1)).expect("replacement target").detail().summary.entity;
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(Accounting::default());
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
    let blocker_bytes = MESH_BYTES - geometry.memory.mesh_state().bytes;
    let blocker = geometry.memory.reserve(blocker_bytes, Resource::Mesh).expect("fill mesh budget");

    let mut patch = world::ObjectPatch::new(
        UpdateSource::new(fixture_region(&world), 20, OrderEvidence::Ordered(20), 0),
        NonZeroU32::new(1).expect("target local id"),
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
            NonZeroU32::new(1).expect("target local id"),
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
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --run-ignored only -E 'test(gpu_repeated_overflow_matches_clean)'"]
fn gpu_repeated_overflow_matches_clean() {
    let asset = AssetId::new(Uuid::from_u128(9801));
    let bytes = mesh::asset_with_lod_faces(&[(3, 1)]);
    let object_count = 1_024u32;
    let object_count_usize = object_count as usize;
    let objects = (1..=object_count).map(|id| (id, asset, Vec3::new(10.0, 0.0, 0.0))).collect::<Vec<_>>();
    let (mut world, mut scene, mut view, camera) = ready_mesh_scene(&objects, bytes);
    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(Accounting::default());
    prepare_until_current(&device, &queue, &mut geometry, &view, camera);
    let old_entity = view
        .objects()
        .find(|object| object.detail().summary.id == Uuid::from_u128(100_001))
        .expect("first fixture object")
        .detail()
        .summary
        .entity;
    let checkpoint = geometry.cache.version.clone().expect("prepared scene checkpoint");

    for order in 1_000..1_065 {
        move_mesh_object(&mut world, 1, order, Vec3::new(10.0, 0.0, 0.0), [order as u8, 64, 192, 255]);
        scene.notify_world_change();
        settle_scene(&world, &mut scene, &mut view, Instant::now());
    }
    let changes = view.changes_since(&checkpoint).expect("same scene session").collect::<Vec<_>>();
    assert!(changes.iter().any(|change| change.reconcile_all), "history loss requests a full derived-cache reconciliation");

    prepare_frame(&device, &queue, &mut geometry, &view, camera);
    assert_committed_positions(&geometry, 0);
    assert!(geometry.cache.rebuild.is_some(), "the first bounded page leaves a large rebuild pending");
    assert!(geometry.needs_frame(Instant::now()), "pending geometry schedules another frame");
    assert!(!geometry.cache.visible.is_empty(), "valid old drawables remain available during catch-up");

    let _ = world
        .remove_object(
            UpdateSource::new(fixture_region(&world), 1_100, OrderEvidence::Ordered(1_100), 0),
            NonZeroU32::new(1).expect("removed local ID"),
        )
        .expect("old object removal applies");
    let mut reused = ObjectObservation::new(
        UpdateSource::new(fixture_region(&world), 1_101, OrderEvidence::Ordered(1_101), 0),
        Uuid::from_u128(100_001),
        NonZeroU32::new(object_count + 1).expect("reused local ID"),
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
            NonZeroU32::new(200).expect("removed local ID"),
        )
        .expect("fixture slot removal applies");
    scene.notify_world_change();
    settle_scene(&world, &mut scene, &mut view, Instant::now());
    assert_eq!(view.object_count(), object_count_usize - 1);

    let visibility_before_insert = view.visibility_version();
    let mut added = ObjectObservation::new(
        UpdateSource::new(fixture_region(&world), 1_103, OrderEvidence::Ordered(1_103), 0),
        Uuid::from_u128(200_902),
        NonZeroU32::new(object_count + 2).expect("new local ID"),
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
    assert_committed_positions(&geometry, 0);
    assert!(geometry.cache.rebuild.is_some(), "a second overflow leaves bounded recovery active");
    assert!(
        geometry.cache.visible.iter().all(|index| geometry.cache.items.get(*index).is_none_or(|item| item.entity != Some(old_entity))),
        "the removed entity disappears from draw visibility immediately, even if its slab slot is reused"
    );
    let far_camera = Camera::new(DVec3::new(10_000.0, 0.0, 0.0), Vec3::X, 1.0, [128, 128]).expect("rebuild camera");
    for camera in [far_camera, camera, far_camera, camera] {
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        assert_committed_positions(&geometry, 0);
        assert!(geometry.cache.rebuild.is_some(), "camera churn does not restart the pending cache pass");
    }

    let deadline = Instant::now() + Duration::from_secs(60);
    for order in 10_000..10_130 {
        assert!(Instant::now() < deadline, "recovery did not stay responsive during updates");
        move_mesh_object(&mut world, 150, order, Vec3::new(10.0, 0.0, 0.0), [order as u8, 128, 64, 255]);
        scene.notify_world_change();
        settle_scene(&world, &mut scene, &mut view, Instant::now());
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        assert_committed_positions(&geometry, 0);
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
    clean.memory = Arc::new(Accounting::default());
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

fn settle_with_meshes(world: &world::World, scene: &mut Reconciler, view: &mut SceneView, assets: &HashMap<AssetId, Vec<u8>>) {
    let requests = settle_scene(world, scene, view, Instant::now());
    let _ = complete_mesh_requests_collect_textures(world, scene, view, assets, requests);
}

fn observe_churn_object(
    world: &mut world::World,
    id: u32,
    order: u64,
    position: Vec3,
    alpha: u8,
    velocity: Option<Vec3>,
    asset_id: Option<AssetId>,
) {
    let mut observation = ObjectObservation::new(
        UpdateSource::new(fixture_region(world), u32::try_from(order).expect("fixture sequence"), OrderEvidence::Ordered(order), 0),
        Uuid::from_u128(100_000 + u128::from(id)),
        NonZeroU32::new(id).expect("fixture local ID"),
        Kind::Object,
        ParentUpdate::Root,
    );
    observation.pose = Some(Pose { position: Some(position), rotation: Some(math::Quat::IDENTITY) });
    observation.linear_motion = velocity.map(|velocity| world::LinearMotion { velocity: Some(velocity), acceleration: None });
    if let Some(asset_id) = asset_id {
        observation.shape = FieldUpdate::Set(Shape::Mesh { asset_id, flags: SculptFlags::default() });
    }
    observation.appearance = FieldUpdate::Set(Appearance::Surface);
    observation.surface_appearance = Some(
        SurfaceAppearance::new(world::FaceAppearance { color: [64, 128, 192, alpha], ..Default::default() }, Vec::new())
            .expect("churn appearance"),
    );
    let _ = world.observe_object(observation).expect("churn observation applies");
}

fn assert_inside_region(geometry: &GeometryRenderer, turn: u32) {
    let inside =
        |position: DVec3, radius: f64| (-radius..=256.0 + radius).contains(&position.x) && (-radius..=256.0 + radius).contains(&position.y);
    let mut largest = 0.0f64;
    for index in &geometry.cache.visible {
        let Some(item) = geometry.cache.items.get(*index) else { continue };
        let bounds = &item.info.bounds;
        let radius = f64::from((bounds.max - bounds.min).length()) * 0.5 * f64::from(item.scale.max_element());
        largest = largest.max(radius);
        assert!(
            inside(item.position, radius),
            "turn {turn}: item {:?} at {:?} lies outside its region (radius {radius})",
            item.id,
            item.position
        );
    }
    if geometry.draw_list.layout_pending() {
        return;
    }
    let mut slot = 0;
    while let Some(drawn) = committed_world_position(geometry, slot) {
        assert!(
            hidden_slot(geometry, slot) || inside(drawn, largest),
            "turn {turn}: slot {slot} draws at {drawn:?} outside the region; active_origin={:?} cache_origin={:?}",
            geometry.active_origin,
            geometry.cache.origin,
        );
        slot += 1;
    }
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --all-features --locked --run-ignored only -E 'test(gpu_origin_crossings_keep_positions)' --no-fail-fast --test-threads=1"]
fn gpu_origin_crossings_keep_positions() {
    const OBJECTS: u32 = 1_100;
    const MESHES: usize = 16;
    const REPLACEMENT_MESHES: usize = 8;
    const CYCLES: u32 = 4;
    const CHURN_STEPS: u32 = 10;
    const HOLD_LIMIT: u32 = 3_000;
    const SWEEP_STEPS: u32 = 30;
    let bytes = mesh::asset_with_lod_faces(&[(3, 1)]);
    let assets = (0..MESHES + REPLACEMENT_MESHES).map(|index| AssetId::new(Uuid::from_u128(98_700 + index as u128))).collect::<Vec<_>>();
    let mesh_bytes = assets.iter().map(|asset| (*asset, bytes.clone())).collect::<HashMap<_, _>>();
    let home = |id: u32| Vec3::new(230.0 + (id % 14) as f32, 230.0 + (id / 14 % 14) as f32, 236.0 + (id / 196 % 6) as f32);
    let alpha = |id: u32| if id.is_multiple_of(5) { 128 } else { 255 };
    let velocity = |id: u32| id.is_multiple_of(8).then_some(Vec3::new(0.05, 0.0, 0.0));
    let camera_at = |turn: u32| {
        let t = f64::from(turn) * TAU;
        let position = DVec3::new(256.0 + 3.0 * (t / 14.0).sin(), 256.0 + 3.0 * (t / 22.0).sin(), 256.0 + 3.0 * (t / 26.0).sin());
        Camera::new(position, Vec3::new(-1.0, -1.0, -1.0), 1.2, [128, 128])
            .expect("walking camera")
            .with_quality(512.0, 4.0)
            .expect("fixture draw distance")
    };
    let objects = (1..=OBJECTS).map(|id| (id, assets[id as usize % MESHES], home(id))).collect::<Vec<_>>();
    let (mut world, mut scene, mut view, _) = ready_mesh_scene(&objects, bytes);
    let mut order = 10_000u64;
    for id in 1..=OBJECTS {
        order += 1;
        observe_churn_object(&mut world, id, order, home(id), alpha(id), velocity(id), None);
    }
    let agent_id = Uuid::from_u128(100_000 + u128::from(OBJECTS) + 1);
    order += 1;
    let mut avatar = ObjectObservation::new(
        UpdateSource::new(fixture_region(&world), u32::try_from(order).expect("fixture sequence"), OrderEvidence::Ordered(order), 0),
        agent_id,
        NonZeroU32::new(OBJECTS + 1).expect("avatar local ID"),
        Kind::Avatar,
        ParentUpdate::Root,
    );
    avatar.pose = Some(Pose { position: Some(Vec3::new(236.0, 236.0, 240.0)), rotation: Some(math::Quat::IDENTITY) });
    avatar.scale = Some(Vec3::ONE);
    avatar.shape = FieldUpdate::Set(Shape::Prim(world::PrimShape::default()));
    avatar.appearance = FieldUpdate::Set(Appearance::Surface);
    avatar.surface_appearance = Some(SurfaceAppearance::new(world::FaceAppearance::default(), Vec::new()).expect("avatar appearance"));
    let _ = world.observe_object(avatar).expect("avatar introduction applies");
    scene.set_camera(camera_at(0));
    scene.notify_world_change();
    settle_with_meshes(&world, &mut scene, &mut view, &mesh_bytes);

    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(Accounting::default());
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut random = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let live = (1..=OBJECTS).collect::<Vec<_>>();
    let mut origins = [BTreeSet::new(), BTreeSet::new(), BTreeSet::new()];
    let mut rebuild_turns = 0u32;
    let mut pending_turns = 0u32;
    let mut blended_turns = 0u32;
    let mut most_committed = 0usize;
    let mut turn = 0u32;
    let mut walk = 0u32;
    for _ in 0..CYCLES {
        for step in 0..CHURN_STEPS + HOLD_LIMIT + SWEEP_STEPS {
            let churning = step < CHURN_STEPS;
            let holding = (CHURN_STEPS..CHURN_STEPS + HOLD_LIMIT).contains(&step);
            if holding && geometry.cache.rebuild.is_none() && !geometry.draw_list.layout_pending() && !geometry.layout_required {
                continue;
            }
            turn += 1;
            walk += u32::from(!holding);
            let camera = camera_at(walk);
            scene.set_camera(camera);
            let drift = (turn % 5) as f32 * 0.01;
            if churning && step == 2 {
                for _ in 0..65 {
                    let id = live[random() as usize % live.len()];
                    order += 1;
                    observe_churn_object(&mut world, id, order, home(id) + Vec3::X * drift, alpha(id), velocity(id), None);
                    scene.notify_world_change();
                    settle_with_meshes(&world, &mut scene, &mut view, &mesh_bytes);
                }
            }
            if churning && step == 5 {
                for _ in 0..8 {
                    let id = live[random() as usize % live.len()];
                    let replacement = assets[MESHES + random() as usize % REPLACEMENT_MESHES];
                    order += 1;
                    observe_churn_object(&mut world, id, order, home(id), alpha(id), velocity(id), Some(replacement));
                }
            }
            for _ in 0..if churning { 128 + random() % 129 } else { 0 } {
                let id = live[random() as usize % live.len()];
                order += 1;
                observe_churn_object(&mut world, id, order, home(id) + Vec3::Y * drift, alpha(id), velocity(id), None);
            }
            scene.notify_world_change();
            settle_with_meshes(&world, &mut scene, &mut view, &mesh_bytes);
            let t = f64::from(walk) * 0.7;
            let pose = WorldPose { position: DVec3::new(236.0 + t.sin(), 236.0 + t.cos(), 240.0), rotation: math::Quat::IDENTITY };
            prepare_agent_frame(&device, &queue, &mut geometry, &view, camera, Some(AgentPose::new(agent_id, pose)));

            assert_committed_positions(&geometry, turn);
            assert_inside_region(&geometry, turn);
            for (set, value) in origins.iter_mut().zip(geometry.active_origin.to_array()) {
                set.insert(value as i64);
            }
            rebuild_turns += u32::from(geometry.cache.rebuild.is_some());
            pending_turns += u32::from(geometry.draw_list.layout_pending());
            blended_turns += u32::from(geometry.draw_list.has_blended());
            most_committed = most_committed.max(geometry.stats.instances);
        }
    }
    assert!(origins.iter().all(|values| values.len() >= 2), "every axis crosses an origin boundary: {origins:?}");
    assert!(rebuild_turns >= 10, "history overflow keeps rebuilds active on enough turns: {rebuild_turns}");
    assert!(pending_turns >= 10, "layouts stay pending across enough turns: {pending_turns}");
    assert!(blended_turns >= 10, "blended faces are committed on enough turns: {blended_turns}");
    assert!(most_committed >= 900, "the fixture commits its objects: {most_committed}");
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --all-features --locked --run-ignored only -E 'test(gpu_late_recovery_applies_changes)' --no-fail-fast --test-threads=1"]
fn gpu_late_recovery_applies_changes() {
    const OBJECTS: u32 = 5_000;
    const PAGE_CHANGES: u32 = 512;
    let asset = AssetId::new(Uuid::from_u128(98_041));
    let bytes = mesh::asset_with_lod_faces(&[(3, 1)]);
    let objects = (1..=OBJECTS).map(|id| (id, asset, Vec3::new(10.0, 0.0, 0.0))).collect::<Vec<_>>();
    let (mut world, mut scene, mut view, camera) = ready_mesh_scene(&objects, bytes);
    let target_entity = view
        .objects()
        .find(|object| object.detail().summary.id == Uuid::from_u128(100_001))
        .expect("late update target exists")
        .detail()
        .summary
        .entity;
    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(Accounting::default());
    prepare_until_current(&device, &queue, &mut geometry, &view, camera);
    assert_eq!(geometry.cache.objects.len(), view.object_count(), "the recovery fixture starts with every object cacheable");
    let checkpoint = geometry.cache.version.clone().expect("initial geometry checkpoint");

    for order in 10_000..10_065 {
        move_mesh_object(&mut world, 1, order, Vec3::new(10.0, 0.0, 0.0), [order as u8, 64, 192, 255]);
        scene.notify_world_change();
        settle_scene(&world, &mut scene, &mut view, Instant::now());
    }
    assert!(
        view.changes_since(&checkpoint).is_some_and(|mut changes| changes.any(|change| change.reconcile_all)),
        "the fixture starts a bounded history recovery"
    );
    prepare_frame(&device, &queue, &mut geometry, &view, camera);
    assert!(geometry.cache.rebuild.is_some(), "history overflow starts recovery");

    let mut order = 20_000;
    for id in 2..=PAGE_CHANGES + 1 {
        let position = Vec3::new(11.0, 0.0, 0.0);
        move_mesh_object(&mut world, id, order, position, [order as u8, 128, 64, 255]);
        order += 1;
        scene.notify_world_change();
        settle_scene(&world, &mut scene, &mut view, Instant::now());
    }

    let progress = |geometry: &GeometryRenderer| geometry.cache.rebuild.as_ref().map(|rebuild| rebuild.progress()).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut trace = Vec::new();
    loop {
        let state = progress(&geometry);
        if state.contains("object_change_page_complete=true") {
            break;
        }
        assert!(geometry.cache.rebuild.is_some(), "recovery finalized before its change scan completed: {state}; trace={trace:?}");
        assert!(geometry.needs_frame(Instant::now()), "active recovery must request its next frame: {state}; trace={trace:?}");
        assert!(Instant::now() < deadline, "recovery did not complete its change scan: {state}; trace={trace:?}");
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        trace.push(progress(&geometry));
        if trace.len() > 16 {
            trace.remove(0);
        }
    }

    let late_position = Vec3::new(10.0, 0.0, 2.0);
    move_mesh_object(&mut world, 1, order, late_position, [order as u8, 32, 224, 255]);
    order += 1;
    scene.notify_world_change();
    settle_scene(&world, &mut scene, &mut view, Instant::now());
    let late_sequence = view.sequence();
    let mut added = ObjectObservation::new(
        UpdateSource::new(
            fixture_region(&world),
            u32::try_from(order).expect("introduction source sequence fits"),
            OrderEvidence::Ordered(order),
            0,
        ),
        Uuid::from_u128(200_001),
        NonZeroU32::new(OBJECTS + 1).expect("new local ID"),
        Kind::Object,
        ParentUpdate::Root,
    );
    added.pose = Some(Pose { position: Some(Vec3::new(10.0, 2.0, 0.0)), rotation: Some(math::Quat::IDENTITY) });
    added.scale = Some(Vec3::ONE);
    added.shape = FieldUpdate::Set(Shape::Prim(world::PrimShape::default()));
    added.appearance = FieldUpdate::Set(Appearance::Surface);
    added.surface_appearance = Some(
        SurfaceAppearance::new(world::FaceAppearance { color: [0, 255, 0, 255], ..Default::default() }, Vec::new())
            .expect("introduced appearance"),
    );
    let outcome = world.observe_object(added).expect("late introduction applies");
    let world::ApplyOutcome::Introduced(added_entity) = outcome.outcome else { panic!("new object creates an entity") };
    scene.notify_world_change();
    settle_scene(&world, &mut scene, &mut view, Instant::now());
    assert!(view.object(added_entity).is_some_and(|object| object.mesh().is_some()), "introduced object is geometry-ready");

    prepare_frame(&device, &queue, &mut geometry, &view, camera);
    assert!(
        geometry.cache.rebuild.is_some(),
        "recovery must not finalize while the view sequence is ahead of its change scan: view_sequence={}, cache_version_current={}, progress={}",
        late_sequence,
        geometry.cache.version.as_ref() == Some(&view.version()),
        progress(&geometry)
    );

    let mut continuation_turns = 0;
    let mut continuation_trace = Vec::new();
    while geometry.needs_frame(Instant::now()) {
        assert!(
            continuation_turns < 300,
            "renderer continuation did not settle after {continuation_turns} turns: rebuild={:?}, cache_objects={}, view_objects={}, cache_version_current={}, changed_objects={}, next_frame={}",
            geometry.cache.rebuild.as_ref().map(|rebuild| rebuild.progress()),
            geometry.cache.objects.len(),
            view.object_count(),
            geometry.cache.version.as_ref() == Some(&view.version()),
            geometry.cache.changed_objects.len(),
            geometry.needs_frame(Instant::now())
        );
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        continuation_turns += 1;
        continuation_trace.push(format!(
            "turn={continuation_turns},view_sequence={},target_z={:?},progress={}",
            view.sequence(),
            geometry.cache.objects.get(&target_entity).and_then(|index| geometry.cache.items.get(*index)).map(|item| item.position.z),
            progress(&geometry)
        ));
        if continuation_trace.len() > 16 {
            continuation_trace.remove(0);
        }
    }
    assert!(!geometry.needs_frame(Instant::now()), "the renderer settles through its continuation contract");
    assert!(geometry.cache.rebuild.is_none());
    assert_eq!(geometry.cache.version.as_ref(), Some(&view.version()));
    let mesh_ready_objects = view.objects().filter(|object| object.mesh().is_some()).count();
    let placed_objects = view.objects().filter(|object| matches!(object.placement(), Placement::Ready(_))).count();
    assert_eq!(
        geometry.cache.objects.len(),
        view.object_count(),
        "cache_objects={}, view_objects={}, mesh_ready_objects={mesh_ready_objects}, placed_objects={placed_objects}, items={}, object_updates={}",
        geometry.cache.objects.len(),
        view.object_count(),
        geometry.cache.items.iter().count(),
        geometry.stats.rebuild_objects
    );
    assert!(geometry.cache.objects.contains_key(&added_entity), "late introduction reaches the render cache");
    for object in view.objects() {
        let entity = object.detail().summary.entity;
        let index = geometry.cache.objects.get(&entity).expect("every live object reaches the render cache");
        let item = geometry.cache.items.get(*index).expect("live object cache item");
        let Placement::Ready(transform) = object.placement() else { panic!("fixture object has a ready pose") };
        assert_eq!(
            item.position, transform.pose.position,
            "accepted poses reach the renderer for {entity:?}; late_sequence={late_sequence}; trace={continuation_trace:?}"
        );
        let material = object.materials().and_then(|materials| materials.first()).expect("fixture face material");
        let expected_color = FaceParams::new(Some(material)).color;
        assert_eq!(item.faces[0].base_params.color, expected_color, "accepted base materials reach the renderer for {entity:?}");
        assert_eq!(item.faces[0].params.color, expected_color, "accepted materials reach the renderer for {entity:?}");
    }

    let mut clean = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    clean.memory = Arc::new(Accounting::default());
    prepare_until_current(&device, &queue, &mut clean, &view, camera);
    let size = [128, 128];
    let incremental_depth = geometry.depth(&device, size).expect("incremental depth target");
    let clean_depth = clean.depth(&device, size).expect("clean depth target");
    let incremental_pixels = render_pixels(&device, &queue, size, &incremental_depth, |pass| geometry.draw(pass));
    let clean_pixels = render_pixels(&device, &queue, size, &clean_depth, |pass| clean.draw(pass));
    assert_eq!(incremental_pixels, clean_pixels, "late poses, materials, and introductions match a clean render");
}

#[test]
#[ignore = "requires a Vulkan adapter; run explicitly with cargo nextest run -p render --run-ignored only gpu_mesh_pages"]
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
#[ignore = "requires a Vulkan adapter; run explicitly with cargo nextest run -p render --run-ignored only -E 'test(gpu_agent_pose_override_patches)'"]
fn gpu_agent_pose_override_patches() {
    let (device, queue) = block_on(async {
        let instance =
            wgpu::Instance::new(InstanceDescriptor { backends: Backends::VULKAN, ..InstanceDescriptor::new_without_display_handle() });
        let adapter = instance.request_adapter(&Default::default()).await.unwrap();
        adapter.request_device(&DeviceDescriptor::default()).await.unwrap()
    });
    let (_scene, view, _) = scene_with_agent();
    let size = [128, 128];
    let agent_id = Uuid::from_u128(1);
    let agent = view.objects().find(|object| object.detail().summary.id == agent_id).expect("fixture avatar");
    let entity = agent.detail().summary.entity;
    let Placement::Ready(simulator_transform) = agent.placement() else { panic!("fixture avatar must have a pose") };
    let camera = Camera::new(DVec3::new(0.0, -2.0, 0.0), Vec3::X, 1.0, size).unwrap();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(Accounting::default());
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
#[ignore = "requires a Vulkan adapter; run with cargo test -p render gpu_large_upload_exceeds_target -- --ignored"]
fn gpu_large_upload_exceeds_target() {
    let asset = AssetId::new(Uuid::from_u128(810));
    let (world, mut scene, mut view, camera, requests) = mesh_scene(&[(1, asset, Vec3::new(2.0, 0.0, 0.0))], None);
    let bytes = mesh::asset_with_lod_geometry(&[(3, 3, 65_535, 65_535)]);
    let header = requests
        .iter()
        .find(|request| matches!(request.request, Request::MeshHeader(id) if id == asset))
        .copied()
        .expect("mesh header request");
    scene.complete_asset(header.ticket, prepared_asset(header.request, bytes.clone()));
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    let high = requests
        .iter()
        .find(|request| matches!(request.request, Request::MeshLod(lod) if lod.lod() == MeshLod::High))
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
#[ignore = "requires a Vulkan adapter; run with cargo test -p render gpu_lod_keeps_fallback -- --ignored"]
fn gpu_lod_keeps_fallback() {
    let target = AssetId::new(Uuid::from_u128(820));
    let other_a = AssetId::new(Uuid::from_u128(821));
    let other_b = AssetId::new(Uuid::from_u128(822));
    let target_bytes = mesh::asset_with_lod_geometry(&[(2, 8, 65_535, 65_535), (3, 8, 65_535, 131_079)]);
    let other_a_bytes = mesh::asset_with_lod_geometry(&[(2, 8, 65_535, 196_605)]);
    let other_b_bytes = mesh::asset_with_lod_geometry(&[(2, 4, 65_535, 65_535)]);
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
        if asset == target && lod.lod() == MeshLod::High {
            high = Some(request);
            continue;
        }
        if lod.lod() == MeshLod::Medium {
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
        Some(DrawnMeshKey::Asset { asset: target, lod: MeshLod::Medium })
    );

    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(Accounting::default());
    for _ in 0..512 {
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        if geometry.cache.rebuild.is_none()
            && geometry.cache.version.as_ref() == Some(&view.version())
            && geometry.draw_list.valid()
            && !geometry.draw_list.layout_pending()
            && !geometry.layout_required
            && geometry.meshes.len() == 3
            && geometry.stats.pending_meshes == 0
        {
            break;
        }
    }
    assert_eq!(geometry.stats.pending_meshes, 0);
    assert!(
        geometry.stats.mesh_bytes > 48 * 1024 * 1024,
        "resident bytes {} across {} meshes; version {:?}, expected {:?}, rebuilding {}, rebuild objects {}, mesh info entries {}, mesh info bytes {}, demands {}, protected {}, active {}, visits {}, uploaded {}",
        geometry.stats.mesh_bytes,
        geometry.meshes.len(),
        geometry.cache.version,
        view.version(),
        geometry.cache.rebuild.is_some(),
        geometry.stats.rebuild_objects,
        geometry.stats.rebuild_mesh_info_entries,
        geometry.stats.rebuild_mesh_info_bytes,
        geometry.cache.mesh_order.len(),
        geometry.cache.admission_protected.len(),
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
    assert!(geometry.memory.mesh_state().bytes <= MESH_BYTES);
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
        assert!(geometry.memory.mesh_state().bytes <= MESH_BYTES);
        assert!(!geometry.needs_frame(Instant::now()), "the blocked upload stays dormant until mesh capacity changes");
    }

    let _applied = world
        .remove_object(
            UpdateSource::new(fixture_region(&world), 30, OrderEvidence::Ordered(30), 0),
            NonZeroU32::new(2).expect("capacity freeing local id"),
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
    assert!(geometry.memory.mesh_state().bytes <= MESH_BYTES);

    assert!(
        !geometry.cache.mesh_order.iter().any(|demand| demand.mesh == fallback_id),
        "promoted fallback no longer reserves upload demand"
    );

    let memory = geometry.memory.clone();
    drop(geometry);
    device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
    assert_eq!(memory.mesh_state().bytes, 0, "renderer retirement releases every mesh page");
}

fn complete_mesh_requests(
    world: &world::World,
    scene: &mut Reconciler,
    view: &mut SceneView,
    assets: &HashMap<AssetId, Vec<u8>>,
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
    assets: &HashMap<AssetId, Vec<u8>>,
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
#[ignore = "requires a Vulkan adapter; run with cargo test -p render --lib gpu_invisible_face_allows_mesh -- --ignored"]
fn gpu_invisible_face_allows_mesh() {
    let asset = AssetId::new(Uuid::from_u128(864));
    let visible_texture = AssetId::new(Uuid::from_u128(865));
    let hidden_texture = AssetId::new(Uuid::from_u128(866));
    let bytes = mesh::asset_with_lod_faces(&[(2, 2), (3, 2)]);
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
    let mesh_assets = HashMap::from([(asset, bytes)]);
    complete_mesh_requests(&world, &mut scene, &mut view, &mesh_assets, requests);
    for request in visible_texture_requests {
        scene.complete_asset(request.ticket, prepared_asset(request.request, RGB_J2C.to_vec()));
    }
    let _requests = settle_scene(&world, &mut scene, &mut view, Instant::now());

    let object = view.objects().next().expect("mesh object");
    let entity = object.detail().summary.entity;
    let materials = object.materials().expect("mesh face materials");
    let opaque = materials.iter().find(|face| face.slot == 0).expect("opaque face material");
    let invisible = materials.iter().find(|face| face.slot == 1).expect("invisible face material");
    assert_eq!(opaque.alpha, AlphaBehavior::Opaque);
    assert_eq!(invisible.alpha, AlphaBehavior::Blend);
    assert_eq!(invisible.base_color_factor()[3], 0.0);
    assert_matches!(invisible.base_color_texture.as_ref().expect("invisible face texture").state().resolution, Resolution::Pending);
    let (_, shared) = object.mesh().expect("uploaded mesh geometry");
    assert_eq!(shared.mesh().faces().len(), 2, "both authored face slots remain in the selected LOD");

    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    let depth = geometry.depth(&device, [128, 128]).expect("depth target");
    let started = Instant::now();
    for _ in 0..3 {
        prepare_frame_at(&device, &queue, &mut geometry, &view, camera, started);
        if geometry.stats.instances == 1 {
            break;
        }
    }
    assert!(geometry.meshes.contains_key(&shared.id()), "the two-face LOD has been uploaded to the GPU");
    assert_eq!(geometry.stats.instances, 1, "the mesh object appears as soon as its geometry is resident");
    let opaque_image = opaque.base_color_texture.as_ref().and_then(|binding| binding.image()).expect("decoded opaque texture");
    assert!(!geometry.textures.is_resident(opaque_image.id()), "the mesh object did not wait for its texture");
    let job = geometry.next_texture_job().expect("opaque face texture upload job");
    geometry.complete_texture(job.run(CompressionQuality::Fast));
    prepare_frame_at(&device, &queue, &mut geometry, &view, camera, started);
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
}

#[test]
#[ignore = "requires a Vulkan adapter"]
fn instance_buffer_grows_on_commit() {
    let (device, queue) = vulkan_device();
    let camera = |draw_distance: f64, direction: Vec3| {
        Camera::new(DVec3::ZERO, direction, 1.0, [128, 128]).unwrap().with_quality(draw_distance, 1.0).unwrap()
    };
    let near = camera(32.0, Vec3::X);
    let (_world, _scene, view) = fixtures::object_scene_with(16, camera(256.0, Vec3::X), |id| {
        let x = if id <= 12 { 8.0 + id as f32 } else { -38.0 - id as f32 };
        (Vec3::new(x, 0.0, 0.0), world::PrimShape::default())
    });
    let mut reference = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    reference.memory = Arc::new(Accounting::default());
    prepare_until_current(&device, &queue, &mut reference, &view, near);
    let near_slots = reference.draw_list.slot_count();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(Accounting::default());
    geometry.instances = InstanceBuffers::new(&device, &geometry.memory, near_slots);
    geometry.pipelines.set_faces(&device, &geometry.textures, &geometry.instances.instances);
    prepare_until_current(&device, &queue, &mut geometry, &view, near);
    assert_eq!(geometry.stats.instance_buffer_growths, 0, "the near layout fits the initial buffer");

    let size = [128, 128];
    let compare = |geometry: &mut GeometryRenderer, reference: &mut GeometryRenderer, camera: Camera, reason: &str| {
        prepare_until_current(&device, &queue, reference, &view, camera);
        prepare_until_current(&device, &queue, geometry, &view, camera);
        let grown_depth = geometry.depth(&device, size).expect("grown depth target");
        let reference_depth = reference.depth(&device, size).expect("reference depth target");
        let grown = render_pixels(&device, &queue, size, &grown_depth, |pass| geometry.draw(pass));
        let expected = render_pixels(&device, &queue, size, &reference_depth, |pass| reference.draw(pass));
        assert!(expected.iter().any(|pixel| *pixel != expected[0]), "the reference draws objects: {reason}");
        assert_eq!(grown, expected, "{reason}");
    };
    compare(&mut geometry, &mut reference, camera(256.0, Vec3::X), "objects placed before growth still draw");
    assert_eq!(geometry.stats.instance_buffer_growths, 1);
    assert!(geometry.instances.cap() * SLOT_BYTES <= INSTANCE_BUFFER_BYTES, "growth stays within the instance budget");
    assert_eq!(geometry.stats.omitted_faces, 0);
    assert!(geometry.draw_list.slot_count() > near_slots, "the far objects enlarge the layout");
    assert!(geometry.stats.instance_buffer_bytes >= geometry.draw_list.slot_count() * INSTANCE_BYTES);
    assert_eq!(
        geometry.memory.instances.load(atomic::Ordering::Acquire),
        geometry.stats.instance_buffer_bytes / INSTANCE_BYTES * SLOT_BYTES,
        "the grown buffers replace the initial charge"
    );
    assert!(geometry.cache.items.iter().all(|(_, item)| item.faces.iter().all(|face| face.slot.is_some())), "every face is placed");
    compare(&mut geometry, &mut reference, camera(256.0, Vec3::NEG_X), "objects placed by the growing layout draw");
}

#[test]
#[ignore = "requires a Vulkan adapter"]
fn gpu_blended_rotation_uploads_instances() {
    let (device, queue) = vulkan_device();
    let (_scene, view, _) = scene(128);
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.instances.instances = device.create_buffer(&BufferDescriptor {
        label: None,
        size: geometry.instances.bytes() as u64,
        usage: BufferUsages::STORAGE | BufferUsages::COPY_DST | BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let camera = |x| Camera::new(DVec3::new(x, -30.0, 0.0), Vec3::Y, 1.0, [128, 128]).unwrap();
    prepare_frame(&device, &queue, &mut geometry, &view, camera(0.0));
    assert!(geometry.draw_list.has_blended());
    let instances = geometry.stats.instances;
    let slots = geometry.draw_list.draws().iter().map(|draw| draw.first + draw.count).max().unwrap();
    let bytes = u64::from(slots) * INSTANCE_BYTES as u64;
    let expected_instances = |geometry: &GeometryRenderer| {
        let mut expected = vec![0; bytes as usize];
        for (_, item) in geometry.cache.items.iter() {
            for face in &item.faces {
                let Some(slot) = face.slot else { continue };
                let start = slot as usize * INSTANCE_BYTES;
                let end = start + INSTANCE_BYTES;
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
        let (sent, received) = mpsc::channel();
        readback.slice(..).map_async(MapMode::Read, move |result| sent.send(result).unwrap());
        device.poll(PollType::Wait { submission_index: None, timeout: None }).unwrap();
        received.recv().unwrap().unwrap();
        let result = readback.get_mapped_range(..).unwrap().to_vec();
        readback.unmap();
        result
    };
    let before = read_instances(&geometry.instances.instances);
    assert_eq!(before, expected_instances(&geometry), "the initial upload matches active transforms and materials");
    let order_before = geometry.main_view.blended_order();
    prepare_frame(&device, &queue, &mut geometry, &view, camera(26.0));
    assert_eq!(geometry.stats.instances, instances);
    assert_eq!(geometry.stats.layouts, 0);
    assert_eq!(geometry.stats.blended_sorts, 1);
    let expected = expected_instances(&geometry);
    let actual = read_instances(&geometry.instances.instances);
    assert_ne!(geometry.main_view.blended_order(), order_before, "the camera change reorders the blended draw commands");
    assert_eq!(expected, before, "camera sorting leaves blended instance slots unchanged");
    assert_eq!(actual, expected, "the GPU instance buffer keeps its transforms and materials");
    assert_eq!(geometry.stats.draw_instance_write_calls, 0, "sorting writes no instance data");
    assert!(geometry.stats.draw_command_bytes_uploaded > 0, "the reordered blended commands reach the GPU buffer");
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --run-ignored only -E 'test(gpu_admission_work_bounded_ordered)'"]
fn gpu_admission_work_bounded_ordered() {
    const RESIDENT_DEMANDS: usize = 24;
    const EXTRA_DEMANDS: usize = 160;
    let (_scene, view, camera) = scene(255);
    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(Accounting::default());
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
    let resident_meshes = distinct_meshes_for_test(RESIDENT_DEMANDS);
    geometry.cache.clear_mesh_demands();
    for mesh in &resident_meshes {
        cache::add_mesh_demand_for_test(&mut geometry.cache, &mut geometry.stats, mesh, (1.0, 0.0));
    }
    for _ in 0..8 {
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        if !geometry.mesh_admission.active && resident_meshes.iter().all(|mesh| geometry.meshes.contains_key(&mesh.id())) {
            break;
        }
    }
    assert!(resident_meshes.iter().all(|mesh| geometry.meshes.contains_key(&mesh.id())), "resident demands are uploaded first");
    assert!(!geometry.mesh_admission.active, "the resident demand pass completes before cold demands are added");

    let extra_meshes = distinct_meshes_for_test(EXTRA_DEMANDS);
    geometry.cache.clear_mesh_demands();
    for object in view.objects() {
        let (_, mesh) = object.mesh().expect("fixture mesh");
        cache::add_mesh_demand_for_test(&mut geometry.cache, &mut geometry.stats, mesh, (1.0, 0.0));
    }
    for mesh in &resident_meshes {
        cache::add_mesh_demand_for_test(&mut geometry.cache, &mut geometry.stats, mesh, (1.0, 0.0));
    }
    for mesh in &extra_meshes {
        cache::add_mesh_demand_for_test(&mut geometry.cache, &mut geometry.stats, mesh, (0.0, 0.0));
    }
    geometry.prepared.as_mut().expect("active drawable preparation").more_meshes = true;
    let total_demands = 2 + RESIDENT_DEMANDS + EXTRA_DEMANDS;
    assert_eq!(geometry.cache.mesh_order.len(), total_demands);
    prepare_frame(&device, &queue, &mut geometry, &view, camera);
    assert!(geometry.mesh_admission.active, "the visible demand list continues after the per-turn admission limit");
    assert!(!geometry.mesh_admission.visible_ready, "unvisited or upload-blocked demands cannot publish partial visibility");
    assert_eq!(geometry.mesh_admission.visits_this_frame, MESH_DEMANDS_PER_FRAME);
    assert_eq!(geometry.stats.mesh_admission_items_visited, MESH_DEMANDS_PER_FRAME);
    assert!(geometry.pending_meshes.len() <= MESH_DEMANDS_PER_FRAME);
    assert_eq!(geometry.stats.mesh_upload_attempts, 0, "already-resident entries consume the full demand visit slice");
    assert_eq!(geometry.meshes.len(), 2 + RESIDENT_DEMANDS, "resident skips do not start uploads for lower-priority work");
    assert!(geometry.pending_meshes.is_empty(), "resident entries do not enter the blocked pending set");
    let ordered = geometry.cache.mesh_order.iter().map(|demand| demand.mesh).collect::<Vec<_>>();
    assert!(ordered[..MESH_DEMANDS_PER_FRAME].iter().all(|mesh| geometry.meshes.contains_key(mesh)), "admission follows priority order");
    assert!(
        !ordered[..MESH_DEMANDS_PER_FRAME].iter().any(|mesh| extra_meshes.iter().any(|demand| demand.id() == *mesh)),
        "lower-priority demands wait for their turn"
    );
    assert_eq!(geometry.draw_list.draws().len(), active_commands, "partial admissions retain active draw commands");
    assert_eq!(geometry.draw_list.visible_items(), active_instances, "partial admissions retain active instances");
    assert_eq!(geometry.cache.visible.len(), 2, "partial admissions do not replace the current visible scene");

    let previous_revision = geometry.cache.mesh_demand_revision;
    let changed_mesh = extra_meshes.last().expect("extra demand");
    cache::add_mesh_demand_for_test(&mut geometry.cache, &mut geometry.stats, changed_mesh, (2.0, 0.0));
    assert!(geometry.cache.mesh_demand_revision > previous_revision, "a changed priority advances the demand revision");
    assert_eq!(geometry.cache.mesh_order.first().map(|demand| demand.mesh), Some(changed_mesh.id()));
    let previous_resident = geometry.meshes.len();
    prepare_frame(&device, &queue, &mut geometry, &view, camera);
    assert!(geometry.mesh_admission.active, "a changed ordered demand keeps admission moving");
    assert_eq!(geometry.mesh_admission.revision, geometry.cache.mesh_demand_revision);
    assert!(geometry.mesh_admission.visits_this_frame <= MESH_DEMANDS_PER_FRAME);
    assert_eq!(geometry.draw_list.draws().len(), active_commands, "changed demand order preserves active commands");
    assert_eq!(geometry.draw_list.visible_items(), active_instances, "changed demand order preserves active instances");
    assert!(geometry.meshes.len() >= previous_resident);

    let mut resident = geometry.meshes.len();
    let mut complete = false;
    for _ in 0..32 {
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        assert!(geometry.mesh_admission.visits_this_frame <= MESH_DEMANDS_PER_FRAME);
        assert!(geometry.pending_meshes.len() <= MESH_DEMANDS_PER_FRAME);
        assert!(geometry.meshes.len() >= resident, "the bounded continuation makes monotonic resident progress");
        resident = geometry.meshes.len();
        if !geometry.mesh_admission.active
            && resident_meshes.iter().chain(&extra_meshes).all(|mesh| geometry.meshes.contains_key(&mesh.id()))
        {
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
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --run-ignored only -E 'test(gpu_eviction_spares_shown_meshes)'"]
fn gpu_eviction_spares_shown_meshes() {
    let assets = [AssetId::new(Uuid::from_u128(9_921)), AssetId::new(Uuid::from_u128(9_922)), AssetId::new(Uuid::from_u128(9_923))];
    let bytes = mesh::asset_with_lod_geometry(&[(3, 8, 65_535, 196_605)]);
    let asset_bytes = HashMap::from([(assets[0], bytes.clone()), (assets[1], bytes.clone()), (assets[2], bytes)]);
    let objects =
        [(1, assets[0], Vec3::new(-3.0, 0.0, 0.0)), (2, assets[1], Vec3::new(4.0, 0.0, 0.0)), (3, assets[2], Vec3::new(6.0, 0.0, 0.0))];
    let (world, mut scene, mut view, ahead, requests) = mesh_scene(&objects, None);
    complete_mesh_requests(&world, &mut scene, &mut view, &asset_bytes, requests);
    let behind = Camera::new(DVec3::ZERO, Vec3::NEG_X, FRAC_PI_3, [128, 128]).expect("test camera");
    scene.set_camera(behind);
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    complete_mesh_requests(&world, &mut scene, &mut view, &asset_bytes, requests);
    let mesh = |x: f64| {
        view.objects()
            .find(|object| matches!(object.placement(), Placement::Ready(transform) if transform.pose.position.x == x))
            .and_then(|object| object.mesh())
            .map(|(_, mesh)| mesh.id())
            .expect("loaded fixture mesh")
    };
    let [near, shown, far] = [mesh(-3.0), mesh(4.0), mesh(6.0)];

    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(Accounting::default());
    let settle = |geometry: &mut GeometryRenderer, camera: Camera, keep: Option<MeshId>| {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let resident = geometry.meshes.keys().copied().collect::<HashSet<_>>();
            prepare_frame(&device, &queue, geometry, &view, camera);
            assert_draws_resident(geometry);
            for evicted in resident.iter().filter(|mesh| !geometry.meshes.contains_key(*mesh)) {
                assert!(!geometry.draw_list.committed_pins_mesh(*evicted), "an evicted mesh keeps live committed commands");
                assert_ne!(Some(*evicted), keep, "a mesh drawn on screen was evicted");
            }
            if geometry.cache.rebuild.is_none()
                && geometry.cache.version.as_ref() == Some(&view.version())
                && geometry.draw_list.valid()
                && !geometry.draw_list.layout_pending()
                && !geometry.mesh_admission.active
                && !geometry.needs_frame(Instant::now())
            {
                return;
            }
            assert!(Instant::now() < deadline, "geometry did not settle");
        }
    };

    settle(&mut geometry, behind, None);
    assert!(geometry.meshes.contains_key(&near), "the viewed mesh is admitted");
    settle(&mut geometry, ahead, Some(shown));
    assert!(geometry.meshes.contains_key(&shown) && geometry.meshes.contains_key(&far), "both viewed meshes are admitted");
    assert!(!geometry.meshes.contains_key(&near), "the mesh behind the camera made room");

    settle(&mut geometry, behind, None);
    assert!(geometry.meshes.contains_key(&near), "turning back re-admits the evicted mesh");
    assert!(geometry.draw_list.committed_pins_mesh(near), "the re-admitted mesh draws again");
    assert!(geometry.meshes.contains_key(&shown) && !geometry.meshes.contains_key(&far), "the lowest-priority mesh made room");

    settle(&mut geometry, ahead, Some(shown));
    assert!(geometry.meshes.contains_key(&shown) && geometry.meshes.contains_key(&far));
    assert!(!geometry.meshes.contains_key(&near), "the off-screen mesh made room although it outranks the shown one");
}

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --run-ignored only -E 'test(gpu_overbudget_visible_subset)'"]
fn gpu_overbudget_visible_subset() {
    let assets = [AssetId::new(Uuid::from_u128(9_911)), AssetId::new(Uuid::from_u128(9_912)), AssetId::new(Uuid::from_u128(9_913))];
    let bytes = mesh::asset_with_lod_geometry(&[(3, 8, 65_535, 196_605)]);
    let asset_bytes = HashMap::from([(assets[0], bytes.clone()), (assets[1], bytes.clone()), (assets[2], bytes)]);
    let objects =
        [(1, assets[0], Vec3::new(2.0, 0.0, 0.0)), (2, assets[1], Vec3::new(4.0, 0.0, 0.0)), (3, assets[2], Vec3::new(6.0, 0.0, 0.0))];
    let (mut world, mut scene, mut view, camera, requests) = mesh_scene(&objects, None);
    complete_mesh_requests(&world, &mut scene, &mut view, &asset_bytes, requests);
    let production_mesh_bytes = view
        .objects()
        .map(|object| mesh_upload_bytes(object.mesh().expect("loaded mesh").1, None).expect("valid mesh upload size"))
        .collect::<Vec<_>>();
    assert!(production_mesh_bytes[0] + production_mesh_bytes[1] < MESH_BYTES);
    assert!(production_mesh_bytes.iter().sum::<usize>() > MESH_BYTES);

    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(Accounting::default());
    let settle_deadline = Instant::now() + Duration::from_secs(60);
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
    assert_eq!(geometry.stats.omission_reason, Some(OmissionReason::MeshMemoryBudget));
    assert!(geometry.memory.mesh_state().bytes <= MESH_BYTES);
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
            NonZeroU32::new(1).expect("first mesh object local id"),
        )
        .expect("first mesh object removal applies");
    scene.notify_world_change();
    settle_scene(&world, &mut scene, &mut view, Instant::now());
    let retry_deadline = Instant::now() + Duration::from_secs(60);
    let mut retry_turns = 0usize;
    let mut retry_trace = Vec::new();
    loop {
        retry_turns += 1;
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        retry_trace.push(format!(
            "turn={retry_turns}:uploads={},demand_visits={},order_updates={},layouts={},queues={:?},active={},restart={},retry_after_layout={},capacity_blocked={},ready={},pending={},old_pinned={},old_resident={}",
            geometry.stats.mesh_upload_attempts,
            geometry.stats.mesh_demand_items_visited,
            geometry.stats.mesh_demand_order_updates,
            geometry.stats.layouts,
            geometry.draw_list.layout_queues(),
            geometry.mesh_admission.active,
            geometry.mesh_admission.restart_pending,
            geometry.mesh_admission.retry_after_layout,
            geometry.mesh_admission.capacity_blocked,
            geometry.cache.admission_protected.len(),
            geometry.stats.pending_meshes,
            geometry.draw_list.committed_pins_mesh(ordered[0]),
            geometry.meshes.contains_key(&ordered[0]),
        ));
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
        assert!(
            retry_turns < 120 && Instant::now() < retry_deadline,
            "released capacity did not retry the previously blocked demand: turns={retry_turns}, memory={}, old_mesh_resident={}, old_ready={}, old_demanded={}, old_pinned={}, target_resident={}, target_demanded={}, demands={}, demand_index={}, admission_active={}, visible_ready={}, admission_revision={}, demand_revision={}, cache_state=({},{}), memory_state=({},{}), queues={:?}, retired_entries={}, layout_required={}, cache_rebuild={}, next_wake={}, trace={:?}",
            geometry.memory.mesh_state().bytes,
            geometry.meshes.contains_key(&ordered[0]),
            geometry.cache.admission_protected.contains(&ordered[0]),
            geometry.cache.mesh_order_by_id.contains_key(&ordered[0]),
            geometry.draw_list.committed_pins_mesh(ordered[0]),
            geometry.meshes.contains_key(&ordered[2]),
            geometry.cache.mesh_order_by_id.contains_key(&ordered[2]),
            geometry.cache.mesh_order.len(),
            geometry.cache.mesh_order_by_id.len(),
            geometry.mesh_admission.active,
            geometry.mesh_admission.visible_ready,
            geometry.mesh_admission.revision,
            geometry.cache.mesh_demand_revision,
            geometry.cache.mesh_state.bytes,
            geometry.cache.mesh_state.revision,
            geometry.memory.mesh_state().bytes,
            geometry.memory.mesh_state().revision,
            geometry.draw_list.layout_queues(),
            geometry.draw_list.retired_entries_remaining(),
            geometry.layout_required,
            geometry.cache.rebuild.is_some(),
            geometry.needs_frame(Instant::now()),
            retry_trace.iter().rev().take(12).collect::<Vec<_>>(),
        );
    }
    assert_eq!(geometry.cache.visible.len(), 2);
    assert_eq!(geometry.draw_list.visible_items(), 2);
    assert_eq!(geometry.stats.mesh_budget_blocked, 0);
    assert!(geometry.meshes.contains_key(&ordered[2]), "the previously omitted mesh is admitted after demand changes");

    let mut clean = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    clean.memory = Arc::new(Accounting::default());
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
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --all-features --locked --run-ignored only -E 'test(gpu_draws_during_churn)'"]
fn gpu_draws_during_churn() {
    for (history_overflow, membership) in [(false, false), (true, false), (false, true)] {
        draws_during_scene_churn(history_overflow, membership);
    }
}

fn draws_during_scene_churn(overflow: bool, membership: bool) {
    const OBJECTS: u32 = 1_100;
    const UPDATES: u64 = 120;
    let first_asset = AssetId::new(Uuid::from_u128(9_921));
    let second_asset = AssetId::new(Uuid::from_u128(9_922));
    let bytes = mesh::asset_with_lod_faces(&[(3, 1)]);
    let objects = (1..=OBJECTS).map(|id| (id, first_asset, Vec3::new(10.0, 0.0, 0.0))).collect::<Vec<_>>();
    let (mut world, mut scene, mut view, camera) = ready_mesh_scene(&objects, bytes.clone());
    assert_eq!(view.object_count(), OBJECTS as usize);
    assert!(view.objects().all(|object| object.mesh().is_some()), "every fixture object has valid geometry before churn");

    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(Accounting::default());
    let depth = geometry.depth(&device, [128, 128]).expect("fixture depth target");
    let assets = HashMap::from([(first_asset, bytes.clone()), (second_asset, bytes)]);

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
            NonZeroU32::new(1).expect("fixture object local ID"),
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
                NonZeroU32::new(OBJECTS + update as u32 + 1).unwrap(),
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
    let mut continuation_turns = 0;
    while geometry.needs_frame(Instant::now()) {
        assert!(continuation_turns < 120, "production continuation did not settle after {continuation_turns} turns");
        prepare_frame(&device, &queue, &mut geometry, &view, camera);
        continuation_turns += 1;
    }
    let mut clean = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    clean.memory = Arc::new(Accounting::default());
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

#[test]
#[ignore = "requires a Vulkan adapter; run with cargo nextest run -p render --run-ignored only -E 'test(gpu_streaming_under_memory_pressure_does_not_flicker)'"]
fn pressure_streaming_no_flicker() {
    let assets = [AssetId::new(Uuid::from_u128(9_951)), AssetId::new(Uuid::from_u128(9_952)), AssetId::new(Uuid::from_u128(9_953))];
    let bytes = mesh::asset_with_lod_geometry(&[(3, 8, 65_535, 196_605)]);
    let asset_bytes = HashMap::from([(assets[0], bytes.clone()), (assets[1], bytes.clone()), (assets[2], bytes)]);
    let initial_objects =
        [(1, assets[0], Vec3::new(3.0, 0.0, 0.0)), (2, assets[1], Vec3::new(5.0, 0.0, 0.0)), (3, assets[2], Vec3::new(-6.0, 0.0, 0.0))];
    let (mut world, mut scene, mut view, camera, requests) = mesh_scene(&initial_objects, None);
    complete_mesh_requests(&world, &mut scene, &mut view, &asset_bytes, requests);

    let (device, queue) = vulkan_device();
    let mut geometry = GeometryRenderer::new(&device, &queue, TextureFormat::Rgba8UnormSrgb);
    geometry.memory = Arc::new(Accounting::default());
    prepare_until_current(&device, &queue, &mut geometry, &view, camera);
    let initial_meshes: Vec<MeshId> = view
        .objects()
        .filter(|object| matches!(object.placement(), Placement::Ready(transform) if transform.pose.position.x > 0.0))
        .filter_map(|object| object.mesh().map(|(_, mesh)| mesh.id()))
        .collect();
    assert_eq!(initial_meshes.len(), 2);
    for mesh_id in &initial_meshes {
        assert!(geometry.meshes.contains_key(mesh_id), "initial in-view meshes are resident");
    }

    move_mesh_object(&mut world, 3, 20, Vec3::new(2.0, 0.0, 0.0), [255, 255, 255, 255]);
    scene.notify_world_change();
    let requests = settle_scene(&world, &mut scene, &mut view, Instant::now());
    complete_mesh_requests(&world, &mut scene, &mut view, &asset_bytes, requests);

    let mut flickers = 0usize;
    let mut ever_evicted = false;
    let cameras = [
        camera,
        Camera::new(DVec3::new(0.1, 0.0, 0.0), Vec3::X, FRAC_PI_3, [128, 128]).expect("test camera"),
        Camera::new(DVec3::new(0.2, 0.0, 0.0), Vec3::X, FRAC_PI_3, [128, 128]).expect("test camera"),
    ];
    for step_camera in cameras {
        for _ in 0..8 {
            let before: Vec<bool> = initial_meshes.iter().map(|id| geometry.meshes.contains_key(id)).collect();
            prepare_frame(&device, &queue, &mut geometry, &view, step_camera);
            assert_draws_resident(&geometry);
            for (index, mesh_id) in initial_meshes.iter().enumerate() {
                let resident = geometry.meshes.contains_key(mesh_id);
                if before[index] && !resident {
                    ever_evicted = true;
                    flickers += 1;
                }
            }
        }
    }

    assert!(!ever_evicted, "already-resident in-view meshes must not be evicted under GPU memory pressure");
    assert_eq!(flickers, 0, "already-resident in-view meshes must not flicker");
    assert!(geometry.stats.mesh_budget_blocked > 0, "over-budget mesh omission must be counted");
    assert!(geometry.stats.omitted_faces > 0, "omitted faces must be reported");
    assert_eq!(geometry.stats.omission_reason, Some(OmissionReason::MeshMemoryBudget));
}
