use std::{array, assert_matches, iter, num::NonZeroU32, time::Instant};

use math::{DVec3, Vec3};
use test_support::mesh;
use world::{OrderEvidence, RegionInfo, RegionSummary, TerrainSettings, World, WorldLimits};

use super::*;
use crate::{
    scene::{
        RegionData,
        budget::{Limits, UNBOUNDED_MEMORY_BYTES},
        model::Change,
    },
    test_support::{registered_region, registered_regions},
};

fn settings(textures: [Uuid; 4]) -> TerrainSettings {
    TerrainSettings::new(textures, [0.0; 4], [64.0; 4]).unwrap()
}

fn region_data(budget: &Budget, region: Region, origin: DVec3, textures: [Uuid; 4]) -> RegionData {
    RegionData::new(
        RegionSummary {
            region,
            info: RegionInfo { origin: Some(origin), ..RegionInfo::default() },
            terrain_settings: Some(settings(textures)),
            water_height: None,
            object_count: 0,
        },
        budget.reserve(512).unwrap(),
    )
}

fn camera() -> Camera {
    Camera::new(DVec3::new(128.0, 128.0, 32.0), DVec3::X.as_vec3(), 1.0, [1280, 720]).unwrap()
}

fn ids(seed: u128) -> [Uuid; 4] {
    array::from_fn(|index| Uuid::from_u128(seed + index as u128))
}

fn process(demand: &mut AssetDemand, mirror: &Mirror) {
    let _ = demand.step(mirror, &BTreeMap::new());
}

fn ready_mesh_status(request: Request, bytes: Vec<u8>) -> Status {
    let now = Instant::now();
    let mut store = assets::Store::new(assets::Budget::default());
    let route = store.register_route().expect("fixture route registers");
    let demand = store.request(route, request, Priority::Visible, now).expect("fixture mesh request registers");
    let Some(assets::LoadJob::Fetch(job)) = store.next_job(now) else { panic!("fixture mesh fetch starts") };
    let input = match request {
        Request::MeshLod(block) => {
            let range = block.range();
            let start = usize::try_from(range.start).expect("mesh range start fits usize");
            let end = usize::try_from(range.end).expect("mesh range end fits usize");
            let partial = bytes.get(start..end).expect("mesh fixture contains requested LOD block").to_vec();
            job.complete_response(Ok(assets::FetchResponse::Partial { bytes: partial, range, total_length: Some(block.asset_length()) }))
        }
        _ => job.complete(Ok(bytes)),
    }
    .expect("fixture mesh response fits");
    let completion = store.start_decode(input).expect("fixture mesh decode starts").run();
    assert!(store.complete(completion, now).is_empty());
    demand.status()
}

fn ready_header_status(asset: AssetId) -> Status {
    ready_mesh_status(Request::MeshHeader(asset), mesh::asset(&[0, 1, 2, 3], 1))
}

fn invalidate_completed(demand: &mut AssetDemand, mirror: &Mirror, ticket: AssetTicket, status: Status) -> usize {
    let users = demand.complete(ticket, status);
    let count = users.len();
    for user in users {
        let AssetUser::Object(entity) = user else { panic!("mesh header users are objects") };
        demand.invalidate(mirror.objects.get(&entity).expect("completed mesh user remains in the scene"));
    }
    count
}

fn mesh_scene(count: u32) -> (Budget, Mirror, BTreeMap<Uuid, Entity>) {
    mesh_scene_with(count, |_, _| {})
}

fn mesh_scene_with(count: u32, customize: impl Fn(u32, &mut world::ObjectObservation)) -> (Budget, Mirror, BTreeMap<Uuid, Entity>) {
    let budget = Budget::default();
    let object_count = usize::try_from(count).expect("fixture object count fits usize");
    let tombstone_bytes = WorldLimits::tombstone_bytes_for_count(object_count);
    let mut world = World::new(WorldLimits {
        max_regions: 1,
        max_entities: object_count,
        max_entities_per_region: object_count,
        max_tombstones_per_region: object_count,
        max_tombstones: object_count,
        max_tombstone_bytes_per_region: tombstone_bytes,
        max_tombstone_bytes: tombstone_bytes,
        max_journal_entries: 4096,
        max_journal_changes: 8192,
        max_region_name_bytes: 64,
        max_recovery_targets: 1,
    });
    let region = world.register_region().expect("fixture region registers");
    let _origin =
        world.patch_region(region, RegionInfo { origin: Some(DVec3::ZERO), ..RegionInfo::default() }).expect("fixture origin applies");

    for local_id in 1..=count {
        let asset = AssetId::new(Uuid::from_u128(10_000 + u128::from(local_id)));
        let mut observation = world::ObjectObservation::new(
            world::UpdateSource::new(region, local_id, OrderEvidence::Ordered(u64::from(local_id)), 0),
            Uuid::from_u128(u128::from(local_id)),
            NonZeroU32::new(local_id).expect("fixture local ID is nonzero"),
            world::Kind::Object,
            world::ParentUpdate::Root,
        );
        observation.pose = Some(world::Pose { position: Some(Vec3::new(10.0, 0.0, 0.0)), rotation: Some(math::Quat::IDENTITY) });
        observation.scale = Some(Vec3::ONE);
        observation.shape = world::FieldUpdate::Set(world::Shape::Mesh { asset_id: asset, flags: SculptFlags::default() });
        customize(local_id, &mut observation);
        let _applied = world.observe_object(observation).expect("fixture mesh applies");
    }

    let mut mirror = Mirror::default();
    let mut identities = BTreeMap::new();
    let mut cursor = None;
    loop {
        let page = world.object_list_page(cursor, 256).expect("fixture object page reads");
        let next = page.next;
        for item in page.objects {
            let detail = world.object_detail(item.entity).expect("fixture object detail reads");
            let lease = budget.reserve(detail.owned_bytes() + 32).expect("fixture object fits budget");
            mirror.apply(Change::Object(Object::new(detail, lease)));
            identities.insert(item.id, item.entity);
        }
        let Some(next) = next else { break };
        cursor = Some(next);
    }
    (budget, mirror, identities)
}

fn shared_texture_scene(count: u32, texture: AssetId) -> (Budget, Region, Mirror, BTreeMap<Uuid, Entity>) {
    textured_scene(count, texture, |_, _| {})
}

fn textured_scene(
    count: u32,
    texture: AssetId,
    customize: impl Fn(u32, &mut world::ObjectObservation),
) -> (Budget, Region, Mirror, BTreeMap<Uuid, Entity>) {
    let budget = Budget::default();
    let (world, region) = textured_world(count, texture, customize);
    let (mirror, identities) = world_mirror(&world, &budget);
    (budget, region, mirror, identities)
}

fn textured_world(count: u32, texture: AssetId, customize: impl Fn(u32, &mut world::ObjectObservation)) -> (World, Region) {
    let object_count = usize::try_from(count).expect("fixture object count fits usize");
    let tombstone_bytes = WorldLimits::tombstone_bytes_for_count(object_count);
    let mut world = World::new(WorldLimits {
        max_regions: 1,
        max_entities: object_count,
        max_entities_per_region: object_count,
        max_tombstones_per_region: object_count,
        max_tombstones: object_count,
        max_tombstone_bytes_per_region: tombstone_bytes,
        max_tombstone_bytes: tombstone_bytes,
        max_journal_entries: 4096,
        max_journal_changes: 8192,
        max_region_name_bytes: 64,
        max_recovery_targets: 1,
    });
    let region = world.register_region().expect("fixture region registers");
    let _origin =
        world.patch_region(region, RegionInfo { origin: Some(DVec3::ZERO), ..RegionInfo::default() }).expect("fixture origin applies");

    for local_id in 1..=count {
        let mut observation = world::ObjectObservation::new(
            world::UpdateSource::new(region, local_id, OrderEvidence::Ordered(u64::from(local_id)), 0),
            Uuid::from_u128(u128::from(local_id)),
            NonZeroU32::new(local_id).expect("fixture local ID is nonzero"),
            world::Kind::Object,
            world::ParentUpdate::Root,
        );
        observation.pose = Some(world::Pose { position: Some(Vec3::new(10.0, 0.0, 0.0)), rotation: Some(math::Quat::IDENTITY) });
        observation.scale = Some(Vec3::ONE);
        observation.shape = world::FieldUpdate::Set(world::Shape::Prim(world::PrimShape::default()));
        observation.surface_appearance = Some(
            world::SurfaceAppearance::new(FaceAppearance { texture: Some(texture), ..FaceAppearance::default() }, Vec::new())
                .expect("fixture surface appearance is valid"),
        );
        customize(local_id, &mut observation);
        let _applied = world.observe_object(observation).expect("fixture textured object applies");
    }
    (world, region)
}

