use std::{
    collections::HashMap,
    future::{pending, ready},
    num::NonZeroU32,
    time::{Duration, UNIX_EPOCH},
};

use futures::{Future, FutureExt, join};
use math::{DVec3, Quat, Vec3};
use render::{
    material::{AlphaBehavior, Resolution},
    scene::{EnvironmentPreset, EnvironmentProperty, EnvironmentPush, LocalChoice, Status},
};
use test_support::texture::GRAY_4096_J2C;
use utils::{AssetId, ExperienceId};
use uuid::Uuid;
use world::{
    Appearance, FaceAppearance, FieldUpdate, Kind, ObjectObservation, ObjectPatch, OrderEvidence::Ordered, ParentUpdate, Pose, RegionInfo,
    Shape, SurfaceAppearance, TerrainPatch, UpdateSource, World, WorldLimits,
};

use super::*;
use crate::{
    asset_coordinator::AssetCoordinator,
    session::EnvironmentCommand,
    test_helpers,
    world_reader::{ReadRequest, WorldReadHost},
};

async fn select_read<T>(service: &mut WorldReadHost, intake: impl Future<Output = T>) -> Result<T, ReadRequest> {
    if let Some(request) = service.take_reserved() {
        return Err(request);
    }
    let result = {
        let intake = intake.fuse();
        let request = service.next_request().fuse();
        pin_mut!(intake, request);
        futures::select_biased! {
            input = intake => Ok(input),
            request = request => Err(request),
        }
    };
    if result.is_ok() {
        service.record_intake_turn();
    }
    result
}

#[tokio::test]
async fn detail_cursor_tracks_consumed_prefix() {
    const DEPTH: u32 = 180;
    const CHAINS: u32 = 9;
    let count = usize::try_from(DEPTH * CHAINS).unwrap();
    let limits = WorldLimits {
        max_regions: 1,
        max_entities: count,
        max_entities_per_region: count,
        max_tombstones_per_region: 32,
        max_tombstones: 32,
        max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(32),
        max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(32),
        max_journal_entries: count + 32,
        max_journal_changes: count * 2,
        max_region_name_bytes: 64,
        max_recovery_targets: 8,
    };
    let mut world = World::new(limits);
    let region = world.register_region().unwrap();
    let mut sequence = 0_u64;
    for chain in 0..CHAINS {
        let base = chain * DEPTH;
        for index in (0..DEPTH).rev() {
            sequence += 1;
            let local = base + index + 1;
            let parent = if index + 1 == DEPTH { ParentUpdate::Root } else { ParentUpdate::Local(NonZeroU32::new(local + 1).unwrap()) };
            let id = if index == 0 { u128::from(chain + 1) } else { 10_000 + u128::from(chain * DEPTH + index) };
            let observation = ObjectObservation::new(
                UpdateSource::new(region, sequence as u32, Ordered(sequence), 0),
                Uuid::from_u128(id),
                NonZeroU32::new(local).unwrap(),
                Kind::Object,
                parent,
            );
            let _ = world.observe_object(observation).unwrap();
        }
    }

    let mut service = WorldReadHost::new(&world, Handle::current());
    let reader = service.reader();
    let (mut reconciler, mut view) = Reconciler::new(Budget::default());
    reconciler.notify_world_change();
    let mut busy_reads = 0;
    let mut now = Instant::now();
    for _ in 0..10_000 {
        match reconciler.poll(now) {
            ReconcileStep::Read(job) => {
                let mut read = Box::pin(execute_world_read(&reader, &job));
                let reply = loop {
                    match select_read(&mut service, &mut read).await {
                        Ok(reply) => break reply,
                        Err(request) => request.serve(&world),
                    }
                };
                if matches!(&reply, Err(ReadFailure::Busy)) {
                    busy_reads += 1;
                }
                drop(read);
                reconciler.complete_read(job, reply, now);
            }
            ReconcileStep::Publish(batch) => view.apply(batch).unwrap(),
            ReconcileStep::Build(job) => reconciler.complete_mesh(job.run()),
            ReconcileStep::Continue => {}
            ReconcileStep::Wait(until) => now = until,
            ReconcileStep::Idle => {
                assert_eq!(busy_reads, 0, "detail cursor state must not exhaust the reader's admission");
                assert_eq!(view.object_count(), count);
                assert_eq!(view.objects().count(), count);
                service.finish();
                return;
            }
            ReconcileStep::Closed => panic!("the scene reader remains active"),
            ReconcileStep::Asset(_) => {}
        }
    }
    panic!("scene reconciliation must converge with more than eight independent deep targets");
}

