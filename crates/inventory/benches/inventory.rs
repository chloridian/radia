use std::{
    collections::HashSet,
    hint::black_box,
    path::{Path, PathBuf},
    sync::Arc,
};

use futures::executor::block_on;
use inventory::{
    AssetType, Category, CategoryKey, Inventory, InventoryCache, InventoryCacheLoad, InventoryChild, InventoryDelta, InventoryLimits,
    InventoryOwner, InventoryType, Item, ItemKey, PermissionMask, Permissions, SaleInfo, SaleType, ServerVersion,
};
use tempfile::tempdir;
use utils::{AssetId, AvatarId, FolderId, GroupId, ItemId};
use uuid::Uuid;

const ENTRY_COUNT: usize = 100_000;
const OWNER_ID: u128 = 77;
const ROOT_ID: u128 = 1;

fn prepared_fetch_fixture(replace: bool) -> (Inventory, inventory::InventorySnapshot, inventory::PreparedFolderFetch) {
    let inventory = item_inventory();
    let before = inventory.snapshot();
    let children = if replace { replacement_item_children() } else { Vec::new() };
    let prepared = before
        .prepare_folder_fetch(
            before.roots().personal(),
            ServerVersion::from_raw(2),
            Some(if replace { ENTRY_COUNT as u32 } else { 0 }),
            children,
        )
        .expect("benchmark transaction should prepare");
    (inventory, before, prepared)
}

fn populate_cache(root: &Path) -> PathBuf {
    let mut inventory = root_inventory();
    let before = inventory.snapshot();
    let folder = inventory.roots().personal();
    let changes = apply_fetch(&mut inventory, folder, ServerVersion::from_raw(1), Some(ENTRY_COUNT as u32), item_children())
        .expect("benchmark cache fixture should apply");
    let after = inventory.snapshot();
    let (cache, _) = InventoryCache::open_at_root(root, "agni", owner().id(), None).expect("benchmark cache create");
    block_on(cache.submit_bootstrap(&before).expect("benchmark cache bootstrap").wait()).expect("benchmark bootstrap write");
    block_on(cache.submit_changeset(&changes, &before, &after).expect("benchmark cache batch").wait()).expect("benchmark cache write");
    let path = cache.path().to_path_buf();
    block_on(cache.close()).expect("benchmark initial cache close");
    path
}

fn apply_restore(load: InventoryCacheLoad, inventory: &mut Inventory, skeleton: &inventory::InventorySnapshot) -> usize {
    let mut restore = load.prepare_restore(skeleton).expect("benchmark cache restore preparation");
    let mut applied = 0usize;
    loop {
        let progress = restore.apply_next(inventory, &HashSet::new()).expect("benchmark cache restore batch");
        applied = applied.saturating_add(progress.units_applied);
        if progress.finished {
            return applied;
        }
    }
}

fn cache_skeleton() -> Inventory {
    let root = Category {
        key: category_key(ROOT_ID),
        parent: None,
        preferred_folder_type: inventory::FolderType::RootInventory,
        name: Arc::from("root"),
        server_version: ServerVersion::from_raw(1),
        server_descendent_count: Some(ENTRY_COUNT as u32),
    };
    Inventory::new(root, None, InventoryLimits::default()).expect("benchmark skeleton should be valid")
}

fn one_item_deltas() -> (Inventory, InventoryDelta, InventoryDelta) {
    let inventory = item_inventory();
    let original = inventory.snapshot().item(item(0).key).expect("benchmark item should exist").clone();
    let mut changed_a = original.clone();
    changed_a.name = Arc::from("delta-a");
    let mut changed_b = original;
    changed_b.name = Arc::from("delta-b");
    (
        inventory,
        InventoryDelta { updated_items: vec![changed_a], ..InventoryDelta::default() },
        InventoryDelta { updated_items: vec![changed_b], ..InventoryDelta::default() },
    )
}

fn root_inventory() -> Inventory {
    let root = Category {
        key: category_key(ROOT_ID),
        parent: None,
        preferred_folder_type: inventory::FolderType::RootInventory,
        name: Arc::from("root"),
        server_version: ServerVersion::UNKNOWN,
        server_descendent_count: None,
    };
    Inventory::new(root, None, InventoryLimits::default()).expect("benchmark root should be valid")
}

