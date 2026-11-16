use std::{
    assert_matches,
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Instant,
};

use assets::{Budget, Content, LoadJob, Priority, Request, Resource, Sampler, Status, Store};
use math::{DVec3, Quat, Vec3};
use proptest::{prelude::*, test_runner::RngSeed};
use utils::AssetId;
use uuid::Uuid;
use world::Region;

use super::{DrawKey, DrawList, MAX_FACE_INSTANCES};
use crate::{
    geometry::{
        ShadingMode,
        cache::FrameCache,
        instance::{instance_base, parameter_part},
        items::{CachedFace, CachedItem, ItemId, ItemIndex, ItemSlab, MeshInfo},
    },
    material::FaceParams,
    pipeline::Pipeline,
    scene::{Camera, SharedMesh},
    texture::TextureKey,
};

type OpaqueOutput = HashMap<(DrawKey, Vec<u8>), usize>;
type BlendedOutput = Vec<(DrawKey, Vec<u8>)>;

#[test]
fn canceled_layout_releases_groups_in_bounded_chunks() {
    let (_region_world, _view, camera, mut items, mut list, index) = active_layout();
    let group = items.items.get(index).expect("active item").faces[0].group;
    let active_refs = group_refs(&list, group);
    let active_commands = list.draws().len();
    let active_items = list.visible_items();
    let active_bytes = list.instance_bytes.len();
    let staged = staged_layout(&mut list, index, camera, &items.items, STAGED_REFS);
    assert_eq!(group_refs(&list, group), active_refs + STAGED_REFS);
    list.pending = Some(staged);
    let initial_entries = list.pending.as_ref().map(staged_entry_count).expect("pending staged build");
    let initial_allocation = list.pending.as_ref().map(staged_allocation_bytes).expect("pending staged build");
    list.cancel_layout();

    assert!(list.layout_pending());
    assert!(list.pending.is_none());
    assert_eq!(staged_entry_count(list.retired.as_ref().expect("full staged build is retained")), initial_entries);
    for _ in 0..8 {
        assert!(!list.request_layout(&[index], camera, false, &items.items));
        list.cancel_layout();
        assert!(list.pending.is_none());
        assert!(list.retired.is_some(), "repeated requests cannot add retired builds");
        assert_eq!(staged_allocation_bytes(list.retired.as_ref().expect("single retired build")), initial_allocation);
    }

    let mut steps = 0;
    while list.layout_pending() {
        let before = list.retired_entries_remaining();
        let _ = list.advance_layout(&mut items.items, &HashMap::new(), false);
        assert!(before.saturating_sub(list.retired_entries_remaining()) <= super::RETIRED_LAYOUT_ENTRIES_PER_FRAME);
        assert_eq!(list.draws().len(), active_commands, "retirement leaves active draw commands intact");
        assert_eq!(list.visible_items(), active_items, "retirement leaves active visibility intact");
        assert_eq!(list.instance_bytes.len(), active_bytes, "retirement leaves active instance data intact");
        steps += 1;
        assert!(steps < 100, "staged group references are released in bounded turns");
    }
    assert!(steps > 1, "many staged group references require multiple retirement turns");
    assert!(list.retired.is_none(), "completed retirement releases the whole staged allocation");
    assert_eq!(group_refs(&list, group), active_refs, "retirement releases every staged group reference");
    assert!(list.request_layout(&[index], camera, false, &items.items));
}