fn world() -> World {
    World::new(WorldLimits {
        max_regions: 2,
        max_entities: 16,
        max_entities_per_region: 16,
        max_tombstones_per_region: 16,
        max_tombstones: 16,
        max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(16),
        max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(16),
        max_journal_entries: 32,
        max_journal_changes: 128,
        max_region_name_bytes: 64,
        max_recovery_targets: 8,
    })
}

fn motion_sample(received_at: Instant) -> (World, AgentMotionSample) {
    let (world, region) = test_helpers::registered_region();
    (
        world,
        AgentMotionSample {
            region,
            position: DVec3::new(10.0, 20.0, 30.0),
            scale: Vec3::ONE,
            velocity: Vec3::ZERO,
            acceleration: Vec3::ZERO,
            time_dilation: 1.0,
            ping_delay: None,
            parented: false,
            simulator_rotation: Quat::IDENTITY,
            received_at,
            seed: 1,
        },
    )
}

#[test]
fn dead_reckoning_extrapolates() {
    for (velocity, acceleration, time_dilation, ping_delay, elapsed, expected) in [
        (Vec3::new(2.0, -1.0, 0.5), Vec3::ZERO, 1.0, None, 2, DVec3::new(14.016, 17.992, 31.004)),
        (Vec3::ZERO, Vec3::new(0.0, 9.0, 0.0), 1.0, None, 1, DVec3::new(10.0, 20.0 + 0.5 * 9.0 * (1.0 - 1.0 / 45.0), 30.0)),
        (Vec3::X * 2.0, Vec3::ZERO, 0.5, Some(Duration::from_millis(100)), 1, DVec3::new(11.058, 20.0, 30.0)),
    ] {
        let received_at = Instant::now();
        let (_region_world, mut sample) = motion_sample(received_at);
        sample.velocity = velocity;
        sample.acceleration = acceleration;
        sample.time_dilation = time_dilation;
        sample.ping_delay = ping_delay;
        let mut dead_reckoning = DeadReckoning::default();
        dead_reckoning.replace(Some(sample));

        let position = dead_reckoning.pose(received_at + Duration::from_secs(elapsed), &[]).unwrap().position;

        assert!(position.distance(expected) < 1e-12, "velocity {velocity} acceleration {acceleration}: {position} != {expected}");
    }
}

#[test]
fn dead_reckoning_stops_at_edge() {
    let received_at = Instant::now();
    let (_region_world, mut sample) = motion_sample(received_at);
    sample.position = DVec3::new(255.0, 20.0, 10.0);
    sample.velocity = Vec3::X * 100.0;
    let mut dead_reckoning = DeadReckoning::default();
    dead_reckoning.replace(Some(sample));
    let regions = [motion::RegionBounds { region_id: sample.region.get(), origin: DVec3::ZERO }];

    let pose = dead_reckoning.pose(received_at + Duration::from_secs(2), &regions).unwrap();

    assert_eq!(pose.position.x, 256.0);
}

#[test]
fn new_sample_replaces_state_immediately() {
    let received_at = Instant::now();
    let (_region_world, sample) = motion_sample(received_at);
    let (_next_region_world, next_sample) = motion_sample(received_at);
    let mut dead_reckoning = DeadReckoning::default();
    dead_reckoning.replace(Some(sample));
    let next = AgentMotionSample { position: DVec3::new(400.0, 500.0, 40.0), seed: 2, ..next_sample };
    dead_reckoning.replace(Some(next));

    assert_eq!(dead_reckoning.pose(received_at, &[]).unwrap().position, next.position);
}

#[test]
fn dead_reckoning_tapers_after_silence() {
    let received_at = Instant::now();
    let phase_out_at = received_at + Duration::from_secs_f64(motion::PHASE_OUT_TIME);
    let phase_out_duration = Duration::from_secs_f64(motion::MAX_INTERPOLATION_TIME - motion::PHASE_OUT_TIME);
    let phase_out_end = phase_out_at + phase_out_duration;
    let (_region_world, mut sample) = motion_sample(received_at);
    sample.velocity = Vec3::X;
    let mut dead_reckoning = DeadReckoning::default();
    dead_reckoning.replace(Some(sample));
    dead_reckoning.start_phase_out(Some(phase_out_at));

    assert!((dead_reckoning.pose(phase_out_at + phase_out_duration / 2, &[]).unwrap().position.x - 12.383).abs() < 1e-12);
    assert!((dead_reckoning.pose(phase_out_end, &[]).unwrap().position.x - 12.508).abs() < 1e-12);
    assert!((dead_reckoning.pose(phase_out_end + phase_out_duration, &[]).unwrap().position.x - 12.508).abs() < 1e-12);
    assert!(!dead_reckoning.active(phase_out_end + Duration::from_nanos(1)));
}