fn item_inventory() -> Inventory {
    let mut inventory = root_inventory();
    let folder = inventory.roots().personal();
    let changes = apply_fetch(&mut inventory, folder, ServerVersion::from_raw(1), Some(ENTRY_COUNT as u32), item_children())
        .expect("benchmark items should apply");
    drop(changes);
    inventory
}

fn item_children() -> Vec<InventoryChild> {
    (0..ENTRY_COUNT).map(|index| InventoryChild::Item(item(index))).collect()
}

fn replacement_item_children() -> Vec<InventoryChild> {
    (0..ENTRY_COUNT)
        .map(|index| {
            let mut replacement = item(index);
            replacement.key = ItemKey::new(owner(), ItemId::new(uuid(index as u128 + ENTRY_COUNT as u128 + 2)));
            replacement.asset_id = AssetId::new(uuid(index as u128 + 2));
            InventoryChild::Item(replacement)
        })
        .collect()
}

fn folder_children(count: usize) -> Vec<InventoryChild> {
    (0..count)
        .map(|index| {
            InventoryChild::Category(Category {
                key: category_key(index as u128 + 2),
                parent: Some(category_key(ROOT_ID)),
                preferred_folder_type: inventory::FolderType::None,
                name: Arc::from(format!("folder-{index}")),
                server_version: ServerVersion::UNKNOWN,
                server_descendent_count: None,
            })
        })
        .collect()
}