#[test]
fn stale_layout_retires_the_whole_build() {
    let (_region_world, _view, camera, mut items, mut list, index) = active_layout();
    let group = items.items.get(index).expect("active item").faces[0].group;
    let active_refs = group_refs(&list, group);
    let active_commands = list.draws().len();
    let active_items = list.visible_items();
    let staged = staged_layout(&mut list, index, camera, &items.items, STAGED_REFS);
    list.pending = Some(staged);
    list.mark_layout_stale();

    let progress = list.advance_layout(&mut items.items, &HashMap::new(), false).expect("stale build is observed");
    assert!(progress.stale);
    assert!(list.pending.is_none());
    assert!(list.retired.is_some(), "stale staged inputs remain owned by incremental retirement");
    assert_eq!(list.draws().len(), active_commands, "stale build cannot replace active commands");
    assert_eq!(list.visible_items(), active_items, "stale build cannot replace active visibility");
    assert!(list.retired_entries_remaining() > 0, "stale group references remain owned until retirement");
    let mut steps = 0;
    while list.layout_pending() {
        let before = list.retired_entries_remaining();
        let _ = list.advance_layout(&mut items.items, &HashMap::new(), false);
        assert!(before.saturating_sub(list.retired_entries_remaining()) <= super::RETIRED_LAYOUT_ENTRIES_PER_FRAME);
        assert_eq!(list.draws().len(), active_commands, "retirement leaves active draw commands intact");
        assert_eq!(list.visible_items(), active_items, "retirement leaves active visibility intact");
        steps += 1;
        assert!(steps < 100, "stale group references are released in bounded turns");
    }
    assert!(list.retired.is_none(), "stale allocations are eventually reclaimed");
    assert_eq!(group_refs(&list, group), active_refs, "retirement releases every staged group reference");
}

#[test]
fn canceled_build_keeps_committed_patches() {
    let (_region_world, region) = crate::test_support::registered_region();
    let (_scene, view, _) = super::super::fixtures::scene(255);
    let mesh = view.objects().next().expect("fixture object").mesh().expect("fixture mesh").1.clone();
    let textures = [loaded_texture(9_100_001), loaded_texture(9_100_002)];
    let camera = camera(0.0);
    let origin = super::super::instance::scene_origin(camera.position());
    let mut items = FrameCache::default();
    let mut list = DrawList::default();
    let mut visible = (1..=1_100)
        .map(|id| items.insert_item(new_item((2, id, 0), 0, 0.0, origin, &mesh, region, &textures, &mut list)).expect("test item index"))
        .collect::<Vec<_>>();
    list.layout(&[], &visible, &mut items.items, camera);
    let target = visible[500];
    let target_slot = |items: &FrameCache| items.items.get(target).expect("target item").faces[0].slot.expect("target face slot");
    let committed = target_slot(&items);
    let rotate = |items: &mut FrameCache, list: &mut DrawList, angle: f32| {
        let item = items.items.get_mut(target).expect("target item");
        item.rotation = Quat::from_rotation_z(angle);
        item.transform = instance_base(item.position, item.rotation, item.scale, false, origin);
        list.patch(&items.items, &[(target, 0)], &[target], camera, origin, origin);
        items.items.get(target).expect("target item").transform
    };

    let entering = items.insert_item(new_item((2, 0, 0), 0, 0.0, origin, &mesh, region, &textures, &mut list)).expect("entering item");
    visible.insert(0, entering);
    assert!(list.request_layout(&visible, camera, true, &items.items));
    for _ in 0..8 {
        let _ = list.advance_layout(&mut items.items, &HashMap::new(), false);
        if list.staged_progress().is_some_and(|(phase, cursor, ..)| phase == "fill" && cursor > 501) {
            break;
        }
    }
    assert_matches!(list.staged_progress(), Some(("fill", cursor, ..)) if cursor > 501, "the build staged the target face");
    assert_ne!(target_slot(&items), committed, "the staged build moved the target face");

    let transform = rotate(&mut items, &mut list, 0.3);
    assert_eq!(list.instance_transform(committed), Some(&transform[..]), "a pending build keeps committed poses patched");
    list.cancel_layout();
    let transform = rotate(&mut items, &mut list, 0.6);
    assert_eq!(list.instance_transform(committed), Some(&transform[..]), "a canceled build keeps committed poses patched");
    for _ in 0..100 {
        if !list.layout_pending() {
            break;
        }
        let _ = list.advance_layout(&mut items.items, &HashMap::new(), false);
    }
    assert!(!list.layout_pending(), "the canceled build retires");
    let transform = rotate(&mut items, &mut list, 0.9);
    assert_eq!(list.instance_transform(committed), Some(&transform[..]), "retirement keeps committed poses patched");

    list.layout(&[], &visible, &mut items.items, camera);
    let next = target_slot(&items);
    assert_ne!(next, committed, "the next generation moves the target face");
    let transform = rotate(&mut items, &mut list, 1.2);
    assert_eq!(list.instance_transform(next), Some(&transform[..]), "the next generation patches its own address");
}