#[test]
fn disconnect_tapers_agent_early() {
    let received_at = Instant::now();
    let disconnect_at = received_at + Duration::from_secs_f64(motion::PHASE_OUT_TIME / 4.0);
    let phase_out_duration = Duration::from_secs_f64(motion::MAX_INTERPOLATION_TIME - motion::PHASE_OUT_TIME);
    let phase_out_end = disconnect_at + phase_out_duration;
    let (_region_world, mut sample) = motion_sample(received_at);
    sample.velocity = Vec3::X;
    let mut dead_reckoning = DeadReckoning::default();
    dead_reckoning.replace(Some(sample));
    dead_reckoning.start_phase_out(Some(disconnect_at));

    let at_sample = dead_reckoning.pose(received_at, &[]).unwrap().position;
    let at_disconnect = dead_reckoning.pose(disconnect_at, &[]).unwrap().position;
    let at_end = dead_reckoning.pose(phase_out_end, &[]).unwrap().position;

    assert_eq!(at_disconnect, at_sample + Vec3::X.as_dvec3() * (disconnect_at - received_at).as_secs_f64());
    assert_eq!(at_end, at_disconnect + Vec3::X.as_dvec3() * (phase_out_duration.as_secs_f64() / 2.0));
    assert_eq!(dead_reckoning.pose(phase_out_end + Duration::from_nanos(1), &[]).unwrap().position, at_end);
    assert!(!dead_reckoning.active(phase_out_end));
}

#[test]
fn dead_reckoning_resumes_from_display() {
    let received_at = Instant::now();
    let phase_out_at = received_at + Duration::from_secs_f64(motion::PHASE_OUT_TIME);
    let phase_out_duration = Duration::from_secs_f64(motion::MAX_INTERPOLATION_TIME - motion::PHASE_OUT_TIME);
    let recovered_at = phase_out_at + phase_out_duration / 2;
    let (_region_world, mut sample) = motion_sample(received_at);
    sample.velocity = Vec3::X;
    let mut dead_reckoning = DeadReckoning::default();
    dead_reckoning.replace(Some(sample));
    dead_reckoning.start_phase_out(Some(phase_out_at));
    let displayed = dead_reckoning.pose(recovered_at, &[]).unwrap().position;

    dead_reckoning.resume_at(recovered_at, &[]);

    assert_eq!(dead_reckoning.pose(recovered_at, &[]).unwrap().position, displayed);
    assert_eq!(dead_reckoning.pose(recovered_at + Duration::from_millis(100), &[]).unwrap().position, displayed + DVec3::X * 0.1);
}

#[test]
fn negligible_motion_is_idle() {
    let received_at = Instant::now();
    let (_region_world, mut sample) = motion_sample(received_at);
    sample.velocity = Vec3::splat(1e-4);
    sample.acceleration = Vec3::splat(1e-4);
    let mut dead_reckoning = DeadReckoning::default();
    dead_reckoning.replace(Some(sample));
    assert!(!dead_reckoning.active(received_at));
    sample.velocity = Vec3::X * 0.01;
    dead_reckoning.replace(Some(sample));
    assert!(dead_reckoning.active(received_at));
}

#[test]
fn non_finite_sample_keeps_state() {
    let received_at = Instant::now();
    let (_region_world, valid) = motion_sample(received_at);
    let mut dead_reckoning = DeadReckoning::default();
    dead_reckoning.replace(Some(valid));
    let invalid = AgentMotionSample { velocity: Vec3::splat(f32::INFINITY), ..valid };
    dead_reckoning.replace(Some(invalid));

    assert_eq!(dead_reckoning.pose(received_at, &[]).unwrap().position, valid.position);
}