fn item(index: usize) -> Item {
    Item {
        key: ItemKey::new(owner(), ItemId::new(uuid(index as u128 + 2))),
        parent: category_key(ROOT_ID),
        asset_id: AssetId::new(uuid(index as u128 + ENTRY_COUNT as u128 + 2)),
        asset_type: AssetType::Notecard,
        inventory_type: InventoryType::Notecard,
        flags: 0,
        name: Arc::from(format!("item-{index}")),
        description: Arc::from(""),
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

fn owner() -> InventoryOwner {
    InventoryOwner::new(uuid(OWNER_ID))
}

fn category_key(id: u128) -> CategoryKey {
    CategoryKey::new(owner(), FolderId::new(uuid(id)))
}

fn uuid(id: u128) -> Uuid {
    Uuid::from_u128(id)
}

fn apply_fetch(
    inventory: &mut Inventory,
    folder: CategoryKey,
    version: ServerVersion,
    descendent_count: Option<u32>,
    children: Vec<InventoryChild>,
) -> Result<inventory::ChangeSet, inventory::InventoryError> {
    let prepared = inventory.snapshot().prepare_folder_fetch(folder, version, descendent_count, children)?;
    let inventory::FolderFetchCommit::Applied(changes) = inventory.commit_folder_fetch(prepared)? else {
        panic!("single-owner fetch preparation became stale before commit");
    };
    Ok(changes)
}

mod benches {
    use super::*;

    #[divan::bench]
    fn build_100k_items_and_snapshot(bencher: divan::Bencher) {
        bencher.with_inputs(item_children).bench_local_values(|children| {
            let mut inventory = root_inventory();
            let folder = inventory.roots().personal();
            let changes = apply_fetch(&mut inventory, folder, ServerVersion::from_raw(1), Some(ENTRY_COUNT as u32), children)
                .expect("benchmark fixture should apply");
            let snapshot = inventory.snapshot();
            black_box((inventory, changes, snapshot))
        });
    }

    #[divan::bench]
    fn snapshot_100k_items(bencher: divan::Bencher) {
        let inventory = item_inventory();
        bencher.bench_local(|| black_box(inventory.snapshot()));
    }

    #[divan::bench]
    fn apply_one_item_delta_100k(bencher: divan::Bencher) {
        let (mut inventory, delta_a, delta_b) = one_item_deltas();
        let mut use_a = false;
        bencher
            .with_inputs(|| {
                use_a = !use_a;
                if use_a { delta_a.clone() } else { delta_b.clone() }
            })
            .bench_local_values(|delta| black_box(inventory.apply_delta(delta).expect("single item delta should apply")));
    }
}

mod folder_fetch_delete {
    use super::*;

    #[divan::bench]
    fn folder_fetch_delete_prepare_delete_100k_worker(bencher: divan::Bencher) {
        bencher.with_inputs(item_inventory).bench_local_values(|inventory| {
            let snapshot = inventory.snapshot();
            let prepared = snapshot
                .prepare_folder_fetch(snapshot.roots().personal(), ServerVersion::from_raw(2), Some(0), Vec::new())
                .expect("delete transaction should prepare");
            black_box((inventory, prepared))
        });
    }

    #[divan::bench]
    fn folder_fetch_delete_model_only_commit_and_direct_drop_delete_100k(bencher: divan::Bencher) {
        bencher.with_inputs(|| prepared_fetch_fixture(false)).bench_local_values(|(mut inventory, before, prepared)| {
            let changes = inventory.commit_folder_fetch(prepared).expect("delete transaction should commit");
            black_box(changes);
            drop(before);
            inventory
        });
    }

    #[divan::bench]
    fn folder_fetch_delete_prepare_replace_100k_worker(bencher: divan::Bencher) {
        bencher.with_inputs(item_inventory).bench_local_values(|inventory| {
            let snapshot = inventory.snapshot();
            let prepared = snapshot
                .prepare_folder_fetch(
                    snapshot.roots().personal(),
                    ServerVersion::from_raw(2),
                    Some(ENTRY_COUNT as u32),
                    replacement_item_children(),
                )
                .expect("replacement transaction should prepare");
            black_box((inventory, prepared))
        });
    }

    #[divan::bench]
    fn folder_fetch_delete_model_only_commit_and_direct_drop_replace_100k(bencher: divan::Bencher) {
        bencher.with_inputs(|| prepared_fetch_fixture(true)).bench_local_values(|(mut inventory, before, prepared)| {
            let changes = inventory.commit_folder_fetch(prepared).expect("replacement transaction should commit");
            black_box(changes);
            drop(before);
            inventory
        });
    }
}

mod cache {
    use super::*;

    #[divan::bench]
    fn cache_warm_read_apply_100k(bencher: divan::Bencher) {
        let directory = tempdir().expect("benchmark cache directory");
        let path = populate_cache(directory.path());
        let (cache, _) = InventoryCache::open_at_root(directory.path(), "agni", owner().id(), None).expect("benchmark cache open");

        bencher
            .with_inputs(|| {
                let inventory = cache_skeleton();
                let skeleton = inventory.snapshot();
                (inventory, skeleton)
            })
            .bench_local_values(|(mut inventory, skeleton)| {
                let load = block_on(cache.load(&inventory)).expect("benchmark cache read");
                let applied = apply_restore(load, &mut inventory, &skeleton);
                black_box((inventory, applied));
            });

        block_on(cache.close()).expect("benchmark cache close");
        black_box(path);
    }
}

mod folder_restore {
    use super::*;

    #[divan::bench]
    fn cache_folder_restore_prepare_restore_10k_folders(bencher: divan::Bencher) {
        const FOLDERS: usize = 10_000;

        let directory = tempdir().expect("folder restore cache directory");
        let mut inventory = root_inventory();
        let folder = inventory.roots().personal();
        apply_fetch(&mut inventory, folder, ServerVersion::from_raw(1), Some(FOLDERS as u32), folder_children(FOLDERS))
            .expect("folder restore skeleton");
        let skeleton = inventory.snapshot();
        let (cache, _) = InventoryCache::open_at_root(directory.path(), "agni", owner().id(), None).expect("folder restore cache");
        block_on(cache.submit_bootstrap(&skeleton).expect("persist skeleton").wait()).expect("skeleton committed");
        let load = block_on(cache.load(&inventory)).expect("verify cached folders");
        let mut restore = load.prepare_restore(&skeleton).expect("verify restore preparation");
        let mut verification = inventory.clone();
        let mut units = 0;
        loop {
            let progress = restore.apply_next(&mut verification, &HashSet::new()).expect("verify restore units");
            units += progress.units_processed;
            if progress.finished {
                break;
            }
        }
        assert_eq!(units, FOLDERS + 1, "the workload includes every cached folder and its root");

        bencher
            .with_inputs(|| block_on(cache.load(&inventory)).expect("load cached folders"))
            .bench_local_values(|load| black_box(load.prepare_restore(&skeleton).expect("prepare folder restore")));

        block_on(cache.close()).expect("close folder restore cache");
    }
}