#[test]
fn plain_staged_data_retires_in_one_turn() {
    let (_region_world, _view, camera, mut items, mut list, index) = active_layout();
    let staged = staged_layout(&mut list, index, camera, &items.items, 0);
    assert!(staged_entry_count(&staged) > 4 * super::RETIRED_LAYOUT_ENTRIES_PER_FRAME);
    list.pending = Some(staged);
    list.cancel_layout();

    let _ = list.advance_layout(&mut items.items, &HashMap::new(), false);
    assert!(!list.layout_pending(), "staged data without resources does not hold the next layout back");
    assert!(list.request_layout(&[index], camera, false, &items.items));
}

#[test]
fn stateful_draw_changes_match_fresh_layouts() {
    let operations = vec![
        (0, 0, 0, 0, 0),
        (0, 1, 0, 0, 0),
        (0, 2, 5, 0, 0),
        (0, 3, 4, 0, 0),
        (2, 0, 1, 0, 0),
        (1, 1, 0, 0, 0),
        (0, 4, 0, 0, 0),
        (3, 2, 0, 1, -1),
        (4, 0, 0, 1, 0),
        (5, 0, 0, 0, 0),
    ];
    verify_operations(&operations);
}

#[test]
fn blended_parameter_patch_does_not_sort() {
    let (_region_world, region) = crate::test_support::registered_region();
    let (_scene, view, _) = super::super::fixtures::scene(255);
    let object = view.objects().next().expect("fixture object");
    let (_, mesh) = object.mesh().expect("fixture mesh");
    let mesh = mesh.clone();
    let textures = [loaded_texture(9_100_001), loaded_texture(9_100_002)];
    let camera = camera(0.0);
    let origin = super::super::instance::scene_origin(camera.position());
    let id = item_id(0);
    let mut items = FrameCache::default();
    let mut list = DrawList::default();
    let index = items.insert_item(new_item(id, 3, 0.0, origin, &mesh, region, &textures, &mut list)).expect("test item index");
    list.layout(&[], &[index], &mut items.items, camera);
    if let Some(face) = items.get_by_id_mut(id).and_then(|item| item.faces.first_mut()) {
        face.parameter_bytes[0] ^= 1;
    }
    let stats = list.patch(&items.items, &[(index, 0)], &[], camera, origin, origin);
    assert_eq!(stats.faces_patched, 1);
    assert!(!stats.blended_sorted);
}

#[test]
fn hiding_one_draw_slot_preserves_other_slots() {
    let (_region_world, region) = crate::test_support::registered_region();
    let (_scene, view, _) = super::super::fixtures::scene(255);
    let object = view.objects().next().expect("fixture object");
    let (_, mesh) = object.mesh().expect("fixture mesh");
    let mesh = mesh.clone();
    let textures = [loaded_texture(9_100_001), loaded_texture(9_100_002)];
    let camera = camera(0.0);
    let origin = super::super::instance::scene_origin(camera.position());
    let mut items = FrameCache::default();
    let mut list = DrawList::default();
    let first = items.insert_item(new_item(item_id(0), 0, 0.0, origin, &mesh, region, &textures, &mut list)).expect("first item index");
    let second = items.insert_item(new_item(item_id(1), 1, 0.0, origin, &mesh, region, &textures, &mut list)).expect("second item index");
    list.layout(&[], &[first, second], &mut items.items, camera);
    let hidden_slot = items.items.get(first).and_then(|item| item.faces.first()).and_then(|face| face.slot).expect("first draw slot");
    let survivor_slot = items.items.get(second).and_then(|item| item.faces.first()).and_then(|face| face.slot).expect("second draw slot");

    let hidden = items.items.get_mut(first).and_then(|item| item.faces.first_mut()).expect("first face");
    assert!(list.hide_face(hidden));

    assert!(list.batches_dirty());
    assert!(list.hides_range(hidden_slot, 1));
    assert!(!list.hides_range(survivor_slot, 1));
}

