use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

use uuid::Uuid;

use crate::{
    Entity, LocalAddress, MAX_ENVIRONMENT_BYTES, MAX_PARCEL_BYTES, MAX_TERRAIN_PATCHES, ObjectMedia, Parent, World, WorldRevision, history,
    history::{address, retained},
    object::{components::Fenced, storage},
};

#[cfg(test)]
impl World {
    pub(crate) fn check_invariants(&self) -> Result<(), &'static str> {
        let environment_bytes: usize =
            self.regions.values().chain(self.retiring_regions.values()).map(|state| state.environment.source_bytes()).sum();
        if environment_bytes != self.environment_bytes || environment_bytes > MAX_ENVIRONMENT_BYTES {
            return Err("environment source accounting is inconsistent");
        }
        let parcel_bytes: usize =
            self.regions.values().chain(self.retiring_regions.values()).map(|state| state.parcels.retained_bytes()).sum();
        if parcel_bytes != self.parcel_bytes || parcel_bytes > MAX_PARCEL_BYTES {
            return Err("parcel state accounting is inconsistent");
        }
        if self.regions.values().chain(self.retiring_regions.values()).any(|state| !state.parcels.invariants_hold()) {
            return Err("parcel state invariants are inconsistent");
        }
        let terrain_patches: usize =
            self.regions.values().chain(self.retiring_regions.values()).map(|state| state.terrain.patch_count()).sum();
        if terrain_patches != self.terrain_patches || terrain_patches > MAX_TERRAIN_PATCHES {
            return Err("terrain patch accounting is inconsistent");
        }
        storage::object_store_invariants_hold(&self.store)?;
        let mut invalidated_region_counts = HashMap::<crate::Region, usize>::new();
        for entity in self.invalidations.keys() {
            if self.store.object_record(*entity).is_none() {
                return Err("removal fence points to a dead entity");
            }
            if self.counted_invalidations.contains(entity) {
                if !self.store.is_current_entity(*entity) {
                    return Err("counted invalidation points to a stale entity");
                }
                let region = self.store.object_record(*entity).expect("current entity has a record").state.binding.address.region();
                *invalidated_region_counts.entry(region).or_default() += 1;
            }
        }
        if self.counted_invalidations.iter().any(|entity| !self.invalidations.contains_key(entity))
            || invalidated_region_counts != self.invalidated_region_counts
        {
            return Err("region removal counts disagree with active fences");
        }
        if self.removal_fences.iter().any(|(address, (entity, _, _))| {
            self.store.object_record(*entity).is_none_or(|record| record.state.binding.address != *address)
                || !self.invalidations.contains_key(entity)
        }) {
            return Err("address removal fences disagree with active entity fences");
        }
        if self.relationship_work.iter().any(|((id, address), work)| {
            self.relationship_work_parents.get(address).and_then(|parents| parents.get(id)) != Some(&work.parent)
        }) || self.relationship_work_parents.iter().any(|(address, parents)| {
            parents.iter().any(|(id, entity)| self.relationship_work.get(&(*id, *address)).is_none_or(|work| work.parent != *entity))
        }) {
            return Err("relationship work address index is inconsistent");
        }
        if self.transfer_cursors.keys().any(|entity| self.store.object_record(*entity).is_none()) {
            return Err("parent transfer work points to a dead entity");
        }
        if !self.transfer_retention_is_valid() {
            return Err("parent transfer retention exceeds its limit");
        }
        if self.visible_entity_count() > self.limits.max_entities
            || self.store.physical_len() > self.limits.max_entities.saturating_mul(2)
            || self.regions.len() > self.limits.max_regions
            || self.retiring_regions.len() > self.limits.max_regions
            || self.regions.keys().any(|region| !region.belongs_to(self.id) || region.get() > self.last_region_generation)
            || self.regions.keys().any(|region| self.objects_in_region(*region).count() > self.limits.max_entities_per_region)
        {
            return Err("World storage exceeds its configured limits");
        }
        let mut expected_children = HashMap::<Entity, BTreeMap<Uuid, Entity>>::new();
        let mut expected_pending_children = HashMap::<LocalAddress, BTreeMap<(WorldRevision, Uuid), Entity>>::new();
        let mut object_media_bytes = 0_usize;
        for (raw_entity, record) in self.store.objects().iter() {
            let entity = Entity(raw_entity, self.id);
            let id = record.id;
            let address = record.state.binding.address;
            let visible = self.entity_is_visible(entity);
            if visible && !self.regions.contains_key(&address.region()) {
                return Err("visible object belongs to an inactive region");
            }
            object_media_bytes = object_media_bytes.saturating_add(
                self.store
                    .component::<Fenced<Arc<ObjectMedia>>>(raw_entity)
                    .and_then(|component| component.value.as_deref())
                    .map_or(0, ObjectMedia::retained_bytes),
            );
            match record.state.parent {
                Parent::Resolved(parent) => {
                    let parent_record = self.store.object_record(parent).ok_or("resolved parent is not live")?;
                    if parent_record.state.binding.address.region() != address.region() && !self.transfer_cursors.contains_key(&parent) {
                        return Err("resolved parent and child belong to different regions");
                    }
                    if self.children.get(&parent).and_then(|children| children.get(&id)) == Some(&entity) {
                        expected_children.entry(parent).or_default().insert(id, entity);
                    } else if self.store.is_current_entity(entity) {
                        return Err("current child is missing from its parent index");
                    }
                }
                Parent::Pending { address, .. } => {
                    if address.region() != record.state.binding.address.region() {
                        return Err("pending parent and child belong to different regions");
                    }
                    let since = record.state.pending_since;
                    if since > self.revision() {
                        return Err("pending child belongs to a future revision");
                    }
                    if self.pending_children.get(&address).and_then(|children| children.get(&(since, id))) == Some(&entity) {
                        expected_pending_children.entry(address).or_default().insert((since, id), entity);
                    } else if self.store.is_current_entity(entity) {
                        return Err("current pending child is missing from its parent index");
                    }
                }
                Parent::Unknown | Parent::Root => {}
            };
        }
        if self.children != expected_children || self.pending_children != expected_pending_children {
            return Err("parent relationship indexes are inconsistent");
        }
        if self.relationship_work.len() > self.limits.max_entities {
            return Err("pending relationship work exceeds its configured limit");
        }
        for ((id, address), work) in &self.relationship_work {
            let record = self.store.object_record(work.parent).ok_or("queued relationship parent is not live")?;
            if record.id != *id || work.binding.address != *address {
                return Err("queued relationship identity is inconsistent");
            }
        }
        let mut checked = HashSet::with_capacity(self.store.physical_len());
        let mut path = HashSet::new();
        for (raw_entity, _) in self.store.objects().iter() {
            let entity = Entity(raw_entity, self.id);
            path.clear();
            let mut current = entity;
            while !checked.contains(&current) {
                if !path.insert(current) {
                    return Err("resolved parent graph contains a cycle");
                }
                let Some(record) = self.store.object_record(current) else {
                    return Err("resolved parent is not live");
                };
                let Parent::Resolved(parent) = record.state.parent else { break };
                current = parent;
            }
            checked.extend(path.iter().copied());
        }
        if self.floors.keys().any(|region| !self.regions.contains_key(region) && !self.retiring_regions.contains_key(region)) {
            return Err("region lifecycle indexes are inconsistent");
        }
        if self.object_media_bytes != object_media_bytes {
            return Err("ObjectMedia byte accounting is inconsistent");
        }
        if !address::tombstones_invariants_hold(self) {
            return Err("tombstone indexes are inconsistent");
        }
        if !retained::retained_invariants_hold(self) {
            return Err("retained override indexes are inconsistent");
        }
        if !history::journal_invariants_hold(&self.journal) {
            return Err("change journal accounting is inconsistent");
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "invariants_tests.rs"]
mod tests;
