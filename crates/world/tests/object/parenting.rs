use super::support::{
    AmbiguousOrder, Avatar, Deferred, Degraded, FieldGroup, FieldRejection, FieldRejectionReason, HashSet, IgnoredStale, Introduced,
    InvalidValue, Local, Object, ObjectPatch, Parent, ParentCycle, ParentDependsOnRejectedParent, Pending, Pose, Quat, Region, Removed,
    Resolved, Root, SitTarget, Unchanged, UnknownObject, UntrustedSource, UpdateSource, Updated, Vec3, World, address, drain_removals,
    limits, local_id, object, source, untracked, uuid, with_region,
};

fn drain_relationships(world: &mut World) {
    while world.has_pending_relationships() {
        let _ = world.resolve_relationships(64).unwrap();
    }
}

fn child_first_world() -> (World, Region, world::Entity) {
    let mut bounds = limits();
    bounds.max_entities = 256;
    bounds.max_entities_per_region = 256;
    let mut world = World::new(bounds);
    let region = world.register_region().unwrap();
    for child in 1..=200 {
        let _ =
            world.observe_object(object(source(region, child), u128::from(child), child as u32 + 1, Object, Local(local_id(1)))).unwrap();
    }
    let Introduced(parent) = world.observe_object(object(source(region, 201), 1000, 1, Object, Root)).unwrap().outcome else {
        panic!("parent must be introduced");
    };
    (world, region, parent)
}

fn deep_chain_world() -> (World, Region, world::Entity) {
    let mut bounds = limits();
    bounds.max_entities = 320;
    bounds.max_entities_per_region = 320;
    let mut world = World::new(bounds);
    let region = world.register_region().unwrap();
    for link in 1..=300 {
        let parent = if link == 1 { Root } else { Local(local_id(link - 1)) };
        let _ = world.observe_object(object(source(region, u64::from(link)), u128::from(link), link, Object, parent)).unwrap();
    }
    for child in 401..=404 {
        let _ = world
            .observe_object(object(source(region, u64::from(child) + 2000), u128::from(child), child, Object, Local(local_id(301))))
            .unwrap();
    }
    let Introduced(parent) = world.observe_object(object(source(region, 1000), 1000, 301, Object, Local(local_id(300)))).unwrap().outcome
    else {
        panic!("deep parent must be introduced");
    };
    let _ = world.resolve_relationships(64).unwrap();
    assert_eq!(world.children(parent).count(), 0, "a deep chain needs more than one turn");
    assert!(world.has_pending_relationships());
    (world, region, parent)
}

#[test]
fn late_pending_child_gets_recovery() {
    let mut bounds = limits();
    bounds.max_entities = 256;
    bounds.max_entities_per_region = 256;
    bounds.max_recovery_targets = 256;
    let mut world = World::new(bounds);
    let region = world.register_region().unwrap();
    let _ = world.observe_object(object(source(region, 1), 1000, 1, Object, Root)).unwrap();
    let _ = world.remove_object(source(region, 2), local_id(1)).unwrap();
    for child in 2..=201 {
        let _ = world
            .observe_object(object(source(region, u64::from(child) + 10), u128::from(child) + 100, child, Object, Local(local_id(1))))
            .unwrap();
    }
    let _ = world.observe_object(object(source(region, 300), 1000, 1, Object, Root)).unwrap();
    let _ = world.resolve_relationships(7).unwrap();
    let late = world.observe_object(object(source(region, 3), 1, 250, Object, Local(local_id(1)))).unwrap();
    let Introduced(late_child) = late.outcome else { panic!("late child must be introduced") };
    let late_address = address(region, 250);
    let mut recovery = world.observe_object(object(source(region, 400), 1000, 1, Object, Root)).unwrap().recovery.requests;
    for _ in 0..200 {
        if !world.has_pending_relationships() {
            break;
        }
        recovery.extend(world.resolve_relationships(7).unwrap().recovery.requests);
    }
    assert!(!world.has_pending_relationships());
    assert!(recovery.contains(&late_address));
    assert_eq!(world.object(late_child).unwrap().parent(), Pending { address: address(region, 1), expected_uuid: None });
}

#[test]
fn recovery_limits_preserve_queued_targets() {
    for recovery_limit in [0, 1, 2, 64] {
        let mut bounds = limits();
        bounds.max_entities = 131;
        bounds.max_entities_per_region = 131;
        bounds.max_recovery_targets = recovery_limit;
        let mut world = World::new(bounds);
        let region = world.register_region().unwrap();
        let _ = world.observe_object(object(source(region, 1), 1000, 1, Object, Root)).unwrap();
        let _ = world.remove_object(source(region, 2), local_id(1)).unwrap();
        let mut expected = HashSet::new();
        for child in 2..=131 {
            let _ = world
                .observe_object(object(source(region, u64::from(child) + 10), u128::from(child), child, Object, Local(local_id(1))))
                .unwrap();
            expected.insert(address(region, child));
        }
        let first = world.observe_object(object(source(region, 200), 1000, 1, Object, Root)).unwrap();
        assert!(first.recovery.requests.len() <= recovery_limit);
        let mut recovery = first.recovery.requests.into_iter().collect::<HashSet<_>>();
        for _ in 0..130 {
            if !world.has_pending_relationships() {
                break;
            }
            let batch = world.resolve_relationships(64).unwrap();
            assert!(batch.recovery.requests.len() <= recovery_limit);
            for target in batch.recovery.requests {
                assert!(recovery.insert(target), "reported targets must not be replayed");
            }
        }
        assert!(!world.has_pending_relationships());
        if recovery_limit == 0 {
            assert!(recovery.is_empty());
        } else {
            assert_eq!(recovery, expected, "recovery capacity {recovery_limit}");
        }
    }
}

#[test]
fn parent_resolution_respects_batch_size() {
    let (mut world, _, parent) = child_first_world();
    let mut resolved = world.children(parent).count();
    assert!(resolved > 0 && resolved < 200);
    let revision = world.revision();
    assert_eq!(world.resolve_relationships(0).unwrap().revision, None);
    assert_eq!(world.revision(), revision);
    while world.has_pending_relationships() {
        let applied = world.resolve_relationships(7).unwrap();
        let next = world.children(parent).count();
        assert!(next > resolved && next - resolved <= 7);
        assert_eq!(applied.recovery_cancellations.len(), next - resolved);
        assert!(applied.recovery.requests.is_empty());
        assert_eq!(applied.revision, Some(world.revision()));
        resolved = next;
    }
    assert_eq!(resolved, 200);
    assert_eq!(world.resolve_relationships(7).unwrap().revision, None);
}