#[test]
fn overflow_omits_items_until_within_budget() {
    let (_region_world, region) = crate::test_support::registered_region();
    let (_scene, view, _) = super::super::fixtures::scene(255);
    let object = view.objects().next().expect("fixture object");
    let (_, mesh) = object.mesh().expect("fixture mesh");
    let mesh = mesh.clone();
    let textures = [loaded_texture(9_100_001), loaded_texture(9_100_002)];
    let camera = camera(0.0);
    let origin = super::super::instance::scene_origin(camera.position());
    let faces_per_item = MAX_FACE_INSTANCES / 2 + 1;
    let mut items = FrameCache::default();
    let mut list = DrawList::default();
    let mut indices = Vec::new();

    for (slot, style, x) in [(0, 0, 1.0), (1, 3, 100.0)] {
        let id = item_id(slot);
        let mut item = new_item(id, style, 0.0, origin, &mesh, region, &textures, &mut list);
        item.position = DVec3::new(x, 0.0, 0.0);
        let first_face = item.faces.pop().expect("test item face");
        let center = item.info.centers[0];
        let scale = item.scale;
        let mut faces = Vec::with_capacity(faces_per_item);
        faces.push(first_face);
        for _ in 1..faces_per_item {
            faces.push(new_face(id, style, &mesh, center, scale, &textures, &mut list));
        }
        item.faces = faces;
        indices.push(items.insert_item(item).expect("test item index"));
    }

    let visible = indices.clone();
    let stats = list.layout(&[], &visible, &mut items.items, camera);
    let omitted = items.items.get(indices[1]).expect("omitted item");
    let emitted = list.draws().iter().map(|draw| draw.count as usize).sum::<usize>();
    let retained_slots: HashSet<_> =
        items.items.get(indices[0]).expect("retained item").faces.iter().filter_map(|face| face.slot).collect();

    assert_eq!(stats.omitted_faces, faces_per_item);
    assert!(list.instance_count <= MAX_FACE_INSTANCES);
    assert_eq!(emitted, list.instance_count);
    assert_eq!(retained_slots.len(), faces_per_item);
    assert!(retained_slots.iter().all(|slot| usize::try_from(*slot).is_ok_and(|slot| slot < MAX_FACE_INSTANCES)));
    assert!(omitted.faces.iter().all(|face| face.slot.is_none()), "omitted items cannot own instance slots");
    assert!(list.draws().iter().all(|draw| draw.pipeline != Pipeline::Blend), "omitted items cannot appear in draw commands");

    let retained_faces = MAX_FACE_INSTANCES - faces_per_item - 1;
    let removed = items.items.get_mut(indices[1]).expect("second item").faces.split_off(retained_faces);
    list.release_faces(&removed);
    let stats = list.layout(&visible, &visible, &mut items.items, camera);
    let emitted = list.draws().iter().map(|draw| draw.count as usize).sum::<usize>();
    let slots: HashSet<_> = indices
        .iter()
        .filter_map(|index| items.items.get(*index))
        .flat_map(|item| item.faces.iter().filter_map(|face| face.slot))
        .collect();

    assert_eq!(stats.omitted_faces, 0);
    assert_eq!(list.instance_count, faces_per_item + retained_faces);
    assert_eq!(emitted, list.instance_count);
    assert_eq!(slots.len(), list.instance_count);
    assert!(slots.iter().all(|slot| usize::try_from(*slot).is_ok_and(|slot| slot < MAX_FACE_INSTANCES)));
    assert!(list.instance_count < MAX_FACE_INSTANCES);
    assert!(indices.iter().all(|index| items.items.get(*index).is_some_and(|item| item.faces.iter().all(|face| face.slot.is_some()))));
    assert!(list.draws().iter().any(|draw| draw.pipeline == Pipeline::Blend), "restored items return to draw commands");
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 48,
        max_shrink_iters: 20_000,
        rng_seed: RngSeed::Fixed(0x8d12_2d3a_a5c7_49e1),
        ..ProptestConfig::default()
    })]

    #[test]
    fn randomized_updates_match_fresh_layouts(
        operations in prop::collection::vec((0u8..6, 0u8..8, 0u8..8, -3i8..4, -3i8..4), 1..80)
    ) {
        let generated = operations;
        let mut operations = vec![
            (0, 0, 0, 0, 0),
            (0, 1, 3, 0, 0),
            (0, 2, 5, 0, 0),
            (2, 0, 1, 0, 0),
            (3, 1, 0, 1, -1),
            (4, 0, 0, 1, 0),
            (5, 0, 0, 0, 0),
            (1, 1, 0, 0, 0),
        ];
        operations.extend(generated);
        verify_operations(&operations);
    }
}