#[tokio::test]
async fn motion_seeds_camera_before_avatar() {
    let agent_id = Uuid::from_u128(1);
    let now = Instant::now();
    let (_region_world, region) = test_helpers::registered_region();
    let sample = AgentMotionSample {
        region,
        position: DVec3::new(270.0, 520.0, 24.0),
        scale: Vec3::ONE,
        velocity: Vec3::X,
        acceleration: Vec3::ZERO,
        time_dilation: 1.0,
        ping_delay: None,
        parented: false,
        simulator_rotation: Quat::IDENTITY,
        received_at: now,
        seed: 1,
    };
    let budget = Budget::default();
    let assets = AssetCoordinator::new(Handle::current(), pending(), None);
    let (_, view) = Reconciler::new(budget.clone());
    let (sender, incoming) = mpsc::channel(1);
    let (cancel, _) = oneshot::channel();
    let (agent_motion_sender, agent_motion) = watch::channel(Some(sample));
    let mut feed = SceneFeed {
        runtime: Handle::current(),
        view,
        budget,
        assets: assets.handle(),
        agent_id,
        agent: None,
        water_height: None,
        environment: ActiveEnvironment::default(),
        camera: watch::channel(None).0,
        movement: watch::channel(None).0,
        agent_motion,
        motion_circuits: watch::channel(Vec::new()).1,
        motion_circuit_scratch: Vec::new(),
        motion_region_scratch: Vec::new(),
        motion_active: HashMap::new(),
        dead_reckoning: {
            let mut dead_reckoning = DeadReckoning::default();
            dead_reckoning.replace(Some(sample));
            dead_reckoning
        },
        last_movement: None,
        environment_selection: watch::channel(EnvironmentSelection {
            choice: EnvironmentChoice::default(),
            agent_region: None,
            active_textures: BTreeSet::new(),
        })
        .0,
        environment_items: watch::channel(EnvironmentItems::Loading).1,
        incoming,
        _cancel: cancel,
    };
    drop((sender, agent_motion_sender));

    assert!(feed.agent.is_none(), "the self avatar has not entered World yet");
    assert_eq!(
        feed.camera_pose_at(now),
        Some(WorldPose { position: sample.position + sample.velocity.as_dvec3() * 0.008, rotation: sample.simulator_rotation }),
        "follow mode uses the sample until World resolves the avatar"
    );
}