#[test]
fn queued_parent_jobs_rotate_fairly() {
    let mut bounds = limits();
    bounds.max_entities = 270;
    bounds.max_entities_per_region = 270;
    let mut world = World::new(bounds);
    let region = world.register_region().unwrap();
    for child in 0..130 {
        let _ = world
            .observe_object(object(source(region, u64::from(child) + 1), u128::from(child + 3), child + 3, Object, Local(local_id(1))))
            .unwrap();
        let _ = world
            .observe_object(object(
                source(region, u64::from(child) + 300),
                u128::from(child + 140),
                child + 140,
                Object,
                Local(local_id(2)),
            ))
            .unwrap();
    }
    let Introduced(first) = world.observe_object(object(source(region, 1000), 1000, 1, Object, Root)).unwrap().outcome else {
        panic!("first parent must be introduced");
    };
    let Introduced(second) = world.observe_object(object(source(region, 1001), 1001, 2, Object, Root)).unwrap().outcome else {
        panic!("second parent must be introduced");
    };
    let first_count = world.children(first).count();
    let second_count = world.children(second).count();
    assert_eq!(first_count, 64);
    assert_eq!(second_count, 64);

    let _ = world.resolve_relationships(64).unwrap();
    assert_eq!(world.children(first).count(), first_count + 64);
    assert_eq!(world.children(second).count(), second_count);
    let _ = world.resolve_relationships(64).unwrap();
    assert_eq!(world.children(first).count(), first_count + 64);
    assert_eq!(world.children(second).count(), second_count + 64);
}

#[test]
fn queued_resolution_advances_after_updates() {
    let mut bounds = limits();
    bounds.max_entities = 220;
    bounds.max_entities_per_region = 220;
    let mut world = World::new(bounds);
    let region = world.register_region().unwrap();
    for child in 1..=204 {
        let order = match child {
            1..=64 => u64::from(child),
            65..=194 => u64::from(child) + 36,
            _ => u64::from(child) - 130,
        };
        let _ = world.observe_object(object(source(region, order), u128::from(child), child + 1, Object, Local(local_id(1)))).unwrap();
    }
    let Introduced(parent) = world.observe_object(object(source(region, 100), 1000, 1, Object, Root)).unwrap().outcome else {
        panic!("parent must be introduced");
    };
    assert_eq!(world.children(parent).count(), 64);
    let moved = world.observe_object(object(source(region, 300), 1000, 250, Object, Root)).unwrap();
    assert_eq!(moved.outcome, Updated(parent));

    for order in 301..=504 {
        let _ = world.resolve_relationships(7).unwrap();
        let applied = world.observe_object(object(source(region, order), 1000, 1, Object, Root)).unwrap();
        assert!(matches!(applied.outcome, Updated(_) | Unchanged));
        if world.children(parent).count() == 74 {
            break;
        }
    }

    for child_id in 195_u32..=204 {
        let child = world.entity(uuid(u128::from(child_id))).expect("valid tail child remains live");
        assert_eq!(world.object(child).unwrap().parent(), Resolved(parent));
    }
    assert_eq!(world.children(parent).count(), 74);
}

#[test]
fn queued_cycle_stays_pending() {
    for rebind in [false, true] {
        let (mut world, region, parent) = child_first_world();
        let child = world.entity(uuid(200)).expect("last child remains pending");
        let parent_local = if rebind { 250 } else { 1 };
        let patch_order = if rebind {
            let moved = world.observe_object(object(source(region, 300), 1000, parent_local, Object, Root)).unwrap();
            assert_eq!(moved.outcome, Updated(parent));
            301
        } else {
            300
        };
        let mut patch = ObjectPatch::new(source(region, patch_order), local_id(parent_local));
        patch.parent = Some(Local(local_id(201)));
        assert_eq!(world.patch_object(patch).unwrap().outcome, Updated(parent));
        assert_eq!(world.object(parent).unwrap().parent(), Resolved(child));

        let child_address = address(region, 201);
        let mut cycle_recovery = None;
        while world.has_pending_relationships() {
            let applied = world.resolve_relationships(7).unwrap();
            if applied.recovery.requests.contains(&child_address) {
                cycle_recovery = Some(applied.recovery.requests);
            }
        }

        assert_eq!(cycle_recovery, Some(vec![child_address]));
        assert_eq!(world.object(parent).unwrap().parent(), Resolved(child));
        assert_eq!(world.object(child).unwrap().parent(), Pending { address: address(region, 1), expected_uuid: Some(uuid(1000)) });
    }
}

#[test]
fn pending_children_follow_parent_removal() {
    for retire in [false, true] {
        let (mut world, region, _) = child_first_world();
        assert!(world.has_pending_relationships());
        if retire {
            let _ = world.retire_region(region).unwrap();
        } else {
            let _ = world.remove_object(source(region, 300), local_id(1)).unwrap();
        }
        assert_eq!(world.object_count(), 0);
        let _ = drain_removals(&mut world);
        drain_relationships(&mut world);
        assert!(!world.has_pending_relationships());
        assert_eq!(world.resolve_relationships(7).unwrap().revision, None);
    }
}

#[test]
fn transfer_preserves_pending_children() {
    let (mut world, region, parent) = child_first_world();
    let destination = world.register_region().unwrap();
    let _ = world.observe_object(object(source(destination, 300), 1000, 1, Object, Root)).unwrap();
    assert_eq!(world.object(parent).unwrap().address(), address(destination, 1));
    assert!(world.has_pending_removals());
    assert!(world.has_pending_relationships());
    let _ = drain_removals(&mut world);
    drain_relationships(&mut world);
    assert!(!world.has_pending_removals());
    assert!(!world.has_pending_relationships());
    for child in 1..=200 {
        let entity = world.entity(uuid(child)).unwrap();
        assert_eq!(world.object(entity).unwrap().parent(), Pending { address: address(region, 1), expected_uuid: Some(uuid(1000)) });
    }
    assert_eq!(world.resolve_relationships(7).unwrap().revision, None);
}