fn verify_operations(operations: &[(u8, u8, u8, i8, i8)]) {
    let (_region_world, region) = crate::test_support::registered_region();
    let (_scene, view, _) = super::super::fixtures::scene(255);
    let first_object = view.objects().next().expect("fixture object");
    let (_, mesh) = first_object.mesh().expect("fixture mesh");
    let mesh = mesh.clone();
    let textures = [loaded_texture(9_100_001), loaded_texture(9_100_002)];
    let camera_x = 0.0;
    let current_camera = camera(camera_x);
    let origin = super::super::instance::scene_origin(current_camera.position());
    let mut items = FrameCache::default();
    let mut list = DrawList::default();
    let mut visible = visible_items(&items.items, camera_x);
    list.layout(&[], &visible, &mut items.items, current_camera);
    assert_matches_fresh(&list, &items.items, &visible, current_camera);
    let mut camera_x = camera_x;
    let mut origin = origin;

    for (kind, item_index, style, delta_x, delta_y) in operations {
        let id = item_id(*item_index);
        let current_camera = camera(camera_x);
        let previous_visible = visible.clone();
        let mut camera_changed = false;
        let mut layout = false;
        let mut blended_patch = false;
        match kind % 6 {
            0 => {
                if items.index(id).is_none() {
                    items
                        .insert_item(new_item(id, *style, camera_x, origin, &mesh, region, &textures, &mut list))
                        .expect("test item index");
                    layout = true;
                }
            }
            1 => {
                if let Some(index) = items.index(id)
                    && let Some((item, _)) = items.remove_item(index)
                {
                    list.release_faces(&item.faces);
                    layout = true;
                }
            }
            2 => {
                if let Some(item) = items.get_by_id_mut(id) {
                    list.release_faces(&item.faces);
                    item.faces = vec![new_face(id, *style, &item.mesh, item.info.centers[0], item.scale, &textures, &mut list)];
                    layout = true;
                }
            }
            3 => {
                let changed = if let Some(item) = items.get_by_id_mut(id) {
                    item.position.x += f64::from(*delta_x);
                    item.position.y += f64::from(*delta_y);
                    item.rotation = Quat::from_rotation_y(f32::from(*delta_x) * 0.025);
                    item.transform = instance_base(item.position, item.rotation, item.scale, item.invert_normals, origin);
                    true
                } else {
                    false
                };
                if changed {
                    let index = items.index(id).expect("updated test item");
                    let stats = list.patch(&items.items, &[(index, 0)], &[index], current_camera, origin, origin);
                    blended_patch = stats.blended_sorted;
                }
            }
            4 => {
                camera_x += f64::from(*delta_x);
                camera_changed = true;
                let next_origin = super::super::instance::scene_origin(camera(camera_x).position());
                if next_origin != origin {
                    origin = next_origin;
                    for (_, item) in items.items.iter_mut() {
                        item.transform = instance_base(item.position, item.rotation, item.scale, item.invert_normals, origin);
                    }
                    layout = true;
                }
            }
            _ => {
                camera_x = (camera_x / 256.0).floor() * 256.0 + 256.0;
                camera_changed = true;
                origin = super::super::instance::scene_origin(camera(camera_x).position());
                for (_, item) in items.items.iter_mut() {
                    item.transform = instance_base(item.position, item.rotation, item.scale, item.invert_normals, origin);
                }
                layout = true;
            }
        }

        let camera = camera(camera_x);
        let next_visible = visible_items(&items.items, camera_x);
        layout |= previous_visible != next_visible;
        if layout {
            list.layout(&previous_visible, &next_visible, &mut items.items, camera);
        } else if camera_changed && list.has_blended() {
            list.sort_blended(&items.items, camera);
        } else if blended_patch {
            list.reorder_blended(&items.items);
        }
        visible = next_visible;
        assert_matches_fresh(&list, &items.items, &visible, camera);
    }
}

