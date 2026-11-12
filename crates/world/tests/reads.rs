use std::{
    collections::HashSet,
    num::{NonZeroU8, NonZeroU32},
};

use math::{DVec3, Quat, Vec3};
use uuid::Uuid;
use world::{
    Appearance, Applied,
    ApplyOutcome::{Introduced, Removed},
    AttachmentPoint, CoordinateFrame, Entity, FieldUpdate, Kind,
    Kind::{Avatar, Object},
    LinearMotion, LocalAddress, MAX_MEDIA_URL_BYTES, MAX_MEDIA_WHITELIST_BYTES, MAX_OBJECT_DETAIL_BATCH_ITEMS,
    MAX_OBJECT_DETAIL_WORK_ITEMS, MAX_OBJECT_LOOKUP_WORK_ITEMS, MAX_OBJECT_PAGE_ITEMS, MAX_OBJECT_PAGE_WORK_ITEMS, MAX_REGION_PAGE_BYTES,
    MediaEntry, ObjectDetailResult, ObjectListItem, ObjectLookupProgress, ObjectMediaUpdate, ObjectObservation, ObjectPatch,
    OrderEvidence::Ordered,
    Parent, ParentUpdate,
    ParentUpdate::{Local, Root},
    Pose, PoseState, Region, RegionHandle, RegionInfo, UnresolvedReason, UpdateSource, World, WorldChange, WorldChangeClasses, WorldLimits,
    WorldPose, WorldReadError, WorldTransform, WorldTransformState,
};

fn world_limits() -> WorldLimits {
    WorldLimits {
        max_regions: 2,
        max_entities: 3,
        max_entities_per_region: 2,
        max_tombstones_per_region: 2,
        max_tombstones: 4,
        max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(2),
        max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(4),
        max_journal_entries: 12,
        max_journal_changes: 24,
        max_region_name_bytes: 32,
        max_recovery_targets: 2,
    }
}

fn uuid(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

fn source(region: Region, order: u64) -> UpdateSource {
    UpdateSource::new(region, order as u32, Ordered(order), 0)
}

fn local_id(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).expect("local ID is nonzero")
}

fn object(region: Region, order: u64, id: u128, local: u32, kind: Kind, parent: ParentUpdate) -> ObjectObservation {
    ObjectObservation::new(source(region, order), uuid(id), local_id(local), kind, parent)
}

fn entity(outcome: Applied) -> Entity {
    let Introduced(entity) = outcome.outcome else { panic!("expected object introduction") };
    entity
}

fn apply_ok<T>(result: Result<T, world::ApplyError>) {
    let _ = result.expect("World update");
}

fn deep_world(depth: u32) -> (World, Region) {
    let mut limits = world_limits();
    limits.max_entities = depth as usize;
    limits.max_entities_per_region = depth as usize;
    let mut world = World::new(limits);
    let region = world.register_region().unwrap();
    for id in 1..=depth {
        let parent = if id == 1 { Root } else { Local(local_id(id - 1)) };
        apply_ok(world.observe_object(object(region, u64::from(id), u128::from(id), id, Object, parent)));
    }
    (world, region)
}

#[test]
fn point_lookup_hides_until_complete() {
    let depth = 600;
    let (mut world, region) = deep_world(depth);
    apply_ok(world.remove_object(source(region, u64::from(depth) + 1), local_id(1)));

    let id = uuid(u128::from(depth));
    let address = LocalAddress::new(region, local_id(depth));
    let mut cursor = None;
    let mut turns = 0;
    loop {
        match world.lookup_entity(id, cursor) {
            ObjectLookupProgress::Pending { cursor: next, work_units } => {
                assert_eq!(work_units, MAX_OBJECT_LOOKUP_WORK_ITEMS);
                turns += 1;
                assert!(turns <= depth as usize / MAX_OBJECT_LOOKUP_WORK_ITEMS);
                cursor = Some(next);
            }
            ObjectLookupProgress::Hidden { work_units } => {
                assert!(work_units <= MAX_OBJECT_LOOKUP_WORK_ITEMS);
                break;
            }
            ObjectLookupProgress::Visible { entity, .. } => panic!("hidden descendant was exposed: {entity:?}"),
        }
    }
    assert!(turns >= 2);
    assert_eq!(world.lookup_entity_at(address, None), ObjectLookupProgress::Hidden { work_units: 0 });
    assert_eq!(world.entity(id), None);
    assert_eq!(world.entity_at(address), None);
}

#[test]
fn point_lookup_restarts_after_reparent() {
    let depth = 600;
    let (mut world, region) = deep_world(depth);
    let id = uuid(u128::from(depth));
    let entity = world.entity(id).unwrap();
    let ObjectLookupProgress::Pending { cursor, work_units } = world.lookup_entity(id, None) else { panic!("deep lookup must yield") };
    assert_eq!(work_units, MAX_OBJECT_LOOKUP_WORK_ITEMS);

    let updated = world.observe_object(object(region, u64::from(depth) + 1, u128::from(depth), depth, Object, Root)).unwrap();
    assert_eq!(updated.outcome, world::ApplyOutcome::Updated(entity));

    assert_eq!(world.lookup_entity(id, Some(cursor)), ObjectLookupProgress::Visible { entity, work_units: 1 });
    assert_eq!(world.entity(id), Some(entity));
    assert_eq!(world.lookup_entity(id, None), ObjectLookupProgress::Visible { entity, work_units: 0 });
}