fn world_mirror(world: &World, budget: &Budget) -> (Mirror, BTreeMap<Uuid, Entity>) {
    let mut mirror = Mirror::default();
    let mut identities = BTreeMap::new();
    let mut cursor = None;
    loop {
        let page = world.object_list_page(cursor, 256).expect("fixture object page reads");
        let next = page.next;
        for item in page.objects {
            let detail = world.object_detail(item.entity).expect("fixture object detail reads");
            let lease = budget.reserve(detail.owned_bytes() + 32).expect("fixture object fits budget");
            mirror.apply(Change::Object(Object::new(detail, lease)));
            identities.insert(item.id, item.entity);
        }
        let Some(next) = next else { break };
        cursor = Some(next);
    }
    (mirror, identities)
}

fn process_all(demand: &mut AssetDemand, mirror: &Mirror, identities: &BTreeMap<Uuid, Entity>) {
    for _ in 0..identities.len().div_ceil(128) + 2 {
        if !demand.has_work() {
            return;
        }
        let _ = demand.step(mirror, identities);
    }
    assert!(!demand.has_work(), "bounded fixture scan finishes");
}

fn assert_updates(demand: &mut AssetDemand, ticket: AssetTicket, size: TextureSize, priority: Priority, importance: u16) {
    assert_matches!(demand.event(), Some(AssetChange::UpdateSize { ticket: updated, size: actual }) if updated == ticket && actual == size);
    assert_matches!(demand.event(), Some(AssetChange::UpdatePriority { ticket: updated, priority: actual }) if updated == ticket && actual == priority);
    assert_matches!(demand.event(), Some(AssetChange::UpdateImportance { ticket: updated, importance: actual }) if updated == ticket && actual == importance);
    assert!(demand.event().is_none());
}

fn mesh_requests(demand: &mut AssetDemand) -> Vec<(AssetId, AssetTicket)> {
    iter::from_fn(|| demand.event())
        .filter_map(|change| match change {
            AssetChange::Request(request) => match request.request {
                Request::MeshHeader(asset) => Some((asset, request.ticket)),
                _ => None,
            },
            AssetChange::Release(_)
            | AssetChange::UpdateImportance { .. }
            | AssetChange::UpdatePriority { .. }
            | AssetChange::UpdateSize { .. } => None,
        })
        .collect()
}

#[test]
fn texture_change_replaces_demands() {
    let budget = Budget::default();
    let (_region_world, region) = registered_region();
    let original = ids(10);
    let replacement = ids(20);
    let mut mirror = Mirror::default();
    mirror.regions.insert(region, region_data(&budget, region, DVec3::ZERO, original));
    let mut demand = AssetDemand::new(budget.clone());
    demand.set_camera(camera());
    demand.invalidate_region(region);
    process(&mut demand, &mirror);
    let old_tickets: Vec<_> = iter::from_fn(|| demand.event())
        .filter_map(|change| match change {
            AssetChange::Request(request) => Some(request.ticket),
            AssetChange::Release(_)
            | AssetChange::UpdateImportance { .. }
            | AssetChange::UpdatePriority { .. }
            | AssetChange::UpdateSize { .. } => None,
        })
        .collect();
    assert_eq!(old_tickets.len(), 4);
    assert_eq!(demand.terrain_targets[&region].textures, original);

    mirror.regions.insert(region, region_data(&budget, region, DVec3::ZERO, replacement));
    demand.invalidate_region(region);
    process(&mut demand, &mirror);
    assert_eq!(demand.terrain_targets[&region].textures, replacement);
    let mut requested = 0;
    let mut released = 0;
    while let Some(change) = demand.event() {
        match change {
            AssetChange::Request(request) => {
                assert_eq!(request.region, region);
                requested += 1;
            }
            AssetChange::Release(_) => released += 1,
            AssetChange::UpdateImportance { .. } => panic!("replacement texture requests use a new ticket"),
            AssetChange::UpdatePriority { .. } => panic!("replacement texture requests use a new ticket"),
            AssetChange::UpdateSize { .. } => panic!("replacement texture requests use a new ticket"),
        }
    }
    assert_eq!(requested, 4);
    assert_eq!(released, 4);
    assert!(demand.entries.keys().all(|key| key.region != region || replacement.contains(&key.resource.id.uuid())));
}

#[test]
fn failed_details_keep_neutral_fallback() {
    let budget = Budget::default();
    let (_region_world, region) = registered_region();
    let textures = ids(30);
    let mut mirror = Mirror::default();
    mirror.regions.insert(region, region_data(&budget, region, DVec3::ZERO, textures));
    let mut demand = AssetDemand::new(budget);
    demand.set_camera(camera());
    demand.invalidate_region(region);
    process(&mut demand, &mirror);
    assert!(demand.terrain_textures(region).iter().all(Option::is_none));
    let tickets: Vec<_> = iter::from_fn(|| demand.event())
        .filter_map(|change| match change {
            AssetChange::Request(request) => Some(request.ticket),
            AssetChange::Release(_)
            | AssetChange::UpdateImportance { .. }
            | AssetChange::UpdatePriority { .. }
            | AssetChange::UpdateSize { .. } => None,
        })
        .collect();
    assert_eq!(tickets.len(), 4);
    for ticket in tickets {
        let users = demand.complete(ticket, Status::Failed { failure: Failure::NotFound, retry_at: None });
        assert_eq!(users, [AssetUser::Terrain(region)]);
    }
    assert!(demand.terrain_textures(region).iter().all(Option::is_none));
    assert_eq!(demand.take_terrain_changes(), [region]);
}

#[test]
fn neighbors_keep_separate_details() {
    let budget = Budget::default();
    let (_region_world, regions) = registered_regions(2);
    let [south, east] = regions.as_slice() else { unreachable!() };
    let (south, east) = (*south, *east);
    let south_textures = ids(40);
    let east_textures = ids(50);
    let mut mirror = Mirror::default();
    mirror.regions.insert(south, region_data(&budget, south, DVec3::ZERO, south_textures));
    mirror.regions.insert(east, region_data(&budget, east, DVec3::new(256.0, 0.0, 0.0), east_textures));
    let mut demand = AssetDemand::new(budget);
    demand.set_camera(camera());
    demand.invalidate_region(south);
    demand.invalidate_region(east);
    process(&mut demand, &mirror);
    process(&mut demand, &mirror);
    assert_eq!(demand.terrain_targets[&south].textures, south_textures);
    assert_eq!(demand.terrain_targets[&east].textures, east_textures);
    assert_eq!(demand.entries.keys().filter(|key| key.region == south).count(), 4);
    assert_eq!(demand.entries.keys().filter(|key| key.region == east).count(), 4);
}

