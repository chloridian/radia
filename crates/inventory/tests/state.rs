use std::{
    assert_matches,
    collections::{HashMap, HashSet},
    sync::{Arc, mpsc},
    thread,
};

use inventory::{
    AssetType, Category, CategoryKey, FolderType, Inventory, InventoryChild, InventoryDelta, InventoryError, InventoryLimits,
    InventoryNodeId, InventoryNodeKey, InventoryOwner, InventoryType, Item, ItemKey, LinkResolution, PermissionMask, Permissions, SaleInfo,
    SaleType, ServerVersion,
};
use proptest::{collection, prelude::*};
use utils::{AssetId, AvatarId, FolderId, GroupId, ItemId};
use uuid::Uuid;

fn uuid(id: u128) -> Uuid {
    Uuid::from_u128(id)
}

fn owner(id: u128) -> InventoryOwner {
    InventoryOwner::new(uuid(id))
}

fn category(owner: InventoryOwner, id: u128, parent: Option<u128>, name: &str) -> Category {
    Category {
        key: CategoryKey::new(owner, FolderId::new(uuid(id))),
        parent: parent.map(|parent| CategoryKey::new(owner, FolderId::new(uuid(parent)))),
        preferred_folder_type: FolderType::None,
        name: Arc::from(name),
        server_version: ServerVersion::UNKNOWN,
        server_descendent_count: None,
    }
}