#[test]
fn transfer_coalescing_keeps_old_region_recovery_addresses() {
    let (mut world, source_region, parent) = child_first_world();
    let first_destination = world.register_region().unwrap();
    let second_destination = world.register_region().unwrap();
    let _ = world.observe_object(object(source(first_destination, 300), 1000, 20, Object, Root)).unwrap();
    let _ = world.observe_object(object(source(second_destination, 301), 1000, 30, Object, Root)).unwrap();
    assert_eq!(world.object(parent).unwrap().address(), address(second_destination, 30));
    let _ = drain_removals(&mut world);
    drain_relationships(&mut world);
    for child in 1..=200 {
        let entity = world.entity(uuid(child)).unwrap();
        assert_eq!(world.object(entity).unwrap().parent(), Pending { address: address(source_region, 1), expected_uuid: Some(uuid(1000)) });
    }
}

#[test]
fn transfer_back_preserves_current_relationship_work() {
    let (mut world, source_region, parent) = child_first_world();
    let destination = world.register_region().unwrap();
    let _ = world.observe_object(object(source(destination, 300), 1000, 20, Object, Root)).unwrap();
    let _ = world.observe_object(object(source(source_region, 301), 1000, 1, Object, Root)).unwrap();
    let _ = drain_removals(&mut world);
    drain_relationships(&mut world);
    assert_eq!(world.object(parent).unwrap().address(), address(source_region, 1));
    for child in 1..=200 {
        let entity = world.entity(uuid(child)).unwrap();
        assert_eq!(world.object(entity).unwrap().parent(), Resolved(parent));
    }
}

#[test]
fn queued_resolution_respects_newer_updates() {
    let (mut world, region, parent) = child_first_world();
    let detached = world.entity(uuid(200)).unwrap();
    let _ = world.observe_object(object(source(region, 300), 200, 201, Object, Root)).unwrap();
    while world.has_pending_relationships() {
        let _ = world.resolve_relationships(7).unwrap();
    }
    assert_eq!(world.object(detached).unwrap().parent(), Parent::Root);
    assert_eq!(world.children(parent).count(), 199);

    let (mut world, region, parent) = child_first_world();
    let Introduced(replacement) = world.observe_object(object(source(region, 300), 1001, 1, Object, Root)).unwrap().outcome else {
        panic!("replacement must be introduced");
    };
    assert!(world.object(parent).is_none());
    assert_eq!(world.object_count(), 1);
    assert_eq!(world.entity_at(address(region, 1)), Some(replacement));
    drain_relationships(&mut world);
    let _ = drain_removals(&mut world);
    drain_relationships(&mut world);
    assert!(!world.has_pending_relationships());
    assert_eq!(world.resolve_relationships(7).unwrap().revision, None);
}

#[test]
fn rebinding_keeps_original_dependents() {
    for drain in [false, true] {
        let (mut world, region, parent) = child_first_world();
        let _ = world.observe_object(object(source(region, 300), 1000, 250, Object, Root)).unwrap();
        let Introduced(later_child) =
            world.observe_object(object(source(region, 301), 2000, 251, Object, Local(local_id(1)))).unwrap().outcome
        else {
            panic!("later child must be introduced");
        };
        if drain {
            while world.has_pending_relationships() {
                let _ = world.resolve_relationships(7).unwrap();
            }
            assert_eq!(world.children(parent).count(), 200);
        }
        let _ = world.remove_object(source(region, 400), local_id(250)).unwrap();
        assert_eq!(world.object_count(), 1);
        assert!(world.object(parent).is_none());
        assert_eq!(world.object(later_child).unwrap().parent(), Pending { address: address(region, 1), expected_uuid: None });
        let _ = drain_removals(&mut world);
        drain_relationships(&mut world);
        assert!(!world.has_pending_relationships());
    }
}

#[test]
fn vacated_address_keeps_distinct_work() {
    for return_before_drain in [false, true] {
        let (mut world, region, parent) = child_first_world();
        let _ = world.observe_object(object(source(region, 300), 1000, 250, Object, Root)).unwrap();
        let Introduced(other) = world.observe_object(object(source(region, 400), 1001, 1, Object, Root)).unwrap().outcome else {
            panic!("other parent must be introduced");
        };
        let Introduced(later_child) =
            world.observe_object(object(source(region, 401), 2000, 251, Object, Local(local_id(1)))).unwrap().outcome
        else {
            panic!("later child must be introduced");
        };
        if return_before_drain {
            let _ = world.observe_object(object(source(region, 500), 1000, 1, Object, Root)).unwrap();
            assert!(world.object(other).is_none());
            assert!(world.object(later_child).is_none());
        }
        while world.has_pending_relationships() {
            let _ = world.resolve_relationships(7).unwrap();
        }
        assert_eq!(world.children(parent).count(), 200);
        if !return_before_drain {
            assert_eq!(world.object(later_child).unwrap().parent(), Resolved(other));
            assert_eq!(world.children(other).count(), 1);
        }
    }
}

#[test]
fn deep_continuation_restarts_after_interruption() {
    for interruption in ["reparent", "cycle", "replace"] {
        let (mut world, region, parent) = deep_chain_world();
        let chain_end = world.entity(uuid(300)).unwrap();
        let cycle_child = world.entity(uuid(404)).unwrap();
        let mut expected_parent = parent;
        match interruption {
            "reparent" | "cycle" => {
                let mut patch = ObjectPatch::new(source(region, 1100), local_id(300));
                patch.parent = Some(if interruption == "cycle" { Local(local_id(404)) } else { Root });
                assert_eq!(world.patch_object(patch).unwrap().outcome, Updated(chain_end));
            }
            _ => {
                let _ = world.remove_object(source(region, 1500), local_id(301)).unwrap();
                let replacement = world.observe_object(object(source(region, 1600), 1001, 301, Object, Local(local_id(300)))).unwrap();
                let Introduced(replacement) = replacement.outcome else { panic!("replacement parent must be introduced") };
                expected_parent = replacement;
                let _ = drain_removals(&mut world);
            }
        }
        drain_relationships(&mut world);

        for child in 401..=404 {
            let entity = world.entity(uuid(child)).unwrap();
            let expected = if interruption == "cycle" && entity == cycle_child {
                Pending { address: address(region, 301), expected_uuid: Some(uuid(1000)) }
            } else {
                Resolved(expected_parent)
            };
            assert_eq!(world.object(entity).unwrap().parent(), expected, "{interruption} child {child}");
        }
        if interruption == "cycle" {
            assert_eq!(world.object(chain_end).unwrap().parent(), Resolved(cycle_child));
        }
        assert!(!world.has_pending_relationships());
    }
}

