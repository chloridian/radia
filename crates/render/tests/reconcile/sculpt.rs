use std::{assert_matches, f64::consts::FRAC_PI_3};

use assets::{Failure, Status, TextureUsage};
use render::scene::{AssetRequest, DrawnMeshKey, Geometry, MEMORY_BYTES, ReconcileStep};
use test_support::{mesh, texture::RGBA_J2C};
use world::{ObjectPatch, PrimShape, SculptFlags, SculptType};

use super::support::{
    AssetId, Budget, Camera, DVec3, FieldUpdate, Instant, NonZeroU32, ParentUpdate, Reconciler, Request, Shape, Uuid, Vec3, add,
    prepared_asset, settle_materials, source, world, world_region,
};

fn high_camera() -> Camera {
    Camera::new(DVec3::ZERO, Vec3::X, FRAC_PI_3, [1000, 1000]).unwrap()
}

fn add_sculpt(world: &mut world::World, id: u32, asset_id: AssetId, sculpt_type: SculptType, flags: SculptFlags, x: f32) {
    add(world, u128::from(id), id, ParentUpdate::Root, x, u64::from(id) * 4);
    let mut patch = ObjectPatch::new(source(world, 0, u64::from(id) * 4 + 1), NonZeroU32::new(id).unwrap());
    patch.shape = FieldUpdate::Set(Shape::Sculpt { asset_id, sculpt_type, flags });
    let _ = world.patch_object(patch).unwrap();
}

fn request_texture(work: &[AssetRequest], asset_id: AssetId) -> AssetRequest {
    work.iter()
        .find(|request| matches!(request.request, Request::Texture { id, .. } if id == asset_id))
        .copied()
        .expect("sculpt map texture was requested")
}

#[test]
fn placeholder_upgrades_to_mesh() {
    let sculpt_map = AssetId::new(Uuid::from_u128(901));
    let mut world = world();
    add_sculpt(&mut world, 1, sculpt_map, SculptType::Sphere, SculptFlags::default(), 2.0);
    let (mut scene, mut view) = Reconciler::new(Budget::default());
    let mut now = Instant::now();
    scene.set_camera(high_camera());

    let work = settle_materials(&mut scene, &mut view, &world, &mut now);
    let req = request_texture(&work.requested, sculpt_map);
    let object = view.objects().next().unwrap();
    assert_matches!(
        object.geometry(),
        Geometry::Sculpt { asset_id, sculpt_type: SculptType::Sphere, .. } if asset_id == sculpt_map
    );
    let Some((DrawnMeshKey::SculptPlaceholder, placeholder)) = object.mesh() else {
        panic!("expected sculpt placeholder before map arrives");
    };
    assert_eq!(placeholder.mesh().faces().len(), 1);
    assert_eq!(placeholder.mesh().faces()[0].slot, 0);
    assert!(!placeholder.mesh().indices().is_empty());

    let status = prepared_asset(req.request, RGBA_J2C.to_vec());
    let Status::Ready(assets::Content::Texture(texture)) = &status else {
        panic!("expected texture fixture");
    };
    let texture = texture.clone();
    scene.complete_asset(req.ticket, status);
    settle_materials(&mut scene, &mut view, &world, &mut now);

    assert_eq!(texture.usage(), TextureUsage::MainAndRender);
    let object = view.objects().next().unwrap();
    let Some((key, mesh)) = object.mesh() else {
        panic!("expected generated sculpt mesh");
    };
    assert_matches!(key, DrawnMeshKey::Sculpt(key) if key.asset_id == sculpt_map && key.sculpt_type == SculptType::Sphere);
    assert!(!mesh.mesh().indices().is_empty());
}

#[test]
fn sculpt_map_failure_keeps_placeholder() {
    let sculpt_map = AssetId::new(Uuid::from_u128(902));
    let mut world = world();
    add_sculpt(&mut world, 1, sculpt_map, SculptType::Torus, SculptFlags::default(), 2.0);
    let (mut scene, mut view) = Reconciler::new(Budget::default());
    let mut now = Instant::now();
    scene.set_camera(high_camera());

    let work = settle_materials(&mut scene, &mut view, &world, &mut now);
    let req = request_texture(&work.requested, sculpt_map);
    assert_matches!(view.objects().next().unwrap().mesh().map(|(key, _)| key), Some(DrawnMeshKey::SculptPlaceholder));

    scene.complete_asset(req.ticket, Status::Failed { failure: Failure::NotFound, retry_at: None });
    settle_materials(&mut scene, &mut view, &world, &mut now);

    assert_matches!(view.objects().next().unwrap().mesh().map(|(key, _)| key), Some(DrawnMeshKey::SculptPlaceholder));
}

