use std::{collections::HashMap, sync::Arc, time::Instant};

use ::inventory::{
    AssetType, Category, CategoryKey, FolderType, Inventory, InventoryChild, InventoryDelta, InventoryLimits, InventoryNodeId,
    InventoryNodeKey, InventoryOwner, InventoryPreparationCancellation, InventoryType, Item, ItemKey, PermissionMask, Permissions,
    SaleInfo, SaleType, ServerVersion,
};
use tokio::sync::oneshot;
use utils::{AssetId, AvatarId, FolderId, GroupId, ItemId};
use uuid::Uuid;

use crate::inventory::{FolderFetch, FolderFetchProvenance, favorites::FavoritesError};
use crate::session::coordinator::inventory_cache::{
    CACHE_WRITER_QUEUE_CAPACITY, InventoryCacheEvent, InventoryCacheSession, MAX_PENDING_BYTES, MAX_PENDING_ROWS, MAX_PENDING_WRITES,
    RECLAIMER_QUEUE_CAPACITY,
};

use super::inventory::{
    FolderFetchApply, PreparedInventoryFetch, apply_inventory_fetch, cache_restore_move_changes, cancel_favorites_refresh,
    prepare_inventory_fetch, publish_favorites_refresh, resolve_pending_inventory_moves, stop_favorites_refresh,
};
use super::{
    MoveExpectation, PendingFolderPreparation, PendingHandshakeReplies, cancel_pending_folder_preparation, network_end_after_stream_close,
};
use crate::session::{
    SessionEnd,
    coordinator::{FavoritesUpdate, PresentationMailbox},
};

#[global_allocator]
static ALLOCATOR: test_support::allocation::CountingAllocator = test_support::allocation::CountingAllocator;

const OWNER_PATH_ENTRY_COUNT: usize = 100_000;

