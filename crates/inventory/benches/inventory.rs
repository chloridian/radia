use std::{
    collections::HashSet,
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use futures::executor::block_on;
use inventory::{
    AssetType, Category, CategoryKey, Inventory, InventoryCache, InventoryCacheLoad, InventoryChild, InventoryDelta, InventoryLimits,
    InventoryOwner, InventoryType, Item, ItemKey, PermissionMask, Permissions, SaleInfo, SaleType, ServerVersion,
};
use tempfile::tempdir;
use test_support::allocation;
use utils::{AssetId, AvatarId, FolderId, GroupId, ItemId};
use uuid::Uuid;

const ENTRY_COUNT: usize = 100_000;
const OWNER_ID: u128 = 77;
const ROOT_ID: u128 = 1;

pub(crate) fn report_diagnostics() {
    report_memory();
    report_prepared_fetch("delete", false);
    report_prepared_fetch("replace", true);
    report_delta_allocations();
}

fn report_prepared_fetch(label: &str, replace: bool) {
    let worker_started = Instant::now();
    let (mut inventory, before, prepared, worker_time, worker_allocations, worker_retained, worker_peak_upper_bound) =
        thread::spawn(move || {
            let inventory = item_inventory();
            let children = if replace { replacement_item_children() } else { Vec::new() };
            let before = inventory.snapshot();
            let baseline_bytes = allocation::live_bytes();
            let worker_started = Instant::now();
            let (prepared, allocations) = allocation::measure(|| {
                before.prepare_folder_fetch(
                    before.roots().personal(),
                    ServerVersion::from_raw(2),
                    Some(if replace { ENTRY_COUNT as u32 } else { 0 }),
                    children,
                )
            });
            let worker_time = worker_started.elapsed();
            let prepared = prepared.expect("benchmark transaction should prepare");
            let worker_retained = allocation::live_bytes();
            let worker_peak_upper_bound = baseline_bytes.saturating_add(allocations.peak_bytes);
            (inventory, before, prepared, worker_time, allocations, worker_retained, worker_peak_upper_bound)
        })
        .join()
        .expect("benchmark worker should finish");
    let end_to_end = worker_started.elapsed();
    let owner_started = Instant::now();
    let (_, owner_allocations) = allocation::measure(|| {
        let changes = inventory.commit_folder_fetch(prepared).expect("benchmark transaction should commit");
        drop(changes);
        drop(before);
    });
    let owner_time = owner_started.elapsed();
    eprintln!(
        "inventory model_only path=commit_plus_direct_drop_without_disposal_permit_or_cache_handoff case={label}_100k worker_ms={} model_only_commit_and_direct_drop_ms={} total_worker_plus_model_ms={} fixture_thread_wall_ms={} worker_alloc_calls={} worker_realloc_calls={} worker_retained_bytes={} worker_peak_upper_bound_bytes={} owner_alloc_calls={} owner_realloc_calls={} owner_net_bytes={} owner_peak_delta_bytes={}",
        duration_millis(worker_time),
        duration_millis(owner_time),
        duration_millis(worker_time.saturating_add(owner_time)),
        duration_millis(end_to_end),
        worker_allocations.allocation_calls,
        worker_allocations.reallocation_calls,
        worker_retained,
        worker_peak_upper_bound,
        owner_allocations.allocation_calls,
        owner_allocations.reallocation_calls,
        owner_allocations.live_bytes,
        owner_allocations.peak_bytes,
    );
    black_box(inventory);
}

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

fn report_cache_load_times(root: &Path, path: &Path) {
    let (cache, _) = InventoryCache::open_at_root(root, "agni", owner().id(), None).expect("benchmark first cache open");
    let mut cold_inventory = cache_skeleton();
    let cold_skeleton = cold_inventory.snapshot();
    let started = Instant::now();
    let cold_load = block_on(cache.load(&cold_inventory)).expect("benchmark first cache read");
    let cold_applied = apply_restore(cold_load, &mut cold_inventory, &cold_skeleton);
    let cold = started.elapsed();
    block_on(cache.close()).expect("benchmark first cache close");

    let (cache, _) = InventoryCache::open_at_root(root, "agni", owner().id(), None).expect("benchmark warm cache open");
    let mut warm_times = Vec::new();
    for _ in 0..5 {
        let mut inventory = cache_skeleton();
        let skeleton = inventory.snapshot();
        let started = Instant::now();
        let load = block_on(cache.load(&inventory)).expect("benchmark warm cache read");
        let applied = apply_restore(load, &mut inventory, &skeleton);
        assert_eq!(applied, 1);
        warm_times.push(started.elapsed());
    }
    warm_times.sort_unstable();
    let warm = warm_times[warm_times.len() / 2];
    block_on(cache.close()).expect("benchmark warm cache close");
    eprintln!(
        "inventory cache at {ENTRY_COUNT} items: file={} bytes, first read/apply={} ms ({cold_applied} folders), warm median read/apply={} ms",
        fs::metadata(path).expect("benchmark database metadata").len(),
        duration_millis(cold),
        duration_millis(warm),
    );
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

fn duration_millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
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

fn report_memory() {
    let (root, root_memory) = allocation::measure(root_inventory);
    let (mut items, item_memory) = allocation::measure(item_inventory);
    let (folders, folder_memory) = allocation::measure(folder_inventory);
    let (snapshot, snapshot_memory) = allocation::measure(|| items.snapshot());
    let (changed_snapshot, changed_memory) = allocation::measure(|| {
        let mut changed_item = item(0);
        changed_item.name = Arc::from("updated item");
        let changes = items.upsert_item(changed_item).expect("single item update should apply");
        drop(changes);
        items.snapshot()
    });
    let item_bytes = item_memory.live_bytes.saturating_sub(root_memory.live_bytes) / ENTRY_COUNT as i128;
    let folder_bytes = folder_memory.live_bytes.saturating_sub(root_memory.live_bytes) / ENTRY_COUNT as i128;

    eprintln!(
        "inventory memory at {ENTRY_COUNT} entries: item model={} B ({item_bytes} B/item), folder model={} B ({folder_bytes} B/folder), snapshot delta={} B, single item change with prior snapshot={} B, Item={} B, Category={} B",
        item_memory.live_bytes,
        folder_memory.live_bytes,
        snapshot_memory.live_bytes,
        changed_memory.live_bytes,
        size_of::<Item>(),
        size_of::<Category>(),
    );
    black_box((root, items, folders, snapshot, changed_snapshot));
}

fn report_delta_allocations() {
    let (mut inventory, delta_a, _) = one_item_deltas();
    let (changes, allocations) = allocation::measure(|| inventory.apply_delta(delta_a).expect("single item delta should apply"));
    eprintln!(
        "inventory apply_one_item_delta_100k allocations: alloc_calls={}, realloc_calls={}, live={} B, peak={} B",
        allocations.allocation_calls, allocations.reallocation_calls, allocations.live_bytes, allocations.peak_bytes,
    );
    black_box((inventory, changes));
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

fn folder_inventory() -> Inventory {
    let mut inventory = root_inventory();
    let folder = inventory.roots().personal();
    let changes = apply_fetch(&mut inventory, folder, ServerVersion::from_raw(1), Some(ENTRY_COUNT as u32), folder_children(ENTRY_COUNT))
        .expect("benchmark folders should apply");
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
        report_cache_load_times(directory.path(), &path);
        let (cache, _) = InventoryCache::open_at_root(directory.path(), "agni", owner().id(), None).expect("benchmark cache open");

        bencher.bench_local(|| {
            let mut inventory = cache_skeleton();
            let skeleton = inventory.snapshot();
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