#[tokio::test]
async fn terrain_clamp_respects_parenting() {
    let mut world = world();
    let agent = Uuid::from_u128(1);
    let first_region = world.register_region().unwrap();
    let second_region = world.register_region().unwrap();
    for (index, region) in [first_region, second_region].into_iter().enumerate() {
        let _ = world
            .patch_region(region, RegionInfo { origin: Some(DVec3::new(index as f64 * 256.0, 0.0, 7.0)), ..RegionInfo::default() })
            .unwrap();
    }
    for region in [first_region, second_region] {
        let terrain_source = UpdateSource::new(region, 1, Ordered(1), 0);
        let terrain = TerrainPatch::new(0, 0, Box::new([5.0; world::TERRAIN_PATCH_SAMPLES])).unwrap();
        let _ = world.observe_terrain_patch(terrain_source, terrain).unwrap();
    }
    let _ = world
        .observe_object(ObjectObservation::new(
            UpdateSource::new(first_region, 1, Ordered(1), 0),
            agent,
            NonZeroU32::new(1).unwrap(),
            Kind::Avatar,
            ParentUpdate::Root,
        ))
        .unwrap();
    let _ = world.observe_water_height(UpdateSource::new(second_region, 1, Ordered(1), 0), 31.0).unwrap();
    let budget = Budget::default();
    let (mut scene, view) = Reconciler::new(budget.clone());
    let (sender, incoming) = mpsc::channel(1);
    let (cancel, _) = oneshot::channel();
    let (camera, _) = watch::channel(None);
    let (_agent_motion_sender, agent_motion) = watch::channel(None);
    let assets = AssetCoordinator::new(Handle::current(), pending(), None);
    let (environment_selection, selected_environment) =
        watch::channel(EnvironmentSelection { choice: EnvironmentChoice::default(), agent_region: None, active_textures: BTreeSet::new() });
    let choice = EnvironmentChoice::Local(LocalChoice::Preset(EnvironmentPreset::Midday));
    let mut feed = SceneFeed {
        runtime: Handle::current(),
        view,
        budget,
        assets: assets.handle(),
        agent_id: agent,
        agent: None,
        water_height: None,
        environment: ActiveEnvironment::default(),
        camera,
        movement: watch::channel(None).0,
        agent_motion,
        motion_circuits: watch::channel(Vec::new()).1,
        motion_circuit_scratch: Vec::new(),
        motion_region_scratch: Vec::new(),
        motion_active: HashMap::new(),
        dead_reckoning: DeadReckoning::default(),
        last_movement: None,
        environment_selection,
        environment_items: watch::channel(EnvironmentItems::Loading).1,
        incoming,
        _cancel: cancel,
    };
    let mut selected_local = false;
    let mut hydrate = |world: &World| {
        scene.notify_world_change();
        let now = Instant::now();
        for _ in 0..1000 {
            match scene.poll(now) {
                ReconcileStep::Read(job) => {
                    let reply = match job.query() {
                        ReadQuery::Barrier => Ok(ReadReply::Barrier(world.read_barrier())),
                        ReadQuery::Objects(cursor) => world.object_list_page(*cursor, 256).map(ReadReply::Objects),
                        ReadQuery::Regions(cursor) => world.region_page(*cursor, 64).map(ReadReply::Regions),
                        ReadQuery::Details(entities) => world.object_detail_batch(entities).map(ReadReply::Details),
                        ReadQuery::Changes(cursor) => world.change_page(*cursor, 256).map(ReadReply::Changes),
                        ReadQuery::Terrain(region, cursor) => world.terrain_page(*region, *cursor, 8).map(ReadReply::Terrain),
                        ReadQuery::Environment(region) => Ok(ReadReply::Environment(world.environment(*region))),
                    };
                    scene.complete_read(job, reply.map_err(ReadFailure::World), now);
                }
                ReconcileStep::Publish(batch) => {
                    sender.try_send(batch).unwrap();
                    feed.drain().unwrap();
                }
                ReconcileStep::Build(job) => scene.complete_mesh(job.run()),
                ReconcileStep::Continue => {}
                ReconcileStep::Idle => {
                    if !selected_local && feed.agent().is_some() {
                        feed.set_environment_choice(choice.clone());
                        selected_local = true;
                    }
                    return (
                        feed.water_height,
                        feed.agent.and_then(|entity| feed.view.object(entity)).map(|object| object.detail().summary.entity),
                    );
                }
                _ => panic!("expected scene reconciliation"),
            }
        }
        panic!("scene did not settle");
    };
    let (height, original) = hydrate(&world);
    assert_eq!(height, None, "another region's water must not supply the water height");
    assert_eq!(selected_environment.borrow().agent_region, Some(first_region));
    assert_eq!(selected_environment.borrow().choice, choice);
    let original = original.expect("the observed avatar is available before its water height");
    let _ = world.observe_water_height(UpdateSource::new(first_region, 2, Ordered(2), 0), 17.0).unwrap();
    assert_eq!(hydrate(&world), (Some(24.0), Some(original)));
    let _ = world.retire_region(first_region).unwrap();
    assert_eq!(hydrate(&world), (None, None), "retirement must remove both the avatar and previous water height");
    let _ = world
        .observe_object(ObjectObservation::new(
            UpdateSource::new(second_region, 3, Ordered(3), 0),
            agent,
            NonZeroU32::new(1).unwrap(),
            Kind::Avatar,
            ParentUpdate::Root,
        ))
        .unwrap();
    let (height, replacement) = hydrate(&world);
    assert_eq!(height, Some(38.0), "the new avatar region supplies its own observed height");
    assert_ne!(replacement.expect("the replacement avatar is available"), original, "a retired entity must not remain cached");
    assert_eq!(selected_environment.borrow().choice, choice);
    assert_eq!(selected_environment.borrow().agent_region, Some(second_region));
    drop(hydrate);
    let received_at = Instant::now();
    let (_motion_world, mut falling_agent) = motion_sample(received_at);
    falling_agent.region = second_region;
    falling_agent.position = DVec3::new(266.0, 10.0, -10.0);
    falling_agent.velocity = Vec3::NEG_Z * 20.0;
    feed.dead_reckoning.replace(Some(falling_agent));
    let pose = feed.camera_pose_at(received_at + Duration::from_secs(1)).expect("self motion pose");
    assert_eq!(pose.position.z, 12.5);

    let (_seated_world, mut seated_agent) = motion_sample(received_at);
    seated_agent.region = second_region;
    seated_agent.position = DVec3::new(266.0, 10.0, -10.0);
    seated_agent.parented = true;
    feed.dead_reckoning.replace(Some(seated_agent));
    let pose = feed.camera_pose_at(received_at).expect("seated self motion pose");
    assert_eq!(pose.position.z, -10.0);
}