#[test]
fn point_lookup_respects_the_callers_remaining_work_budget() {
    let (mut world, _) = deep_world(600);
    let id = uuid(600);
    let ObjectLookupProgress::Pending { cursor, work_units } = world.lookup_entity_with_budget(id, None, 7) else {
        panic!("deep lookup must stop at the smaller owner budget");
    };
    assert_eq!(work_units, 7);
    let ObjectLookupProgress::Pending { work_units, .. } = world.lookup_entity_with_budget(id, Some(cursor), 3) else {
        panic!("deep lookup must resume within the supplied budget");
    };
    assert_eq!(work_units, 3);
}

#[test]
fn object_pages_reuse_recent_ancestry_across_page_boundaries() {
    let depth = 4_096;
    let (mut world, region) = deep_world(depth);
    apply_ok(world.remove_object(source(region, u64::from(depth) + 1), local_id(1)));

    let mut cursor = None;
    let mut work = 0;
    let mut visible = 0;
    loop {
        let page = world.object_list_page_with_cache(cursor, MAX_OBJECT_PAGE_ITEMS).unwrap();
        work += page.work_units;
        visible += page.objects.len();
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }

    assert_eq!(visible, 0);
    assert!(work <= depth as usize * 3);
}

#[test]
fn borrowed_reads_expose_current_state() {
    let mut world = World::new(world_limits());
    let first = world.register_region().unwrap();
    let second = world.register_region().unwrap();
    let first_info = RegionInfo {
        region_id: Some(uuid(10)),
        handle: Some(RegionHandle::new((256_u64 << 32) | 512)),
        name: Some("First".to_owned()),
        origin: None,
    };
    std::assert_matches!(world.patch_region(first, first_info.clone()).unwrap().outcome, world::RegionOutcome::Patched { .. });
    assert_eq!(world.region(first), Some(&first_info));
    assert_eq!(first_info.handle.unwrap().origin(), DVec3::new(256.0, 512.0, 0.0));
    let mut foreign_world = World::new(world_limits());
    let foreign_region = foreign_world.register_region().unwrap();
    assert_eq!(world.region(foreign_region), None);

    let mut root = object(first, 1, 20, 1, Object, Root);
    root.pose = Some(Pose { position: Some(Vec3::new(3.0, 4.0, 5.0)), rotation: Some(Quat::IDENTITY) });
    root.linear_motion = Some(LinearMotion { velocity: Some(Vec3::X), acceleration: Some(Vec3::Y) });
    root.angular_velocity = Some(Vec3::Z);
    root.scale = Some(Vec3::splat(2.0));
    root.attachment_point = NonZeroU8::new(7).map_or(FieldUpdate::Clear, |value| FieldUpdate::Set(AttachmentPoint::new(value)));
    let root = entity(world.observe_object(root).unwrap());
    let child = entity(world.observe_object(object(first, 2, 21, 2, Object, Local(local_id(1)))).unwrap());
    let avatar = entity(world.observe_object(object(second, 1, 22, 1, Avatar, Root)).unwrap());

    assert_eq!(world.entity(uuid(20)), Some(root));
    assert_eq!(world.entity(uuid(99)), None);
    assert_eq!(world.entity_at(LocalAddress::new(first, local_id(1))), Some(root));
    assert_eq!(world.object_count(), 3);
    assert!(world.revision().get() > 0);
    assert_eq!(world.objects().iter().count(), 3);
    assert_eq!(world.objects_in_region(first).count(), 2);
    assert_eq!(world.objects_in_region(second).count(), 1);
    assert_eq!(world.regions().count(), 2);
    assert_eq!(world.children(root).count(), 1);
    assert_eq!(world.avatars().map(|object| object.entity()).collect::<Vec<_>>(), vec![avatar]);

    let view = world.object(root).expect("root exists");
    assert_eq!(view.entity(), root);
    assert_eq!(view.id(), uuid(20));
    assert_eq!(view.kind(), Object);
    assert_eq!(view.address(), LocalAddress::new(first, local_id(1)));
    assert_eq!(view.parent(), Parent::Root);
    assert_eq!(view.position(), Some(Vec3::new(3.0, 4.0, 5.0)));
    assert_eq!(view.rotation(), Some(Quat::IDENTITY));
    assert_eq!(view.scale(), Some(Vec3::splat(2.0)));
    assert_eq!(view.linear_motion(), LinearMotion { velocity: Some(Vec3::X), acceleration: Some(Vec3::Y) });
    assert_eq!(view.angular_velocity(), Some(Vec3::Z));
    assert_eq!(view.shape(), None);
    assert_eq!(view.appearance(), None);
    assert_eq!(view.attachment_point(), NonZeroU8::new(7).map(AttachmentPoint::new));
    assert_eq!(view.sit_target(), None);
    assert_eq!(view.coordinate_frame().unwrap(), CoordinateFrame::RegionRelative { region: first });
    assert_eq!(
        view.world_transform().unwrap(),
        WorldTransformState::Resolved(WorldTransform {
            pose: WorldPose { position: DVec3::new(259.0, 516.0, 5.0), rotation: Quat::IDENTITY },
            scale: Vec3::splat(2.0),
        })
    );
    assert_eq!(world.object(child).unwrap().coordinate_frame().unwrap(), CoordinateFrame::LinksetRoot { root });
    assert_eq!(world.object(child).unwrap().world_pose().unwrap(), PoseState::Unresolved(UnresolvedReason::MissingLocalPose));
}