#[test]
fn shared_map_deduplicates() {
    let sculpt_map = AssetId::new(Uuid::from_u128(903));
    let mut world = world();
    add_sculpt(&mut world, 1, sculpt_map, SculptType::Sphere, SculptFlags::default(), 2.0);
    add_sculpt(&mut world, 2, sculpt_map, SculptType::Sphere, SculptFlags::default(), 3.0);
    let (mut scene, mut view) = Reconciler::new(Budget::default());
    let mut now = Instant::now();
    scene.set_camera(high_camera());

    let work = settle_materials(&mut scene, &mut view, &world, &mut now);
    let matching: Vec<_> =
        work.requested.iter().filter(|req| matches!(req.request, Request::Texture { id, .. } if id == sculpt_map)).copied().collect();
    assert_eq!(matching.len(), 1, "two sculpts with the same map share one texture request");
    let req = matching[0];

    let status = prepared_asset(req.request, RGBA_J2C.to_vec());
    let Status::Ready(assets::Content::Texture(texture)) = &status else {
        panic!("expected texture fixture");
    };
    let texture = texture.clone();
    scene.complete_asset(req.ticket, status);
    settle_materials(&mut scene, &mut view, &world, &mut now);

    let meshes: Vec<_> = view.objects().map(|obj| obj.mesh().map(|(k, m)| (k, m.id()))).collect();
    assert_eq!(meshes.len(), 2);
    assert_eq!(meshes[0], meshes[1], "both sculpts share the same generated mesh");
    assert_eq!(texture.usage(), TextureUsage::MainAndRender);

    let _ = world.remove_object(source(&world, 0, 100), NonZeroU32::new(1).unwrap()).unwrap();
    scene.notify_world_change();
    let work = settle_materials(&mut scene, &mut view, &world, &mut now);
    assert!(!work.released.contains(&req.ticket), "removing one user keeps the shared sculpt map");
    assert_eq!(texture.usage(), TextureUsage::MainAndRender);

    let _ = world.remove_object(source(&world, 0, 101), NonZeroU32::new(2).unwrap()).unwrap();
    scene.notify_world_change();
    let work = settle_materials(&mut scene, &mut view, &world, &mut now);
    assert!(work.released.contains(&req.ticket), "removing the last user releases the sculpt map");
    assert_eq!(texture.usage(), TextureUsage::RenderOnly);
}

#[test]
fn shape_change_releases_stale() {
    let first_map = AssetId::new(Uuid::from_u128(904));
    let second_map = AssetId::new(Uuid::from_u128(905));
    let mut world = world();
    add_sculpt(&mut world, 1, first_map, SculptType::Sphere, SculptFlags::default(), 2.0);
    let (mut scene, mut view) = Reconciler::new(Budget::default());
    let mut now = Instant::now();
    scene.set_camera(high_camera());

    let work = settle_materials(&mut scene, &mut view, &world, &mut now);
    let first_req = request_texture(&work.requested, first_map);
    scene.complete_asset(first_req.ticket, prepared_asset(first_req.request, RGBA_J2C.to_vec()));

    let mut held_job = None;
    for _ in 0..64 {
        match scene.poll(now) {
            ReconcileStep::Build(job) => {
                held_job = Some(job);
                break;
            }
            ReconcileStep::Publish(batch) => view.apply(batch).unwrap(),
            ReconcileStep::Continue => {}
            ReconcileStep::Wait(next) => now = next,
            _ => break,
        }
    }
    let stale_completion = held_job.expect("sculpt build job started").run();

    let mut patch = ObjectPatch::new(source(&world, 0, 20), NonZeroU32::new(1).unwrap());
    patch.shape = FieldUpdate::Set(Shape::Sculpt {
        asset_id: second_map,
        sculpt_type: SculptType::Cylinder,
        flags: SculptFlags { invert: true, mirror: false },
    });
    let _ = world.patch_object(patch).unwrap();
    scene.notify_world_change();
    let work = settle_materials(&mut scene, &mut view, &world, &mut now);
    assert!(work.released.contains(&first_req.ticket));
    let second_req = request_texture(&work.requested, second_map);

    scene.complete_mesh(stale_completion);
    settle_materials(&mut scene, &mut view, &world, &mut now);
    assert_matches!(
        view.objects().next().unwrap().mesh().map(|(key, _)| key),
        Some(DrawnMeshKey::SculptPlaceholder),
        "stale sculpt completion for previous map is ignored"
    );

    scene.complete_asset(second_req.ticket, prepared_asset(second_req.request, RGBA_J2C.to_vec()));
    settle_materials(&mut scene, &mut view, &world, &mut now);
    assert_matches!(
        view.objects().next().unwrap().mesh().map(|(key, _)| key),
        Some(DrawnMeshKey::Sculpt(key))
            if key.asset_id == second_map && key.sculpt_type == SculptType::Cylinder && key.flags.invert
    );

    let mut patch = ObjectPatch::new(source(&world, 0, 21), NonZeroU32::new(1).unwrap());
    patch.shape = FieldUpdate::Set(Shape::Prim(PrimShape::default()));
    let _ = world.patch_object(patch).unwrap();
    scene.notify_world_change();
    let work = settle_materials(&mut scene, &mut view, &world, &mut now);
    assert!(work.released.contains(&second_req.ticket));
    assert_matches!(view.objects().next().unwrap().mesh().map(|(key, _)| key), Some(DrawnMeshKey::Prim(_)));
}

