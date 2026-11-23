use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use math::{DVec3, Quat, Vec3};
use world::Region;

use super::{CacheRebuild, CacheUpdate, FrameCache, ItemId, MeshInfoBuild};
use crate::{
    geometry::{
        ShadingMode, fixtures,
        instance::instance_base,
        items::{CachedItem, CachedReplacement, mesh_info},
    },
    scene::{Camera, RANGE_MARGIN, SharedMesh},
    test_support,
};

#[test]
fn item_order_tracks_lifecycle() {
    let (_region_world, region) = test_support::registered_region();
    let (_scene, view, _) = fixtures::scene(255);
    let mesh = view.objects().next().expect("fixture object").mesh().expect("fixture mesh").1.clone();
    let mut cache = FrameCache::default();
    let high = (2, 9, 0);
    let middle = (1, 9, 0);
    let low = (0, 9, 0);
    let high_index = cache.insert_item(item(high, &mesh, region)).expect("high item index");
    let middle_index = cache.insert_item(item(middle, &mesh, region)).expect("middle item index");
    let low_index = cache.insert_item(item(low, &mesh, region)).expect("low item index");

    assert_eq!(ordered_ids(&mut cache), vec![low, middle, high]);

    assert_eq!(cache.insert_item(item(high, &mesh, region)), Some(high_index));
    assert_eq!(ordered_ids(&mut cache), vec![low, middle, high]);

    assert!(cache.remove_item(middle_index).is_some());
    let earlier = (0, 1, 0);
    assert_eq!(cache.insert_item(item(earlier, &mesh, region)), Some(middle_index));
    assert_eq!(ordered_ids(&mut cache), vec![earlier, low, high]);

    cache.items.get_mut(high_index).expect("high item").replacement = Some(Box::new(CachedReplacement { item: item(high, &mesh, region) }));
    cache.replacements.insert(high_index);
    assert_eq!(cache.promote_replacement(high_index).expect("promoted item").id, high);
    assert!(cache.items.get(high_index).is_some_and(|item| item.replacement.is_none()));
    assert!(!cache.replacements.contains(&high_index));
    assert_eq!(ordered_ids(&mut cache), vec![earlier, low, high]);

    cache.clear_items();
    assert!(ordered_ids(&mut cache).is_empty());
    cache.insert_item(item(high, &mesh, region)).expect("recovered high item index");
    cache.insert_item(item(low, &mesh, region)).expect("recovered low item index");
    assert_eq!(ordered_ids(&mut cache), vec![low, high]);

    assert!(cache.items.get(low_index).is_none());
}

#[test]
fn camera_recovery_progresses() {
    let (world, _scene, view) = fixtures::object_scene(1_100);
    let region = world.region_page(None, 1).expect("fixture region page").regions[0].region;
    let mesh = view.objects().next().expect("fixture object").mesh().expect("fixture mesh").1.clone();
    let mut cache = FrameCache::default();
    for object in view.objects() {
        let summary = &object.detail().summary;
        let mut cached = item((2, summary.id.as_u128(), 0), &mesh, region);
        cached.entity = Some(summary.entity);
        let index = cache.insert_item(cached).expect("cached object index");
        cache.objects.insert(summary.entity, index);
    }
    let camera = |turn: u32| {
        Camera::new(DVec3::new(0.0, 0.0, f64::from(turn) * RANGE_MARGIN), Vec3::X, 1.0, [128, 128]).expect("moving fixture camera")
    };
    let mut rebuild = CacheRebuild::new(&view, cache.slot_count(), false);
    rebuild.object_change_page_complete = true;
    rebuild.added_object_page_complete = true;
    rebuild.note_camera(camera(0));
    rebuild.note_camera(camera(1));
    rebuild.finish_mesh_info();
    cache.rebuild = Some(rebuild);

    let mut visited = HashSet::new();
    let mut pass_cameras = Vec::new();
    let mut erased_cursors = 0;
    let mut completed = None;
    for turn in 2..1_000 {
        let rebuild = cache.rebuild.as_mut().expect("recovery is pending until it completes");
        let cursor = rebuild.visibility_object_after;
        pass_cameras.push(rebuild.note_camera(camera(turn)));
        erased_cursors += usize::from(cursor.is_some() && rebuild.visibility_object_after.is_none());
        let mut update = CacheUpdate::default();
        let mut items = 0;
        while cache.advance_visibility(&view, view.sequence(), Instant::now(), &mut items, &mut update) {}
        visited.extend(update.visibility_items);
        if update.rebuild_completed {
            completed = Some(turn);
            break;
        }
    }

    assert!(
        completed.is_some(),
        "visibility recovery never completed while the camera moved every turn: {:?}",
        cache.rebuild.as_ref().map(CacheRebuild::progress)
    );
    assert_eq!(erased_cursors, 0, "camera updates must not erase the in-progress pass cursor");
    assert_eq!(visited.len(), cache.objects.len(), "the pass revisits every cached object");
    assert!(pass_cameras.iter().all(|pass| *pass == camera(1)), "the whole pass evaluates its snapshot camera");
}

#[test]
fn info_resumes_within_limit() {
    let (_scene, view, _) = fixtures::scene(255);
    let mesh = view.objects().next().expect("fixture object").mesh().expect("fixture mesh").1.clone();
    let indices = mesh.mesh().indices().len();
    let bytes_per_index = size_of::<u32>() + size_of::<Vec3>();
    let limit = 3 * bytes_per_index;
    let mut build = MeshInfoBuild::new(mesh.clone());
    let mut total = 0;
    let mut steps = 0;
    while !build.is_complete() {
        let (bytes, _) = build.advance(limit, usize::MAX, Duration::MAX);
        assert!(bytes <= limit, "one step stays within its byte limit: {bytes}");
        total += bytes;
        steps += 1;
        assert!(steps <= indices + mesh.mesh().faces().len(), "every step makes progress");
    }
    assert!(steps > mesh.mesh().faces().len(), "faces larger than the limit resume across steps");
    assert_eq!(total, indices * bytes_per_index, "each index is processed exactly once");
    assert_eq!(build.centers, mesh_info(&mesh).centers, "a resumed build matches the one-shot build");
}

fn ordered_ids(cache: &mut FrameCache) -> Vec<ItemId> {
    cache.rebuild_item_order();
    cache.item_order.iter().map(|index| cache.items.get(*index).expect("ordered item exists").id).collect()
}

fn item(id: ItemId, mesh: &SharedMesh, region: Region) -> CachedItem {
    let position = DVec3::ZERO;
    let rotation = Quat::IDENTITY;
    let scale = Vec3::ONE;
    CachedItem {
        id,
        entity: None,
        region,
        mesh: mesh.clone(),
        info: mesh_info(mesh),
        position,
        rotation,
        scale,
        invert_normals: false,
        replacement: None,
        transform: instance_base(position, rotation, scale, false, DVec3::ZERO),
        pickable: true,
        shading: ShadingMode::Surface,
        terrain_textures: None,
        terrain_composition: None,
        surface_kind: None,
        source: None,
        faces: Vec::new(),
        animated_faces: Vec::new(),
        animation_deadline: None,
        parameters_dirty: false,
    }
}
