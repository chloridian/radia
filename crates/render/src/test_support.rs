use world::{Region, World, WorldLimits};

pub(crate) fn registered_regions(count: usize) -> (World, Vec<Region>) {
    let mut world = World::new(WorldLimits {
        max_regions: count,
        max_entities: 0,
        max_entities_per_region: 0,
        max_tombstones_per_region: 0,
        max_tombstones: 0,
        max_tombstone_bytes_per_region: 0,
        max_tombstone_bytes: 0,
        max_journal_entries: count,
        max_journal_changes: count,
        max_region_name_bytes: 0,
        max_recovery_targets: 0,
    });
    let regions = (0..count).map(|_| world.register_region().expect("fixture region registers")).collect();
    (world, regions)
}

pub(crate) fn registered_region() -> (World, Region) {
    let (world, regions) = registered_regions(1);
    (world, regions[0])
}
