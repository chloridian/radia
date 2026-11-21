use std::{
    assert_matches,
    collections::{HashMap, VecDeque},
    future::pending,
    num::NonZeroU32,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use futures::{FutureExt, future, poll};
use inventory::{
    AssetType, Category, CategoryKey, FolderType, Inventory, InventoryChild, InventoryDelta, InventoryLimits, InventoryNodeId,
    InventoryNodeKey, InventoryOwner, InventoryPreparationCancellation, InventoryType, Item, ItemKey, PermissionMask, Permissions,
    SaleInfo, SaleType, ServerVersion,
};
use math::{Quat, Vec3};
use network::PacketOrder;
use protocol::{
    lludp::Message,
    messages::{
        AvatarSitResponse,
        agent::avatar_sit_response::{SitObject, SitTransform},
    },
};
use tokio::{
    runtime::Handle,
    sync::{oneshot, oneshot::error::TryRecvError},
    task,
};
use utils::{AssetId, AvatarId, FolderId, GroupId, ItemId, Summary};
use uuid::Uuid;
use world::{Kind, LocalAddress, ObjectObservation, ObjectPatch, OrderEvidence, Parent, ParentUpdate, SitTarget, UpdateSource};

use super::{
    DECODE_REJECTION_LOSS_REPORT_INTERVAL, DecodeRejectionLosses, MoveExpectation, PendingFolderPreparation, PendingHandshakeReplies,
    cancel_pending_folder_preparation,
    inventory_sync::{
        FolderFetchApply, PreparedInventoryFetch, apply_inventory_fetch, cancel_favorites_refresh, confirm_pending_moves,
        prepare_inventory_fetch, publish_favorites_refresh, stop_favorites_refresh,
    },
    network_end_after_stream_close,
};
use crate::{
    inventory::{FolderApplyOutcome, FolderFetch, FolderFetchProvenance, favorites::FavoritesError},
    session::{
        SessionEnd,
        coordinator::{FavoritesUpdate, PresentationMailbox, inventory_cache::InventoryCacheSession},
        inbound::{ObjectBatch, ObjectOperation},
        object_cache::ObjectCacheSession,
        world::{BlockOutcome, ObjectBatchProgress, SessionWorld, viewer_world_limits},
    },
    test_helpers,
    world_reader::WorldReadHost,
};

#[tokio::test(flavor = "current_thread")]
async fn pending_update_serves_read_first() {
    const DEPTH: u32 = 600;
    let simulator = test_helpers::simulator_id();
    let mut session = SessionWorld::new(viewer_world_limits());
    let region = session.register_simulator(simulator, None).expect("register simulator");
    for local_id in 1..=DEPTH {
        let parent = if local_id == 1 {
            ParentUpdate::Root
        } else {
            ParentUpdate::Local(NonZeroU32::new(local_id - 1).expect("parent local ID is nonzero"))
        };
        let source = UpdateSource::new(region, local_id, OrderEvidence::Ordered(u64::from(local_id)), 0);
        let observation = ObjectObservation::new(
            source,
            Uuid::from_u128(u128::from(local_id)),
            NonZeroU32::new(local_id).expect("local ID is nonzero"),
            Kind::Object,
            parent,
        );
        let summary = session
            .apply_object_batch(
                simulator,
                ObjectBatch {
                    region,
                    region_handle: None,
                    blocks: vec![Some(ObjectOperation::Observe(observation))],
                    invalid_blocks: 0,
                    invalid_fields: 0,
                },
            )
            .expect("registered simulator");
        assert_eq!(summary.blocks.len(), 1);
    }

    let local_id = NonZeroU32::new(DEPTH).expect("leaf local ID is nonzero");
    let address = LocalAddress::new(region, local_id);
    let sequence = DEPTH + 1;
    let mut patch = ObjectPatch::new(UpdateSource::new(region, sequence, OrderEvidence::Ordered(u64::from(sequence)), 0), local_id);
    patch.scale = Some(Vec3::splat(2.0));
    let later_sequence = sequence + 1;
    let mut later_patch =
        ObjectPatch::new(UpdateSource::new(region, later_sequence, OrderEvidence::Ordered(u64::from(later_sequence)), 0), local_id);
    later_patch.scale = Some(Vec3::splat(3.0));
    let mut later_update = Some(ObjectBatch {
        region,
        region_handle: None,
        blocks: vec![Some(ObjectOperation::Patch(later_patch))],
        invalid_blocks: 0,
        invalid_fields: 0,
    });
    let mut continuation = session
        .begin_object_batch(
            simulator,
            ObjectBatch {
                region,
                region_handle: None,
                blocks: vec![Some(ObjectOperation::Patch(patch))],
                invalid_blocks: 0,
                invalid_fields: 0,
            },
        )
        .expect("registered simulator");
    let revision_before_update = session.world().revision();
    let mut on_chunk = |_, _| {};

    let mut reads = WorldReadHost::new(session.world(), Handle::current());
    let reader = reads.reader();
    let mut barrier_read = Box::pin(reader.barrier());
    assert!(poll!(barrier_read.as_mut()).is_pending());
    let cancellation = pending::<()>().boxed().shared();
    let (turn, first_progress) = super::serve_owner_turn_with_advance(&mut reads, &mut session, &cancellation, |world| {
        world.advance_object_batch(&mut continuation, &mut on_chunk)
    })
    .await;
    assert_matches!(turn, super::OwnerTurnProgress::ReadServed);
    assert_matches!(first_progress, Some(ObjectBatchProgress::Pending));
    let barrier = barrier_read.await.expect("barrier request is served by the owner");
    assert_eq!(barrier.object_count_upper_bound, DEPTH as usize);
    assert_eq!(session.world().revision(), revision_before_update);
    assert!(later_update.is_some());

    let summary = loop {
        let (turn, progress) = super::serve_owner_turn_with_advance(&mut reads, &mut session, &cancellation, |world| {
            world.advance_object_batch(&mut continuation, &mut on_chunk)
        })
        .await;
        assert_matches!(turn, super::OwnerTurnProgress::Continue | super::OwnerTurnProgress::ReadServed);
        match progress.expect("noncancelled owner turn advances the retained update") {
            ObjectBatchProgress::Pending => {}
            ObjectBatchProgress::Complete(summary) => break summary,
        }
    };

    assert_matches!(summary.blocks.first(), Some(BlockOutcome::Applied { .. }));
    let entity = session.world().entity_at(address).expect("updated object remains visible");
    assert_eq!(session.world().object(entity).and_then(|object| object.scale()), Some(Vec3::splat(2.0)));
    assert!(session.world().revision() > revision_before_update);

    let mut later_continuation = session
        .begin_object_batch(simulator, later_update.take().expect("later ordered update remains queued"))
        .expect("registered simulator");
    let later_summary = loop {
        match session.advance_object_batch(&mut later_continuation, &mut on_chunk) {
            ObjectBatchProgress::Pending => {}
            ObjectBatchProgress::Complete(summary) => break summary,
        }
    };
    assert_matches!(later_summary.blocks.first(), Some(BlockOutcome::Applied { .. }));
    let entity = session.world().entity_at(address).expect("later ordered update remains visible");
    assert_eq!(session.world().object(entity).and_then(|object| object.scale()), Some(Vec3::splat(3.0)));
    reads.finish();
}

#[tokio::test(flavor = "current_thread")]
async fn pending_parent_yields_in_order() {
    const DEPTH: u32 = 600;
    let simulator = test_helpers::simulator_id();
    let mut session = SessionWorld::new(viewer_world_limits());
    let region = session.register_simulator(simulator, None).expect("register simulator");
    for local_id in 1..=DEPTH {
        let parent = if local_id == 1 {
            ParentUpdate::Root
        } else {
            ParentUpdate::Local(NonZeroU32::new(local_id - 1).expect("parent local ID is nonzero"))
        };
        let source = UpdateSource::new(region, local_id, OrderEvidence::Ordered(u64::from(local_id)), 0);
        let observation = ObjectObservation::new(
            source,
            Uuid::from_u128(u128::from(local_id)),
            NonZeroU32::new(local_id).expect("local ID is nonzero"),
            Kind::Object,
            parent,
        );
        session
            .apply_object_batch(
                simulator,
                ObjectBatch {
                    region,
                    region_handle: None,
                    blocks: vec![Some(ObjectOperation::Observe(observation))],
                    invalid_blocks: 0,
                    invalid_fields: 0,
                },
            )
            .expect("registered simulator");
    }
    let removed_local = NonZeroU32::new(DEPTH + 1).expect("nonzero local ID");
    let existing_local = NonZeroU32::new(DEPTH + 2).expect("nonzero local ID");
    for (local, id, order) in [(removed_local, 601_u128, DEPTH + 1), (existing_local, 602_u128, DEPTH + 2)] {
        let observation = ObjectObservation::new(
            UpdateSource::new(region, order, OrderEvidence::Ordered(u64::from(order)), 0),
            Uuid::from_u128(id),
            local,
            Kind::Object,
            ParentUpdate::Root,
        );
        session
            .apply_object_batch(
                simulator,
                ObjectBatch {
                    region,
                    region_handle: None,
                    blocks: vec![Some(ObjectOperation::Observe(observation))],
                    invalid_blocks: 0,
                    invalid_fields: 0,
                },
            )
            .expect("registered simulator");
    }
    session
        .apply_object_batch(
            simulator,
            ObjectBatch {
                region,
                region_handle: None,
                blocks: vec![Some(ObjectOperation::Remove {
                    source: UpdateSource::new(region, DEPTH + 3, OrderEvidence::Ordered(u64::from(DEPTH + 3)), 0),
                    local_id: removed_local,
                })],
                invalid_blocks: 0,
                invalid_fields: 0,
            },
        )
        .expect("registered simulator");
    assert!(session.world().has_pending_removals(), "unrelated removal remains physically pending");

    let parent_address = LocalAddress::new(region, NonZeroU32::new(DEPTH).expect("nonzero parent local ID"));
    let child_local = NonZeroU32::new(DEPTH + 4).expect("nonzero child local ID");
    let child_address = LocalAddress::new(region, child_local);
    let child_order = DEPTH + 5;
    let child = ObjectObservation::new(
        UpdateSource::new(region, child_order, OrderEvidence::Ordered(u64::from(child_order)), 0),
        Uuid::from_u128(10_000),
        child_local,
        Kind::Object,
        ParentUpdate::Local(NonZeroU32::new(DEPTH).expect("nonzero parent local ID")),
    );
    let existing_address = LocalAddress::new(region, existing_local);
    let mut reparent =
        ObjectPatch::new(UpdateSource::new(region, DEPTH + 6, OrderEvidence::Ordered(u64::from(DEPTH + 6)), 0), existing_local);
    reparent.parent = Some(ParentUpdate::Local(NonZeroU32::new(DEPTH).expect("nonzero parent local ID")));
    let mut cycle = ObjectPatch::new(
        UpdateSource::new(region, DEPTH + 7, OrderEvidence::Ordered(u64::from(DEPTH + 7)), 0),
        NonZeroU32::new(1).expect("nonzero root local ID"),
    );
    cycle.parent = Some(ParentUpdate::Local(NonZeroU32::new(DEPTH).expect("nonzero parent local ID")));
    let mut later_patch =
        ObjectPatch::new(UpdateSource::new(region, DEPTH + 8, OrderEvidence::Ordered(u64::from(DEPTH + 8)), 0), child_local);
    later_patch.scale = Some(Vec3::splat(2.0));
    let mut later_inputs = VecDeque::from([
        ObjectBatch {
            region,
            region_handle: None,
            blocks: vec![Some(ObjectOperation::Patch(reparent))],
            invalid_blocks: 0,
            invalid_fields: 0,
        },
        ObjectBatch {
            region,
            region_handle: None,
            blocks: vec![Some(ObjectOperation::Patch(cycle))],
            invalid_blocks: 0,
            invalid_fields: 0,
        },
        ObjectBatch {
            region,
            region_handle: None,
            blocks: vec![Some(ObjectOperation::Patch(later_patch))],
            invalid_blocks: 0,
            invalid_fields: 0,
        },
    ]);
    let mut continuation = session
        .begin_object_batch(
            simulator,
            ObjectBatch {
                region,
                region_handle: None,
                blocks: vec![Some(ObjectOperation::Observe(child))],
                invalid_blocks: 0,
                invalid_fields: 0,
            },
        )
        .expect("registered simulator");
    let revision_before_update = session.world().revision();
    let mut reads = WorldReadHost::new(session.world(), Handle::current());
    let reader = reads.reader();
    let cancellation = pending::<()>().boxed().shared();
    let mut on_chunk = |_, _| {};
    let mut turns = 0;
    let summary = loop {
        let revision_before_turn = session.world().revision();
        let mut read = Box::pin(reader.barrier());
        assert!(poll!(read.as_mut()).is_pending(), "a barrier request is queued before each owner turn");
        let (turn, progress) = super::serve_owner_turn_with_advance(&mut reads, &mut session, &cancellation, |world| {
            world.advance_object_batch(&mut continuation, &mut on_chunk)
        })
        .await;
        assert_matches!(turn, super::OwnerTurnProgress::ReadServed);
        let barrier = read.await.expect("the owner serves each queued barrier");
        assert_eq!(barrier.revision, revision_before_turn);
        if turns == 0 {
            assert_eq!(barrier.revision, revision_before_update);
        }
        turns += 1;
        match progress.expect("noncancelled owner turn advances the retained update") {
            ObjectBatchProgress::Pending => {
                assert_eq!(later_inputs.len(), 3);
                if turns == 1 {
                    assert!(session.world().entity_at(child_address).is_none(), "the first quantum cannot commit a deep parent link");
                }
            }
            ObjectBatchProgress::Complete(summary) => break summary,
        }
    };
    assert!(turns >= 3, "the 600-edge parent lookup spans bounded owner turns");
    assert_matches!(summary.blocks.first(), Some(BlockOutcome::Applied { .. }));
    let parent = session.world().entity_at(parent_address).expect("deep parent remains visible");
    let child_entity = session.world().entity_at(child_address).expect("child is added after parent validation");
    assert_eq!(session.world().object(child_entity).expect("child is visible").parent(), Parent::Resolved(parent));

    while let Some(batch) = later_inputs.pop_front() {
        let mut later = session.begin_object_batch(simulator, batch).expect("registered simulator");
        let later_summary = loop {
            let (turn, progress) = super::serve_owner_turn_with_advance(&mut reads, &mut session, &cancellation, |world| {
                world.advance_object_batch(&mut later, &mut on_chunk)
            })
            .await;
            assert_matches!(turn, super::OwnerTurnProgress::Continue | super::OwnerTurnProgress::ReadServed);
            match progress.expect("noncancelled owner turn advances ordered input") {
                ObjectBatchProgress::Pending => {}
                ObjectBatchProgress::Complete(summary) => break summary,
            }
        };
        assert_matches!(later_summary.blocks.first(), Some(BlockOutcome::Applied { .. }));
    }

    let existing_entity = session.world().entity_at(existing_address).expect("reparented child remains visible");
    assert_eq!(session.world().object(existing_entity).expect("existing child is visible").parent(), Parent::Resolved(parent));
    let root = session
        .world()
        .object(session.world().entity_at(LocalAddress::new(region, NonZeroU32::new(1).unwrap())).unwrap())
        .expect("root remains visible");
    assert_eq!(root.parent(), Parent::Root, "cycle validation preserves the existing parent");
    assert_eq!(session.world().object(child_entity).and_then(|object| object.scale()), Some(Vec3::splat(2.0)));
    reads.finish();
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_precedes_queued_read() {
    let simulator = test_helpers::simulator_id();
    let mut session = SessionWorld::new(viewer_world_limits());
    let region = session.register_simulator(simulator, None).expect("register simulator");
    let local_id = NonZeroU32::new(1).expect("nonzero local ID");
    let batch = ObjectBatch {
        region,
        region_handle: None,
        blocks: vec![Some(ObjectOperation::Observe(ObjectObservation::new(
            UpdateSource::new(region, 1, OrderEvidence::Ordered(1), 0),
            Uuid::from_u128(1),
            local_id,
            Kind::Object,
            ParentUpdate::Root,
        )))],
        invalid_blocks: 0,
        invalid_fields: 0,
    };
    let mut continuation = session.begin_object_batch(simulator, batch).expect("registered simulator");
    let mut reads = WorldReadHost::new(session.world(), Handle::current());
    let reader = reads.reader();
    let mut read = Box::pin(reader.barrier());
    assert!(poll!(read.as_mut()).is_pending());
    let cancellation = future::ready(()).boxed().shared();
    let (turn, progress) = super::serve_owner_turn_with_advance(&mut reads, &mut session, &cancellation, |world| {
        world.advance_object_batch(&mut continuation, &mut |_, _| {})
    })
    .await;

    assert_matches!(turn, super::OwnerTurnProgress::Cancelled);
    assert!(progress.is_none());
    assert!(poll!(read.as_mut()).is_pending(), "cancellation wins before the queued read is served");
    assert_eq!(session.world().object_count(), 0, "the pending update does not advance after cancellation");
    reads.finish();
}

#[tokio::test(flavor = "current_thread")]
async fn pending_sit_serves_read_first() {
    const DEPTH: u32 = 600;
    let simulator = test_helpers::simulator_id();
    let mut session = SessionWorld::new(viewer_world_limits());
    let region = session.register_simulator(simulator, None).expect("register simulator");
    for local_id in 1..=DEPTH {
        let parent = if local_id == 1 {
            ParentUpdate::Root
        } else {
            ParentUpdate::Local(NonZeroU32::new(local_id - 1).expect("parent local ID is nonzero"))
        };
        let source = UpdateSource::new(region, local_id, OrderEvidence::Ordered(u64::from(local_id)), 0);
        let observation = ObjectObservation::new(
            source,
            Uuid::from_u128(u128::from(local_id)),
            NonZeroU32::new(local_id).expect("local ID is nonzero"),
            Kind::Object,
            parent,
        );
        session
            .apply_object_batch(
                simulator,
                ObjectBatch {
                    region,
                    region_handle: None,
                    blocks: vec![Some(ObjectOperation::Observe(observation))],
                    invalid_blocks: 0,
                    invalid_fields: 0,
                },
            )
            .expect("registered simulator");
    }

    let expected = SitTarget { position: Vec3::new(1.0, 2.0, 3.0), rotation: Quat::IDENTITY };
    let message = Message::AvatarSitResponse(AvatarSitResponse {
        sit_object: SitObject { id: Some(Uuid::from_u128(u128::from(DEPTH))) },
        sit_transform: SitTransform {
            auto_pilot: Some(true),
            sit_position: Some(expected.position),
            sit_rotation: Some(expected.rotation),
            camera_eye_offset: Some(Vec3::ZERO),
            camera_at_offset: Some(Vec3::ZERO),
            force_mouselook: Some(false),
        },
    });
    let mut handoff = super::WorldHandoff::default();
    let mut object_cache = ObjectCacheSession::new(network::Grid::Agni);
    let preparation = handoff.begin_object_update(
        &mut session,
        simulator,
        DEPTH + 1,
        PacketOrder::Ordered(u64::from(DEPTH + 1)),
        Instant::now(),
        None,
        &message,
        None,
        &mut object_cache,
    );
    let Some(super::ObjectUpdatePreparation::Entity(mut continuation)) = preparation else {
        panic!("avatar sit response must retain its visibility lookup");
    };
    let revision_before_update = session.world().revision();
    assert_matches!(handoff.advance_entity_update(&mut session, &mut continuation), super::EntityUpdateProgress::Pending);

    let mut reads = WorldReadHost::new(session.world(), Handle::current());
    let reader = reads.reader();
    let mut barrier_read = Box::pin(reader.barrier());
    assert!(poll!(barrier_read.as_mut()).is_pending());
    let cancellation = pending::<()>().boxed().shared();
    assert_matches!(super::serve_owner_turn(&mut reads, &mut session, &cancellation).await, super::OwnerTurnProgress::ReadServed);
    let barrier = barrier_read.await.expect("barrier request is served by the owner");
    assert_eq!(barrier.object_count_upper_bound, DEPTH as usize);
    assert_eq!(session.world().revision(), revision_before_update);

    let outcome = loop {
        match super::serve_owner_turn(&mut reads, &mut session, &cancellation).await {
            super::OwnerTurnProgress::Continue | super::OwnerTurnProgress::ReadServed => {}
            super::OwnerTurnProgress::Cancelled => panic!("test cancellation remains pending"),
        }
        match handoff.advance_entity_update(&mut session, &mut continuation) {
            super::EntityUpdateProgress::Pending => {}
            super::EntityUpdateProgress::Complete(outcome) => break outcome,
        }
    };

    assert!(outcome.summary.is_some());
    let address = LocalAddress::new(region, NonZeroU32::new(DEPTH).expect("leaf local ID is nonzero"));
    let entity = session.world().entity_at(address).expect("sit response target remains visible");
    assert_eq!(session.world().object(entity).and_then(|object| object.sit_target()), Some(expected));
    assert!(session.world().revision() > revision_before_update);
    reads.finish();
}

#[test]
fn handshake_replies_share_one_drain() {
    let simulator = network::SimulatorId::new(7);
    let mut pending = PendingHandshakeReplies::default();

    assert!(pending.queue(simulator, 3));
    assert!(!pending.queue(simulator, 3));
    assert!(!pending.queue(simulator, 3));
    assert_eq!(pending.by_simulator.len(), 1);
    assert_eq!(pending.by_simulator.get(&simulator).map(|reply| reply.remaining), Some(3));

    assert_eq!(pending.take_next(simulator), Some(3));
    assert_eq!(pending.take_next(simulator), Some(3));
    assert_eq!(pending.take_next(simulator), Some(3));
    assert_eq!(pending.take_next(simulator), None);
    assert!(pending.by_simulator.is_empty());
}

#[test]
fn stream_close_session_end() {
    for (requested, failure, primary_closed, expected) in [
        (true, None, true, SessionEnd::Requested),
        (false, Some("heartbeat timeout"), false, SessionEnd::Failed("heartbeat timeout".into())),
        (true, Some("heartbeat timeout"), false, SessionEnd::Failed("heartbeat timeout".into())),
        (false, None, true, SessionEnd::Failed("The primary simulator disconnected.".into())),
    ] {
        assert_eq!(
            network_end_after_stream_close(requested, failure.map(Into::into), primary_closed),
            expected,
            "requested={requested} failure={failure:?} primary_closed={primary_closed}"
        );
    }
}

fn category(key: CategoryKey, parent: Option<CategoryKey>, version: ServerVersion, descendent_count: Option<u32>) -> Category {
    Category {
        key,
        parent,
        preferred_folder_type: FolderType::None,
        name: Arc::from("folder"),
        server_version: version,
        server_descendent_count: descendent_count,
    }
}

fn item(key: ItemKey, parent: CategoryKey) -> Item {
    Item {
        key,
        parent,
        asset_id: AssetId::new(Uuid::nil()),
        asset_type: AssetType::Notecard,
        inventory_type: InventoryType::Notecard,
        flags: 0,
        name: Arc::from("item"),
        description: Arc::from("fixture"),
        created_at: 0,
        permissions: Permissions {
            creator: AvatarId::new(Uuid::nil()),
            owner: AvatarId::new(Uuid::nil()),
            group: GroupId::new(Uuid::nil()),
            last_owner: AvatarId::new(Uuid::nil()),
            base: PermissionMask::new(0),
            owner_mask: PermissionMask::new(0),
            group_mask: PermissionMask::new(0),
            everyone: PermissionMask::new(0),
            next_owner: PermissionMask::new(0),
        },
        sale: SaleInfo { sale_type: SaleType::NotForSale, price: 0 },
    }
}

#[test]
fn stale_fetch_keeps_newer_folder() {
    let owner = InventoryOwner::new(Uuid::from_u128(1));
    let folder = CategoryKey::new(owner, FolderId::new(Uuid::from_u128(2)));
    let root = Category {
        key: folder,
        parent: None,
        preferred_folder_type: FolderType::None,
        name: Arc::from("root"),
        server_version: ServerVersion::from_raw(10),
        server_descendent_count: Some(0),
    };
    let mut inventory = Inventory::new(root, None, InventoryLimits::default()).expect("valid inventory root");
    let revision = inventory.snapshot().revision();
    let stale_item = Item {
        key: ItemKey::new(owner, ItemId::new(Uuid::from_u128(3))),
        parent: folder,
        asset_id: AssetId::new(Uuid::nil()),
        asset_type: AssetType::Notecard,
        inventory_type: InventoryType::Notecard,
        flags: 0,
        name: Arc::from("stale"),
        description: Arc::from("stale response"),
        created_at: 0,
        permissions: Permissions {
            creator: AvatarId::new(Uuid::nil()),
            owner: AvatarId::new(Uuid::nil()),
            group: GroupId::new(Uuid::nil()),
            last_owner: AvatarId::new(Uuid::nil()),
            base: PermissionMask::new(0),
            owner_mask: PermissionMask::new(0),
            group_mask: PermissionMask::new(0),
            everyone: PermissionMask::new(0),
            next_owner: PermissionMask::new(0),
        },
        sale: SaleInfo { sale_type: SaleType::NotForSale, price: 0 },
    };
    let stale_fetch = FolderFetch {
        folder,
        version: ServerVersion::from_raw(9),
        descendent_count: Some(1),
        children: vec![InventoryChild::Item(stale_item)],
        provenance: FolderFetchProvenance::Untracked,
    };

    assert_matches!(apply_inventory_fetch(&mut inventory, stale_fetch), Ok(FolderFetchApply::Stale));
    let snapshot = inventory.snapshot();
    assert_eq!(snapshot.revision(), revision);
    assert_eq!(snapshot.items().count(), 0);
    assert_eq!(snapshot.category(folder).map(|category| category.server_version), Some(ServerVersion::from_raw(10)));
}

#[test]
fn prepared_fetch_precedes_deferred_delta() {
    let owner = InventoryOwner::new(Uuid::from_u128(11));
    let root = CategoryKey::new(owner, FolderId::new(Uuid::from_u128(12)));
    let old_item = ItemKey::new(owner, ItemId::new(Uuid::from_u128(13)));
    let new_item = ItemKey::new(owner, ItemId::new(Uuid::from_u128(14)));
    let mut root_category = category(root, None, ServerVersion::from_raw(7), Some(1));
    root_category.name = Arc::from("root");
    let mut inventory = Inventory::new(root_category, None, InventoryLimits::default()).expect("valid root");
    inventory.upsert_item(item(old_item, root)).expect("old child applies");
    let snapshot = inventory.snapshot();
    let generation = snapshot.folder_generation(root);
    let owner_generation = snapshot.owner_refresh_generation(owner);
    let fetch = FolderFetch {
        folder: root,
        version: ServerVersion::from_raw(8),
        descendent_count: Some(0),
        children: Vec::new(),
        provenance: FolderFetchProvenance::StartedAt { folder: generation, owner: owner_generation },
    };
    let cancellation = InventoryPreparationCancellation::default();
    let worker_cancellation = cancellation;
    let (started_tx, started_rx) = mpsc::sync_channel(0);
    let (continue_tx, continue_rx) = mpsc::sync_channel(0);
    let worker = thread::spawn(move || {
        started_tx.send(()).expect("test is waiting for worker");
        continue_rx.recv().expect("test releases worker");
        prepare_inventory_fetch(&snapshot, fetch, &worker_cancellation)
    });
    started_rx.recv().expect("worker reached controlled delay");
    let deferred_delta = InventoryDelta {
        created_items: vec![item(new_item, root)],
        updated_category_versions: vec![(root, ServerVersion::from_raw(9))],
        ..InventoryDelta::default()
    };
    continue_tx.send(()).expect("worker is waiting to prepare");
    let prepared = worker.join().expect("worker completes").expect("fetch transaction prepares");
    let PreparedInventoryFetch::Prepared(prepared) = prepared else {
        panic!("fetch should be prepared against its base revision");
    };
    let inventory::FolderFetchCommit::Applied(_) = inventory.commit_folder_fetch(prepared).expect("owner commits prepared fetch") else {
        panic!("base revision is still current");
    };
    let changes = inventory.apply_delta(deferred_delta).expect("deferred live delta applies after the fetch");

    let current = inventory.snapshot();
    assert!(current.item(old_item).is_none());
    assert!(current.item(new_item).is_some());
    assert_eq!(current.category(root).map(|folder| folder.server_version), Some(ServerVersion::from_raw(9)));
    assert!(current.is_complete(root));
    assert!(changes.refetch_categories.is_empty());
}

#[test]
fn folder_change_stales_preparation() {
    let owner = InventoryOwner::new(Uuid::from_u128(41));
    let root = CategoryKey::new(owner, FolderId::new(Uuid::from_u128(42)));
    let old_item = ItemKey::new(owner, ItemId::new(Uuid::from_u128(43)));
    let fetched_item = ItemKey::new(owner, ItemId::new(Uuid::from_u128(44)));
    let live_item = ItemKey::new(owner, ItemId::new(Uuid::from_u128(45)));
    let mut root_category = category(root, None, ServerVersion::from_raw(7), Some(1));
    root_category.name = Arc::from("root");
    let mut inventory = Inventory::new(root_category, None, InventoryLimits::default()).expect("valid root");
    inventory.upsert_item(item(old_item, root)).expect("old child applies");
    let snapshot = inventory.snapshot();
    let generation = snapshot.folder_generation(root);
    let owner_generation = snapshot.owner_refresh_generation(owner);
    let fetch = FolderFetch {
        folder: root,
        version: ServerVersion::from_raw(8),
        descendent_count: Some(1),
        children: vec![InventoryChild::Item(item(fetched_item, root))],
        provenance: FolderFetchProvenance::StartedAt { folder: generation, owner: owner_generation },
    };
    let worker_cancellation = InventoryPreparationCancellation::default();
    let (started_tx, started_rx) = mpsc::sync_channel(0);
    let (continue_tx, continue_rx) = mpsc::sync_channel(0);
    let worker = thread::spawn(move || {
        let prepared = prepare_inventory_fetch(&snapshot, fetch, &worker_cancellation);
        started_tx.send(()).expect("test is waiting for prepared candidate");
        continue_rx.recv().expect("test releases preparation");
        prepared
    });
    started_rx.recv().expect("worker finished preparation and holds the candidate");

    inventory
        .apply_delta(InventoryDelta {
            created_items: vec![item(live_item, root)],
            updated_category_versions: vec![(root, ServerVersion::from_raw(9))],
            ..InventoryDelta::default()
        })
        .expect("live update changes the same folder during preparation");
    continue_tx.send(()).expect("worker is waiting to prepare");

    let prepared = worker.join().expect("worker completes").expect("fetch transaction prepares");
    let PreparedInventoryFetch::Prepared(prepared) = prepared else { panic!("response should prepare against its captured snapshot") };
    let inventory::FolderFetchCommit::StaleBase(prepared) = inventory.commit_folder_fetch(prepared).expect("stale base is an outcome")
    else {
        panic!("the owner must not publish a result prepared against the changed folder");
    };
    drop(prepared);

    let current = inventory.snapshot();
    assert!(current.item(old_item).is_some());
    assert!(current.item(live_item).is_some());
    assert!(current.item(fetched_item).is_none());
    assert_eq!(current.category(root).map(|folder| folder.server_version), Some(ServerVersion::from_raw(9)));

    let replacement = current
        .prepare_folder_fetch(root, ServerVersion::from_raw(10), Some(1), vec![InventoryChild::Item(item(live_item, root))])
        .expect("next operation prepares from the current revision");
    let inventory::FolderFetchCommit::Applied(_) = inventory.commit_folder_fetch(replacement).expect("next operation commits") else {
        panic!("fresh transaction must commit against the current folder");
    };
    let current = inventory.snapshot();
    assert!(current.item(old_item).is_none());
    assert!(current.item(live_item).is_some());
    assert!(current.item(fetched_item).is_none());
    assert_eq!(current.category(root).map(|folder| folder.server_version), Some(ServerVersion::from_raw(10)));
    assert!(current.is_complete(root));
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_joins_inventory_preparation() {
    let owner = InventoryOwner::new(Uuid::from_u128(51));
    let root = CategoryKey::new(owner, FolderId::new(Uuid::from_u128(52)));
    let inventory =
        Inventory::new(category(root, None, ServerVersion::UNKNOWN, None), None, InventoryLimits::default()).expect("valid inventory root");
    let cache = InventoryCacheSession::new(&Handle::current());
    let disposal = cache.try_reserve_disposal().expect("reserve bounded shutdown reclamation");
    let before = inventory.snapshot();
    let cancellation = InventoryPreparationCancellation::default();
    let (started, started_rx) = oneshot::channel();
    let (release, release_rx) = mpsc::sync_channel(0);
    let task = task::spawn_blocking(move || {
        started.send(()).expect("test waits for preparation worker");
        release_rx.recv().expect("test releases active preparation");
        Ok(PreparedInventoryFetch::Stale)
    });
    let (applied, applied_rx) = oneshot::channel();
    let preparation = PendingFolderPreparation { folder: root, before, applied, cancellation: cancellation.clone(), disposal, task };
    started_rx.await.expect("worker started");

    let shutdown = cancel_pending_folder_preparation(preparation);
    assert!(cancellation.is_cancelled());
    release.send(()).expect("active preparation is waiting for shutdown release");
    shutdown.await;
    assert_matches!(applied_rx.await.expect("shutdown acknowledges the pending fetch"), FolderApplyOutcome::Rejected);
    cache.close().await;
}

#[test]
fn older_parent_keeps_newer_child() {
    let owner = InventoryOwner::new(Uuid::from_u128(1));
    let root = CategoryKey::new(owner, FolderId::new(Uuid::from_u128(2)));
    let child = CategoryKey::new(owner, FolderId::new(Uuid::from_u128(3)));
    let item_key = ItemKey::new(owner, ItemId::new(Uuid::from_u128(4)));
    let mut inventory =
        Inventory::new(category(root, None, ServerVersion::UNKNOWN, None), None, InventoryLimits::default()).expect("valid inventory root");
    inventory.upsert_category(category(child, Some(root), ServerVersion::UNKNOWN, None)).expect("add child folder");
    let item = Item {
        key: item_key,
        parent: child,
        asset_id: AssetId::new(Uuid::nil()),
        asset_type: AssetType::Notecard,
        inventory_type: InventoryType::Notecard,
        flags: 0,
        name: Arc::from("new item"),
        description: Arc::from("new child response"),
        created_at: 0,
        permissions: Permissions {
            creator: AvatarId::new(Uuid::nil()),
            owner: AvatarId::new(Uuid::nil()),
            group: GroupId::new(Uuid::nil()),
            last_owner: AvatarId::new(Uuid::nil()),
            base: PermissionMask::new(0),
            owner_mask: PermissionMask::new(0),
            group_mask: PermissionMask::new(0),
            everyone: PermissionMask::new(0),
            next_owner: PermissionMask::new(0),
        },
        sale: SaleInfo { sale_type: SaleType::NotForSale, price: 0 },
    };

    let FolderFetchApply::Applied(_) = apply_inventory_fetch(
        &mut inventory,
        FolderFetch {
            folder: child,
            version: ServerVersion::from_raw(10),
            descendent_count: Some(1),
            children: vec![InventoryChild::Item(item)],
            provenance: FolderFetchProvenance::Untracked,
        },
    )
    .expect("child response applies") else {
        panic!("child response is not stale");
    };
    let stale_child = category(child, Some(root), ServerVersion::from_raw(9), Some(0));
    apply_inventory_fetch(
        &mut inventory,
        FolderFetch {
            folder: root,
            version: ServerVersion::from_raw(3),
            descendent_count: Some(1),
            children: vec![InventoryChild::Category(stale_child)],
            provenance: FolderFetchProvenance::Untracked,
        },
    )
    .expect("parent response applies");

    let snapshot = inventory.snapshot();
    let child_category = snapshot.category(child).expect("child remains present");
    assert_eq!(child_category.server_version, ServerVersion::from_raw(10));
    assert_eq!(child_category.server_descendent_count, Some(1));
    assert!(snapshot.is_complete(child));
    assert!(snapshot.item(item_key).is_some());
}

#[tokio::test]
async fn fetch_move_resolves_pending_edit() {
    let owner = InventoryOwner::new(Uuid::from_u128(21));
    let root = CategoryKey::new(owner, FolderId::new(Uuid::from_u128(22)));
    let first = CategoryKey::new(owner, FolderId::new(Uuid::from_u128(23)));
    let second = CategoryKey::new(owner, FolderId::new(Uuid::from_u128(24)));
    let folder = CategoryKey::new(owner, FolderId::new(Uuid::from_u128(25)));
    let mut inventory =
        Inventory::new(category(root, None, ServerVersion::UNKNOWN, None), None, InventoryLimits::default()).expect("valid inventory root");
    inventory.upsert_category(category(first, Some(root), ServerVersion::UNKNOWN, None)).expect("add first folder");
    inventory.upsert_category(category(second, Some(root), ServerVersion::UNKNOWN, None)).expect("add second folder");
    inventory.upsert_category(category(folder, Some(first), ServerVersion::UNKNOWN, None)).expect("add moved folder");

    let node = InventoryNodeKey::new(owner, InventoryNodeId::Category(folder.id()));
    let expectation = MoveExpectation { node, from: first, to: second };
    let (confirmed, confirmation) = oneshot::channel();
    let mut pending = HashMap::from([(node, (expectation, confirmed))]);
    let before = inventory.snapshot();
    let FolderFetchApply::Applied(changes) = apply_inventory_fetch(
        &mut inventory,
        FolderFetch {
            folder: second,
            version: ServerVersion::UNKNOWN,
            descendent_count: Some(1),
            children: vec![InventoryChild::Category(category(folder, Some(second), ServerVersion::UNKNOWN, Some(0)))],
            provenance: FolderFetchProvenance::Untracked,
        },
    )
    .expect("apply destination fetch") else {
        panic!("destination fetch should apply");
    };
    let selected = changes
        .changes
        .iter()
        .find(|change| matches!(change, inventory::InventoryChange::NodeMoved { node: changed, .. } if *changed == node));
    assert_matches!(
        selected,
        Some(inventory::InventoryChange::NodeMoved { node: changed, from, to })
            if *changed == node && *from == first && *to == second
    );
    confirm_pending_moves(&mut pending, &before, &inventory.snapshot());
    confirmation.await.expect("fetch change set confirms pending edit");
    assert!(pending.is_empty());
}

#[test]
fn snapshot_moves_confirm_only_matches() {
    #[derive(Clone, Copy, Debug)]
    enum Case {
        Match,
        WrongDestination,
        WrongSource,
        Unrelated,
    }

    let owner = InventoryOwner::new(Uuid::from_u128(41));
    let key = |value: u128| CategoryKey::new(owner, FolderId::new(Uuid::from_u128(value)));
    let (root, first, second, third) = (key(42), key(43), key(44), key(45));
    let moved_folder = key(46);
    let moved_item = ItemKey::new(owner, ItemId::new(Uuid::from_u128(47)));
    let bystander = key(48);
    let nodes = [
        InventoryNodeKey::new(owner, InventoryNodeId::Category(moved_folder.id())),
        InventoryNodeKey::new(owner, InventoryNodeId::Item(moved_item.id())),
    ];
    for node in nodes {
        for case in [Case::Match, Case::WrongDestination, Case::WrongSource, Case::Unrelated] {
            let mut inventory = Inventory::new(category(root, None, ServerVersion::UNKNOWN, None), None, InventoryLimits::default())
                .expect("valid inventory root");
            for folder in [first, second, third] {
                inventory.upsert_category(category(folder, Some(root), ServerVersion::UNKNOWN, None)).expect("add folder");
            }
            inventory.upsert_category(category(moved_folder, Some(first), ServerVersion::UNKNOWN, None)).expect("add moved folder");
            inventory.upsert_category(category(bystander, Some(first), ServerVersion::UNKNOWN, None)).expect("add bystander folder");
            inventory.upsert_item(item(moved_item, first)).expect("add moved item");

            let expectation = match case {
                Case::Match | Case::Unrelated => MoveExpectation { node, from: first, to: second },
                Case::WrongDestination => MoveExpectation { node, from: first, to: third },
                Case::WrongSource => MoveExpectation { node, from: third, to: second },
            };
            let (confirmed, mut confirmation) = oneshot::channel();
            let mut pending = HashMap::from([(node, (expectation, confirmed))]);
            let before = inventory.snapshot();
            let mover = match case {
                Case::Unrelated => InventoryNodeKey::new(owner, InventoryNodeId::Category(bystander.id())),
                _ => node,
            };
            inventory.move_node(mover, second).expect("apply move");
            confirm_pending_moves(&mut pending, &before, &inventory.snapshot());

            match case {
                Case::Match => {
                    assert_eq!(confirmation.try_recv(), Ok(()), "{node:?} {case:?}");
                    assert!(pending.is_empty(), "{node:?} {case:?}");
                }
                _ => {
                    assert_eq!(confirmation.try_recv(), Err(TryRecvError::Empty), "{node:?} {case:?}");
                    assert_eq!(pending.len(), 1, "{node:?} {case:?}");
                }
            }
        }
    }
}

fn favorites_update(session_id: Uuid) -> FavoritesUpdate {
    FavoritesUpdate {
        grid: network::Grid::Agni,
        agent_id: Uuid::from_u128(1),
        session_id,
        result: Err(FavoritesError::MissingFavoritesFolder),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn completed_favorites_refresh_is_superseded() {
    let (mailbox, mut receiver) = PresentationMailbox::new(19);
    let old_session = Uuid::from_u128(2);
    let new_session = Uuid::from_u128(3);
    let (old_started, old_started_rx) = oneshot::channel();
    let (finish_old, finish_old_rx) = oneshot::channel();
    let (old_finished, old_finished_rx) = oneshot::channel();
    let old = tokio::spawn(async move {
        old_started.send(()).expect("test is waiting for the old worker");
        finish_old_rx.await.expect("test releases the old worker");
        old_finished.send(()).expect("test is waiting for old completion");
        favorites_update(old_session)
    });
    let mut current = Some(old);
    old_started_rx.await.expect("old worker started");
    finish_old.send(()).expect("old worker is waiting");
    old_finished_rx.await.expect("old worker returned its update");
    assert!(current.as_ref().is_some_and(|task| task.is_finished()));

    cancel_favorites_refresh(&mut current);
    let (new_started, new_started_rx) = oneshot::channel();
    let (finish_new, finish_new_rx) = oneshot::channel();
    let new = tokio::spawn(async move {
        new_started.send(()).expect("test is waiting for the new worker");
        finish_new_rx.await.expect("test releases the new worker");
        favorites_update(new_session)
    });
    current = Some(new);
    new_started_rx.await.expect("new worker started");
    finish_new.send(()).expect("new worker is waiting");
    let result = current.as_mut().expect("new favorites refresh is owned").await;
    publish_favorites_refresh(&mut current, &mailbox, result);

    let snapshot = receiver.next().await.expect("owner published the current result");
    assert_eq!(snapshot.favorites.as_ref().map(|update| update.session_id), Some(new_session));
    assert!(current.is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn favorites_shutdown_reaps_worker() {
    let (started, started_rx) = oneshot::channel();
    let (dropped, dropped_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        struct DropSignal(Option<oneshot::Sender<()>>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    sender.send(()).expect("shutdown observer remains alive");
                }
            }
        }

        let _signal = DropSignal(Some(dropped));
        started.send(()).expect("test is waiting for the worker");
        pending::<FavoritesUpdate>().await
    });
    let mut current = Some(task);
    started_rx.await.expect("worker started");

    stop_favorites_refresh(&mut current).await;

    assert!(current.is_none());
    dropped_rx.await.expect("shutdown reaped the cancelled worker");
}

#[test]
fn decode_rejection_coverage_loss_aggregates_and_flushes() {
    let simulator = test_helpers::simulator_id();
    let mut summary = Summary::<DecodeRejectionLosses>::new(DECODE_REJECTION_LOSS_REPORT_INTERVAL);
    let start = Instant::now();

    let first = summary
        .record(start, |losses| losses.record(simulator, Some(42), "lludp"))
        .expect("first decode rejection loss reports immediately");
    assert_eq!(first.count, 1);
    assert_eq!(first.sample_simulator, Some(simulator));
    assert_eq!(first.sample_region, Some(42));
    assert_eq!(first.sample_source, Some("lludp"));

    assert!(summary.record(start + Duration::from_secs(1), |losses| losses.record(simulator, Some(42), "lludp")).is_none());
    assert!(summary.record(start + Duration::from_secs(2), |losses| losses.record(simulator, Some(42), "lludp")).is_none());

    let flushed = summary.flush().expect("pending decode rejection losses flush on teardown");
    assert_eq!(flushed.count, 2);
    assert_eq!(flushed.sample_simulator, Some(simulator));
    assert_eq!(flushed.sample_region, Some(42));
    assert_eq!(flushed.sample_source, Some("lludp"));
    assert!(summary.flush().is_none());

    assert!(summary.record(start + Duration::from_secs(3), |losses| losses.record(simulator, Some(99), "lludp")).is_none());
    let due = summary
        .record(start + DECODE_REJECTION_LOSS_REPORT_INTERVAL, |losses| losses.record(simulator, Some(99), "lludp"))
        .expect("decode rejection losses report when interval elapses");
    assert_eq!(due.count, 2);
    assert_eq!(due.sample_region, Some(99));
}