#[test]
fn objects_returns_live_entities() {
    let mut world = World::new(world_limits());
    let region = world.register_region().unwrap();

    let obj = entity(world.observe_object(object(region, 1, 40, 1, Object, Root)).unwrap());
    let avatar = entity(world.observe_object(object(region, 2, 41, 2, Avatar, Root)).unwrap());
    assert_eq!(world.objects().iter().map(|object| object.id()).collect::<HashSet<_>>(), HashSet::from([uuid(40), uuid(41)]));
    assert_eq!(world.objects().iter().count(), world.object_count());

    assert_eq!(world.remove_object(source(region, 3), local_id(2)).unwrap().outcome, Removed(uuid(41)));
    assert!(world.object(avatar).is_none());
    assert_eq!(world.objects().iter().map(|object| object.id()).collect::<HashSet<_>>(), HashSet::from([uuid(40)]));

    let replacement = entity(world.observe_object(object(region, 4, 41, 2, Avatar, Root)).unwrap());
    assert_ne!(avatar, replacement);
    assert_eq!(world.objects().iter().map(|object| object.id()).collect::<HashSet<_>>(), HashSet::from([uuid(40), uuid(41)]));
    assert_eq!(world.objects().iter().count(), world.object_count());
    assert_eq!(world.object(obj).unwrap().id(), uuid(40));
}

#[test]
fn insert_after_cursor_is_listed() {
    let mut limits = world_limits();
    limits.max_entities = 4;
    limits.max_entities_per_region = 4;
    let mut world = World::new(limits);
    let region = world.register_region().unwrap();
    entity(world.observe_object(object(region, 1, 10, 1, Object, Root)).unwrap());
    entity(world.observe_object(object(region, 2, 20, 2, Object, Root)).unwrap());
    let barrier = world.read_barrier();

    let first = world.object_list_page(None, 1).unwrap();
    assert_eq!(first.hydration_revision, barrier.revision);
    assert_eq!(first.objects.iter().map(|item| item.id).collect::<Vec<_>>(), [uuid(10)]);
    let cursor = first.next.expect("second object remains");

    let inserted = entity(world.observe_object(object(region, 3, 5, 3, Object, Root)).unwrap());
    let second = world.object_list_page(Some(cursor), 1).unwrap();

    assert_eq!(second.objects.iter().map(|item| item.id).collect::<Vec<_>>(), [uuid(20)]);
    assert_eq!(second.next, None);
    let changes = world.change_page(barrier.changes, 8).unwrap();
    assert!(
        changes
            .records
            .iter()
            .any(|record| { matches!(record.change, WorldChange::EntityAdded { id, entity, .. } if id == uuid(5) && entity == inserted) })
    );
}

#[test]
fn object_pages_advance_over_retired_entities() {
    let mut limits = world_limits();
    limits.max_entities = MAX_OBJECT_PAGE_WORK_ITEMS + 1;
    limits.max_entities_per_region = MAX_OBJECT_PAGE_WORK_ITEMS;
    let mut world = World::new(limits);
    let retired = world.register_region().unwrap();
    let active = world.register_region().unwrap();
    for id in 1..=MAX_OBJECT_PAGE_WORK_ITEMS {
        let _ = world.observe_object(object(retired, id as u64, id as u128, id as u32, Object, Root)).unwrap();
    }
    let _ = world.observe_object(object(active, 1, 1000, 1, Object, Root)).unwrap();

    let _ = world.retire_region(retired).unwrap();
    let barrier = world.read_barrier();
    let first = world.object_list_page(None, 1).unwrap();

    assert_eq!(barrier.object_count_upper_bound, 1);
    assert!(first.objects.is_empty());
    assert_eq!(first.work_units, MAX_OBJECT_PAGE_WORK_ITEMS);
    let cursor = first.next.expect("hidden objects consume one bounded page");
    let second = world.object_list_page(Some(cursor), 1).unwrap();
    assert!(second.work_units <= MAX_OBJECT_PAGE_WORK_ITEMS);
    assert_eq!(second.objects.iter().map(|item| item.id).collect::<Vec<_>>(), [uuid(1000)]);
    assert_eq!(second.next, None);
}