#[tokio::test]
async fn pending_size_wins_before_acquisition() {
    let (_region_world, region) = test_helpers::registered_region();
    let id = AssetId::new(Uuid::from_u128(101));
    let (mut scene, _) = Reconciler::new(Budget::default());
    scene.set_active_environment_textures(Some(region), BTreeSet::from([id]));
    let request = match scene.poll(Instant::now()) {
        ReconcileStep::Asset(AssetChange::Request(request)) => request,
        _ => panic!("environment texture request is queued"),
    };
    let ticket = request.ticket;
    let coordinator = AssetCoordinator::new(Handle::current(), pending(), None);
    let mut subscriptions = AssetSubscriptions::new(coordinator.handle());
    subscriptions.apply_change(AssetChange::Request(request));
    subscriptions.apply_change(AssetChange::UpdateSize { ticket, size: assets::TextureSize::new(256) });
    subscriptions.apply_change(AssetChange::UpdateSize { ticket, size: assets::TextureSize::new(512) });

    let mut store = assets::Store::new(assets::Budget::default());
    let route = store.register_route().expect("asset route");
    let now = Instant::now();
    let demand =
        store.request_with_importance(route, request.request, request.priority, request.importance, now).expect("initial texture demand");
    subscriptions.resume(ticket, false, Some(demand));
    let assets::LoadJob::Fetch(fetch) = store.next_job(now).expect("texture fetch") else { panic!("texture demand fetches") };
    let input = fetch.complete(Ok(GRAY_4096_J2C.to_vec())).expect("fixture response fits");
    let completion = store.start_decode(input).expect("decode admission").run();
    store.complete(completion, now);

    let assets::Status::Ready(assets::Content::Texture(texture)) = subscriptions.demands[&ticket].status() else {
        panic!("texture demand becomes ready")
    };
    assert_eq!(texture.info().expect("decoded image info").size, [512; 2]);

    drop(subscriptions);
    store.close();
    coordinator.stop().await;
}

#[tokio::test]
async fn pending_asset_preserves_scene() {
    let mut world = world();
    let region = world.register_region().unwrap();
    let _ = world.patch_region(region, RegionInfo { origin: Some(DVec3::ZERO), ..RegionInfo::default() }).unwrap();
    let mut object = ObjectObservation::new(
        UpdateSource::new(region, 1, Ordered(1), 0),
        Uuid::from_u128(1),
        NonZeroU32::new(1).unwrap(),
        Kind::Object,
        ParentUpdate::Root,
    );
    object.pose = Some(Pose { position: Some(Vec3::new(10.0, 0.0, 0.0)), rotation: Some(Quat::IDENTITY) });
    object.scale = Some(Vec3::ONE);
    object.shape = FieldUpdate::Set(Shape::Prim(Default::default()));
    object.appearance = FieldUpdate::Set(Appearance::Surface);
    object.surface_appearance = Some(
        SurfaceAppearance::new(
            FaceAppearance { texture: Some(AssetId::new(Uuid::from_u128(100))), color: [255, 0, 0, 255], ..FaceAppearance::default() },
            Vec::new(),
        )
        .unwrap(),
    );
    let _ = world.observe_object(object).unwrap();
    let mut reads = WorldReadHost::new(&world, Handle::current());
    let assets = AssetCoordinator::new(Handle::current(), pending(), None);
    let (_pushes, environments) = EnvironmentInbox::channel();
    let (_agent_motion_sender, agent_motion) = watch::channel(None);
    let mut feed = SceneFeed::start(
        SceneSources {
            reader: reads.reader(),
            environments,
            assets: assets.handle(),
            motion_circuits: watch::channel(Vec::new()).1,
            inventory_updates: watch::channel(None).1,
        },
        Uuid::from_u128(2),
        watch::channel(None).0,
        watch::channel(None).0,
        agent_motion,
        Budget::default(),
        WorkerPool::new(1),
        Handle::current(),
        || {},
    );
    feed.set_camera(Some(Camera::new(DVec3::ZERO, Vec3::X, 1.0, [1000, 1000]).unwrap()));
    let receiving = async {
        loop {
            let batch = feed.incoming.recv().await.expect("scene remains active");
            feed.view.apply(batch).unwrap();
            if let Some(face) = feed.view.objects().next().and_then(|object| object.materials()).and_then(|faces| faces.first())
                && let Some(binding) = &face.base_color_texture
                && binding.state().resolution == Resolution::Pending
            {
                assert_eq!(face.base_color_factor(), [1.0, 0.0, 0.0, 1.0]);
                assert!(binding.image().is_none());
                assert_eq!(face.alpha, AlphaBehavior::Unknown);
                break;
            }
        }
    }
    .fuse();
    let serving = async {
        loop {
            let Err(request) = select_read(&mut reads, pending::<()>()).await else { unreachable!() };
            request.serve(&world);
        }
    }
    .fuse();
    pin_mut!(receiving, serving);
    futures::select_biased! {
        _ = receiving => {}
        _ = serving => unreachable!(),
    }
}