fn active_layout() -> (world::World, crate::scene::SceneView, Camera, FrameCache, DrawList, ItemIndex) {
    let (_region_world, region) = crate::test_support::registered_region();
    let (_scene, view, _) = super::super::fixtures::scene(255);
    let object = view.objects().next().expect("fixture object");
    let (_, mesh) = object.mesh().expect("fixture mesh");
    let mesh = mesh.clone();
    let textures = [loaded_texture(9_100_001), loaded_texture(9_100_002)];
    let camera = camera(0.0);
    let origin = super::super::instance::scene_origin(camera.position());
    let mut items = FrameCache::default();
    let mut list = DrawList::default();
    let index = items.insert_item(new_item(item_id(0), 0, 0.0, origin, &mesh, region, &textures, &mut list)).expect("test item index");
    list.layout(&[], &[index], &mut items.items, camera);
    (_region_world, view, camera, items, list, index)
}

const STAGED_REFS: usize = 2_047;

fn staged_layout(list: &mut DrawList, index: ItemIndex, camera: Camera, items: &ItemSlab, refs: usize) -> super::LayoutBuild {
    let input = vec![index; 2_047];
    let mut build = super::LayoutBuild::new(Default::default(), &input, &input, camera, true, items);
    let group = items.get(index).expect("staged item").faces[0].group;
    for _ in 0..refs {
        assert!(list.groups.retain(group));
        build.state.draw_group_refs.push(group);
    }
    build.state.instance_bytes.resize(3 * 1024 * 1024, 0);
    build.state.indirect_bytes.resize(2 * 1024 * 1024, 0);
    build
}

fn group_refs(list: &DrawList, group: u32) -> usize {
    list.groups.group(group).expect("interned group").refs
}

fn staged_entry_count(build: &super::LayoutBuild) -> usize {
    let state = &build.state;
    build.previous.len()
        + build.visible.len()
        + build.mesh_pins.len()
        + build.layout_items.len()
        + build.selected_items.len()
        + build.batch_seen.len()
        + build.group_limits.len()
        + build.priority.len()
        + build.blended.len()
        + state.draws.len()
        + state.mesh_references.len()
        + state.active_groups.len()
        + state.active_textures.len()
        + state.active_terrain.len()
        + state.texture_seen.len()
        + state.terrain_seen.len()
        + state.group_counts.len()
        + state.group_offsets.len()
        + state.group_cursors.len()
        + state.layout_items.len()
        + state.priority_items.len()
        + state.draw_group_refs.len()
        + state.draw_group_seen.len()
        + state.blended.len()
        + state.dirty_slots.len()
        + state.batch_order.len()
        + state.draw_batches.len()
        + state.first_instances.len()
}

fn staged_allocation_bytes(build: &super::LayoutBuild) -> usize {
    fn vec_bytes<T>(values: &Vec<T>) -> usize {
        values.capacity().saturating_mul(std::mem::size_of::<T>())
    }

    fn set_bytes<T>(values: &HashSet<T>) -> usize {
        values.capacity().saturating_mul(std::mem::size_of::<T>())
    }

    let state = &build.state;
    [
        vec_bytes(&build.previous),
        vec_bytes(&build.visible),
        set_bytes(&build.mesh_pins),
        vec_bytes(&build.layout_items),
        set_bytes(&build.selected_items),
        set_bytes(&build.batch_seen),
        vec_bytes(&build.group_limits),
        build.priority.capacity().saturating_mul(std::mem::size_of::<super::PriorityItem>()),
        build.blended.capacity().saturating_mul(std::mem::size_of::<super::BlendedFace>()),
        vec_bytes(&state.draws),
        set_bytes(&state.mesh_references),
        vec_bytes(&state.instance_bytes),
        vec_bytes(&state.active_groups),
        vec_bytes(&state.active_textures),
        vec_bytes(&state.active_terrain),
        set_bytes(&state.texture_seen),
        set_bytes(&state.terrain_seen),
        vec_bytes(&state.group_counts),
        vec_bytes(&state.group_offsets),
        vec_bytes(&state.group_cursors),
        vec_bytes(&state.layout_items),
        vec_bytes(&state.priority_items),
        vec_bytes(&state.draw_group_refs),
        set_bytes(&state.draw_group_seen),
        vec_bytes(&state.blended),
        vec_bytes(&state.dirty_slots),
        vec_bytes(&state.batch_order),
        vec_bytes(&state.draw_batches),
        vec_bytes(&state.indirect_bytes),
        vec_bytes(&state.first_instances),
    ]
    .into_iter()
    .fold(0usize, usize::saturating_add)
}