#[test]
fn nearby_details_refine_size() {
    let budget = Budget::default();
    let (_region_world, region) = registered_region();
    let mut mirror = Mirror::default();
    mirror.regions.insert(region, region_data(&budget, region, DVec3::ZERO, ids(60)));
    let close = camera().with_quality(64.0, 1.0).unwrap();
    let mut demand = AssetDemand::new(budget);
    demand.set_camera(close);
    demand.invalidate_region(region);
    process(&mut demand, &mirror);
    let nearby_size = demand.terrain_targets[&region].size;
    assert!(nearby_size.edge() > 128);

    demand.set_camera(close.with_quality(512.0, 1.0).unwrap());
    process(&mut demand, &mirror);
    assert_eq!(demand.terrain_targets[&region].size, nearby_size);
}

#[test]
fn terrain_background_keeps_resolution() {
    let budget = Budget::default();
    let (_region_world, region) = registered_region();
    let textures = ids(70);
    let mut mirror = Mirror::default();
    mirror.regions.insert(region, region_data(&budget, region, DVec3::ZERO, textures));
    let close = camera().with_quality(64.0, 1.0).unwrap();
    let mut demand = AssetDemand::new(budget.clone());
    demand.set_camera(close);
    demand.invalidate_region(region);
    process(&mut demand, &mirror);

    let visible_size = demand.terrain_targets[&region].size;
    assert!(visible_size.edge() > 128, "a nearby region requests more than its baseline detail");
    assert_eq!(demand.terrain_targets[&region].priority, Priority::Visible);
    let initial_tickets: Vec<_> = iter::from_fn(|| demand.event())
        .filter_map(|change| match change {
            AssetChange::Request(request) => {
                let Request::Texture { id, size } = request.request else { panic!("terrain uses texture requests") };
                assert!(textures.contains(&id.uuid()));
                assert_eq!(size, visible_size);
                assert_eq!(request.priority, Priority::Visible);
                Some(request.ticket)
            }
            AssetChange::Release(_) | AssetChange::UpdateImportance { .. } | AssetChange::UpdatePriority { .. } => None,
            AssetChange::UpdateSize { .. } => panic!("initial terrain demand has no previous size to update"),
        })
        .collect();
    assert_eq!(initial_tickets.len(), 4);

    mirror.regions.insert(
        region,
        RegionData::new(
            RegionSummary {
                region,
                info: RegionInfo::default(),
                terrain_settings: Some(settings(textures)),
                water_height: None,
                object_count: 0,
            },
            budget.reserve(512).unwrap(),
        ),
    );
    demand.invalidate_region(region);
    process(&mut demand, &mirror);
    assert_eq!(demand.terrain_targets[&region].size, visible_size, "an unknown origin keeps the last regional texture size");
    assert_eq!(demand.terrain_targets[&region].priority, Priority::Background);
    let background_updates: Vec<_> = iter::from_fn(|| demand.event())
        .map(|change| match change {
            AssetChange::UpdatePriority { ticket, priority } => (ticket, priority),
            AssetChange::Request(_) | AssetChange::Release(_) | AssetChange::UpdateImportance { .. } | AssetChange::UpdateSize { .. } => {
                panic!("background terrain keeps its existing texture tickets")
            }
        })
        .collect();
    assert_eq!(background_updates.len(), 4);
    assert!(background_updates.iter().all(|(_, priority)| *priority == Priority::Background));
    assert!(background_updates.iter().all(|(ticket, _)| initial_tickets.contains(ticket)));

    mirror.regions.insert(region, region_data(&budget, region, DVec3::ZERO, textures));
    demand.invalidate_region(region);
    process(&mut demand, &mirror);
    assert_eq!(demand.terrain_targets[&region].size, visible_size);
    assert_eq!(demand.terrain_targets[&region].priority, Priority::Visible);
    let visible_updates: Vec<_> = iter::from_fn(|| demand.event())
        .map(|change| match change {
            AssetChange::UpdatePriority { ticket, priority } => (ticket, priority),
            AssetChange::Request(_) | AssetChange::Release(_) | AssetChange::UpdateImportance { .. } | AssetChange::UpdateSize { .. } => {
                panic!("returning terrain keeps its existing texture tickets")
            }
        })
        .collect();
    assert_eq!(visible_updates.len(), 4);
    assert!(visible_updates.iter().all(|(_, priority)| *priority == Priority::Visible));
    assert!(visible_updates.iter().all(|(ticket, _)| initial_tickets.contains(ticket)));

    demand.remove_region(region);
    let released: HashSet<_> = iter::from_fn(|| demand.event())
        .map(|change| match change {
            AssetChange::Release(ticket) => ticket,
            AssetChange::Request(_)
            | AssetChange::UpdateImportance { .. }
            | AssetChange::UpdatePriority { .. }
            | AssetChange::UpdateSize { .. } => {
                panic!("region removal releases its retained terrain tickets")
            }
        })
        .collect();
    assert_eq!(released, initial_tickets.into_iter().collect());
    assert!(!demand.terrain_targets.contains_key(&region));
}

#[test]
fn mesh_importance_keeps_its_ticket() {
    let budget = Budget::default();
    let mut world = World::new(WorldLimits {
        max_regions: 1,
        max_entities: 1,
        max_entities_per_region: 1,
        max_tombstones_per_region: 1,
        max_tombstones: 1,
        max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(1),
        max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(1),
        max_journal_entries: 8,
        max_journal_changes: 16,
        max_region_name_bytes: 64,
        max_recovery_targets: 1,
    });
    let region = world.register_region().expect("fixture region registers");
    let _origin =
        world.patch_region(region, RegionInfo { origin: Some(DVec3::ZERO), ..RegionInfo::default() }).expect("fixture origin applies");
    let asset = AssetId::new(Uuid::from_u128(90));
    let mut observation = world::ObjectObservation::new(
        world::UpdateSource::new(region, 1, OrderEvidence::Ordered(1), 0),
        Uuid::from_u128(91),
        NonZeroU32::new(1).expect("fixture local ID"),
        world::Kind::Object,
        world::ParentUpdate::Root,
    );
    observation.pose = Some(world::Pose { position: Some(Vec3::new(10.0, 0.0, 0.0)), rotation: Some(math::Quat::IDENTITY) });
    observation.scale = Some(Vec3::ONE);
    observation.shape = world::FieldUpdate::Set(world::Shape::Mesh { asset_id: asset, flags: SculptFlags::default() });
    let _applied = world.observe_object(observation).expect("fixture mesh applies");
    let item = world.object_list_page(None, 1).expect("fixture object list").objects.into_iter().next().expect("fixture object");
    let detail = world.object_detail(item.entity).expect("fixture detail");
    let lease = budget.reserve(detail.owned_bytes() + 32).expect("fixture detail fits");
    let mut mirror = Mirror::default();
    mirror.apply(Change::Object(Object::new(detail, lease)));
    let identities = BTreeMap::from([(item.id, item.entity)]);

    let mut demand = AssetDemand::new(budget);
    let first_camera = Camera::new(DVec3::ZERO, Vec3::X, 1.0, [1280, 720]).expect("first camera");
    demand.set_camera(first_camera);
    let _ = demand.step(&mirror, &identities);
    let ticket = iter::from_fn(|| demand.event())
        .find_map(|change| match change {
            AssetChange::Request(request) if matches!(request.request, Request::MeshHeader(id) if id == asset) => Some(request.ticket),
            _ => None,
        })
        .expect("mesh header request");

    let next_camera = Camera::new(DVec3::new(5.0, 0.0, 0.0), Vec3::X, 1.0, [1280, 720]).expect("camera shift");
    assert_ne!(
        first_camera.mesh_importance(DVec3::new(10.0, 0.0, 0.0), Vec3::ONE),
        next_camera.mesh_importance(DVec3::new(10.0, 0.0, 0.0), Vec3::ONE)
    );
    demand.set_camera(next_camera);
    let _ = demand.step(&mirror, &identities);
    assert_matches!(demand.event(), Some(AssetChange::UpdateImportance { ticket: updated, .. }) if updated == ticket);
    assert!(demand.event().is_none(), "importance-only movement leaves the active demand alone");

    demand.set_camera(first_camera);
    let _ = demand.step(&mirror, &identities);
    assert_matches!(demand.event(), Some(AssetChange::UpdateImportance { ticket: updated, .. }) if updated == ticket);
    assert!(demand.event().is_none());
}