#[test]
fn object_pages_resume_deep_visibility_walks() {
    let depth = MAX_OBJECT_PAGE_WORK_ITEMS + 44;
    let mut limits = world_limits();
    limits.max_entities = depth + 1;
    limits.max_entities_per_region = depth + 1;
    let mut world = World::new(limits);
    let region = world.register_region().unwrap();
    let root_id = depth as u128 + 1;
    let _ = world.observe_object(object(region, 1, root_id, depth as u32 + 1, Object, Root)).unwrap();
    for local in (1..=depth).rev() {
        let _ = world
            .observe_object(object(
                region,
                depth as u64 + 2 - local as u64,
                local as u128,
                local as u32,
                Object,
                Local(local_id(local as u32 + 1)),
            ))
            .unwrap();
    }
    let _ = world.remove_object(source(region, depth as u64 + 3), local_id(depth as u32 + 1)).unwrap();

    let first = world.object_list_page(None, 1).unwrap();
    assert!(first.objects.is_empty());
    let mut cursor = first.next.expect("a deep visibility check resumes from its ancestor cursor");
    let mut pages = 1;
    assert!(first.work_units <= MAX_OBJECT_PAGE_WORK_ITEMS);
    for _ in 0..2_000 {
        let page = world.object_list_page(Some(cursor), 1).unwrap();
        assert!(page.work_units <= MAX_OBJECT_PAGE_WORK_ITEMS);
        assert!(page.objects.is_empty());
        pages += 1;
        let Some(next) = page.next else {
            assert!(pages > 1);
            return;
        };
        cursor = next;
    }
    panic!("visibility cursors must finish within the bounded page sequence");
}

#[test]
fn detail_pages_resume_deep_visibility_walks() {
    let depth = MAX_OBJECT_DETAIL_WORK_ITEMS + 12;
    let mut limits = world_limits();
    limits.max_entities = depth + 1;
    limits.max_entities_per_region = depth + 1;
    let mut world = World::new(limits);
    let region = world.register_region().unwrap();
    let root_id = depth as u128 + 1;
    let _ = world.observe_object(object(region, 1, root_id, depth as u32 + 1, Object, Root)).unwrap();
    let mut target = None;
    for local in (1..=depth).rev() {
        let introduced = world
            .observe_object(object(
                region,
                depth as u64 + 2 - local as u64,
                local as u128,
                local as u32,
                Object,
                Local(local_id(local as u32 + 1)),
            ))
            .unwrap();
        if local == 1 {
            target = match introduced.outcome {
                Introduced(entity) => Some(entity),
                _ => panic!("expected target introduction"),
            };
        }
    }
    let target = target.expect("deepest object was introduced");
    let _ = world.remove_object(source(region, depth as u64 + 3), local_id(depth as u32 + 1)).unwrap();

    let first = world.object_detail_batch_page(&[target], None).unwrap();
    assert!(first.work_units <= MAX_OBJECT_DETAIL_WORK_ITEMS);
    assert!(first.batch.details.is_empty());
    let mut cursor = first.next.expect("detail visibility walk continues at the work limit");
    for _ in 0..8 {
        let page = world.object_detail_batch_page(&[target], Some(cursor)).unwrap();
        assert!(page.work_units <= MAX_OBJECT_DETAIL_WORK_ITEMS);
        if let Some(next) = page.next {
            assert!(page.batch.details.is_empty());
            cursor = next;
        } else {
            assert_eq!(page.batch.details, [ObjectDetailResult::Gone(target)]);
            return;
        }
    }
    panic!("detail visibility cursor must finish within the bounded page sequence");
}

#[test]
fn detail_batch_cursor_advances_across_entities() {
    let depth = 180;
    let mut limits = world_limits();
    limits.max_entities = depth * 2;
    limits.max_entities_per_region = depth * 2;
    let mut world = World::new(limits);
    let region = world.register_region().unwrap();
    let mut roots = Vec::new();
    let mut targets = Vec::new();
    for (id_base, local_base, order_base) in [(1_u128, 1_u32, 1_u64), (1_000_u128, 181_u32, 1_000_u64)] {
        let root_local = local_base + depth as u32 - 1;
        let _ = world.observe_object(object(region, order_base, id_base + depth as u128 - 1, root_local, Object, Root)).unwrap();
        roots.push(local_id(root_local));
        for local in (local_base..root_local).rev() {
            let observation = object(
                region,
                order_base + u64::from(root_local - local),
                id_base + u128::from(local - local_base),
                local,
                Object,
                Local(local_id(local + 1)),
            );
            let introduced = world.observe_object(observation).unwrap();
            if local == local_base {
                targets.push(entity(introduced));
            }
        }
    }
    for (index, root) in roots.into_iter().enumerate() {
        let _ = world.remove_object(source(region, 2_000 + index as u64), root).unwrap();
    }

    let first = world.object_detail_batch_page(&targets, None).unwrap();
    assert_eq!(first.work_units, MAX_OBJECT_DETAIL_WORK_ITEMS);
    assert_eq!(first.batch.details, [ObjectDetailResult::Gone(targets[0])]);
    let cursor = first.next.expect("second entity visibility walk continues");
    assert_eq!(cursor.entity(), targets[1]);
    let second = world.object_detail_batch_page(&targets[1..], Some(cursor)).unwrap();
    assert_eq!(second.batch.details, [ObjectDetailResult::Gone(targets[1])]);
    assert!(second.next.is_none());
    assert!(second.work_units <= MAX_OBJECT_DETAIL_WORK_ITEMS);
}