#[test]
fn queued_work_backpressures_atomically() {
    let mut bounds = limits();
    bounds.max_entities = 512;
    bounds.max_entities_per_region = 512;
    let mut world = World::new(bounds);
    let region = world.register_region().unwrap();
    for parent in 1..=3 {
        let _ = world.observe_object(object(source(region, u64::from(parent)), u128::from(parent), parent, Object, Root)).unwrap();
        let _ = world.remove_object(source(region, 10), local_id(parent)).unwrap();
    }
    for child in 0..195 {
        let _ = world
            .observe_object(object(
                source(region, u64::from(child) + 20),
                u128::from(child) + 100,
                child + 10,
                Object,
                Local(local_id(child / 65 + 1)),
            ))
            .unwrap();
    }
    let mut blocked = None;
    'parents: for parent in 0..200 {
        for (step, destination) in [1, 2, 3, parent + 300].into_iter().enumerate() {
            let observation =
                object(source(region, 1000 + u64::from(parent) * 4 + step as u64), 10_000 + u128::from(parent), destination, Object, Root);
            let revision = world.revision();
            let objects = world.object_count();
            let entity = world.entity(observation.id);
            let prior_address = entity.map(|entity| world.object(entity).unwrap().address());
            match world.observe_object(observation.clone()) {
                Ok(applied) if applied.outcome == Deferred(world::DeferredReason::RelationshipCapacity) => {
                    assert_eq!(applied.revision, None);
                    assert_eq!(applied.recovery.requests, [address(region, destination)]);
                    assert_eq!(world.revision(), revision);
                    assert_eq!(world.object_count(), objects);
                    assert_eq!(world.entity(observation.id), entity);
                    assert_eq!(entity.map(|entity| world.object(entity).unwrap().address()), prior_address);
                    blocked = Some(observation);
                    break 'parents;
                }
                Ok(_) => {}
                Err(error) => panic!("unexpected relationship admission error: {error:?}"),
            }
        }
    }
    let blocked = blocked.expect("reused addresses saturate pending work before the entity limit");
    while world.has_pending_relationships() {
        let _ = world.resolve_relationships(512).unwrap();
    }
    let applied = world.observe_object(blocked).expect("draining restores admission");
    assert_ne!(applied.outcome, Deferred(world::DeferredReason::RelationshipCapacity));
}

#[test]
fn self_parented_introduction_is_atomic() {
    let (mut world, region) = with_region();
    let revision = world.revision();
    let id = uuid(420);
    let addr = address(region, 1);

    let result = world.observe_object(object(source(region, 1), 420, 1, Object, Local(local_id(1))));

    assert_eq!(result, Err(InvalidValue { field: "parent_cycle" }));
    assert_eq!(world.revision(), revision);
    assert_eq!(world.entity(id), None);
    assert_eq!(world.entity_at(addr), None);
    assert_eq!(world.objects_in_region(region).count(), 0);
}

#[test]
fn sit_target_clear_respects_freshness() {
    let (mut world, region) = with_region();
    let entity = match world.observe_object(object(source(region, 1), 470, 1, Object, Root)).unwrap().outcome {
        Introduced(entity) => entity,
        other => panic!("expected object introduction, got {other:?}"),
    };
    let initial = SitTarget { position: Vec3::X, rotation: Quat::IDENTITY };
    assert_eq!(world.patch_sit_target(source(region, 10), uuid(470), Some(initial)).unwrap().outcome, Updated(entity));

    let stale = SitTarget { position: Vec3::Y, rotation: Quat::IDENTITY };
    assert_eq!(world.patch_sit_target(source(region, 9), uuid(470), Some(stale)).unwrap().outcome, Unchanged);
    assert_eq!(world.object(entity).unwrap().sit_target(), Some(initial));

    assert_eq!(world.patch_sit_target(source(region, 11), uuid(470), None).unwrap().outcome, Updated(entity));
    assert_eq!(world.object(entity).unwrap().sit_target(), None);
    let revision = world.revision();
    assert_eq!(world.patch_sit_target(source(region, 12), uuid(470), None).unwrap().outcome, Unchanged);
    assert_eq!(world.revision(), revision);
}

#[test]
fn sit_target_respects_tracked_authority() {
    let (mut world, region) = with_region();
    let entity = match world.observe_object(object(source(region, 1), 480, 1, Object, Root)).unwrap().outcome {
        Introduced(entity) => entity,
        other => panic!("expected object introduction, got {other:?}"),
    };
    let tracked = SitTarget { position: Vec3::X, rotation: Quat::IDENTITY };
    assert_eq!(world.patch_sit_target(source(region, 2), uuid(480), Some(tracked)).unwrap().outcome, Updated(entity));

    let untrusted_target = SitTarget { position: Vec3::Y, rotation: Quat::IDENTITY };
    let applied = world.patch_sit_target(untracked(region, 1_000), uuid(480), Some(untrusted_target)).unwrap();

    assert_eq!(applied.outcome, Unchanged);
    assert_eq!(
        applied.rejections.iter().copied().collect::<Vec<_>>(),
        [FieldRejection { group: FieldGroup::SitTarget, reason: UntrustedSource }]
    );
    assert_eq!(world.object(entity).unwrap().sit_target(), Some(tracked));
}

#[test]
fn sit_target_defers_unknown_and_unordered() {
    let (mut world, region) = with_region();
    let target = SitTarget { position: Vec3::X, rotation: Quat::IDENTITY };

    let unknown = world.patch_sit_target(source(region, 1), uuid(490), Some(target)).unwrap();
    assert_eq!(unknown.outcome, Deferred(UnknownObject));
    assert!(unknown.recovery.requests.is_empty());

    let entity = match world.observe_object(object(source(region, 2), 490, 1, Object, Root)).unwrap().outcome {
        Introduced(entity) => entity,
        other => panic!("expected object introduction, got {other:?}"),
    };
    let uncertain = UpdateSource::new(region, 3, Degraded, 0);
    let applied = world.patch_sit_target(uncertain, uuid(490), Some(target)).unwrap();

    assert_eq!(applied.outcome, Deferred(AmbiguousOrder));
    assert_eq!(applied.recovery.requests, [address(region, 1)]);
    assert_eq!(world.object(entity).unwrap().sit_target(), None);
}