#[test]
fn priority_keeps_ticket_until_resize() {
    let (_region_world, region) = registered_region();
    let id = AssetId::new(Uuid::from_u128(100));
    let size = TextureSize::new(128);
    let user = AssetUser::Terrain(region);
    let mut demand = AssetDemand::new(Budget::default());
    let mut keys = HashSet::new();
    let key = demand.register(user, region, Request::Texture { id, size }, size, Priority::Visible, 600, &mut keys);
    let Some(AssetChange::Request(initial)) = demand.event() else { panic!("texture request is queued") };
    let ticket = initial.ticket;

    for (priority, importance) in [(Priority::Background, 0), (Priority::Visible, 600)] {
        demand.remove_user(user, key);
        demand.register(user, region, Request::Texture { id, size }, size, priority, importance, &mut keys);
        demand.refresh(key);

        assert_matches!(
            demand.event(),
            Some(AssetChange::UpdatePriority { ticket: updated, priority: actual }) if updated == ticket && actual == priority
        );
        assert_matches!(
            demand.event(),
            Some(AssetChange::UpdateImportance { ticket: updated, importance: actual }) if updated == ticket && actual == importance
        );
        assert!(demand.event().is_none(), "priority and importance updates keep the same request active");
        assert_eq!(demand.tickets.get(&ticket), Some(&key));
    }

    let resized = TextureSize::new(256);
    demand.remove_user(user, key);
    demand.register(user, region, Request::Texture { id, size: resized }, resized, Priority::Visible, 600, &mut keys);
    demand.refresh(key);

    assert_matches!(demand.event(), Some(AssetChange::UpdateSize { ticket: updated, size }) if updated == ticket && size == resized);
    assert!(demand.event().is_none());
    let entry = demand.entries.get(&key).expect("registered entry exists");
    assert_eq!(entry.request, Request::Texture { id, size: resized });
    assert_eq!(entry.priority, Priority::Visible);
    assert_eq!(entry.importance, 600);
    assert_eq!(demand.tickets.get(&ticket), Some(&key));
    assert!(demand.event().is_none());
}

#[test]
fn shared_maxima_track_changes() {
    let id = AssetId::new(Uuid::from_u128(700));
    let (budget, region, _, identities) = shared_texture_scene(2, id);
    let mut objects = identities.values();
    let first = AssetUser::Object(*objects.next().expect("first fixture object"));
    let second = AssetUser::Object(*objects.next().expect("second fixture object"));
    let terrain = AssetUser::Terrain(region);
    let low_size = TextureSize::new(64);
    let high_size = TextureSize::new(512);
    let mut demand = AssetDemand::new(budget);
    let mut keys = HashSet::new();
    let key = demand.register(terrain, region, Request::Texture { id, size: low_size }, low_size, Priority::Background, 100, &mut keys);
    let Some(AssetChange::Request(request)) = demand.event() else { panic!("initial shared texture request is queued") };
    let ticket = request.ticket;
    assert_eq!(request.request, Request::Texture { id, size: low_size });

    for user in [first, second] {
        demand.register(user, region, Request::Texture { id, size: high_size }, high_size, Priority::Visible, 900, &mut keys);
    }
    let work = demand.entries.get(&key).expect("registered entry exists").extrema.work;
    demand.register(second, region, Request::Texture { id, size: high_size }, high_size, Priority::Visible, 900, &mut keys);
    assert_eq!(demand.entries.get(&key).expect("registered entry exists").extrema.work, work);
    demand.refresh(key);
    assert_updates(&mut demand, ticket, high_size, Priority::Visible, 900);

    demand.release(first, key);
    assert!(demand.event().is_none(), "one tied maximum keeps the shared demand unchanged");
    assert_eq!(demand.tickets.get(&ticket), Some(&key));

    demand.remove_user(second, key);
    demand.register(second, region, Request::Texture { id, size: low_size }, low_size, Priority::Background, 100, &mut keys);
    demand.refresh(key);
    assert_updates(&mut demand, ticket, low_size, Priority::Background, 100);

    demand.register(first, region, Request::Texture { id, size: high_size }, high_size, Priority::Visible, 900, &mut keys);
    demand.refresh(key);
    assert_updates(&mut demand, ticket, high_size, Priority::Visible, 900);
    demand.release(first, key);
    assert_updates(&mut demand, ticket, low_size, Priority::Background, 100);
    assert_eq!(demand.tickets.get(&ticket), Some(&key));

    demand.release(terrain, key);
    assert!(demand.event().is_none());
    demand.release(second, key);
    assert_matches!(demand.event(), Some(AssetChange::Release(released)) if released == ticket);
    assert!(demand.event().is_none());
    assert!(!demand.tickets.contains_key(&ticket));
}

#[test]
fn object_terrain_share_ticket() {
    let texture = AssetId::new(Uuid::from_u128(701));
    let textures = [texture.uuid(); 4];
    let (budget, region, mut mirror, identities) = shared_texture_scene(1, texture);
    mirror.regions.insert(region, region_data(&budget, region, DVec3::new(-128.0, -128.0, 0.0), textures));
    let entity = *identities.values().next().expect("fixture object exists");
    let camera = Camera::new(DVec3::ZERO, Vec3::X, 1.0, [1280, 720]).expect("camera");
    let mut demand = AssetDemand::new(budget);
    demand.set_camera(camera);
    process_all(&mut demand, &mirror, &identities);
    let key = AssetKey { region, resource: Resource { id: texture, kind: Kind::Texture }, unit: AssetUnit::Whole };
    let ticket = demand.entries.get(&key).expect("shared texture entry exists").ticket;
    assert!(iter::from_fn(|| demand.event()).any(|change| matches!(change, AssetChange::Request(request) if request.ticket == ticket)));

    demand.remove(entity);
    assert!(iter::from_fn(|| demand.event()).all(|change| !matches!(change, AssetChange::Release(released) if released == ticket)));
    assert_eq!(demand.tickets.get(&ticket), Some(&key));

    demand.remove_region(region);
    assert_matches!(demand.event(), Some(AssetChange::Release(released)) if released == ticket);
    assert!(demand.event().is_none());
    assert!(!demand.tickets.contains_key(&ticket));
}

