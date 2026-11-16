use std::assert_matches;
use std::{num::NonZeroU32, time::Instant};

use futures::executor::block_on;
use math::{DVec3, Quat, Vec3};
use uuid::Uuid;
use wgpu::{Backends, Device, DeviceDescriptor, InstanceDescriptor, Queue};
use world::{
    Appearance, FaceAppearance, FieldUpdate, Kind, ObjectObservation, OrderEvidence, ParentUpdate, Pose, PrimShape, Shape,
    SurfaceAppearance, UpdateSource, World, WorldLimits,
};

use crate::scene::{Budget, Camera, ReadQuery, ReadReply, ReconcileStep, Reconciler, SceneView};

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