#[test]
fn detail_cursor_restarts_on_revision() {
    let depth = MAX_OBJECT_DETAIL_WORK_ITEMS as u32 + 1;
    let (mut world, region) = deep_world(depth);
    let target = world.entity(uuid(u128::from(depth))).unwrap();
    apply_ok(world.remove_object(source(region, u64::from(depth) + 1), local_id(1)));

    let first = world.object_detail_batch_page(&[target], None).unwrap();
    assert!(first.batch.details.is_empty());
    let cursor = first.next.expect("deep detail read needs another page");
    apply_ok(world.observe_object(object(region, u64::from(depth) + 2, 100_000, depth + 1, Object, Root)));

    let second = world.object_detail_batch_page(&[target], Some(cursor)).unwrap();
    assert!(second.batch.details.is_empty(), "a revision change restarts the walk at the unfinished target");
    let third = world.object_detail_batch_page(&[target], second.next).unwrap();
    assert_eq!(third.batch.details, [ObjectDetailResult::Gone(target)]);
}

#[test]
fn detail_cursor_rejects_entity_reuse() {
    let depth = MAX_OBJECT_DETAIL_WORK_ITEMS as u32 + 1;
    let (mut world, region) = deep_world(depth);
    let target_id = uuid(u128::from(depth));
    let target = world.entity(target_id).unwrap();
    apply_ok(world.remove_object(source(region, u64::from(depth) + 1), local_id(1)));
    let first = world.object_detail_batch_page(&[target], None).unwrap();
    let cursor = first.next.expect("deep detail read needs another page");

    while world.has_pending_removals() {
        let progress = world.reclaim_removals(world::MAX_REMOVAL_WORK_PER_TURN);
        assert!(progress.work_units <= world::MAX_REMOVAL_WORK_PER_TURN);
    }
    let replacement = entity(world.observe_object(object(region, u64::from(depth) + 2, u128::from(depth), depth, Object, Root)).unwrap());
    assert_ne!(replacement, target);
    assert_eq!(world.object_detail_batch_page(&[replacement], Some(cursor)), Err(WorldReadError::InvalidCursor));
}

#[test]
fn list_pages_return_identity() {
    let mut world = World::new(world_limits());
    let region = world.register_region().unwrap();
    let mut child = object(region, 1, 190, 1, Object, Local(local_id(9)));
    child.pose = Some(Pose { position: Some(Vec3::X), rotation: None });
    let entity = entity(world.observe_object(child).unwrap());

    let page = world.object_list_page(None, 1).unwrap();

    assert_eq!(page.hydration_revision, world.revision());
    assert_eq!(page.objects, [ObjectListItem { entity, id: uuid(190), kind: Object, address: LocalAddress::new(region, local_id(1)) }]);
    assert_eq!(world.object(entity).unwrap().world_pose(), Ok(PoseState::Unresolved(UnresolvedReason::PendingParent)));
}

#[test]
fn detail_rejects_a_reused_entity() {
    let mut world = World::new(world_limits());
    let region = world.register_region().unwrap();
    let original = entity(world.observe_object(object(region, 1, 60, 1, Object, Root)).unwrap());
    let barrier = world.read_barrier();

    assert_eq!(world.remove_object(source(region, 2), local_id(1)).unwrap().outcome, Removed(uuid(60)));
    let replacement = entity(world.observe_object(object(region, 3, 60, 2, Object, Root)).unwrap());

    assert_ne!(original, replacement);
    assert_eq!(world.object_detail(original), Err(WorldReadError::StaleIdentity));
    assert_eq!(world.object_media(original), Err(WorldReadError::StaleIdentity));
    assert_eq!(world.object_media(replacement).unwrap().media, None);
    assert_eq!(world.object_detail(replacement).unwrap().summary.id, uuid(60));
    let changes = world.change_page(barrier.changes, 8).unwrap();
    assert!(changes.records.iter().any(|record| {
        matches!(record.change, WorldChange::SubtreeRemovalPending { id, entity, .. } if id == uuid(60) && entity == original)
    }));
    assert!(
        changes.records.iter().any(|record| {
            matches!(record.change, WorldChange::EntityAdded { id, entity, .. } if id == uuid(60) && entity == replacement)
        })
    );
    assert!(!changes.records.iter().any(|record| matches!(record.change, WorldChange::EntityRemoved { entity, .. } if entity == original)));
    while world.has_pending_removals() {
        let progress = world.reclaim_removals(world::MAX_REMOVAL_WORK_PER_TURN);
        assert!(progress.work_units <= world::MAX_REMOVAL_WORK_PER_TURN);
    }
    let removed = world.change_page(changes.cursor, 8).unwrap();
    assert!(
        removed.records.iter().any(|record| {
            matches!(record.change, WorldChange::EntityRemoved { id, entity, .. } if id == uuid(60) && entity == original)
        })
    );
}