#[test]
fn visible_demand_displaces_background() {
    const SLOTS: u8 = 32;
    const FILLER: u32 = 64;
    const BACKGROUND: u32 = 8;
    let texture = AssetId::new(Uuid::from_u128(703));
    let face = |color| FaceAppearance { texture: Some(texture), color: [color, 0, 0, 255], ..FaceAppearance::default() };
    let appearance =
        || world::SurfaceAppearance::new(face(0), (1..SLOTS).map(|slot| (slot - 1, face(slot))).collect()).expect("fixture faces");

    let probe_budget = Budget::new(Limits { memory_bytes: UNBOUNDED_MEMORY_BYTES, worker_threads: 1 });
    let (_, _, mut probe_mirror, probe_identities) = textured_scene(1, texture, |_, observation| {
        observation.pose = Some(world::Pose { position: Some(Vec3::new(100.0, 128.0, 32.0)), rotation: Some(math::Quat::IDENTITY) });
        observation.surface_appearance = Some(appearance());
    });
    let mut probe_demand = AssetDemand::new(probe_budget.clone());
    probe_demand.set_camera(camera());
    let _ = probe_demand.step(&Mirror::default(), &BTreeMap::new());
    let base_bytes = probe_budget.charged_bytes();
    probe_demand.invalidate_all();
    for change in probe_demand.step(&probe_mirror, &probe_identities) {
        probe_mirror.apply(Change::Material(change));
    }
    let after_apply = probe_budget.charged_bytes();
    probe_mirror.objects.clear();
    let after_clear = probe_budget.charged_bytes();
    let per_object = after_apply - base_bytes;
    let materials_bytes = after_apply - after_clear;
    let memory_bytes = base_bytes + usize::try_from(BACKGROUND).expect("background count fits usize") * per_object + materials_bytes;

    let farthest = FILLER + BACKGROUND;
    let visible = farthest + 1;
    let late = farthest + 2;
    let (_, _, mut mirror, identities) = textured_scene(late, texture, |local_id, observation| {
        let x = match local_id {
            id if id == visible => 140.0,
            id if id == late => 60.0,
            id => 100.0 - id as f32 * 0.001,
        };
        observation.pose = Some(world::Pose { position: Some(Vec3::new(x, 128.0, 32.0)), rotation: Some(math::Quat::IDENTITY) });
        observation.surface_appearance = (local_id > FILLER).then(&appearance);
    });
    let entity = |local_id: u32| identities[&Uuid::from_u128(u128::from(local_id))];
    let settle = |demand: &mut AssetDemand, mirror: &mut Mirror| {
        for _ in 0..64 {
            if !demand.has_work() {
                return;
            }
            for change in demand.step(mirror, &identities) {
                mirror.apply(Change::Material(change));
            }
        }
        panic!("bounded fixture scan finishes");
    };
    let consistent = |demand: &AssetDemand, mirror: &Mirror| {
        mirror.objects.iter().all(|(entity, object)| object.materials().is_none() || demand.targets.contains_key(entity))
    };
    let materials = |mirror: &Mirror, local_id: u32| mirror.objects[&entity(local_id)].materials().map(<[_]>::len);
    let demand_budget = Budget::new(Limits { memory_bytes, worker_threads: 1 });
    let mut demand = AssetDemand::new(demand_budget);
    demand.set_camera(camera());
    settle(&mut demand, &mut mirror);

    assert_eq!(materials(&mirror, visible), Some(usize::from(SLOTS)));
    assert_eq!(materials(&mirror, farthest), None, "the farthest background object is displaced in the step that admitted it");
    assert!((FILLER + 1..farthest).all(|local_id| materials(&mirror, local_id).is_some()));
    assert_eq!(materials(&mirror, late), None, "background demand never displaces");
    assert!(demand.rejected_entities.contains(&entity(farthest)) && demand.rejected_entities.contains(&entity(late)));
    assert!(consistent(&demand, &mirror));

    demand.remove(entity(visible));
    mirror.objects.remove(&entity(visible));
    settle(&mut demand, &mut mirror);
    assert_eq!(materials(&mirror, farthest), Some(usize::from(SLOTS)), "displaced demand returns once space frees");
    assert_eq!(materials(&mirror, late), None);
    assert!(consistent(&demand, &mirror));
}

#[test]
fn camera_rescan_keeps_demand() {
    let texture = AssetId::new(Uuid::from_u128(702));
    let (budget, _, mirror, identities) = shared_texture_scene(32, texture);
    let first_camera = Camera::new(DVec3::ZERO, Vec3::X, 1.0, [1280, 720]).expect("first camera");
    let next_camera = Camera::new(DVec3::new(0.001, 0.0, 0.0), Vec3::X, 1.0, [1280, 720]).expect("next camera");
    let mut demand = AssetDemand::new(budget);
    demand.set_camera(first_camera);
    process_all(&mut demand, &mirror, &identities);
    let key = AssetKey {
        region: identities
            .values()
            .next()
            .and_then(|entity| mirror.objects.get(entity))
            .expect("fixture object")
            .detail()
            .summary
            .address
            .region(),
        resource: Resource { id: texture, kind: Kind::Texture },
        unit: AssetUnit::Whole,
    };
    let ticket = demand.entries.get(&key).expect("shared texture entry exists").ticket;
    assert!(iter::from_fn(|| demand.event()).any(|change| matches!(change, AssetChange::Request(request) if request.ticket == ticket)));

    demand.set_camera(next_camera);
    process_all(&mut demand, &mirror, &identities);
    assert!(demand.event().is_none(), "rescan with unchanged texture demand emits no changes");
    assert_eq!(demand.entries.get(&key).expect("shared texture entry remains").users.len(), identities.len());
    assert_eq!(demand.tickets.get(&ticket), Some(&key));
}

fn shared_texture_work(count: u32) -> [usize; 3] {
    let texture = AssetId::new(Uuid::from_u128(703));
    let (budget, region, mirror, identities) = shared_texture_scene(count, texture);
    let first_camera = Camera::new(DVec3::ZERO, Vec3::X, 1.0, [1280, 720]).expect("first camera");
    let next_camera = Camera::new(DVec3::new(0.001, 0.0, 0.0), Vec3::X, 1.0, [1280, 720]).expect("next camera");
    let key = AssetKey { region, resource: Resource { id: texture, kind: Kind::Texture }, unit: AssetUnit::Whole };
    let mut demand = AssetDemand::new(budget);
    demand.set_camera(first_camera);
    process_all(&mut demand, &mirror, &identities);
    let entry = demand.entries.get(&key).expect("shared texture entry exists");
    assert_eq!(entry.users.len(), identities.len());
    let ticket = entry.ticket;
    let initial_work = entry.extrema.work;
    assert!(initial_work <= 3 * identities.len(), "registration work stays bounded by user count");
    let initial_events: Vec<_> = iter::from_fn(|| demand.event()).collect();
    assert_eq!(initial_events.len(), 1, "all objects share one request");
    assert_matches!(&initial_events[0], AssetChange::Request(request) if request.ticket == ticket);

    demand.set_camera(next_camera);
    process_all(&mut demand, &mirror, &identities);
    assert!(demand.event().is_none(), "unchanged shared demand emits no camera update");
    let entry = demand.entries.get(&key).expect("shared texture entry remains");
    assert_eq!(entry.users.len(), identities.len());
    let rescan_work = entry.extrema.work.checked_sub(initial_work).expect("aggregate work grows during rescan");
    assert!(rescan_work <= 6 * identities.len(), "rescan work stays bounded by user count");
    assert_eq!(demand.tickets.get(&ticket), Some(&key));

    let mut entities = identities.values().copied();
    for entity in entities.by_ref().take(identities.len() - 1) {
        demand.remove(entity);
    }
    let reverse_entry = demand.entries.get(&key).expect("one shared user remains");
    let reverse_work =
        reverse_entry.extrema.work.checked_sub(initial_work + rescan_work).expect("aggregate work grows during reverse transitions");
    demand.remove(entities.next().expect("one fixture user remains"));
    let releases: Vec<_> = iter::from_fn(|| demand.event()).collect();
    assert_eq!(releases.len(), 1, "the final user releases the shared ticket once");
    assert_matches!(&releases[0], AssetChange::Release(released) if *released == ticket);
    assert!(!demand.entries.contains_key(&key));
    assert!(!demand.tickets.contains_key(&ticket));
    [initial_work, rescan_work, reverse_work]
}

#[test]
fn shared_texture_work_scales() {
    const USERS: u32 = 256;
    let one_scene = shared_texture_work(USERS);
    let two_scenes = shared_texture_work(2 * USERS);

    for (work, users) in [(one_scene, USERS as usize), (two_scenes, 2 * USERS as usize)] {
        for (actual, bound) in work.into_iter().zip([3 * users, 6 * users, 3 * users]) {
            assert!(actual <= bound, "aggregate work {actual} exceeds linear bound {bound} for {users} users");
        }
    }
}