#[test]
fn region_retire_releases_demand() {
    let sculpt_map = AssetId::new(Uuid::from_u128(906));
    let mut world = world();
    let region = world_region(&world, 0);
    add_sculpt(&mut world, 1, sculpt_map, SculptType::Plane, SculptFlags::default(), 2.0);
    let (mut scene, mut view) = Reconciler::new(Budget::default());
    let mut now = Instant::now();
    scene.set_camera(high_camera());

    let work = settle_materials(&mut scene, &mut view, &world, &mut now);
    let req = request_texture(&work.requested, sculpt_map);
    scene.complete_asset(req.ticket, prepared_asset(req.request, RGBA_J2C.to_vec()));
    settle_materials(&mut scene, &mut view, &world, &mut now);
    assert_matches!(view.objects().next().unwrap().mesh().map(|(key, _)| key), Some(DrawnMeshKey::Sculpt(_)));

    let _ = world.retire_region(region).unwrap();
    scene.notify_world_change();
    let work = settle_materials(&mut scene, &mut view, &world, &mut now);
    assert!(work.released.contains(&req.ticket));
    assert_eq!(view.objects().count(), 0);
}

#[test]
fn mixed_stays_within_budget() {
    let budget = Budget::default();
    let mesh_asset = AssetId::new(Uuid::from_u128(907));
    let sculpt_map = AssetId::new(Uuid::from_u128(908));
    let bytes = mesh::asset(&[3], 1);
    let mut world = world();
    add(&mut world, 1, 1, ParentUpdate::Root, 2.0, 4);
    let mut patch = ObjectPatch::new(source(&world, 0, 5), NonZeroU32::new(1).unwrap());
    patch.shape = FieldUpdate::Set(Shape::Mesh { asset_id: mesh_asset, flags: SculptFlags::default() });
    let _ = world.patch_object(patch).unwrap();
    add_sculpt(&mut world, 2, sculpt_map, SculptType::Sphere, SculptFlags::default(), 2.5);

    let (mut scene, mut view) = Reconciler::new(budget.clone());
    let mut now = Instant::now();
    scene.set_camera(high_camera());

    let work = settle_materials(&mut scene, &mut view, &world, &mut now);
    let header_req = work.requested.iter().find(|r| matches!(r.request, Request::MeshHeader(id) if id == mesh_asset)).copied().unwrap();
    let sculpt_req = request_texture(&work.requested, sculpt_map);
    scene.complete_asset(header_req.ticket, prepared_asset(header_req.request, bytes.clone()));
    scene.complete_asset(sculpt_req.ticket, prepared_asset(sculpt_req.request, RGBA_J2C.to_vec()));
    let work = settle_materials(&mut scene, &mut view, &world, &mut now);
    if let Some(lod_req) = work.requested.iter().find(|r| matches!(r.request, Request::MeshLod(_))).copied() {
        scene.complete_asset(lod_req.ticket, prepared_asset(lod_req.request, bytes));
        settle_materials(&mut scene, &mut view, &world, &mut now);
    }

    assert!(budget.charged_bytes() <= MEMORY_BYTES);
    let mut saw_mesh = false;
    let mut saw_sculpt = false;
    for object in view.objects() {
        match object.mesh().map(|(k, _)| k) {
            Some(DrawnMeshKey::Asset { asset, .. }) if asset == mesh_asset => saw_mesh = true,
            Some(DrawnMeshKey::Sculpt(key)) if key.asset_id == sculpt_map => saw_sculpt = true,
            _ => {}
        }
    }
    assert!(saw_mesh && saw_sculpt, "both uploaded mesh and sculpt completed within budget");
}