#[tokio::test]
async fn stalled_publication_preserves_intake() {
    let mut world = world();
    let region = world.register_region().unwrap();
    let _ = world
        .observe_object(ObjectObservation::new(
            UpdateSource::new(region, 1, Ordered(1), 0),
            Uuid::from_u128(1),
            NonZeroU32::new(1).unwrap(),
            Kind::Object,
            ParentUpdate::Root,
        ))
        .unwrap();
    let mut service = WorldReadHost::new(&world, Handle::current());
    let reader = service.reader();
    let assets = AssetCoordinator::new(Handle::current(), pending(), None);
    let (pushes, environments) = EnvironmentInbox::channel();
    let (_agent_motion_sender, agent_motion) = watch::channel(None);
    let mut feed = SceneFeed::start(
        SceneSources {
            reader: reader.clone(),
            environments,
            assets: assets.handle(),
            motion_circuits: watch::channel(Vec::new()).1,
            inventory_updates: watch::channel(None).1,
        },
        Uuid::from_u128(1),
        watch::channel(None).0,
        watch::channel(None).0,
        agent_motion,
        Budget::default(),
        WorkerPool::new(1),
        Handle::current(),
        || {},
    );
    let batch = {
        let receiving = async {
            loop {
                let batch = feed.incoming.recv().await.expect("live Scene publication");
                if batch.status() == Status::Ready {
                    break batch;
                }
                feed.view.apply(batch).unwrap();
            }
        }
        .fuse();
        let serving = async {
            loop {
                let Err(request) = select_read(&mut service, pending::<()>()).await else { unreachable!() };
                request.serve(&world);
            }
        }
        .fuse();
        pin_mut!(receiving, serving);
        futures::select_biased! {
            batch = receiving => batch,
            _ = serving => unreachable!(),
        }
    };
    assert_eq!(feed.view.status(), Status::Loading);
    let initial = world.revision();
    let (barrier, turns) = join!(reader.barrier(), async {
        let mut turns = 0;
        while turns < 1024 {
            match select_read(&mut service, ready(())).await {
                Ok(()) => {
                    turns += 1;
                    let mut patch = ObjectPatch::new(
                        UpdateSource::new(region, turns + 1, Ordered(u64::from(turns + 1)), 0),
                        NonZeroU32::new(1).unwrap(),
                    );
                    patch.pose = Some(Pose { position: Some(Vec3::new(turns as f32, 0.0, 0.0)), rotation: None });
                    let _ = world.patch_object(patch).unwrap();
                    service.publish(&world);
                }
                Err(request) => request.serve(&world),
            }
        }
        turns
    });
    assert_eq!(turns, 1024);
    assert!(barrier.unwrap().revision > initial);
    assert!(world.revision() > initial);
    let experience = ExperienceId::new(Uuid::from_u128(44)).unwrap();
    let received_at = Instant::now() - Duration::from_secs(2);
    let push = EnvironmentPush::Partial {
        experience,
        sky: vec![EnvironmentProperty { name: "gamma".into(), source: b"r1.4".as_slice().into() }],
        water: Vec::new(),
        transition: Duration::from_secs(10),
    };
    assert!(pushes.try_send(EnvironmentCommand { region, received_at, push }).is_ok());
    feed.view.apply(batch).unwrap();
    assert_eq!(feed.view.object_count(), 1);
    {
        let receiving = async {
            loop {
                let batch = feed.incoming.recv().await.expect("live Scene publication");
                feed.view.apply(batch).unwrap();
                if let Some(environment) = feed.view.viewer_environment() {
                    assert_eq!(environment.sky_properties()["gamma"].experience(), experience);
                    break;
                }
            }
        }
        .fuse();
        let serving = async {
            loop {
                let Err(request) = select_read(&mut service, pending::<()>()).await else { unreachable!() };
                request.serve(&world);
            }
        }
        .fuse();
        {
            pin_mut!(receiving, serving);
            futures::select_biased! { _ = receiving => {}, _ = serving => unreachable!() }
        }
    }
    let mut active = ActiveEnvironment::default();
    assert!(active.update(&feed.view, Some((region, 0.0)), UNIX_EPOCH, received_at + Duration::from_secs(2),));
    assert!((active.evaluated().unwrap().sky.gamma - 1.08).abs() < 0.00001);

    service.finish();
    assert!(feed.incoming.recv().await.is_none());
    assert_eq!(feed.view.status(), Status::Closed);
}