#[test]
fn cyclic_parent_rejects_pose_but_preserves_siblings_and_blocks_older_declarations() {
    for use_observation in [false, true] {
        let (mut world, region) = with_region();
        let mut first = object(source(region, 1), 290, 1, Object, Root);
        first.pose = Some(Pose { position: Some(Vec3::X), rotation: None });
        let introduced = world.observe_object(first).unwrap();
        let Introduced(first_entity) = introduced.outcome else { panic!("expected first object") };
        let second = world.observe_object(object(source(region, 2), 291, 2, Object, Local(local_id(1)))).unwrap();
        let Introduced(second_entity) = second.outcome else { panic!("expected second object") };
        let third = world.observe_object(object(source(region, 3), 292, 3, Object, Root)).unwrap();
        let Introduced(third_entity) = third.outcome else { panic!("expected third object") };

        let revision = world.revision();
        let cyclic = ObjectPatch {
            parent: Some(Local(local_id(2))),
            pose: Some(Pose { position: Some(Vec3::Y), rotation: None }),
            scale: Some(Vec3::splat(2.0)),
            ..ObjectPatch::new(source(region, 50), local_id(1))
        };
        let applied = if use_observation {
            let mut observation = object(source(region, 50), 290, 1, Object, Local(local_id(2)));
            observation.pose = cyclic.pose;
            observation.scale = cyclic.scale;
            world.observe_object(observation).unwrap()
        } else {
            world.patch_object(cyclic).unwrap()
        };
        assert_eq!(applied.outcome, Updated(first_entity), "use_observation={use_observation}");
        assert_eq!(applied.revision.unwrap().get(), revision.get() + 1);
        assert_eq!(
            applied.rejections.iter().copied().collect::<Vec<_>>(),
            [
                FieldRejection { group: FieldGroup::Pose, reason: ParentDependsOnRejectedParent },
                FieldRejection { group: FieldGroup::Parent, reason: ParentCycle },
            ]
        );
        assert_eq!(world.object(first_entity).unwrap().parent(), Parent::Root);
        assert_eq!(world.object(first_entity).unwrap().position(), Some(Vec3::X));
        assert_eq!(world.object(first_entity).unwrap().scale(), Some(Vec3::splat(2.0)));
        assert_eq!(world.object(second_entity).unwrap().parent(), Resolved(first_entity));

        let delayed = ObjectPatch { parent: Some(Local(local_id(3))), ..ObjectPatch::new(source(region, 40), local_id(1)) };
        let applied = world.patch_object(delayed).unwrap();
        assert_eq!(applied.outcome, Unchanged);
        assert_eq!(world.object(first_entity).unwrap().parent(), Parent::Root);

        let fresh = ObjectPatch { parent: Some(Local(local_id(3))), ..ObjectPatch::new(source(region, 60), local_id(1)) };
        assert_eq!(world.patch_object(fresh).unwrap().outcome, Updated(first_entity));
        assert_eq!(world.object(first_entity).unwrap().parent(), Resolved(third_entity));
    }
}

#[test]
fn transfer_pends_old_region_children() {
    let mut world = World::new(limits());
    let source_region = world.register_region().unwrap();
    let destination = world.register_region().unwrap();
    let parent_address = address(source_region, 10);
    let child_address = address(source_region, 11);
    let parent = world.observe_object(object(source(source_region, 1), 330, 10, Object, Root)).unwrap();
    let Introduced(parent_entity) = parent.outcome else { panic!("expected parent introduction") };
    let child = world.observe_object(object(source(source_region, 2), 331, 11, Object, Local(local_id(10)))).unwrap();
    let Introduced(child_entity) = child.outcome else { panic!("expected child introduction") };

    let moved_parent = world.observe_object(object(source(destination, 1), 330, 20, Object, Root)).unwrap();
    assert_eq!(moved_parent.outcome, Updated(parent_entity));
    assert!(moved_parent.recovery.requests.is_empty());
    assert_eq!(world.object(parent_entity).unwrap().address(), address(destination, 20));
    assert_eq!(world.object(child_entity).unwrap().parent(), Pending { address: parent_address, expected_uuid: Some(uuid(330)) });
    let progress = drain_removals(&mut world);
    assert!(progress.iter().flat_map(|turn| &turn.recovery.requests).any(|target| *target == child_address));

    let replacement = world.observe_object(object(source(source_region, 10), 332, 10, Object, Root)).unwrap();
    assert!(matches!(replacement.outcome, Introduced(_)));
    assert_eq!(world.object(child_entity).unwrap().parent(), Pending { address: parent_address, expected_uuid: Some(uuid(330)) });
}

#[test]
fn transfer_retention_is_bounded_until_reclamation() {
    let mut world_limits = limits();
    world_limits.max_regions = 3;
    world_limits.max_entities = 2;
    world_limits.max_entities_per_region = 2;
    let mut world = World::new(world_limits);
    let first = world.register_region().unwrap();
    let second = world.register_region().unwrap();
    let third = world.register_region().unwrap();
    let Introduced(parent) = world.observe_object(object(source(first, 1), 330, 1, Object, Root)).unwrap().outcome else {
        panic!("expected first parent introduction")
    };
    let Introduced(other) = world.observe_object(object(source(first, 2), 331, 2, Object, Root)).unwrap().outcome else {
        panic!("expected second parent introduction")
    };
    let _ = world.observe_object(object(source(second, 1), 330, 1, Object, Root)).unwrap();
    let _ = world.observe_object(object(source(second, 2), 331, 2, Object, Root)).unwrap();

    assert_eq!(world.observe_object(object(source(third, 1), 330, 1, Object, Root)), Err(world::ApplyError::RemovalCapacityExhausted));
    assert_eq!(world.object(parent).unwrap().address(), address(second, 1));
    assert_eq!(world.object(other).unwrap().address(), address(second, 2));

    let _ = drain_removals(&mut world);
    assert_eq!(world.observe_object(object(source(third, 1), 330, 1, Object, Root)).unwrap().outcome, Updated(parent));
}