#[test]
fn summary_tracks_parent_pose() {
    let mut world = World::new(world_limits());
    let region = world.register_region().unwrap();
    let parent = entity(world.observe_object(object(region, 1, 170, 1, Object, Root)).unwrap());
    let mut child = object(region, 2, 171, 2, Object, Local(local_id(1)));
    child.pose = Some(Pose { position: Some(Vec3::new(2.0, 3.0, 4.0)), rotation: None });
    let child = entity(world.observe_object(child).unwrap());

    let summary = world.object_detail(child).unwrap().summary;

    assert_eq!(summary.parent, Parent::Resolved(parent));
    assert_eq!(summary.local_transform.position(), Some(Vec3::new(2.0, 3.0, 4.0)));
    assert_eq!(summary.pose, Ok(PoseState::Unresolved(UnresolvedReason::MissingLocalPose)));

    apply_ok(world.patch_object(ObjectPatch {
        pose: Some(Pose { position: Some(Vec3::X), rotation: Some(Quat::IDENTITY) }),
        ..ObjectPatch::new(source(region, 3), local_id(1))
    }));
    apply_ok(world.patch_object(ObjectPatch {
        pose: Some(Pose { position: None, rotation: Some(Quat::IDENTITY) }),
        ..ObjectPatch::new(source(region, 4), local_id(2))
    }));
    assert_eq!(world.object(child).unwrap().world_pose(), Ok(PoseState::Unresolved(UnresolvedReason::MissingRegionOrigin)));
    let barrier = world.read_barrier();
    apply_ok(world.patch_region(region, RegionInfo { origin: Some(DVec3::new(256.0, 512.0, 0.0)), ..RegionInfo::default() }));
    let page = world.change_page(barrier.changes, 1).unwrap();
    assert!(matches!(page.records[0].change, WorldChange::RegionUpdated(updated) if updated == region));
    assert_eq!(page.completed_revision, world.revision());
    let expected = WorldPose { position: DVec3::new(259.0, 515.0, 4.0), rotation: Quat::IDENTITY };
    assert_eq!(world.object(child).unwrap().world_pose(), Ok(PoseState::Resolved(expected)));

    let barrier = world.read_barrier();
    apply_ok(world.patch_object(ObjectPatch {
        pose: Some(Pose { position: Some(Vec3::X * 2.0), rotation: None }),
        ..ObjectPatch::new(source(region, 5), local_id(1))
    }));
    let page = world.change_page(barrier.changes, 8).unwrap();
    assert!(page.records.iter().any(|record| matches!(record.change, WorldChange::SubtreeInvalidated { entity, .. } if entity == parent)));
    let child = world.object_detail(child).unwrap();
    assert_eq!(child.hydration_revision, page.completed_revision);
    assert_eq!(child.summary.pose, Ok(PoseState::Resolved(WorldPose { position: DVec3::new(260.0, 515.0, 4.0), ..expected })));
}

#[test]
fn detail_batches_preserve_order() {
    let mut world = World::new(world_limits());
    let region = world.register_region().unwrap();
    let original = entity(world.observe_object(object(region, 1, 180, 1, Object, Root)).unwrap());
    let other = entity(world.observe_object(object(region, 2, 181, 2, Object, Root)).unwrap());
    assert_eq!(world.remove_object(source(region, 3), local_id(1)).unwrap().outcome, Removed(uuid(180)));
    let replacement = entity(world.observe_object(object(region, 4, 180, 3, Object, Root)).unwrap());

    let batch = world.object_detail_batch(&[other, original, replacement]).unwrap();

    assert_eq!(batch.hydration_revision, world.revision());
    assert!(matches!(
        batch.details.as_slice(),
        [
            ObjectDetailResult::Found(first),
            ObjectDetailResult::Gone(gone),
            ObjectDetailResult::Found(last)
        ] if first.summary.entity == other && first.hydration_revision == batch.hydration_revision
            && *gone == original && last.summary.entity == replacement && last.hydration_revision == batch.hydration_revision
    ));
}

#[test]
fn detail_pose_matches_transform() {
    let mut world = World::new(world_limits());
    let region = world.register_region().unwrap();
    assert!(matches!(
        world.patch_region(region, RegionInfo { origin: Some(DVec3::new(256.0, 512.0, 0.0)), ..RegionInfo::default() }).unwrap().outcome,
        world::RegionOutcome::Patched { .. }
    ));
    let pose = Pose { position: Some(Vec3::new(3.0, 4.0, 5.0)), rotation: Some(Quat::IDENTITY) };
    let mut object = object(region, 1, 190, 1, Object, Root);
    object.pose = Some(pose);
    let entity = entity(world.observe_object(object).unwrap());
    let expected = WorldPose { position: DVec3::new(259.0, 516.0, 5.0), rotation: Quat::IDENTITY };

    let detail = world.object_detail(entity).unwrap();

    assert_eq!(detail.world_transform, Ok(WorldTransformState::MissingScale(expected)));
    assert_eq!(detail.summary.pose, Ok(PoseState::Resolved(expected)));
}