#[test]
fn scan_exhaustion_keeps_dirty_work() {
    const OBJECTS: u32 = 128;
    const PROCESSED: u32 = 128;

    let (budget, mirror, identities) = mesh_scene(OBJECTS);
    let first_camera = Camera::new(DVec3::ZERO, Vec3::X, 1.0, [1280, 720]).expect("first camera");
    let next_camera = Camera::new(DVec3::new(5.0, 0.0, 0.0), Vec3::X, 1.0, [1280, 720]).expect("next camera");
    let mut demand = AssetDemand::new(budget);
    demand.set_camera(first_camera);
    let _ = demand.step(&mirror, &identities);
    let initial_requests = mesh_requests(&mut demand);
    let expected_assets: HashSet<_> = (1..=PROCESSED).map(|id| AssetId::new(Uuid::from_u128(10_000 + u128::from(id)))).collect();
    assert_eq!(initial_requests.len(), PROCESSED as usize);
    assert_eq!(initial_requests.iter().map(|(asset, _)| *asset).collect::<HashSet<_>>(), expected_assets);

    demand.set_camera(next_camera);
    for &entity in identities.values() {
        demand.invalidate(mirror.objects.get(&entity).expect("fixture object remains in the scene"));
    }
    let _ = demand.step(&mirror, &identities);
    let updates: Vec<_> = iter::from_fn(|| demand.event())
        .map(|change| match change {
            AssetChange::UpdateImportance { ticket, .. } => ticket,
            AssetChange::Request(_) | AssetChange::Release(_) | AssetChange::UpdatePriority { .. } | AssetChange::UpdateSize { .. } => {
                panic!("camera movement updates existing mesh request importance")
            }
        })
        .collect();
    assert_eq!(updates.len(), PROCESSED as usize);
    assert_eq!(updates.into_iter().collect::<HashSet<_>>(), initial_requests.iter().map(|(_, ticket)| *ticket).collect());
}

#[test]
fn dirty_exhaustion_keeps_scan_work() {
    const OBJECTS: u32 = 256;
    const PROCESSED: u32 = 128;
    const SCANNED: u32 = PROCESSED - 1;

    let (budget, mirror, identities) = mesh_scene(OBJECTS);
    let first_camera = Camera::new(DVec3::ZERO, Vec3::X, 1.0, [1280, 720]).expect("first camera");
    let next_camera = Camera::new(DVec3::new(5.0, 0.0, 0.0), Vec3::X, 1.0, [1280, 720]).expect("next camera");
    let dirty_id = Uuid::from_u128(1);
    let dirty_entity = identities[&dirty_id];
    let mut demand = AssetDemand::new(budget);
    demand.set_camera(first_camera);
    let _ = demand.step(&mirror, &identities);
    let initial_requests = mesh_requests(&mut demand);
    assert_eq!(initial_requests.len(), PROCESSED as usize);

    demand.set_camera(next_camera);
    demand.invalidate(mirror.objects.get(&dirty_entity).expect("fixture object remains in the scene"));

    let _ = demand.step(&mirror, &identities);
    let mut requested = Vec::new();
    let mut updates = Vec::new();
    while let Some(change) = demand.event() {
        match change {
            AssetChange::Request(request) => match request.request {
                Request::MeshHeader(asset) => requested.push(asset),
                _ => panic!("fixture mesh work requests mesh headers"),
            },
            AssetChange::UpdateImportance { ticket, .. } => updates.push(ticket),
            AssetChange::Release(_) | AssetChange::UpdatePriority { .. } | AssetChange::UpdateSize { .. } => {
                panic!("camera movement retains mesh requests and priority")
            }
        }
    }
    let expected_assets: HashSet<_> = (129..=129 + SCANNED - 1).map(|id| AssetId::new(Uuid::from_u128(10_000 + u128::from(id)))).collect();
    assert_eq!(requested.len(), SCANNED as usize);
    assert_eq!(requested.into_iter().collect::<HashSet<_>>(), expected_assets);
    let dirty_ticket =
        initial_requests.iter().find_map(|(asset, ticket)| (*asset == AssetId::new(Uuid::from_u128(10_001))).then_some(*ticket));
    assert_eq!(updates, [dirty_ticket.expect("dirty mesh header request")]);
}

#[test]
fn camera_scan_reaches_target() {
    const LOW_USERS: u32 = 1024;
    const TARGET_LOCAL_ID: u32 = LOW_USERS + 1;
    const OBJECTS: usize = TARGET_LOCAL_ID as usize;
    const DIRTY_USERS: u32 = 128;
    const MAX_TURNS: usize = 80;

    let budget = Budget::default();
    let mesh_asset = AssetId::new(Uuid::from_u128(10_000));
    let tombstone_bytes = WorldLimits::tombstone_bytes_for_count(OBJECTS);
    let mut world = World::new(WorldLimits {
        max_regions: 1,
        max_entities: OBJECTS,
        max_entities_per_region: OBJECTS,
        max_tombstones_per_region: OBJECTS,
        max_tombstones: OBJECTS,
        max_tombstone_bytes_per_region: tombstone_bytes,
        max_tombstone_bytes: tombstone_bytes,
        max_journal_entries: 4096,
        max_journal_changes: 8192,
        max_region_name_bytes: 64,
        max_recovery_targets: 1,
    });
    let region = world.register_region().expect("fixture region registers");
    let _origin =
        world.patch_region(region, RegionInfo { origin: Some(DVec3::ZERO), ..RegionInfo::default() }).expect("fixture origin applies");

    for local_id in 1..=TARGET_LOCAL_ID {
        let is_target = local_id == TARGET_LOCAL_ID;
        let id = if is_target { Uuid::from_u128(u128::MAX) } else { Uuid::from_u128(u128::from(local_id)) };
        let position = if is_target { Vec3::new(100.0, 0.0, 0.0) } else { Vec3::new(200.0, 0.0, 0.0) };
        let mut observation = world::ObjectObservation::new(
            world::UpdateSource::new(region, local_id, OrderEvidence::Ordered(u64::from(local_id)), 0),
            id,
            NonZeroU32::new(local_id).expect("fixture local ID is nonzero"),
            world::Kind::Object,
            world::ParentUpdate::Root,
        );
        observation.pose = Some(world::Pose { position: Some(position), rotation: Some(math::Quat::IDENTITY) });
        observation.scale = Some(Vec3::ONE);
        observation.shape = world::FieldUpdate::Set(world::Shape::Mesh { asset_id: mesh_asset, flags: SculptFlags::default() });
        let _applied = world.observe_object(observation).expect("fixture mesh applies");
    }

    let mut mirror = Mirror::default();
    let mut identities = BTreeMap::new();
    let mut cursor = None;
    loop {
        let page = world.object_list_page(cursor, 256).expect("fixture object page reads");
        let next = page.next;
        for item in page.objects {
            let detail = world.object_detail(item.entity).expect("fixture object detail reads");
            let lease = budget.reserve(detail.owned_bytes() + 32).expect("fixture object fits budget");
            mirror.apply(Change::Object(Object::new(detail, lease)));
            identities.insert(item.id, item.entity);
        }
        let Some(next) = next else { break };
        cursor = Some(next);
    }
    assert_eq!(identities.len(), OBJECTS);

    let first_camera = Camera::new(DVec3::ZERO, Vec3::X, 1.0, [1280, 720]).expect("first camera");
    let high_camera = Camera::new(DVec3::new(99.5, 0.0, 0.0), Vec3::X, 1.0, [1280, 720]).expect("high camera");
    assert_eq!(high_camera.mesh_lod(DVec3::new(100.0, 0.0, 0.0), Vec3::ONE, None), Some(MeshLod::High));

    let mut demand = AssetDemand::new(budget.clone());
    demand.set_camera(first_camera);
    for _ in 0..(LOW_USERS as usize / 128) {
        let _ = demand.step(&mirror, &identities);
    }
    let mut shared_header_ticket = None;
    while let Some(change) = demand.event() {
        if let AssetChange::Request(request) = change
            && matches!(request.request, Request::MeshHeader(asset) if asset == mesh_asset)
        {
            assert!(shared_header_ticket.replace(request.ticket).is_none(), "shared users use one mesh header request");
        }
    }
    let shared_header_ticket = shared_header_ticket.expect("low UUID users requested their shared mesh header");

    assert_eq!(invalidate_completed(&mut demand, &mirror, shared_header_ticket, Status::Fetching), LOW_USERS as usize);
    assert_eq!(invalidate_completed(&mut demand, &mirror, shared_header_ticket, Status::Decoding), LOW_USERS as usize);

    demand.set_camera(high_camera);
    let ready_header = ready_header_status(mesh_asset);
    assert_matches!(&ready_header, Status::Ready(Content::MeshHeader(_)));
    assert_eq!(invalidate_completed(&mut demand, &mirror, shared_header_ticket, ready_header), LOW_USERS as usize);

    let mut update_order = u64::from(TARGET_LOCAL_ID);
    let mut requested_high_lod = false;
    for turn in 0..MAX_TURNS {
        for local_id in 1..=DIRTY_USERS {
            update_order += 1;
            let id = Uuid::from_u128(u128::from(local_id));
            let entity = identities[&id];
            let mut patch = world::ObjectPatch::new(
                world::UpdateSource::new(
                    region,
                    u32::try_from(update_order).expect("fixture sequence fits"),
                    OrderEvidence::Ordered(update_order),
                    0,
                ),
                NonZeroU32::new(local_id).expect("fixture local ID is nonzero"),
            );
            let x = if turn % 2 == 0 { 201.0 } else { 200.0 };
            patch.pose = Some(world::Pose { position: Some(Vec3::new(x, 0.0, 0.0)), rotation: Some(math::Quat::IDENTITY) });
            let _applied = world.patch_object(patch).expect("ordered fixture pose update applies");
            let detail = world.object_detail(entity).expect("updated fixture detail reads");
            let lease = budget.reserve(detail.owned_bytes() + 32).expect("updated fixture object fits budget");
            let object = Object::new(detail, lease);
            mirror.apply(Change::Object(object.clone()));
            demand.invalidate(&object);
        }

        let _ = demand.step(&mirror, &identities);
        while let Some(change) = demand.event() {
            if let AssetChange::Request(request) = change
                && let Request::MeshLod(lod) = request.request
                && lod.lod() == MeshLod::High
            {
                assert_eq!(request.priority, Priority::Visible);
                assert_eq!(lod.asset_id(), mesh_asset);
                requested_high_lod = true;
            }
        }
        if requested_high_lod {
            break;
        }
    }

    assert!(requested_high_lod, "a high UUID target receives its High LOD request under sustained low UUID updates");
}