#[test]
fn rebind_preserves_an_unrelated_parent() {
    for parent_present in [false, true] {
        let (mut world, region) = with_region();
        let _ = world.observe_object(object(source(region, 1), 360, 1, Object, Root)).unwrap();
        if parent_present {
            let _ = world.observe_object(object(source(region, 2), 362, 3, Object, Root)).unwrap();
        }
        let child = world.observe_object(object(source(region, 3), 361, 2, Object, Local(local_id(3)))).unwrap();
        let Introduced(child) = child.outcome else { panic!("expected child introduction") };
        let parent = world.object(child).unwrap().parent();

        let rebound = world.observe_object(object(source(region, 4), 361, 1, Object, Local(local_id(3)))).unwrap();
        assert_eq!(rebound.outcome, Updated(child));
        assert_eq!(world.entity(uuid(360)), None);
        assert_eq!(world.object(child).unwrap().parent(), parent);
        if !parent_present {
            let _ = world.observe_object(object(source(region, 5), 362, 3, Object, Root)).unwrap();
        }
        let parent = world.entity(uuid(362)).unwrap();
        assert_eq!(world.object(child).unwrap().parent(), Resolved(parent));
        assert_eq!(world.children(parent).map(|object| object.entity()).collect::<Vec<_>>(), [child]);
    }
}

#[test]
fn rebind_survives_replacing_its_parent() {
    let (mut world, region) = with_region();
    let parent = world.observe_object(object(source(region, 1), 360, 1, Object, Root)).unwrap();
    let Introduced(parent_entity) = parent.outcome else { panic!("expected parent introduction") };
    let child = world.observe_object(object(source(region, 2), 361, 2, Object, Local(local_id(1)))).unwrap();
    let Introduced(child_entity) = child.outcome else { panic!("expected child introduction") };
    assert_eq!(world.object(child_entity).unwrap().parent(), Resolved(parent_entity));

    let rebound = world.observe_object(object(source(region, 3), 361, 1, Object, Root)).unwrap();
    assert_eq!(rebound.outcome, Updated(child_entity));
    assert_eq!(world.entity(uuid(360)), None);
    assert_eq!(world.entity(uuid(361)), Some(child_entity));
    assert_eq!(world.entity_at(address(region, 1)), Some(child_entity));
    assert_eq!(world.object(child_entity).unwrap().parent(), Parent::Root);
    assert_eq!(world.object_count(), 1);
}

#[test]
fn parent_cascade_respects_child_order() {
    let (mut world, region) = with_region();
    let parent_address = address(region, 1);
    let parent_id = uuid(30);
    let child_id = uuid(31);

    let child = world.observe_object(object(source(region, 10), 31, 2, Object, Local(local_id(1)))).unwrap();
    let Introduced(child_entity) = child.outcome else { panic!("expected child introduction") };
    assert_eq!(world.object(child_entity).unwrap().parent(), Pending { address: parent_address, expected_uuid: None });
    assert_eq!(child.recovery.requests, [parent_address]);

    let parent = world.observe_object(object(source(region, 11), 30, 1, Object, Root)).unwrap();
    let Introduced(parent_entity) = parent.outcome else { panic!("expected parent introduction") };
    assert_eq!(world.object(child_entity).unwrap().parent(), Resolved(parent_entity));

    assert_eq!(
        world
            .patch_object(ObjectPatch { scale: Some(Vec3::splat(2.0)), ..ObjectPatch::new(source(region, 13), local_id(2)) })
            .unwrap()
            .outcome,
        Updated(child_entity)
    );
    let revision = world.revision();
    let removed = world.remove_object(source(region, 12), local_id(1)).unwrap();
    assert_eq!(removed.outcome, Removed(parent_id));
    assert_eq!(world.revision().get(), revision.get() + 1);
    assert!(removed.recovery.requests.is_empty());
    assert!(world.entity(child_id).is_some());
    assert_eq!(world.object(child_entity).unwrap().parent(), Pending { address: parent_address, expected_uuid: Some(parent_id) });

    let replacement = world.observe_object(object(source(region, 14), 30, 1, Object, Root)).unwrap();
    let Introduced(replacement_entity) = replacement.outcome else { panic!("expected parent reintroduction") };
    let progress = drain_removals(&mut world);
    assert!(!progress.iter().flat_map(|turn| &turn.recovery.requests).any(|target| *target == address(region, 2)));
    drain_relationships(&mut world);
    assert_eq!(world.object(child_entity).unwrap().parent(), Resolved(replacement_entity));

    let revision = world.revision();
    assert_eq!(world.remove_object(source(region, 15), local_id(1)).unwrap().outcome, Removed(parent_id));
    assert_eq!(world.revision().get(), revision.get() + 1);
    assert_eq!(world.entity(child_id), None);
    assert!(world.object(replacement_entity).is_none());
    let _ = drain_removals(&mut world);

    assert_eq!(world.observe_object(object(source(region, 14), 32, 2, Object, Root)).unwrap().outcome, IgnoredStale);
    let reintroduced = world.observe_object(object(source(region, 16), 32, 2, Object, Root)).unwrap();
    let Introduced(reintroduced) = reintroduced.outcome else { panic!("expected a new child after the cascade tombstone") };
    assert_ne!(child_entity, reintroduced);
}