fn new_item(
    id: ItemId,
    style: u8,
    camera_x: f64,
    origin: DVec3,
    mesh: &SharedMesh,
    region: Region,
    textures: &[assets::TextureHandle; 2],
    list: &mut DrawList,
) -> CachedItem {
    let scale = Vec3::ONE;
    let position = DVec3::new(camera_x + (id.1 % 3) as f64 - 1.0, (id.1 % 2) as f64, 0.0);
    let rotation = Quat::IDENTITY;
    let bounds = mesh.mesh().bounds();
    let center = bounds.min * 0.5 + bounds.max * 0.5;
    let info = Arc::new(MeshInfo { bounds, centers: vec![center] });
    let transform = instance_base(position, rotation, scale, false, origin);
    let face = new_face(id, style, mesh, center, scale, textures, list);
    CachedItem {
        id,
        entity: None,
        region,
        mesh: mesh.clone(),
        info,
        position,
        rotation,
        scale,
        invert_normals: false,
        replacement: None,
        transform,
        pickable: true,
        shading: ShadingMode::Surface,
        terrain_textures: None,
        terrain_composition: None,
        surface_kind: None,
        source: None,
        faces: vec![face],
        animated_faces: Vec::new(),
        animation_deadline: None,
        parameters_dirty: false,
    }
}

fn new_face(
    id: ItemId,
    style: u8,
    mesh: &SharedMesh,
    center: Vec3,
    scale: Vec3,
    textures: &[assets::TextureHandle; 2],
    list: &mut DrawList,
) -> CachedFace {
    let mesh_face = mesh.mesh().faces().first().expect("fixture mesh face");
    let (pipeline, texture_index, blend, double_sided): (Pipeline, Option<usize>, bool, bool) = match style % 6 {
        0 => (Pipeline::Surface, None, false, false),
        1 => (Pipeline::Surface, Some(0), false, false),
        2 => (Pipeline::DoubleSided, Some(1), false, true),
        3 => (Pipeline::Blend, None, true, false),
        4 => (Pipeline::BlendDoubleSided, Some(0), true, true),
        _ => (Pipeline::BlendFlipped, Some(1), true, false),
    };
    let texture = texture_index.map(|index| textures[index].clone());
    let mut params = FaceParams::new(None);
    params.blend = blend;
    params.double_sided = double_sided;
    params.texture = texture.as_ref().map(|handle| TextureKey { id: handle.id(), sampler: Sampler::default() });
    params.color = [0.3 + f32::from(style) * 0.05, 0.4, 0.5, 1.0];
    let key = DrawKey {
        mesh: mesh.id(),
        start: mesh_face.indices.start,
        end: mesh_face.indices.end,
        pipeline,
        texture: params.texture,
        terrain: None,
    };
    let group = list.intern_face(key, texture.as_ref(), None).expect("test draw group ID");
    CachedFace {
        order: (id.0, id.1, id.2, mesh_face.slot),
        indices: mesh_face.indices.clone(),
        center,
        base_params: params,
        params,
        texture,
        material_index: None,
        texture_entry_transform: Default::default(),
        group,
        blended: key.blended(),
        parameter_bytes: parameter_part(&params, scale),
        slot: None,
        blended_index: None,
        layout: Default::default(),
        retained: None,
    }
}

fn assert_matches_fresh(list: &DrawList, items: &ItemSlab, visible: &[ItemIndex], camera: Camera) {
    let expected = fresh_output(list, items, visible, camera);
    let mut opaque = OpaqueOutput::new();
    let mut blended = BlendedOutput::new();
    let mut slots = HashSet::new();
    for (index, draw) in list.draws.iter().enumerate() {
        let key = DrawKey {
            mesh: draw.mesh,
            start: draw.indices.start,
            end: draw.indices.end,
            pipeline: draw.pipeline,
            texture: draw.texture,
            terrain: draw.terrain,
        };
        for slot in draw.first..draw.first.saturating_add(draw.count) {
            assert!(slots.insert(slot), "each draw command covers a unique instance slot");
            let bytes = instance_bytes(list, slot);
            if index < list.opaque_draws {
                *opaque.entry((key, bytes)).or_default() += 1;
            } else {
                blended.push((key, bytes));
            }
        }
    }
    let expected_slots =
        items.iter().filter(|(index, _)| contains_item(visible, items, *index)).map(|(_, item)| item.faces.len()).sum::<usize>();
    assert_eq!(slots.len(), expected_slots, "every visible face has one draw slot");
    assert_eq!(list.instance_count, expected_slots, "instance output contains only visible face bytes");
    assert!(opaque == expected.0, "opaque draw key and packed byte multiset differs from a fresh layout");
    assert!(blended == expected.1, "blended draw keys or back-to-front packed byte order differs from a fresh layout");
    for (index, item) in items.iter() {
        for face in &item.faces {
            assert_eq!(face.slot.is_some(), contains_item(visible, items, index), "hidden items cannot retain instance slots");
        }
    }
}