fn item(owner: InventoryOwner, id: u128, parent: u128, asset_type: AssetType, asset_id: u128) -> Item {
    Item {
        key: ItemKey::new(owner, ItemId::new(uuid(id))),
        parent: CategoryKey::new(owner, FolderId::new(uuid(parent))),
        asset_id: AssetId::new(uuid(asset_id)),
        asset_type,
        inventory_type: InventoryType::Unknown(777),
        flags: 0,
        name: Arc::from(format!("item-{id}")),
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

fn inventory(owner: InventoryOwner) -> Inventory {
    Inventory::new(category(owner, 1, None, "root"), None, InventoryLimits::default()).expect("valid root")
}

fn inventory_with_limits(owner: InventoryOwner, limits: InventoryLimits) -> Inventory {
    Inventory::new(category(owner, 1, None, "root"), None, limits).expect("valid root")
}

fn apply_fetch(
    inventory: &mut Inventory,
    folder: CategoryKey,
    version: ServerVersion,
    descendent_count: Option<u32>,
    children: Vec<InventoryChild>,
) -> Result<inventory::ChangeSet, InventoryError> {
    let prepared = inventory.snapshot().prepare_folder_fetch(folder, version, descendent_count, children)?;
    let inventory::FolderFetchCommit::Applied(changes) = inventory.commit_folder_fetch(prepared)? else {
        panic!("single-owner fetch preparation became stale before commit");
    };
    Ok(changes)
}

#[test]
fn owner_refresh_invalidates_nested_completeness() {
    let personal = owner(100);
    let library = owner(200);
    let personal_root = CategoryKey::new(personal, FolderId::new(uuid(1)));
    let library_root = CategoryKey::new(library, FolderId::new(uuid(101)));
    let mut personal_root_category = category(personal, 1, None, "personal");
    personal_root_category.server_version = ServerVersion::from_raw(1);
    personal_root_category.server_descendent_count = Some(1);
    let mut library_root_category = category(library, 101, None, "library");
    library_root_category.server_version = ServerVersion::from_raw(1);
    library_root_category.server_descendent_count = Some(0);
    let mut inventory = Inventory::new(personal_root_category, Some(library_root_category), InventoryLimits::default())
        .expect("valid personal and Library roots");
    let mut child = category(personal, 2, Some(1), "child");
    child.server_version = ServerVersion::from_raw(1);
    child.server_descendent_count = Some(1);
    inventory.upsert_category(child).expect("add child folder");
    let mut grandchild = category(personal, 3, Some(2), "grandchild");
    grandchild.server_version = ServerVersion::from_raw(1);
    grandchild.server_descendent_count = Some(1);
    inventory.upsert_category(grandchild).expect("add grandchild folder");
    let mut deepest = category(personal, 4, Some(3), "deepest");
    deepest.server_version = ServerVersion::from_raw(1);
    deepest.server_descendent_count = Some(0);
    let deepest_key = deepest.key;
    inventory.upsert_category(deepest).expect("add deepest folder");

    let complete = inventory.snapshot();
    let child_key = CategoryKey::new(personal, FolderId::new(uuid(2)));
    let grandchild_key = CategoryKey::new(personal, FolderId::new(uuid(3)));
    assert!([personal_root, child_key, grandchild_key, deepest_key].into_iter().all(|folder| complete.is_complete(folder)));
    assert!(complete.is_complete(library_root));
    let stale = complete
        .prepare_folder_fetch(
            personal_root,
            ServerVersion::from_raw(2),
            Some(1),
            vec![InventoryChild::Category(complete.category(child_key).expect("child remains present").clone())],
        )
        .expect("prepare root response before loss");

    inventory.invalidate_owner(personal).expect("advance personal refresh generation");
    let invalidated = inventory.snapshot();
    assert_eq!(invalidated.owner_refresh_generation(personal), complete.owner_refresh_generation(personal) + 1);
    assert!([personal_root, child_key, grandchild_key, deepest_key].into_iter().all(|folder| !invalidated.is_complete(folder)));
    assert!(invalidated.is_complete(library_root));
    assert_eq!(invalidated.incomplete_category_count(personal), 4);
    assert_eq!(invalidated.incomplete_category_count(library), 0);
    assert!(invariant(&invalidated).is_none());
    assert_matches!(inventory.commit_folder_fetch(stale), Ok(inventory::FolderFetchCommit::StaleBase(_)));

    let mut newer_metadata = invalidated.category(deepest_key).expect("deep folder remains present").clone();
    newer_metadata.server_version = ServerVersion::from_raw(2);
    inventory.upsert_category(newer_metadata).expect("apply newer metadata after coverage loss");
    assert!(!inventory.snapshot().is_complete(deepest_key));

    apply_fetch(&mut inventory, deepest_key, ServerVersion::from_raw(3), Some(0), Vec::new()).expect("reconcile deepest folder");
    let reconciled = inventory.snapshot();
    assert!(reconciled.is_complete(deepest_key));
    assert!([personal_root, child_key, grandchild_key].into_iter().all(|folder| !reconciled.is_complete(folder)));
    assert_eq!(reconciled.incomplete_category_count(personal), 3);
    inventory.invalidate_owner(library).expect("advance Library refresh generation");
    assert!(!inventory.snapshot().is_complete(library_root));
    assert!(invariant(&inventory.snapshot()).is_none());
}

fn invariant(snapshot: &inventory::InventorySnapshot) -> Option<&'static str> {
    let roots = snapshot.roots();
    let categories = snapshot.categories().collect::<Vec<_>>();
    let mut expected_children = HashSet::new();
    let mut owners = HashSet::new();
    let mut categories_by_type = HashMap::<(InventoryOwner, FolderType), Vec<CategoryKey>>::new();
    for category in &categories {
        owners.insert(category.key.owner());
        categories_by_type.entry((category.key.owner(), category.preferred_folder_type)).or_default().push(category.key);
        if roots.contains(category.key) {
            if category.parent.is_some() {
                return Some("root has a parent");
            }
        } else {
            let Some(parent) = category.parent else { return Some("category has no parent") };
            if parent.owner() != category.key.owner() || snapshot.category(parent).is_none() {
                return Some("category parent is missing");
            }
            if !expected_children.insert((parent, InventoryNodeId::Category(category.key.id()))) {
                return Some("category is indexed twice");
            }
        }
        let mut path = HashSet::new();
        let mut current = category.key;
        loop {
            if !path.insert(current) {
                return Some("category parent chain cycles");
            }
            let Some(current_category) = snapshot.category(current) else { return Some("parent chain has a missing category") };
            match current_category.parent {
                Some(parent) => current = parent,
                None if roots.contains(current) => break,
                None => return Some("parent chain does not reach a root"),
            }
        }
        let expected_complete = category.server_version.is_known()
            && category.server_descendent_count.and_then(|count| usize::try_from(count).ok()) == Some(snapshot.child_count(category.key));
        if snapshot.is_complete(category.key) && !expected_complete {
            return Some("complete category has inconsistent version or child count");
        }
    }
    for item in snapshot.items() {
        if item.parent.owner() != item.key.owner() || snapshot.category(item.parent).is_none() {
            return Some("item parent is missing");
        }
        if !expected_children.insert((item.parent, InventoryNodeId::Item(item.key.id()))) {
            return Some("item is indexed twice");
        }
    }
    for category in categories {
        let actual = snapshot.children(category.key).map(|node| (category.key, node.id())).collect::<HashSet<_>>();
        let expected = expected_children.iter().copied().filter(|(parent, _)| *parent == category.key).collect::<HashSet<_>>();
        if actual != expected {
            return Some("child index disagrees with parent links");
        }
    }
    for owner in owners {
        let expected_incomplete = snapshot.categories_for(owner).filter(|category| !snapshot.is_complete(category.key)).count();
        if snapshot.incomplete_category_count(owner) != expected_incomplete {
            return Some("incomplete-category count disagrees with a full recount");
        }
        if snapshot.owner_is_complete(owner) != (expected_incomplete == 0) {
            return Some("owner completeness disagrees with a full recount");
        }
    }
    for ((owner, folder_type), mut keys) in categories_by_type {
        keys.sort_unstable();
        if snapshot.category_with_type(owner, folder_type).map(|category| category.key) != keys.first().copied() {
            return Some("preferred folder-type index disagrees with categories");
        }
    }
    None
}

#[test]
fn folder_generations_follow_direct_changes() {
    let owner = owner(16);
    let mut inventory = inventory(owner);
    let root = inventory.snapshot().roots().personal();
    let first = category(owner, 2, Some(1), "first");
    let second = category(owner, 3, Some(1), "second");
    let first_key = first.key;
    let second_key = second.key;
    inventory.upsert_category(first.clone()).expect("add first folder");
    inventory.upsert_category(second).expect("add second folder");

    let before_category_change = inventory.snapshot();
    let mut renamed = first;
    renamed.name = Arc::from("renamed");
    inventory.upsert_category(renamed).expect("rename first folder");
    let after_category_change = inventory.snapshot();
    assert_eq!(after_category_change.folder_generation(first_key), Some(after_category_change.revision()));
    assert_eq!(after_category_change.folder_generation(root), Some(after_category_change.revision()));
    assert_eq!(after_category_change.folder_generation(second_key), before_category_change.folder_generation(second_key));

    let before_item_change = inventory.snapshot();
    inventory.upsert_item(item(owner, 4, 2, AssetType::Notecard, 5)).expect("add item to first folder");
    let after_item_change = inventory.snapshot();
    assert_eq!(after_item_change.folder_generation(first_key), Some(after_item_change.revision()));
    assert_eq!(after_item_change.folder_generation(root), before_item_change.folder_generation(root));
    assert_eq!(after_item_change.folder_generation(second_key), before_item_change.folder_generation(second_key));
}

#[test]
fn item_move_stamps_both_folders() {
    let owner = owner(17);
    let mut inventory = inventory(owner);
    let root = inventory.snapshot().roots().personal();
    let first = CategoryKey::new(owner, FolderId::new(uuid(2)));
    let second = CategoryKey::new(owner, FolderId::new(uuid(3)));
    inventory.upsert_category(category(owner, 2, Some(1), "first")).expect("add first folder");
    inventory.upsert_category(category(owner, 3, Some(1), "second")).expect("add second folder");
    inventory.upsert_item(item(owner, 4, 2, AssetType::Notecard, 5)).expect("add item");
    let before_move = inventory.snapshot();

    inventory.move_node(InventoryNodeKey::new(owner, InventoryNodeId::Item(ItemId::new(uuid(4)))), second).expect("move item");

    let after_move = inventory.snapshot();
    assert_eq!(after_move.folder_generation(first), Some(after_move.revision()));
    assert_eq!(after_move.folder_generation(second), Some(after_move.revision()));
    assert_eq!(after_move.folder_generation(root), before_move.folder_generation(root));
}

#[test]
fn deleted_generation_is_not_reused() {
    let owner = owner(18);
    let mut inventory = inventory(owner);
    let folder = category(owner, 2, Some(1), "folder");
    let key = folder.key;
    inventory.upsert_category(folder.clone()).expect("add folder");
    let old_generation = inventory.snapshot().folder_generation(key).expect("folder generation exists");

    inventory.remove_subtree(owner, key.id()).expect("remove folder");
    assert_eq!(inventory.snapshot().folder_generation(key), None);

    inventory.upsert_category(folder).expect("recreate folder");
    let new_snapshot = inventory.snapshot();
    let new_generation = new_snapshot.folder_generation(key).expect("recreated folder has a generation");
    assert_ne!(new_generation, old_generation);
    assert_eq!(new_generation, new_snapshot.revision());
}

#[test]
fn failed_and_noop_keep_generations() {
    let owner = owner(19);
    let mut inventory = inventory(owner);
    let root = inventory.snapshot().roots().personal();
    let before = inventory.snapshot();

    assert_eq!(
        apply_fetch(
            &mut inventory,
            root,
            ServerVersion::UNKNOWN,
            None,
            vec![InventoryChild::Item(item(owner, 4, 99, AssetType::Notecard, 5))],
        ),
        Err(InventoryError::InvalidParent)
    );
    assert_eq!(inventory.snapshot().folder_generation(root), before.folder_generation(root));
    assert_eq!(inventory.revision(), before.revision());

    let no_op = inventory.upsert_category(category(owner, 1, None, "root")).expect("same root is a no-op");
    assert!(no_op.is_empty());
    assert_eq!(inventory.snapshot().folder_generation(root), before.folder_generation(root));
    assert_eq!(inventory.revision(), before.revision());
}

#[test]
fn fetch_and_version_stamp_folder() {
    let owner = owner(20);
    let mut inventory = inventory(owner);
    let root = inventory.snapshot().roots().personal();
    let before_fetch = inventory.snapshot();

    let changes = apply_fetch(&mut inventory, root, ServerVersion::UNKNOWN, None, Vec::new()).expect("empty folder fetch is valid");
    let after_fetch = inventory.snapshot();
    assert_eq!(changes.revision, after_fetch.revision());
    assert_eq!(after_fetch.folder_generation(root), Some(after_fetch.revision()));
    assert_ne!(after_fetch.revision(), before_fetch.revision());

    inventory.set_version(root, ServerVersion::from_raw(7)).expect("set server version");
    let after_version = inventory.snapshot();
    assert_eq!(after_version.folder_generation(root), Some(after_version.revision()));
    assert_ne!(after_version.revision(), after_fetch.revision());
}

#[test]
fn cycle_attempt_keeps_state() {
    let owner = owner(9);
    let mut inventory = inventory(owner);
    let root = inventory.roots().personal();
    let first = category(owner, 2, Some(1), "first");
    let second = category(owner, 3, Some(2), "second");
    let first_key = first.key;
    let second_key = second.key;
    inventory.upsert_category(first).expect("add first category");
    inventory.upsert_category(second).expect("add second category");
    let before = inventory.snapshot();

    let result = inventory.move_node(
        InventoryNodeKey::new(owner, InventoryNodeId::Category(FolderId::new(uuid(2)))),
        CategoryKey::new(owner, FolderId::new(uuid(3))),
    );

    assert_eq!(result, Err(InventoryError::Cycle));
    assert_eq!(inventory.revision(), before.revision());
    assert!(before.category(root).is_some());
    let after = inventory.snapshot();
    assert_eq!(after.category(first_key).and_then(|category| category.parent), Some(root));
    assert_eq!(after.category(second_key).and_then(|category| category.parent), Some(first_key));
    assert_eq!(invariant(&after), None);
}

#[test]
fn removed_id_can_be_reused() {
    let owner = owner(10);
    let mut inventory = inventory(owner);
    let category_key = CategoryKey::new(owner, FolderId::new(uuid(2)));
    let item_key = ItemKey::new(owner, ItemId::new(uuid(3)));
    inventory.upsert_category(category(owner, 2, Some(1), "old")).expect("add category");
    inventory.upsert_item(item(owner, 3, 2, AssetType::Notecard, 4)).expect("add item");

    inventory.remove_subtree(owner, category_key.id()).expect("remove subtree");

    assert!(inventory.snapshot().category(category_key).is_none());
    assert!(inventory.snapshot().item(item_key).is_none());
    inventory.upsert_category(category(owner, 2, Some(1), "new")).expect("reuse category ID");
    inventory.upsert_item(item(owner, 3, 2, AssetType::Notecard, 5)).expect("reuse item ID");

    let snapshot = inventory.snapshot();
    assert_eq!(snapshot.category(category_key).map(|category| category.name.as_ref()), Some("new"));
    assert!(snapshot.item(item_key).is_some());
    assert_eq!(invariant(&snapshot), None);
}

#[test]
fn fetch_replaces_direct_children() {
    let owner = owner(11);
    let mut inventory = inventory(owner);
    let root = inventory.roots().personal();
    let folder = CategoryKey::new(owner, FolderId::new(uuid(2)));
    let retained = ItemKey::new(owner, ItemId::new(uuid(3)));
    let removed = ItemKey::new(owner, ItemId::new(uuid(4)));
    inventory.upsert_category(category(owner, 2, Some(1), "folder")).expect("add folder");
    inventory.upsert_item(item(owner, 3, 2, AssetType::Notecard, 30)).expect("add retained item");
    inventory.upsert_item(item(owner, 4, 2, AssetType::Notecard, 40)).expect("add stale item");

    let changes = apply_fetch(
        &mut inventory,
        folder,
        ServerVersion::from_raw(8),
        Some(2),
        vec![
            InventoryChild::Item(item(owner, 3, 2, AssetType::Notecard, 31)),
            InventoryChild::Category(category(owner, 5, Some(2), "added")),
        ],
    )
    .expect("apply folder result");

    let snapshot = inventory.snapshot();
    assert!(changes.changes.len() >= 3);
    assert!(snapshot.item(retained).is_some());
    assert!(snapshot.item(removed).is_none());
    assert!(snapshot.category(CategoryKey::new(owner, FolderId::new(uuid(5)))).is_some());
    assert!(!snapshot.is_complete(root));
    assert!(snapshot.is_complete(folder));
    assert_eq!(invariant(&snapshot), None);
}

#[test]
fn prepared_delete_rejects_live_revision() {
    let owner = owner(27);
    let root = category(owner, 1, None, "root");
    let mut target = category(owner, 2, Some(1), "target");
    target.server_version = ServerVersion::from_raw(7);
    target.server_descendent_count = Some(1);
    let folder = target.key;
    let mut descendants = vec![target];
    let mut parent = 2;
    for id in 3..=4_098 {
        descendants.push(category(owner, id, Some(parent), "nested"));
        parent = id;
    }
    let mut inventory = Inventory::from_categories(root, None, descendants, InventoryLimits::default()).expect("large subtree is valid");
    let snapshot = inventory.snapshot();
    let (started_tx, started_rx) = mpsc::sync_channel(0);
    let (continue_tx, continue_rx) = mpsc::sync_channel(0);
    let worker = thread::spawn(move || {
        started_tx.send(()).expect("test is waiting for the worker");
        continue_rx.recv().expect("test releases the worker");
        snapshot.prepare_folder_fetch(folder, ServerVersion::from_raw(8), Some(0), Vec::new())
    });
    started_rx.recv().expect("worker reached the controlled delay");

    inventory
        .apply_delta(InventoryDelta { updated_category_versions: vec![(folder, ServerVersion::from_raw(9))], ..InventoryDelta::default() })
        .expect("live version update applies");
    continue_tx.send(()).expect("worker is waiting to prepare");
    let prepared = worker.join().expect("worker completes preparation").expect("fetch transaction prepares");

    assert_matches!(inventory.commit_folder_fetch(prepared), Ok(inventory::FolderFetchCommit::StaleBase(_)));
    let snapshot = inventory.snapshot();
    assert_eq!(snapshot.category(folder).map(|category| category.server_version), Some(ServerVersion::from_raw(9)));
    assert_eq!(snapshot.categories().count(), 4_098);
    assert_eq!(invariant(&snapshot), None);

    let prepared = snapshot
        .prepare_folder_fetch(folder, ServerVersion::from_raw(10), Some(0), Vec::new())
        .expect("next fetch prepares against the latest revision");
    let inventory::FolderFetchCommit::Applied(_) = inventory.commit_folder_fetch(prepared).expect("next fetch commits") else {
        panic!("latest-base transaction was rejected");
    };
    assert_eq!(inventory.snapshot().categories().count(), 2);
    assert_eq!(invariant(&inventory.snapshot()), None);
}

#[test]
fn cancelled_preparation_allows_next_fetch() {
    let owner = owner(28);
    let root = category(owner, 1, None, "root");
    let mut target = category(owner, 2, Some(1), "target");
    target.server_version = ServerVersion::from_raw(7);
    target.server_descendent_count = Some(1);
    let folder = target.key;
    let mut inventory = Inventory::from_categories(root, None, vec![target], InventoryLimits::default()).expect("valid subtree");
    let before = inventory.snapshot();
    let cancellation = inventory::InventoryPreparationCancellation::default();
    cancellation.cancel();

    let result = before.prepare_folder_fetch_cancellable(folder, ServerVersion::from_raw(8), Some(0), Vec::new(), &cancellation);
    assert_eq!(result.err(), Some(InventoryError::Cancelled));
    assert_eq!(inventory.snapshot().revision(), before.revision());
    assert_eq!(inventory.snapshot().category_count(), 2);

    let changes = apply_fetch(&mut inventory, folder, ServerVersion::from_raw(9), Some(0), Vec::new()).expect("later fetch can commit");
    assert!(!changes.is_empty());
    assert_eq!(inventory.snapshot().category_count(), 2);
    assert!(inventory.snapshot().is_complete(folder));
}

#[test]
fn fetch_keeps_other_parents_items() {
    let owner = owner(26);
    let mut inventory = inventory(owner);
    let source = CategoryKey::new(owner, FolderId::new(uuid(2)));
    let target_parent = CategoryKey::new(owner, FolderId::new(uuid(3)));
    let target = ItemKey::new(owner, ItemId::new(uuid(4)));
    let sibling = ItemKey::new(owner, ItemId::new(uuid(5)));
    let link = ItemKey::new(owner, ItemId::new(uuid(6)));
    inventory.upsert_category(category(owner, 2, Some(1), "source")).expect("add source folder");
    inventory.upsert_category(category(owner, 3, Some(1), "target parent")).expect("add target parent");
    inventory.upsert_item(item(owner, 4, 3, AssetType::Notecard, 40)).expect("add target");
    inventory.upsert_item(item(owner, 5, 3, AssetType::Notecard, 50)).expect("add target sibling");
    let mut linked_item = item(owner, 6, 2, AssetType::Link, 4);
    linked_item.inventory_type = InventoryType::Unknown(24);

    apply_fetch(&mut inventory, source, ServerVersion::from_raw(8), None, vec![InventoryChild::Item(linked_item)])
        .expect("apply direct source child");

    let snapshot = inventory.snapshot();
    assert_eq!(snapshot.item(target).map(|item| item.parent), Some(target_parent));
    assert_eq!(snapshot.item(sibling).map(|item| item.parent), Some(target_parent));
    assert_eq!(
        snapshot.link_resolution(link),
        Some(LinkResolution::Resolved(InventoryNodeKey::new(owner, InventoryNodeId::Item(target.id()))))
    );
    assert_eq!(snapshot.child_count(target_parent), 2);
    assert!(!snapshot.is_complete(source));
    assert_eq!(invariant(&snapshot), None);
}

#[test]
fn fetch_replaces_items_at_capacity() {
    let owner = owner(16);
    let limits = InventoryLimits { max_categories: 2, max_items: 1, max_folder_fetch_children: 1 };
    let mut inventory = inventory_with_limits(owner, limits);
    let folder = CategoryKey::new(owner, FolderId::new(uuid(2)));
    inventory.upsert_category(category(owner, 2, Some(1), "folder")).expect("add folder");
    apply_fetch(
        &mut inventory,
        folder,
        ServerVersion::UNKNOWN,
        Some(1),
        vec![InventoryChild::Item(item(owner, 3, 2, AssetType::Notecard, 30))],
    )
    .expect("fill item capacity");

    apply_fetch(
        &mut inventory,
        folder,
        ServerVersion::from_raw(1),
        Some(1),
        vec![InventoryChild::Item(item(owner, 5, 2, AssetType::Notecard, 50))],
    )
    .expect("replace item at capacity");
    let snapshot = inventory.snapshot();
    assert_eq!(snapshot.items().count(), limits.max_items);
    assert!(snapshot.item(ItemKey::new(owner, ItemId::new(uuid(3)))).is_none());
    assert!(snapshot.item(ItemKey::new(owner, ItemId::new(uuid(5)))).is_some());
    assert!(snapshot.is_complete(folder));
    assert_eq!(invariant(&snapshot), None);
}

#[test]
fn fetch_replaces_categories_at_capacity() {
    let owner = owner(17);
    let limits = InventoryLimits { max_categories: 2, max_items: 0, max_folder_fetch_children: 1 };
    let mut inventory = inventory_with_limits(owner, limits);
    let root = inventory.roots().personal();
    apply_fetch(&mut inventory, root, ServerVersion::UNKNOWN, Some(1), vec![InventoryChild::Category(category(owner, 2, Some(1), "old"))])
        .expect("fill category capacity");

    apply_fetch(
        &mut inventory,
        root,
        ServerVersion::from_raw(1),
        Some(1),
        vec![InventoryChild::Category(category(owner, 3, Some(1), "new"))],
    )
    .expect("replace category at capacity");
    let snapshot = inventory.snapshot();
    assert_eq!(snapshot.categories().count(), limits.max_categories);
    assert!(snapshot.category(CategoryKey::new(owner, FolderId::new(uuid(2)))).is_none());
    assert!(snapshot.category(CategoryKey::new(owner, FolderId::new(uuid(3)))).is_some());
    assert!(snapshot.is_complete(root));
    assert_eq!(invariant(&snapshot), None);
}

#[test]
fn fetch_keeps_moved_category() {
    let owner = owner(18);
    let mut inventory = inventory(owner);
    let target = CategoryKey::new(owner, FolderId::new(uuid(2)));
    let moved = CategoryKey::new(owner, FolderId::new(uuid(4)));
    let retained_item = ItemKey::new(owner, ItemId::new(uuid(5)));
    inventory.upsert_category(category(owner, 2, Some(1), "target")).expect("add target folder");
    inventory.upsert_category(category(owner, 3, Some(2), "stale")).expect("add stale folder");
    inventory.upsert_category(category(owner, 4, Some(3), "moved")).expect("add nested incoming folder");
    inventory.upsert_item(item(owner, 5, 4, AssetType::Notecard, 50)).expect("add descendant item");

    apply_fetch(
        &mut inventory,
        target,
        ServerVersion::from_raw(1),
        Some(1),
        vec![InventoryChild::Category(category(owner, 4, Some(2), "moved"))],
    )
    .expect("move incoming folder out before removing the stale subtree");

    let snapshot = inventory.snapshot();
    assert!(snapshot.category(CategoryKey::new(owner, FolderId::new(uuid(3)))).is_none());
    assert_eq!(snapshot.category(moved).and_then(|category| category.parent), Some(target));
    assert_eq!(snapshot.item(retained_item).map(|item| item.parent), Some(moved));
    assert_eq!(invariant(&snapshot), None);
}

#[test]
fn snapshots_keep_prior_state() {
    let owner = owner(12);
    let mut inventory = inventory(owner);
    let old = inventory.snapshot();
    inventory.upsert_item(item(owner, 2, 1, AssetType::Notecard, 3)).expect("add item");
    let current = inventory.snapshot();

    assert_eq!(old.revision().get(), 0);
    assert_eq!(old.child_count(inventory.roots().personal()), 0);
    assert_eq!(current.child_count(inventory.roots().personal()), 1);
    assert_eq!(invariant(&old), None);
    assert_eq!(invariant(&current), None);
}

#[test]
fn links_distinguish_unloaded_and_broken() {
    let owner = owner(13);
    let mut inventory = inventory(owner);
    let root = inventory.roots().personal();
    let link = item(owner, 2, 1, AssetType::Link, 99);
    let link_key = link.key;
    inventory.upsert_item(link.clone()).expect("add link");
    assert_eq!(inventory.snapshot().link_resolution(link_key), Some(LinkResolution::NotLoaded(uuid(99))));

    apply_fetch(&mut inventory, root, ServerVersion::from_raw(1), Some(1), vec![InventoryChild::Item(link)]).expect("complete root fetch");

    assert_eq!(inventory.snapshot().link_resolution(link_key), Some(LinkResolution::Broken(uuid(99))));
}

#[test]
fn folder_types_preserve_unknowns() {
    assert_eq!(FolderType::from_raw(23), FolderType::Favorite);
    assert_eq!(FolderType::from_raw(33).raw(), 33);
    assert!(FolderType::from_raw(999).is_protected());
    assert_eq!(AssetType::from_raw(24), AssetType::Link);
    assert_eq!(AssetType::from_raw(999).raw(), 999);
    assert_eq!(InventoryType::from_raw(999).raw(), 999);
    assert_eq!(ServerVersion::from_raw(-1).raw(), -1);
    assert!(ServerVersion::known(-1).is_none());
    assert_eq!(SaleType::from_raw(999).raw(), 999);
}

#[test]
fn asset_type_query_spans_roots() {
    let personal_owner = owner(21);
    let library_owner = owner(22);
    let personal_root = category(personal_owner, 1, None, "personal");
    let library_root = category(library_owner, 2, None, "library");
    let mut inventory = Inventory::new(personal_root, Some(library_root), InventoryLimits::default()).expect("valid roots");
    let personal_texture = item(personal_owner, 3, 1, AssetType::Texture, 30);
    let library_texture = item(library_owner, 4, 2, AssetType::Texture, 40);
    let other_type = item(personal_owner, 5, 1, AssetType::Sound, 50);
    let personal_key = personal_texture.key;
    let library_key = library_texture.key;
    let roots = inventory.snapshot().roots();

    apply_fetch(
        &mut inventory,
        roots.personal(),
        ServerVersion::from_raw(1),
        Some(2),
        vec![InventoryChild::Item(personal_texture), InventoryChild::Item(other_type)],
    )
    .expect("personal children");
    apply_fetch(
        &mut inventory,
        roots.library().expect("Library root"),
        ServerVersion::from_raw(1),
        Some(1),
        vec![InventoryChild::Item(library_texture)],
    )
    .expect("Library children");

    let mut matching = inventory.snapshot().items_of_asset_type(AssetType::Texture).map(|item| item.key).collect::<Vec<_>>();
    matching.sort_unstable();

    assert_eq!(matching, [personal_key, library_key]);
}

#[test]
fn folder_type_index_tracks_changes() {
    let owner = owner(15);
    let mut inventory = inventory(owner);
    let mut first = category(owner, 2, Some(1), "first");
    let mut second = category(owner, 3, Some(1), "second");
    first.preferred_folder_type = FolderType::Favorite;
    second.preferred_folder_type = FolderType::Favorite;
    let first_key = first.key;
    let second_key = second.key;
    inventory.upsert_category(first.clone()).expect("add first favorite folder");
    inventory.upsert_category(second.clone()).expect("add second favorite folder");
    assert_eq!(inventory.snapshot().category_with_type(owner, FolderType::Favorite).map(|category| category.key), Some(first_key));

    first.preferred_folder_type = FolderType::Unknown(71);
    inventory.upsert_category(first).expect("change the first folder type");
    assert_eq!(inventory.snapshot().category_with_type(owner, FolderType::Favorite).map(|category| category.key), Some(second_key));
    assert_eq!(inventory.snapshot().category_with_type(owner, FolderType::Unknown(71)).map(|category| category.key), Some(first_key));

    inventory.remove_subtree(owner, second_key.id()).expect("remove second folder");
    assert!(inventory.snapshot().category_with_type(owner, FolderType::Favorite).is_none());
    inventory.upsert_category(second).expect("reuse removed folder ID");
    assert_eq!(inventory.snapshot().category_with_type(owner, FolderType::Favorite).map(|category| category.key), Some(second_key));
    assert_eq!(invariant(&inventory.snapshot()), None);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 72, ..ProptestConfig::default() })]

    #[test]
    fn sequences_preserve_invariants(operations in collection::vec((0_u8..9, 0_u8..8, 0_u8..8, 0_u8..8), 0..72)) {
        let owner = owner(14);
        let limits = InventoryLimits { max_categories: 4, max_items: 3, max_folder_fetch_children: 4 };
        let mut inventory = inventory_with_limits(owner, limits);
        for id in 1..=3 {
            let parent = if id == 2 { 101 } else { 1 };
            inventory.upsert_category(category(owner, 100 + id, Some(parent), "capacity-category")).expect("fill category capacity");
            prop_assert_eq!(invariant(&inventory.snapshot()), None);
            inventory
                .upsert_item(item(owner, 200 + id, 1, AssetType::Notecard, 300 + id))
                .expect("fill item capacity");
            prop_assert_eq!(invariant(&inventory.snapshot()), None);
        }
        prop_assert_eq!(inventory.snapshot().categories().count(), limits.max_categories);
        prop_assert_eq!(inventory.snapshot().items().count(), limits.max_items);
        let category_revision = inventory.revision();
        prop_assert_eq!(
            inventory.upsert_category(category(owner, 999, Some(1), "over capacity")),
            Err(InventoryError::TooManyCategories)
        );
        prop_assert_eq!(inventory.revision(), category_revision);
        prop_assert_eq!(invariant(&inventory.snapshot()), None);
        let item_revision = inventory.revision();
        prop_assert_eq!(
            inventory.upsert_item(item(owner, 999, 1, AssetType::Notecard, 999)),
            Err(InventoryError::TooManyItems)
        );
        prop_assert_eq!(inventory.revision(), item_revision);
        prop_assert_eq!(invariant(&inventory.snapshot()), None);
        let root = inventory.snapshot().roots().personal();
        let a_key = CategoryKey::new(owner, FolderId::new(uuid(101)));
        let b_key = CategoryKey::new(owner, FolderId::new(uuid(102)));
        let snapshot = inventory.snapshot();
        let mut moved_a = snapshot.category(a_key).expect("A exists").clone();
        let mut moved_b = snapshot.category(b_key).expect("B exists").clone();
        moved_a.parent = Some(b_key);
        moved_b.parent = Some(root);
        inventory
            .apply_delta(InventoryDelta { updated_categories: vec![moved_a, moved_b], ..InventoryDelta::default() })
            .expect("property sequence starts with the valid root-B-A move");
        prop_assert_eq!(invariant(&inventory.snapshot()), None);
        for (operation, a, b, c) in operations {
            let before = inventory.snapshot();
            let before_revision = before.revision();
            let before_category_count = before.categories().count();
            let before_item_count = before.items().count();
            let before_root_child_count = before.child_count(before.roots().personal());
            let category_id = 100 + u128::from(a);
            let parent_id = 100 + u128::from(b);
            let item_id = 200 + u128::from(a);
            let result = match operation {
                0 => {
                    let parent = if c % 2 == 0 { 1 } else { parent_id };
                    let mut category = category(owner, category_id, Some(parent), &format!("folder-{a}-{c}"));
                    category.preferred_folder_type = FolderType::from_raw(i32::from(c % 4));
                    inventory.upsert_category(category)
                }
                1 => {
                    let parent = if c % 2 == 0 { 1 } else { parent_id };
                    inventory.upsert_item(item(owner, item_id, parent, AssetType::Notecard, 300 + u128::from(c)))
                }
                2 => {
                    let parent = if c % 2 == 0 { 1 } else { parent_id };
                    inventory.move_node(
                        InventoryNodeKey::new(owner, InventoryNodeId::Category(FolderId::new(uuid(category_id)))),
                        CategoryKey::new(owner, FolderId::new(uuid(parent))),
                    )
                }
                3 => {
                    let parent = if c % 2 == 0 { 1 } else { parent_id };
                    inventory.move_node(
                        InventoryNodeKey::new(owner, InventoryNodeId::Item(ItemId::new(uuid(item_id)))),
                        CategoryKey::new(owner, FolderId::new(uuid(parent))),
                    )
                }
                4 => inventory.remove_subtree(owner, FolderId::new(uuid(category_id))),
                5 => {
                    let folder = CategoryKey::new(owner, FolderId::new(uuid(category_id)));
                    apply_fetch(&mut inventory,
                        folder,
                        ServerVersion::from_raw(i32::from(c)),
                        Some(2),
                        vec![
                            InventoryChild::Category(category(owner, parent_id, Some(category_id), "fetch-child")),
                            InventoryChild::Item(item(owner, 400 + u128::from(c), category_id, AssetType::Notecard, 500 + u128::from(c))),
                        ],
                    )
                }
                6 | 7 => inventory.set_version(CategoryKey::new(owner, FolderId::new(uuid(category_id))), ServerVersion::UNKNOWN),
                8 => {
                    let snapshot = inventory.snapshot();
                    let a_key = CategoryKey::new(owner, FolderId::new(uuid(101)));
                    let b_key = CategoryKey::new(owner, FolderId::new(uuid(102)));
                    match (snapshot.category(a_key), snapshot.category(b_key)) {
                        (Some(a), Some(b)) => {
                            let mut a = a.clone();
                            let mut b = b.clone();
                            if a.parent == Some(b_key) && b.parent == Some(snapshot.roots().personal()) {
                                a.parent = Some(snapshot.roots().personal());
                                b.parent = Some(a_key);
                            } else {
                                a.parent = Some(b_key);
                                b.parent = Some(snapshot.roots().personal());
                            }
                            inventory.apply_delta(InventoryDelta { updated_categories: vec![a, b], ..InventoryDelta::default() })
                        }
                        _ => inventory.apply_delta(InventoryDelta::default()),
                    }
                }
                _ => inventory.apply_delta(InventoryDelta::default()),
            };
            match result {
                Ok(changes) => prop_assert_eq!(changes.revision, inventory.revision()),
                Err(_) => prop_assert_eq!(before_revision, inventory.revision()),
            }
            prop_assert_eq!(before.revision(), before_revision);
            prop_assert_eq!(before.categories().count(), before_category_count);
            prop_assert_eq!(before.items().count(), before_item_count);
            prop_assert_eq!(before.child_count(before.roots().personal()), before_root_child_count);
            prop_assert_eq!(invariant(&before), None);
            prop_assert_eq!(invariant(&inventory.snapshot()), None);
        }
    }
}