#[test]
fn pending_ancestor_uses_its_own_parent_fence() {
    let mut bounds = limits();
    bounds.max_entities = 80;
    bounds.max_entities_per_region = 80;
    let mut world = World::new(bounds);
    let region = world.register_region().unwrap();
    for id in 1u64..=70 {
        let _ = world.observe_object(object(source(region, id), u128::from(id), id as u32 + 1, Object, Local(local_id(1)))).unwrap();
    }
    let Introduced(parent) = world.observe_object(object(source(region, 71), 1000, 1, Object, Root)).unwrap().outcome else {
        panic!("expected parent introduction")
    };
    let child = world.entity(uuid(70)).unwrap();
    assert!(matches!(world.object(child).unwrap().parent(), Pending { .. }));
    let descendant_id = uuid(65);
    let descendant = world.entity(descendant_id).unwrap();
    assert_eq!(
        world
            .patch_object(ObjectPatch { parent: Some(Local(local_id(71))), ..ObjectPatch::new(source(region, 300), local_id(66)) })
            .unwrap()
            .outcome,
        Updated(descendant)
    );
    assert_eq!(world.object(descendant).unwrap().parent(), Resolved(child));

    let _ = world.remove_object(source(region, 203), local_id(1)).unwrap();

    let mut cursor = None;
    let mut visible = HashSet::new();
    loop {
        let page = world.object_list_page(cursor, 256).unwrap();
        visible.extend(page.objects.into_iter().map(|object| object.id));
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }

    assert!(visible.contains(&descendant_id));
    assert!(!visible.contains(&uuid(70)));
    assert_eq!(world.entity(uuid(1000)), None);
    assert!(world.object(parent).is_none());
}

#[test]
fn pending_cycle_pins_parent_identity() {
    let (mut world, region) = with_region();
    let first = world.observe_object(object(source(region, 1), 370, 1, Object, Local(local_id(2)))).unwrap();
    let Introduced(first) = first.outcome else { panic!("expected first object introduction") };
    let waiting = world.observe_object(object(source(region, 2), 371, 3, Object, Local(local_id(2)))).unwrap();
    let Introduced(waiting) = waiting.outcome else { panic!("expected waiting object introduction") };

    let parent_applied = world.observe_object(object(source(region, 3), 372, 2, Object, Local(local_id(1)))).unwrap();
    let Introduced(parent) = parent_applied.outcome else { panic!("expected parent introduction") };

    assert_eq!(world.object(first).unwrap().parent(), Pending { address: address(region, 2), expected_uuid: Some(uuid(372)) });
    assert_eq!(world.object(parent).unwrap().parent(), Resolved(first));
    assert_eq!(world.object(waiting).unwrap().parent(), Resolved(parent));
    assert_eq!(world.children(parent).map(|object| object.entity()).collect::<HashSet<_>>(), HashSet::from([waiting]));
    assert_eq!(parent_applied.recovery.requests, [address(region, 1)]);
    assert_eq!(
        parent_applied.recovery_cancellations.iter().map(|cancellation| cancellation.address()).collect::<Vec<_>>(),
        [address(region, 3)]
    );

    let unrelated = world.observe_object(object(source(region, 4), 373, 2, Object, Root)).unwrap();
    let Introduced(unrelated) = unrelated.outcome else { panic!("expected local-ID replacement") };
    assert_eq!(world.entity(uuid(372)), None);
    assert_eq!(world.entity(uuid(373)), Some(unrelated));
    assert!(world.object(waiting).is_none());
    assert_eq!(world.object(first).unwrap().parent(), Pending { address: address(region, 2), expected_uuid: Some(uuid(372)) });
}

#[test]
fn invalid_sit_target_keeps_freshness() {
    let (mut world, region) = with_region();
    let object_id = uuid(400);
    let added = world.observe_object(object(source(region, 1), 400, 1, Object, Root)).unwrap();
    let Introduced(entity) = added.outcome else { panic!("expected object introduction") };
    let accepted = SitTarget { position: Vec3::X, rotation: Quat::IDENTITY };
    assert_eq!(world.patch_sit_target(source(region, 10), object_id, Some(accepted)).unwrap().outcome, Updated(entity));
    let revision = world.revision();

    let invalid = SitTarget { position: Vec3::new(f32::NAN, 0.0, 0.0), rotation: Quat::IDENTITY };
    let rejected = world.patch_sit_target(source(region, 100), object_id, Some(invalid)).unwrap();
    assert_eq!(rejected.outcome, Unchanged);
    assert_eq!(rejected.revision, None);
    assert_eq!(world.revision(), revision);
    assert_eq!(
        rejected.rejections.iter().copied().collect::<Vec<_>>(),
        [FieldRejection { group: FieldGroup::SitTarget, reason: FieldRejectionReason::InvalidValue { field: "sit_position" } }]
    );
    assert_eq!(world.object(entity).unwrap().sit_target(), Some(accepted));

    let newer_than_valid = SitTarget { position: Vec3::Y, rotation: Quat::IDENTITY };
    assert_eq!(world.patch_sit_target(source(region, 90), object_id, Some(newer_than_valid)).unwrap().outcome, Updated(entity));
    assert_eq!(world.object(entity).unwrap().sit_target(), Some(newer_than_valid));
}

#[test]
fn fresh_declaration_rebinds_pending_parent() {
    let (mut world, region) = with_region();
    let parent_address = address(region, 1);
    let parent_id = uuid(140);
    let parent = world.observe_object(object(source(region, 10), 140, 1, Object, Root)).unwrap();
    let Introduced(parent_entity) = parent.outcome else { panic!("expected parent introduction") };
    let child = world.observe_object(object(source(region, 11), 141, 2, Object, Local(local_id(1)))).unwrap();
    let Introduced(child_entity) = child.outcome else { panic!("expected child introduction") };

    assert_eq!(
        world.patch_object(ObjectPatch { scale: Some(Vec3::ONE), ..ObjectPatch::new(source(region, 200), local_id(2)) }).unwrap().outcome,
        Updated(child_entity)
    );
    assert_eq!(world.remove_object(source(region, 12), local_id(1)).unwrap().outcome, Removed(parent_id));
    assert_eq!(world.object(child_entity).unwrap().parent(), Pending { address: parent_address, expected_uuid: Some(parent_id) });

    assert_eq!(world.observe_object(object(source(region, 11), 141, 2, Object, Local(local_id(1)))).unwrap().outcome, IgnoredStale);
    let unrelated = world.observe_object(object(source(region, 20), 143, 1, Object, Root)).unwrap();
    let Introduced(unrelated_entity) = unrelated.outcome else { panic!("expected local ID reuse") };
    assert_ne!(parent_entity, unrelated_entity);
    assert_eq!(world.object(child_entity).unwrap().parent(), Pending { address: parent_address, expected_uuid: Some(parent_id) });

    let declaration = world.observe_object(object(source(region, 15), 141, 2, Object, Local(local_id(1)))).unwrap();
    assert_eq!(declaration.outcome, Unchanged);
    assert_eq!(world.object(child_entity).unwrap().parent(), Pending { address: parent_address, expected_uuid: Some(parent_id) });

    let declaration = world.observe_object(object(source(region, 25), 141, 2, Object, Local(local_id(1)))).unwrap();
    assert_eq!(declaration.outcome, Updated(child_entity));
    assert_eq!(world.object(child_entity).unwrap().parent(), Resolved(unrelated_entity));

    let replacement = world.observe_object(object(source(region, 30), 140, 1, Object, Root)).unwrap();
    std::assert_matches!(replacement.outcome, Introduced(_));
    assert_eq!(world.entity(uuid(143)), None);
    assert_eq!(world.object(child_entity).unwrap().parent(), Pending { address: parent_address, expected_uuid: Some(uuid(143)) });
}

