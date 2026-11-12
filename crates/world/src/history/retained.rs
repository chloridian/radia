use std::{
    collections::{BTreeSet, HashMap},
    num::NonZeroU32,
};

use uuid::Uuid;

#[cfg(test)]
use crate::World;
use crate::update::Stamp;
use crate::{LocalAddress, PbrMaterialOverrides, Region};

const MAX_RETAINED_PER_REGION: usize = 1024;
const MAX_RETAINED: usize = 4096;
const MAX_RETAINED_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct RetainedOverride {
    pub(crate) stamp: Stamp,
    pub(crate) expected_id: Option<Uuid>,
    pub(crate) overrides: PbrMaterialOverrides,
}

impl RetainedOverride {
    fn bytes(&self) -> usize {
        size_of::<(LocalAddress, Self)>() + size_of::<(Stamp, NonZeroU32)>() + self.overrides.heap_bytes()
    }
}

#[derive(Debug, Default)]
pub(crate) struct RetainedOverrides {
    by_address: HashMap<LocalAddress, RetainedOverride>,
    oldest_by_region: HashMap<Region, BTreeSet<(Stamp, NonZeroU32)>>,
    bytes: usize,
}

impl RetainedOverrides {
    pub(crate) fn applicable(&self, address: &LocalAddress, id: Uuid) -> Option<&PbrMaterialOverrides> {
        self.by_address.get(address).filter(|entry| entry.expected_id.is_none_or(|expected| expected == id)).map(|entry| &entry.overrides)
    }

    pub(crate) fn contains(&self, address: &LocalAddress) -> bool {
        self.by_address.contains_key(address)
    }

    pub(crate) fn retain(&mut self, address: LocalAddress, entry: RetainedOverride) {
        if self.by_address.get(&address).is_some_and(|existing| existing.stamp >= entry.stamp) {
            return;
        }
        self.remove(&address);
        let region = address.region();
        self.bytes += entry.bytes();
        let inserted = self.oldest_by_region.entry(region).or_default().insert((entry.stamp, address.local_id()));
        assert!(inserted, "retained override address index and eviction index disagree");
        self.by_address.insert(address, entry);
        while self.region_len(region) > MAX_RETAINED_PER_REGION || self.by_address.len() > MAX_RETAINED || self.bytes > MAX_RETAINED_BYTES {
            let Some((_, local_id)) = self.oldest_by_region.get(&region).and_then(BTreeSet::first).copied() else { break };
            self.remove(&LocalAddress::new(region, local_id));
        }
    }

    pub(crate) fn remove(&mut self, address: &LocalAddress) -> Option<RetainedOverride> {
        let entry = self.by_address.remove(address)?;
        let region = address.region();
        let entries = self.oldest_by_region.get_mut(&region).expect("every retained override has a region index");
        assert!(entries.remove(&(entry.stamp, address.local_id())), "retained override eviction index is missing an address");
        if entries.is_empty() {
            self.oldest_by_region.remove(&region);
        }
        self.bytes -= entry.bytes();
        Some(entry)
    }

    pub(crate) fn remove_older(&mut self, address: LocalAddress, stamp: Stamp) {
        if self.by_address.get(&address).is_some_and(|entry| entry.stamp < stamp) {
            self.remove(&address);
        }
    }

    pub(crate) fn first_region_address(&self, region: Region) -> Option<LocalAddress> {
        self.oldest_by_region.get(&region)?.first().map(|(_, local_id)| LocalAddress::new(region, *local_id))
    }

    fn region_len(&self, region: Region) -> usize {
        self.oldest_by_region.get(&region).map_or(0, BTreeSet::len)
    }
}

#[cfg(test)]
pub(crate) fn retained_invariants_hold(world: &World) -> bool {
    let retained = &world.retained;
    retained.by_address.len() == retained.oldest_by_region.values().map(BTreeSet::len).sum::<usize>()
        && retained.by_address.len() <= MAX_RETAINED
        && retained.bytes <= MAX_RETAINED_BYTES
        && retained.bytes == retained.by_address.values().map(RetainedOverride::bytes).sum::<usize>()
        && retained.by_address.iter().all(|(address, entry)| {
            (world.regions.contains_key(&address.region()) || world.retiring_regions.contains_key(&address.region()))
                && world.entity_at(*address).is_none()
                && retained
                    .oldest_by_region
                    .get(&address.region())
                    .is_some_and(|entries| entries.contains(&(entry.stamp, address.local_id())))
        })
        && retained.oldest_by_region.iter().all(|(region, entries)| {
            !entries.is_empty()
                && entries.len() <= MAX_RETAINED_PER_REGION
                && entries.iter().all(|(stamp, local_id)| {
                    retained.by_address.get(&LocalAddress::new(*region, *local_id)).is_some_and(|entry| entry.stamp == *stamp)
                })
        })
}