#[tokio::test(flavor = "current_thread")]
#[ignore = "manual faithful inventory owner-path measurement"]
async fn owner_path_reclaim_measurement() {
    for replace in [false, true] {
        let owner = InventoryOwner::new(Uuid::from_u128(if replace { 72 } else { 71 }));
        let root = CategoryKey::new(owner, FolderId::new(Uuid::from_u128(73)));
        let fixture_baseline = test_support::allocation::live_bytes();
        let fixture_started = Instant::now();
        let mut inventory = Inventory::new(category(root, None, ServerVersion::UNKNOWN, None), None, InventoryLimits::default())
            .expect("valid inventory root");
        let initial_children = (0..OWNER_PATH_ENTRY_COUNT)
            .map(|index| {
                let item_key = ItemKey::new(owner, ItemId::new(Uuid::from_u128(1_000_000 + index as u128)));
                InventoryChild::Item(item(item_key, root))
            })
            .collect();
        let initial = FolderFetch {
            folder: root,
            version: ServerVersion::from_raw(1),
            descendent_count: Some(OWNER_PATH_ENTRY_COUNT as u32),
            children: initial_children,
            provenance: FolderFetchProvenance::Untracked,
        };
        std::assert_matches!(apply_inventory_fetch(&mut inventory, initial), Ok(FolderFetchApply::Applied(_)));
        let fixture_elapsed = fixture_started.elapsed();
        let fixture_retained_bytes = test_support::allocation::live_bytes().saturating_sub(fixture_baseline);

        let (inventory_changes, _inventory_change_receiver) = tokio::sync::watch::channel(None);
        let mut cache = InventoryCacheSession::new(&tokio::runtime::Handle::current());
        cache.handle_event(
            &tokio::runtime::Handle::current(),
            InventoryCacheEvent::OpenFailed("owner-path measurement cache unavailable".into()),
            &mut inventory,
        );
        let before = inventory.snapshot();
        let worker_snapshot = before.clone();
        let cancellation = InventoryPreparationCancellation::default();
        let (prepared, worker_allocations, worker_elapsed) = tokio::task::spawn_blocking(move || {
            let worker_started = Instant::now();
            let (prepared, allocations) = test_support::allocation::measure(|| {
                let children = if replace {
                    (0..OWNER_PATH_ENTRY_COUNT)
                        .map(|index| {
                            let item_key = ItemKey::new(owner, ItemId::new(Uuid::from_u128(2_000_000 + index as u128)));
                            InventoryChild::Item(item(item_key, root))
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let fetch = FolderFetch {
                    folder: root,
                    version: ServerVersion::from_raw(2),
                    descendent_count: Some(if replace { OWNER_PATH_ENTRY_COUNT as u32 } else { 0 }),
                    children,
                    provenance: FolderFetchProvenance::Untracked,
                };
                prepare_inventory_fetch(&worker_snapshot, fetch, &cancellation)
            });
            (prepared.expect("worker preparation succeeds"), allocations, worker_started.elapsed())
        })
        .await
        .expect("preparation worker completes");
        let PreparedInventoryFetch::Prepared(prepared) = prepared else {
            panic!("folder response is current");
        };
        let disposal = cache.try_reserve_disposal().expect("owner mutation reserves bounded reclamation");
        let (applied, applied_result) = oneshot::channel();
        let owner_started = Instant::now();
        let (_, owner_allocations) = test_support::allocation::measure(|| {
            let changes = match inventory.commit_folder_fetch(prepared).expect("prepared folder commits") {
                ::inventory::FolderFetchCommit::Applied(changes) => changes,
                ::inventory::FolderFetchCommit::StaleBase(_) => panic!("single owner kept the base revision current"),
            };
            let after = inventory.snapshot();
            super::inventory::publish_inventory_change(&inventory_changes, &mut cache, changes, before, after, disposal);
            if applied.send(crate::inventory::FolderApplyOutcome::Applied).is_err() {
                tracing::trace!("inventory fetch worker stopped before apply acknowledgement");
            }
        });
        let owner_elapsed = owner_started.elapsed();
        std::assert_matches!(applied_result.await, Ok(crate::inventory::FolderApplyOutcome::Applied));
        let cleanup_started = Instant::now();
        let final_snapshot = inventory_changes.send_replace(None);
        cache.close_with_payload((inventory, final_snapshot)).await;
        let cleanup_elapsed = cleanup_started.elapsed();
        let label = if replace { "replace" } else { "delete" };
        eprintln!(
            "inventory_owner_path case={label} entries={OWNER_PATH_ENTRY_COUNT} cache=disabled fixture_build_ms={} fixture_retained_bytes={} worker_input_plus_prepare_ms={} worker_input_plus_prepare_live_delta_bytes={} worker_input_plus_prepare_peak_delta_bytes={} worker_alloc_calls={} worker_realloc_calls={} owner_commit_publish_handoff_ack_ms={} owner_thread_live_delta_bytes={} owner_thread_peak_delta_bytes={} owner_alloc_calls={} owner_realloc_calls={} cleanup_completion_ms={} cleanup_complete=true reclaimer_queued_capacity={} reclaimer_active_limit=1 deferred_inventory_event_limit={} deferred_inventory_byte_limit={} deferred_network_event_limit_bytes={} cache_writer_queue_capacity={} cache_pending_write_limit={} cache_pending_row_limit={} cache_pending_byte_limit={} memory_scope=per_thread_allocator_deltas_fixture_and_worker_peak_are_separate_and_exclude_whole_process_concurrency",
            fixture_elapsed.as_secs_f64() * 1_000.0,
            fixture_retained_bytes,
            worker_elapsed.as_secs_f64() * 1_000.0,
            worker_allocations.live_bytes,
            worker_allocations.peak_bytes,
            worker_allocations.allocation_calls,
            worker_allocations.reallocation_calls,
            owner_elapsed.as_secs_f64() * 1_000.0,
            owner_allocations.live_bytes,
            owner_allocations.peak_bytes,
            owner_allocations.allocation_calls,
            owner_allocations.reallocation_calls,
            cleanup_elapsed.as_secs_f64() * 1_000.0,
            RECLAIMER_QUEUE_CAPACITY,
            super::MAX_DEFERRED_INVENTORY_EVENTS,
            super::MAX_DEFERRED_INVENTORY_BYTES,
            super::MAX_DEFERRED_NETWORK_EVENT_BYTES,
            CACHE_WRITER_QUEUE_CAPACITY,
            MAX_PENDING_WRITES,
            MAX_PENDING_ROWS,
            MAX_PENDING_BYTES,
        );
    }
}

#[test]
fn pending_handshake_replies_share_one_drain_and_keep_each_reply() {
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
fn requested_disconnect_ends_normally() {
    assert_eq!(network_end_after_stream_close(true, None, true), SessionEnd::Requested);
}

#[test]
fn stream_close_keeps_simulator_failure() {
    assert_eq!(
        network_end_after_stream_close(false, Some("heartbeat timeout".into()), false),
        SessionEnd::Failed("heartbeat timeout".into())
    );
}

#[test]
fn disconnect_request_keeps_failure() {
    assert_eq!(
        network_end_after_stream_close(true, Some("heartbeat timeout".into()), false),
        SessionEnd::Failed("heartbeat timeout".into())
    );
}

#[test]
fn unexpected_primary_disconnect_fails() {
    assert_eq!(network_end_after_stream_close(false, None, true), SessionEnd::Failed("The primary simulator disconnected.".into()));
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
fn stale_fetch_does_not_replace_a_complete_newer_folder() {
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

    std::assert_matches!(apply_inventory_fetch(&mut inventory, stale_fetch), Ok(FolderFetchApply::Stale));
    let snapshot = inventory.snapshot();
    assert_eq!(snapshot.revision(), revision);
    assert_eq!(snapshot.items().count(), 0);
    assert_eq!(snapshot.category(folder).map(|category| category.server_version), Some(ServerVersion::from_raw(10)));
}

#[test]
fn prepared_fetch_commits_before_a_deferred_live_delta() {
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
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
    let (continue_tx, continue_rx) = std::sync::mpsc::sync_channel(0);
    let worker = std::thread::spawn(move || {
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
    let ::inventory::FolderFetchCommit::Applied(_) = inventory.commit_folder_fetch(prepared).expect("owner commits prepared fetch") else {
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
fn same_folder_change_makes_inflight_preparation_stale_and_next_fetch_commits() {
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
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
    let (continue_tx, continue_rx) = std::sync::mpsc::sync_channel(0);
    let worker = std::thread::spawn(move || {
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
    let ::inventory::FolderFetchCommit::StaleBase(prepared) = inventory.commit_folder_fetch(prepared).expect("stale base is an outcome")
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
    let ::inventory::FolderFetchCommit::Applied(_) = inventory.commit_folder_fetch(replacement).expect("next operation commits") else {
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
async fn shutdown_cancels_and_joins_active_inventory_preparation() {
    let owner = InventoryOwner::new(Uuid::from_u128(51));
    let root = CategoryKey::new(owner, FolderId::new(Uuid::from_u128(52)));
    let inventory =
        Inventory::new(category(root, None, ServerVersion::UNKNOWN, None), None, InventoryLimits::default()).expect("valid inventory root");
    let cache = InventoryCacheSession::new(&tokio::runtime::Handle::current());
    let disposal = cache.try_reserve_disposal().expect("reserve bounded shutdown reclamation");
    let before = inventory.snapshot();
    let cancellation = InventoryPreparationCancellation::default();
    let (started, started_rx) = oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::sync_channel(0);
    let task = tokio::task::spawn_blocking(move || {
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
    std::assert_matches!(
        applied_rx.await.expect("shutdown acknowledges the pending fetch"),
        crate::inventory::FolderApplyOutcome::Rejected
    );
    cache.close().await;
}

#[test]
fn older_parent_result_preserves_newer_child_version() {
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
async fn folder_fetch_move_resolves_pending_inventory_edit() {
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
        .find(|change| matches!(change, ::inventory::InventoryChange::NodeMoved { node: changed, .. } if *changed == node));
    std::assert_matches!(
        selected,
        Some(::inventory::InventoryChange::NodeMoved { node: changed, from, to })
            if *changed == node && *from == first && *to == second
    );
    resolve_pending_inventory_moves(&mut pending, &changes);
    confirmation.await.expect("fetch change set confirms pending edit");
    assert!(pending.is_empty());
}

#[tokio::test]
async fn cache_restore_move_resolves_pending_inventory_edit() {
    let owner = InventoryOwner::new(Uuid::from_u128(31));
    let root = CategoryKey::new(owner, FolderId::new(Uuid::from_u128(32)));
    let first = CategoryKey::new(owner, FolderId::new(Uuid::from_u128(33)));
    let second = CategoryKey::new(owner, FolderId::new(Uuid::from_u128(34)));
    let folder = CategoryKey::new(owner, FolderId::new(Uuid::from_u128(35)));
    let mut inventory =
        Inventory::new(category(root, None, ServerVersion::UNKNOWN, None), None, InventoryLimits::default()).expect("valid inventory root");
    inventory.upsert_category(category(first, Some(root), ServerVersion::UNKNOWN, None)).expect("add first folder");
    inventory.upsert_category(category(second, Some(root), ServerVersion::UNKNOWN, None)).expect("add second folder");
    inventory.upsert_category(category(folder, Some(first), ServerVersion::UNKNOWN, None)).expect("add cached moved folder");

    let before = inventory.snapshot();
    inventory.move_node(InventoryNodeKey::new(owner, InventoryNodeId::Category(folder.id())), second).expect("restore cached folder move");
    let after = inventory.snapshot();
    let node = InventoryNodeKey::new(owner, InventoryNodeId::Category(folder.id()));
    let expectation = MoveExpectation { node, from: first, to: second };
    let (confirmed, confirmation) = oneshot::channel();
    let mut pending = HashMap::from([(node, (expectation, confirmed))]);
    let changes = cache_restore_move_changes(&pending, &before, &after);
    resolve_pending_inventory_moves(&mut pending, &changes);
    confirmation.await.expect("cache restore change set confirms pending edit");
    assert!(pending.is_empty());
}

fn favorites_update(username: &str) -> FavoritesUpdate {
    FavoritesUpdate {
        grid: network::Grid::Agni,
        agent_id: Uuid::from_u128(1),
        session_id: Uuid::from_u128(2),
        username: username.to_owned(),
        result: Err(FavoritesError::MissingFavoritesFolder),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn completed_favorites_refresh_is_superseded() {
    let (mailbox, mut receiver) = PresentationMailbox::new(19);
    let (old_started, old_started_rx) = oneshot::channel();
    let (finish_old, finish_old_rx) = oneshot::channel();
    let (old_finished, old_finished_rx) = oneshot::channel();
    let old = tokio::spawn(async move {
        old_started.send(()).expect("test is waiting for the old worker");
        finish_old_rx.await.expect("test releases the old worker");
        old_finished.send(()).expect("test is waiting for old completion");
        favorites_update("old")
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
        favorites_update("new")
    });
    current = Some(new);
    new_started_rx.await.expect("new worker started");
    finish_new.send(()).expect("new worker is waiting");
    let result = current.as_mut().expect("new favorites refresh is owned").await;
    publish_favorites_refresh(&mut current, &mailbox, result);

    let snapshot = receiver.next().await.expect("owner published the current result");
    assert_eq!(snapshot.favorites.as_ref().map(|update| update.username.as_str()), Some("new"));
    assert!(current.is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn favorites_shutdown_cancels_and_reaps_worker() {
    use std::future::pending;

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
