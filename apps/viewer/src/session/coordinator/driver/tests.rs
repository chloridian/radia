use std::{collections::HashMap, sync::Arc};

use ::inventory::{
    AssetType, Category, CategoryKey, FolderType, Inventory, InventoryChild, InventoryLimits, InventoryNodeId, InventoryNodeKey,
    InventoryOwner, InventoryType, Item, ItemKey, PermissionMask, Permissions, SaleInfo, SaleType, ServerVersion,
};
use tokio::sync::oneshot;
use utils::{AssetId, AvatarId, FolderId, GroupId, ItemId};
use uuid::Uuid;

use crate::inventory::{FolderFetch, FolderFetchProvenance, favorites::FavoritesError};

use super::inventory::{
    FolderFetchApply, apply_inventory_fetch, cache_restore_move_changes, cancel_favorites_refresh, publish_favorites_refresh,
    resolve_pending_inventory_moves, stop_favorites_refresh,
};
use super::{MoveExpectation, PendingHandshakeReplies, network_end_after_stream_close};
use crate::session::{
    SessionEnd,
    coordinator::{FavoritesUpdate, PresentationMailbox},
};

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