#[test]
fn media_reads_leave_details_bounded() {
    let mut world = World::new(world_limits());
    let region = world.register_region().unwrap();
    let mut observation = object(region, 1, 220, 1, Object, Root);
    observation.appearance = FieldUpdate::Set(Appearance::Surface);
    let entity = entity(world.observe_object(observation).unwrap());
    let media = MediaEntry { current_url: "https://media.test/item".to_owned(), ..MediaEntry::default() };
    let update = ObjectMediaUpdate::new(1, vec![Some(media)]).unwrap();
    assert!(
        world
            .patch_object(ObjectPatch { object_media: Some(update), ..ObjectPatch::new(source(region, 2), local_id(1)) })
            .unwrap()
            .revision
            .is_some()
    );

    let detail = world.object_detail(entity).unwrap();
    let media = world.object_media(entity).unwrap();
    assert_eq!(media.entity, entity);
    assert_eq!(media.hydration_revision, detail.hydration_revision);
    assert_eq!(media.media.as_ref().unwrap().face(0).unwrap().current_url, "https://media.test/item");

    let large_entry = MediaEntry {
        current_url: "x".repeat(MAX_MEDIA_URL_BYTES),
        home_url: "y".repeat(MAX_MEDIA_URL_BYTES),
        whitelist: vec!["z".repeat(MAX_MEDIA_WHITELIST_BYTES / world::MAX_MEDIA_WHITELIST_ENTRIES); world::MAX_MEDIA_WHITELIST_ENTRIES],
        ..MediaEntry::default()
    };
    let large = ObjectMediaUpdate::new(2, vec![Some(large_entry); world::MAX_SURFACE_FACES]).unwrap();
    let barrier = world.read_barrier();
    assert!(
        world
            .patch_object(ObjectPatch { object_media: Some(large), ..ObjectPatch::new(source(region, 3), local_id(1)) })
            .unwrap()
            .revision
            .is_some()
    );

    let changes = world.change_page(barrier.changes, 1).unwrap();
    assert!(matches!(changes.records[0].change, WorldChange::EntityUpdated { entity: changed, classes, .. }
        if changed == entity && classes == WorldChangeClasses::MEDIA));
    let current = world.object_detail(entity).unwrap();
    assert_eq!(current.summary, detail.summary);
    assert_eq!(current.appearance, detail.appearance);
    assert!(current.hydration_revision > detail.hydration_revision);
    assert!(
        matches!(world.object_detail_batch(&[entity]).unwrap().details.as_slice(), [ObjectDetailResult::Found(value)] if value == &current)
    );
    let large = world.object_media(entity).unwrap();
    assert_eq!(large.hydration_revision, current.hydration_revision);
    let large = large.media.unwrap();
    assert_eq!(large.version(), 2);
    assert_eq!(large.faces().count(), world::MAX_SURFACE_FACES);
    assert_eq!(large.face(44).unwrap().current_url.len(), MAX_MEDIA_URL_BYTES);
    assert_eq!(media.media.unwrap().version(), 1, "owned reads retain the earlier snapshot");
    let cleared = ObjectMediaUpdate::new(3, vec![None; world::MAX_SURFACE_FACES]).unwrap();
    apply_ok(world.patch_object(ObjectPatch { object_media: Some(cleared), ..ObjectPatch::new(source(region, 4), local_id(1)) }));
    let cleared = world.object_media(entity).unwrap().media.expect("an observed empty snapshot retains its version");
    assert_eq!(cleared.version(), 3);
    assert_eq!(cleared.faces().count(), 0);
    assert_eq!(large.faces().count(), world::MAX_SURFACE_FACES);
}

#[test]
fn change_pages_resume_mid_revision() {
    let mut world = World::new(world_limits());
    let region = world.register_region().unwrap();
    entity(world.observe_object(object(region, 1, 70, 1, Object, Root)).unwrap());
    entity(world.observe_object(object(region, 2, 71, 2, Object, Root)).unwrap());
    let barrier = world.read_barrier();
    let retired = world.retire_region(region).unwrap();
    let revision = match retired.outcome {
        world::RegionOutcome::Retired { revision, .. } => revision,
        outcome => panic!("expected region retirement, got {outcome:?}"),
    };

    let retired_page = world.change_page(barrier.changes, 1).unwrap();
    assert_eq!(retired_page.observed_revision, revision);
    assert_eq!(retired_page.completed_revision, revision);
    assert!(!retired_page.has_more);
    assert!(matches!(retired_page.records.as_slice(), [record]
        if record.revision == revision && matches!(record.change, WorldChange::RegionRetired { region: retired, .. } if retired == region)));

    while world.has_pending_removals() {
        let progress = world.reclaim_removals(world::MAX_REMOVAL_WORK_PER_TURN);
        assert!(progress.work_units <= world::MAX_REMOVAL_WORK_PER_TURN);
    }
    let first_removal = world.change_page(retired_page.cursor, 1).unwrap();
    let second_removal = world.change_page(first_removal.cursor, 1).unwrap();
    assert!(first_removal.has_more);
    assert!(!second_removal.has_more);
    assert!(first_removal.records.iter().chain(&second_removal.records).all(|record| {
        matches!(record.change, WorldChange::EntityRemoved { reason: world::WorldEntityRemovalReason::RegionRetired, .. })
            && record.revision > revision
    }));
    assert_eq!(first_removal.records.len() + second_removal.records.len(), 2);
    assert!(second_removal.completed_revision > first_removal.completed_revision);
}

#[test]
fn journal_eviction_requires_resync() {
    let mut limits = world_limits();
    limits.max_journal_entries = 1;
    let mut world = World::new(limits);
    let before_registration = world.read_barrier();
    let region = world.register_region().unwrap();
    let after_registration = world.read_barrier();
    assert!(matches!(
        world.patch_region(region, RegionInfo { name: Some("region".to_owned()), ..RegionInfo::default() }).unwrap().outcome,
        world::RegionOutcome::Patched { .. }
    ));

    assert_eq!(world.change_page(before_registration.changes, 8), Err(WorldReadError::ResyncRequired));
    let changes = world.change_page(after_registration.changes, 8).unwrap();
    assert!(matches!(changes.records.as_slice(), [record] if record.change == WorldChange::RegionUpdated(region)));
}