#[test]
fn turning_keeps_materials() {
    const OBJECTS: u32 = 1_200;
    const SLOTS: u8 = 32;
    let texture = AssetId::new(Uuid::from_u128(704));
    let (budget, _, mut mirror, identities) = textured_scene(OBJECTS, texture, |local_id, observation| {
        let x = if local_id <= 600 { 140.0 } else { 100.0 };
        observation.pose = Some(world::Pose { position: Some(Vec3::new(x, 128.0, 32.0)), rotation: Some(math::Quat::IDENTITY) });
        let face = |color| FaceAppearance { texture: Some(texture), color: [color, 0, 0, 255], ..FaceAppearance::default() };
        observation.surface_appearance =
            Some(world::SurfaceAppearance::new(face(0), (1..SLOTS).map(|slot| (slot - 1, face(slot))).collect()).expect("fixture faces"));
    });
    let entity = |local_id: u32| identities[&Uuid::from_u128(u128::from(local_id))];
    let settle = |demand: &mut AssetDemand, mirror: &mut Mirror| {
        for _ in 0..64 {
            if !demand.has_work() {
                return;
            }
            for change in demand.step(mirror, &identities) {
                mirror.apply(Change::Material(change));
            }
        }
        panic!("bounded fixture scan finishes");
    };
    let assert_all_materials = |demand: &AssetDemand, mirror: &Mirror| {
        let missing =
            (1..=OBJECTS).find(|&local_id| mirror.objects[&entity(local_id)].materials().map(<[_]>::len) != Some(usize::from(SLOTS)));
        assert_eq!(missing, None, "object {missing:?} missing materials");
        assert!(demand.rejected_entities.is_empty(), "rejected entities: {}", demand.rejected_entities.len());
    };

    let mut demand = AssetDemand::new(budget);
    demand.set_camera(camera());
    settle(&mut demand, &mut mirror);
    assert_all_materials(&demand, &mirror);

    demand.set_camera(Camera::new(DVec3::new(128.0, 128.0, 32.0), Vec3::NEG_X, 1.0, [1280, 720]).expect("camera"));
    settle(&mut demand, &mut mirror);
    assert_all_materials(&demand, &mirror);
}

#[test]
fn shrunk_slots_readmit_rejected() {
    const ADMITTED: u32 = 8;
    let shared_asset = AssetId::new(Uuid::from_u128(50_000));
    let rejected_asset = AssetId::new(Uuid::from_u128(50_001));
    let eight_faces = || {
        let overrides = (1..8).map(|slot| (slot, FaceAppearance { color: [slot, 30, 60, 255], ..FaceAppearance::default() })).collect();
        world::SurfaceAppearance::new(FaceAppearance::default(), overrides).expect("fixture faces")
    };
    let view_camera = Camera::new(DVec3::ZERO, Vec3::X, 1.0, [1280, 720]).expect("camera");

    let probe_budget = Budget::new(Limits { memory_bytes: UNBOUNDED_MEMORY_BYTES, worker_threads: 1 });
    let (_, mut probe_mirror, probe_identities) = mesh_scene_with(1, |_, observation| {
        observation.shape = world::FieldUpdate::Set(world::Shape::Mesh { asset_id: shared_asset, flags: SculptFlags::default() });
        observation.surface_appearance = Some(eight_faces());
    });
    let mut probe_demand = AssetDemand::new(probe_budget.clone());
    probe_demand.set_camera(view_camera);
    let _ = probe_demand.step(&Mirror::default(), &BTreeMap::new());
    let base_bytes = probe_budget.charged_bytes();
    probe_demand.invalidate_all();
    for change in probe_demand.step(&probe_mirror, &probe_identities) {
        probe_mirror.apply(Change::Material(change));
    }
    let per_object = probe_budget.charged_bytes() - base_bytes;
    let memory_bytes = base_bytes + usize::try_from(ADMITTED).expect("admitted count fits usize") * per_object + per_object / 2;

    let rejected_id = ADMITTED + 1;
    let (_, mut mirror, identities) = mesh_scene_with(rejected_id, |local_id, observation| {
        let asset = if local_id == rejected_id { rejected_asset } else { shared_asset };
        observation.shape = world::FieldUpdate::Set(world::Shape::Mesh { asset_id: asset, flags: SculptFlags::default() });
        observation.surface_appearance = Some(eight_faces());
    });
    let rejected_entity = identities[&Uuid::from_u128(u128::from(rejected_id))];
    let settle = |demand: &mut AssetDemand, mirror: &mut Mirror| {
        for _ in 0..64 {
            if !demand.has_work() {
                return;
            }
            for change in demand.step(mirror, &identities) {
                mirror.apply(Change::Material(change));
            }
        }
        panic!("bounded fixture scan finishes");
    };

    let demand_budget = Budget::new(Limits { memory_bytes, worker_threads: 1 });
    let mut demand = AssetDemand::new(demand_budget);
    demand.set_camera(view_camera);
    settle(&mut demand, &mut mirror);

    let initial_events: Vec<_> = iter::from_fn(|| demand.event()).collect();
    let shared_header = initial_events
        .iter()
        .find_map(|change| match change {
            AssetChange::Request(request) if matches!(request.request, Request::MeshHeader(id) if id == shared_asset) => {
                Some(request.ticket)
            }
            _ => None,
        })
        .expect("shared mesh header requested");
    let rejected_header = initial_events
        .iter()
        .find_map(|change| match change {
            AssetChange::Request(request) if matches!(request.request, Request::MeshHeader(id) if id == rejected_asset) => {
                Some(request.ticket)
            }
            _ => None,
        })
        .expect("rejected mesh starts provisional header demand");
    assert!(
        initial_events.iter().any(|change| matches!(change, AssetChange::Release(ticket) if *ticket == rejected_header)),
        "provisional mesh header ticket is released on rejection"
    );
    assert!(demand.rejected_entities.contains(&rejected_entity));

    let shared_bytes = mesh::asset_with_lod_faces(&[(2, 1)]);
    assert_eq!(
        invalidate_completed(
            &mut demand,
            &mirror,
            shared_header,
            ready_mesh_status(Request::MeshHeader(shared_asset), shared_bytes.clone())
        ),
        usize::try_from(ADMITTED).expect("admitted count fits usize")
    );
    settle(&mut demand, &mut mirror);

    let lod_request = iter::from_fn(|| demand.event())
        .find_map(|change| match change {
            AssetChange::Request(request) if matches!(request.request, Request::MeshLod(block) if block.asset_id() == shared_asset) => {
                Some(request)
            }
            _ => None,
        })
        .expect("shared mesh LOD requested");
    assert_eq!(
        invalidate_completed(&mut demand, &mirror, lod_request.ticket, ready_mesh_status(lod_request.request, shared_bytes)),
        usize::try_from(ADMITTED).expect("admitted count fits usize")
    );
    settle(&mut demand, &mut mirror);

    let readmitted_events: Vec<_> = iter::from_fn(|| demand.event()).collect();
    let requeued_header = readmitted_events
        .iter()
        .find_map(|change| match change {
            AssetChange::Request(request) if matches!(request.request, Request::MeshHeader(id) if id == rejected_asset) => {
                Some(request.ticket)
            }
            _ => None,
        })
        .expect("rejected mesh requests its header again after admitted slots shrink");
    assert!(!readmitted_events.iter().any(|change| matches!(change, AssetChange::Release(ticket) if *ticket == requeued_header)));
    assert!(!demand.rejected_entities.contains(&rejected_entity));
    assert!(demand.targets.contains_key(&rejected_entity));
}