#[tokio::test]
async fn cached_burst_reaches_scene() {
    const BURST_OBJECTS: usize = 20_000;
    let mut world = World::new(WorldLimits {
        max_regions: 33,
        max_entities: 65_536,
        max_entities_per_region: 65_536,
        max_tombstones_per_region: 4_096,
        max_tombstones: 16_384,
        max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(4_096),
        max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(16_384),
        max_journal_entries: 4_096,
        max_journal_changes: 65_536,
        max_region_name_bytes: 256,
        max_recovery_targets: 65_536,
    });
    let region = world.register_region().unwrap();
    let _ = world.patch_region(region, RegionInfo { origin: Some(DVec3::ZERO), ..RegionInfo::default() }).unwrap();
    let terrain = TerrainPatch::new(0, 0, Box::new([20.0; world::TERRAIN_PATCH_SAMPLES])).unwrap();
    let _ = world.observe_terrain_patch(UpdateSource::new(region, 1, Ordered(1), 0), terrain).unwrap();

    let mut service = WorldReadHost::new(&world, Handle::current());
    let assets = AssetCoordinator::new(Handle::current(), pending(), None);
    let (_pushes, environments) = EnvironmentInbox::channel();
    let (_agent_motion_sender, agent_motion) = watch::channel(None);
    let mut feed = SceneFeed::start(
        SceneSources {
            reader: service.reader(),
            environments,
            assets: assets.handle(),
            motion_circuits: watch::channel(Vec::new()).1,
            inventory_updates: watch::channel(None).1,
        },
        Uuid::from_u128(1),
        watch::channel(None).0,
        watch::channel(None).0,
        agent_motion,
        Budget::default(),
        WorkerPool::new(1),
        Handle::current(),
        || {},
    );

    time::timeout(Duration::from_secs(30), async {
        let receiving = async {
            while feed.view.status() != Status::Ready {
                let batch = feed.incoming.recv().await.expect("scene feed stayed active before initial Ready");
                feed.view.apply(batch).unwrap();
            }
        }
        .fuse();
        let serving = async {
            loop {
                let Err(request) = select_read(&mut service, pending::<()>()).await else { unreachable!() };
                request.serve(&world);
            }
        }
        .fuse();
        pin_mut!(receiving, serving);
        futures::select_biased! {
            _ = receiving => {}
            _ = serving => unreachable!(),
        }
    })
    .await
    .expect("initial Ready publication did not stall");

    assert_eq!(feed.view.status(), Status::Ready);
    assert!(feed.view.patch_count() > 0, "initial Ready must include terrain");

    for index in 0..BURST_OBJECTS {
        let seq = u64::try_from(index + 2).unwrap();
        let local_id = NonZeroU32::new(u32::try_from(index + 1).unwrap()).unwrap();
        let mut observation = ObjectObservation::new(
            UpdateSource::new(region, u32::try_from(seq).unwrap(), Ordered(seq), 0),
            Uuid::from_u128(100 + u128::from(seq)),
            local_id,
            Kind::Object,
            ParentUpdate::Root,
        );
        observation.pose = Some(Pose {
            position: Some(Vec3::new((index % 256) as f32, ((index / 256) % 256) as f32, 25.0)),
            rotation: Some(Quat::IDENTITY),
        });
        observation.scale = Some(Vec3::ONE);
        observation.shape = FieldUpdate::Set(Shape::Prim(Default::default()));
        let _ = world.observe_object(observation).unwrap();
    }
    service.publish(&world);

    let burst_result = time::timeout(Duration::from_secs(30), async {
        let receiving = async {
            while feed.view.object_count() < BURST_OBJECTS || feed.view.status() != Status::Ready {
                let Some(batch) = feed.incoming.recv().await else {
                    return Err("scene feed exited before publishing burst");
                };
                feed.view.apply(batch).unwrap();
            }
            Ok(())
        }
        .fuse();
        let serving = async {
            loop {
                let Err(request) = select_read(&mut service, pending::<()>()).await else { unreachable!() };
                request.serve(&world);
            }
        }
        .fuse();
        pin_mut!(receiving, serving);
        futures::select_biased! {
            result = receiving => result,
            _ = serving => unreachable!(),
        }
    })
    .await;

    assert_eq!(
        burst_result,
        Ok(Ok(())),
        "cached burst must reach scene (status={:?}, objects={}, patches={})",
        feed.view.status(),
        feed.view.object_count(),
        feed.view.patch_count(),
    );
    assert_eq!(feed.view.object_count(), BURST_OBJECTS);
    assert!(feed.view.patch_count() > 0, "terrain patches must remain present after burst");

    service.finish();
    assets.stop().await;
}