#[test]
fn foreign_handles_or_zero_limits_fail() {
    let mut first = World::new(world_limits());
    let mut second = World::new(world_limits());
    let first_region = first.register_region().unwrap();
    let _second_region = second.register_region().unwrap();
    let selected = entity(first.observe_object(object(first_region, 1, 90, 1, Object, Root)).unwrap());
    entity(first.observe_object(object(first_region, 2, 91, 2, Object, Root)).unwrap());
    let list_cursor = first.object_list_page(None, 1).unwrap().next.expect("second list item remains");
    let barrier = first.read_barrier();

    assert_eq!(second.object_list_page(Some(list_cursor), 1), Err(WorldReadError::StaleSession));
    assert_eq!(second.object_detail(selected), Err(WorldReadError::StaleSession));
    assert_eq!(second.object_media(selected), Err(WorldReadError::StaleSession));
    assert_eq!(second.object_detail_batch(&[selected]), Err(WorldReadError::StaleSession));
    assert_eq!(first.object_detail_batch(&[]), Err(WorldReadError::InvalidLimit));
    assert_eq!(first.object_detail_batch(&[selected; MAX_OBJECT_DETAIL_BATCH_ITEMS + 1]), Err(WorldReadError::InvalidLimit));
    assert_eq!(second.change_page(barrier.changes, 1), Err(WorldReadError::StaleSession));
    assert_eq!(first.object_list_page(None, 0), Err(WorldReadError::InvalidLimit));
    assert_eq!(first.change_page(barrier.changes, 0), Err(WorldReadError::InvalidLimit));
}

#[test]
fn region_pages_follow_active_regions() {
    let mut world = World::new(world_limits());
    let first = world.register_region().unwrap();
    let second = world.register_region().unwrap();
    assert_eq!(world.read_barrier().region_count, 2);

    let page = world.region_page(None, 1).unwrap();
    assert_eq!(page.regions.iter().map(|summary| summary.region).collect::<Vec<_>>(), [first]);
    let cursor = page.next.expect("second region remains");
    assert!(matches!(world.retire_region(first).unwrap().outcome, world::RegionOutcome::Retired { entities_removed: 0, .. }));

    let page = world.region_page(Some(cursor), 1).unwrap();
    assert_eq!(page.regions.iter().map(|summary| summary.region).collect::<Vec<_>>(), [second]);
    assert_eq!(page.next, None);
}

#[test]
fn large_region_name_is_rejected() {
    let mut limits = world_limits();
    limits.max_region_name_bytes = MAX_REGION_PAGE_BYTES + 1;
    let mut world = World::new(limits);
    let region = world.register_region().unwrap();
    let name = "r".repeat(MAX_REGION_PAGE_BYTES);
    assert!(matches!(
        world.patch_region(region, RegionInfo { name: Some(name), ..RegionInfo::default() }).unwrap().outcome,
        world::RegionOutcome::Patched { .. }
    ));

    assert_eq!(world.region_page(None, 1), Err(WorldReadError::TooLarge));
}

#[test]
fn oversized_revision_requires_resync() {
    let mut limits = world_limits();
    limits.max_journal_changes = 1;
    let mut world = World::new(limits);
    let region = world.register_region().unwrap();
    entity(world.observe_object(object(region, 1, 130, 1, Object, Root)).unwrap());
    let mut child = object(region, 2, 131, 2, Object, Local(local_id(1)));
    child.pose = Some(Pose { position: Some(Vec3::ZERO), rotation: None });
    entity(world.observe_object(child).unwrap());
    let barrier = world.read_barrier();
    let mut movement = ObjectPatch::new(source(region, 3), local_id(1));
    movement.pose = Some(Pose { position: Some(Vec3::X), rotation: None });
    assert!(world.patch_object(movement).unwrap().revision.is_some());

    assert_eq!(world.change_page(barrier.changes, 8), Err(WorldReadError::ResyncRequired));
}

#[test]
fn evicted_partial_revision_requires_resync() {
    let mut limits = world_limits();
    limits.max_journal_entries = 1;
    let mut world = World::new(limits);
    let first = world.register_region().unwrap();
    let second = world.register_region().unwrap();
    entity(world.observe_object(object(first, 1, 140, 1, Object, Root)).unwrap());
    entity(world.observe_object(object(first, 2, 141, 2, Object, Root)).unwrap());
    let barrier = world.read_barrier();
    assert!(matches!(world.retire_region(first).unwrap().outcome, world::RegionOutcome::Retired { entities_removed: 2, .. }));
    let partial = world.change_page(barrier.changes, 1).unwrap();
    assert!(!partial.has_more);
    assert!(matches!(
        world.patch_region(second, RegionInfo { name: Some("second".to_owned()), ..RegionInfo::default() }).unwrap().outcome,
        world::RegionOutcome::Patched { .. }
    ));

    while world.has_pending_removals() {
        let progress = world.reclaim_removals(world::MAX_REMOVAL_WORK_PER_TURN);
        assert!(progress.work_units <= world::MAX_REMOVAL_WORK_PER_TURN);
    }
    assert_eq!(world.change_page(barrier.changes, 1), Err(WorldReadError::ResyncRequired));
}