fn fresh_output(list: &DrawList, items: &ItemSlab, visible: &[ItemIndex], camera: Camera) -> (OpaqueOutput, BlendedOutput) {
    let mut opaque = OpaqueOutput::new();
    let mut blended = Vec::new();
    for id in visible {
        let Some(item) = items.get(*id) else { continue };
        for face in &item.faces {
            let Some(key) = list.face_key(face.group) else { continue };
            let bytes = instance_from_parts(&item.transform, &face.parameter_bytes);
            if key.blended() {
                let relative = item.position - camera.position() + (item.rotation * (face.center * item.scale)).as_dvec3();
                blended.push((relative.length_squared(), face.order, key, bytes));
            } else {
                *opaque.entry((key, bytes)).or_default() += 1;
            }
        }
    }
    blended.sort_unstable_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    (opaque, blended.into_iter().map(|(_, _, key, bytes)| (key, bytes)).collect())
}

fn instance_bytes(list: &DrawList, slot: u32) -> Vec<u8> {
    let start = slot as usize * crate::pipeline::INSTANCE_BYTES;
    let end = start + crate::pipeline::INSTANCE_BYTES;
    list.instance_bytes.get(start..end).expect("draw slot has packed instance bytes").to_vec()
}

fn instance_from_parts(
    transform: &[u8; super::super::TRANSFORM_BYTES],
    parameters: &[u8; super::super::instance::PARAMETER_BYTES],
) -> Vec<u8> {
    transform.iter().chain(parameters).copied().collect()
}

fn visible_items(items: &ItemSlab, camera_x: f64) -> Vec<ItemIndex> {
    let mut visible =
        items.iter().filter(|(_, item)| (item.position.x - camera_x).abs() <= 6.0).map(|(index, _)| index).collect::<Vec<_>>();
    visible.sort_unstable_by_key(|index| items.get(*index).map(|item| item.id));
    visible
}

fn contains_item(visible: &[ItemIndex], items: &ItemSlab, index: ItemIndex) -> bool {
    let Some(id) = items.get(index).map(|item| item.id) else { return false };
    visible.binary_search_by(|visible| items.get(*visible).map_or(std::cmp::Ordering::Less, |item| item.id.cmp(&id))).is_ok()
}

fn camera(camera_x: f64) -> Camera {
    Camera::new(DVec3::new(camera_x, 0.0, 0.0), Vec3::X, 1.0, [128, 128]).expect("test camera")
}

fn item_id(index: u8) -> ItemId {
    (2, u128::from(index), 0)
}

fn loaded_texture(id: u128) -> assets::TextureHandle {
    let mut store = Store::new(Budget::default());
    let route = store.register_route().expect("test asset route");
    let now = Instant::now();
    let resource = Resource { id: AssetId::new(Uuid::from_u128(id)), kind: assets::Kind::Texture };
    let demand = store.request(route, Request::new(resource), Priority::Visible, now).expect("test texture request");
    let LoadJob::Fetch(fetch) = store.next_job(now).expect("fresh test texture fetch") else {
        panic!("fresh test texture used a cache entry")
    };
    let input = fetch.complete(Ok(test_support::texture::GRAY_ALPHA_J2C.to_vec())).expect("test texture response");
    let completion = store.start_decode(input).expect("test decode admission").run();
    store.complete(completion, now);
    let Status::Ready(Content::Texture(texture)) = demand.status() else { panic!("test texture did not decode") };
    texture
}