#[test]
fn freed_budget_readmits_rejected() {
    let (_, _, mut mirror, identities) = textured_scene(1, AssetId::new(Uuid::from_u128(704)), |_, observation| {
        posed_faces(observation, 140.0, 4);
    });
    let entity = identities[&Uuid::from_u128(1)];
    let budget = Budget::new(Limits { memory_bytes: 64 << 20, worker_threads: 1 });
    let mut demand = AssetDemand::new(budget.clone());
    demand.set_camera(camera());
    let _ = demand.step(&Mirror::default(), &BTreeMap::new());
    let blocker = budget.reserve(budget.memory_bytes() - budget.charged_bytes()).expect("blocker fills the budget");
    demand.invalidate_all();
    settle_changes(&mut demand, &mut mirror, &identities);
    assert!(demand.rejected_entities.contains(&entity), "a full budget rejects the object");

    drop(blocker);
    assert!(demand.retry_admission(), "freed memory schedules a rejected retry");
    settle_changes(&mut demand, &mut mirror, &identities);
    assert!(demand.targets.contains_key(&entity), "the rejected object is admitted once memory frees");
}

#[test]
fn displacement_frees_missing_faces() {
    let (_, _, mut mirror, identities) = textured_scene(2, AssetId::new(Uuid::from_u128(705)), |local_id, observation| match local_id {
        1 => posed_faces(observation, 100.0, 2),
        _ => posed_faces(observation, 140.0, 8),
    });
    let background = identities[&Uuid::from_u128(1)];
    let visible = identities[&Uuid::from_u128(2)];
    let budget = Budget::new(Limits { memory_bytes: 64 << 20, worker_threads: 1 });
    let mut demand = AssetDemand::new(budget.clone());
    demand.set_camera(camera());
    settle_changes(&mut demand, &mut mirror, &identities);
    let background_faces = demand.targets[&background].faces;
    let visible_faces = demand.targets[&visible].faces;
    assert!(background_faces > 0 && background_faces < visible_faces, "background {background_faces}, visible {visible_faces}");

    demand.remove(visible);
    let room = (visible_faces - 1) * FACE_METADATA_BYTES;
    let _blocker = budget.reserve(budget.memory_bytes() - budget.charged_bytes() - room).expect("blocker leaves one face short");
    demand.invalidate_all();
    settle_changes(&mut demand, &mut mirror, &identities);
    assert!(demand.targets.contains_key(&visible), "displacing one background object covers the missing face");
}

#[test]
fn growth_charges_added_faces() {
    let budget = Budget::new(Limits { memory_bytes: 64 << 20, worker_threads: 1 });
    let (mut world, region) = textured_world(1, AssetId::new(Uuid::from_u128(706)), |_, observation| posed_faces(observation, 100.0, 2));
    let (mut mirror, identities) = world_mirror(&world, &budget);
    let entity = identities[&Uuid::from_u128(1)];
    let mut demand = AssetDemand::new(budget.clone());
    demand.set_camera(camera());
    settle_changes(&mut demand, &mut mirror, &identities);
    let small_faces = demand.targets[&entity].faces;

    let mut observation = world::ObjectObservation::new(
        world::UpdateSource::new(region, 1, OrderEvidence::Ordered(2), 0),
        Uuid::from_u128(1),
        NonZeroU32::MIN,
        world::Kind::Object,
        world::ParentUpdate::Root,
    );
    posed_faces(&mut observation, 100.0, 8);
    let _applied = world.observe_object(observation).expect("grown appearance applies");
    let (mut grown, grown_identities) = world_mirror(&world, &budget);
    assert_eq!(grown_identities, identities);
    let probe = Budget::new(Limits { memory_bytes: UNBOUNDED_MEMORY_BYTES, worker_threads: 1 });
    let mut probe_demand = AssetDemand::new(probe);
    probe_demand.set_camera(camera());
    let (mut probe_mirror, _) = world_mirror(&world, &budget);
    settle_changes(&mut probe_demand, &mut probe_mirror, &identities);
    let large_faces = probe_demand.targets[&entity].faces;
    assert!(small_faces < large_faces, "small {small_faces}, large {large_faces}");

    let room = (large_faces - small_faces) * FACE_METADATA_BYTES;
    let _blocker = budget.reserve(budget.memory_bytes() - budget.charged_bytes() - room).expect("blocker leaves the added faces");
    demand.invalidate_all();
    for _ in 0..64 {
        if !demand.has_work() {
            break;
        }
        for change in demand.step(&grown, &identities) {
            grown.apply(Change::Material(change));
        }
        assert!(demand.targets.contains_key(&entity), "growth reserves only the added faces and is never rejected");
    }
    assert_eq!(demand.targets.get(&entity).map(|target| target.faces), Some(large_faces));
}

fn posed_faces(observation: &mut world::ObjectObservation, x: f32, faces: u8) {
    observation.pose = Some(world::Pose { position: Some(Vec3::new(x, 128.0, 32.0)), rotation: Some(math::Quat::IDENTITY) });
    let overrides = (1..faces).map(|slot| (slot, FaceAppearance { color: [slot, 30, 60, 255], ..FaceAppearance::default() })).collect();
    observation.surface_appearance = Some(world::SurfaceAppearance::new(FaceAppearance::default(), overrides).expect("fixture faces"));
}

fn settle_changes(demand: &mut AssetDemand, mirror: &mut Mirror, identities: &BTreeMap<Uuid, Entity>) {
    for _ in 0..64 {
        if !demand.has_work() {
            return;
        }
        for change in demand.step(mirror, identities) {
            mirror.apply(Change::Material(change));
        }
    }
    panic!("bounded fixture scan finishes");
}