#[test]
fn delayed_declaration_keeps_pending_parent() {
    let (mut world, region) = with_region();
    let parent_address = address(region, 1);
    let parent_id = uuid(170);

    let parent = world.observe_object(object(source(region, 10), 170, 1, Object, Root)).unwrap();
    std::assert_matches!(parent.outcome, Introduced(_));
    let child = world.observe_object(object(source(region, 11), 171, 2, Object, Local(local_id(1)))).unwrap();
    let Introduced(child_entity) = child.outcome else { panic!("expected child introduction") };
    assert_eq!(
        world.patch_object(ObjectPatch { scale: Some(Vec3::ONE), ..ObjectPatch::new(source(region, 200), local_id(2)) }).unwrap().outcome,
        Updated(child_entity)
    );
    assert_eq!(world.remove_object(source(region, 12), local_id(1)).unwrap().outcome, Removed(parent_id));

    let delayed = world.observe_object(object(source(region, 13), 171, 2, Object, Local(local_id(1)))).unwrap();
    assert_eq!(delayed.outcome, Unchanged);
    assert_eq!(world.object(child_entity).unwrap().parent(), Pending { address: parent_address, expected_uuid: Some(parent_id) });

    let replacement = world.observe_object(object(source(region, 14), 170, 1, Object, Root)).unwrap();
    let Introduced(parent_entity) = replacement.outcome else { panic!("expected parent reintroduction") };
    assert_eq!(world.object(child_entity).unwrap().parent(), Resolved(parent_entity));
}

#[test]
fn parent_address_reuse_needs_declaration() {
    let (mut world, region) = with_region();
    let parent_address = address(region, 1);

    let parent = world.observe_object(object(source(region, 10), 190, 1, Object, Root)).unwrap();
    assert!(matches!(parent.outcome, Introduced(_)));
    assert_eq!(world.remove_object(source(region, 12), local_id(1)).unwrap().outcome, Removed(uuid(190)));

    let reused_parent = world.observe_object(object(source(region, 20), 191, 1, Object, Root)).unwrap();
    let Introduced(reused_parent) = reused_parent.outcome else { panic!("expected local ID reuse") };
    let child = world.observe_object(object(source(region, 15), 192, 2, Object, Local(local_id(1)))).unwrap();
    let Introduced(child) = child.outcome else { panic!("expected child introduction") };
    assert_eq!(world.object(child).unwrap().parent(), Pending { address: parent_address, expected_uuid: None });

    let refreshed_parent = world.observe_object(object(source(region, 21), 191, 1, Object, Root)).unwrap();
    assert_eq!(refreshed_parent.outcome, Unchanged);
    assert_eq!(refreshed_parent.recovery.requests, [address(region, 2)]);
    assert_eq!(world.object(child).unwrap().parent(), Pending { address: parent_address, expected_uuid: None });

    assert_eq!(world.observe_object(object(source(region, 22), 192, 2, Object, Local(local_id(1)))).unwrap().outcome, Updated(child));
    assert_eq!(world.object(child).unwrap().parent(), Resolved(reused_parent));
}

#[test]
fn parent_resolution_keeps_parent_fence() {
    let (mut world, region) = with_region();
    let child = world.observe_object(object(source(region, 10), 160, 2, Object, Local(local_id(1)))).unwrap();
    let Introduced(child_entity) = child.outcome else { panic!("expected child introduction") };
    let other_parent = world.observe_object(object(source(region, 14), 163, 3, Object, Root)).unwrap();
    let Introduced(other_parent_entity) = other_parent.outcome else { panic!("expected other parent introduction") };

    let parent = world.observe_object(object(source(region, 20), 161, 1, Object, Root)).unwrap();
    let Introduced(parent_entity) = parent.outcome else { panic!("expected parent introduction") };
    assert_eq!(world.object(child_entity).unwrap().parent(), Resolved(parent_entity));

    let delayed_declaration = ObjectPatch { parent: Some(Local(local_id(3))), ..ObjectPatch::new(source(region, 15), local_id(2)) };
    let updated = world.patch_object(delayed_declaration).unwrap();
    assert_eq!(updated.outcome, Updated(child_entity));
    assert_eq!(world.object(child_entity).unwrap().parent(), Resolved(other_parent_entity));
}

#[test]
fn parent_removal_detaches_avatar_without_resolved_pose() {
    let (mut world, region) = with_region();
    let parent = world.observe_object(object(source(region, 10), 150, 1, Object, Root)).unwrap();
    let Introduced(parent_entity) = parent.outcome else { panic!("expected parent introduction") };
    let avatar = world.observe_object(object(source(region, 11), 151, 2, Avatar, Local(local_id(1)))).unwrap();
    let Introduced(avatar_entity) = avatar.outcome else { panic!("expected avatar introduction") };

    let removed = world.remove_object(source(region, 12), local_id(1)).unwrap();
    let cleanup = drain_removals(&mut world);
    assert_eq!(removed.outcome, Removed(uuid(150)));
    assert!(world.object(parent_entity).is_none());
    assert_eq!(world.entity(uuid(151)), Some(avatar_entity));
    assert_eq!(world.object(avatar_entity).unwrap().parent(), Parent::Root);
    assert_eq!(
        world.object(avatar_entity).unwrap().world_pose().unwrap(),
        world::PoseState::Unresolved(world::UnresolvedReason::MissingLocalPose)
    );
    assert!(removed.recovery.requests.is_empty());
    assert!(cleanup.iter().all(|turn| turn.recovery.requests.is_empty()));
}
