#![allow(unexpected_cfgs)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
#[cfg(world_bench_detailed)]
use std::time::Duration;

use hecs::{Component, EntityBuilder, QueryBorrow, QueryOne, Ref, ViewBorrow};
use math::{Quat, Vec3};
use uuid::Uuid;

use crate::identity::WorldId;
use crate::mutation::facet::{FacetChanges, FacetSet, FacetUpdates};
use crate::object::components::Fenced;
use crate::{Entity, Kind, LocalAddress, ObjectMedia, Parent, Region, WorldRevision};
use crate::{ObjectOrdering, ObjectRecord, ObjectState};

pub(crate) struct ObjectStore {
    world: WorldId,
    ecs: hecs::World,
    entities: HashMap<Uuid, Entity>,
    ids: BTreeSet<Uuid>,
    addresses: HashMap<LocalAddress, Entity>,
    regions: HashMap<Region, BTreeMap<Uuid, Entity>>,
    avatars: BTreeMap<Uuid, Entity>,
}

pub(crate) struct StoreRemovalTimes {
    #[cfg(world_bench_detailed)]
    pub(crate) component_destruction: Duration,
    #[cfg(world_bench_detailed)]
    pub(crate) index_removal: Duration,
}

pub(crate) struct ComponentView<'a, T: Component> {
    store: &'a ObjectStore,
    view: ViewBorrow<'a, &'a T>,
}

impl<'a, T: Component> ComponentView<'a, T> {
    pub(crate) fn get(&self, entity: hecs::Entity) -> Option<&'a T> {
        self.store.shared_component_ref(entity, Some(&self.view))
    }
}

impl ObjectStore {
    pub(crate) fn new(world: WorldId) -> Self {
        Self {
            world,
            ecs: hecs::World::new(),
            entities: HashMap::new(),
            ids: BTreeSet::new(),
            addresses: HashMap::new(),
            regions: HashMap::new(),
            avatars: BTreeMap::new(),
        }
    }

    pub(crate) fn entity(&self, id: Uuid) -> Option<Entity> {
        self.entities.get(&id).copied()
    }

    pub(crate) fn entity_at(&self, address: LocalAddress) -> Option<Entity> {
        self.addresses.get(&address).copied()
    }

    pub(crate) fn identity_entities(&self) -> impl Iterator<Item = Entity> + '_ {
        self.entities.values().copied()
    }

    pub(crate) fn is_current_entity(&self, entity: Entity) -> bool {
        let Some(record) = self.object_record(entity) else { return false };
        self.entities.get(&record.id) == Some(&entity)
            && self.addresses.get(&record.state.binding.address) == Some(&entity)
            && self.regions.get(&record.state.binding.address.region()).and_then(|objects| objects.get(&record.id)) == Some(&entity)
    }

    pub(crate) fn len(&self) -> usize {
        self.entities.len()
    }

    pub(crate) fn physical_len(&self) -> usize {
        self.ecs.len() as usize
    }

    pub(crate) fn ids(&self) -> &BTreeSet<Uuid> {
        &self.ids
    }

    pub(crate) fn avatar_ids(&self) -> &BTreeMap<Uuid, Entity> {
        &self.avatars
    }

    pub(crate) fn region_ids(&self, region: Region) -> Option<&BTreeMap<Uuid, Entity>> {
        self.regions.get(&region)
    }

    pub(crate) fn insert(&mut self, record: ObjectRecord, facets: FacetSet, ordering: ObjectOrdering) -> Entity {
        let id = record.id;
        let address = record.state.binding.address;
        let kind = record.kind;
        if let Some(previous) = self.entities.get(&id).copied() {
            let previous_address = self.object_record(previous).expect("replaced identity remains physically stored").state.binding.address;
            if self.addresses.get(&previous_address) == Some(&previous) {
                self.addresses.remove(&previous_address);
            }
            self.remove_region_id(previous_address.region(), id, previous);
            if self.avatars.get(&id) == Some(&previous) {
                self.avatars.remove(&id);
            }
        }
        let mut builder = EntityBuilder::new();
        builder.add(record);
        builder.add(ordering);
        facets.add_components(&mut builder);
        let entity = self.ecs.spawn(builder.build());
        let entity = Entity(entity, self.world);
        self.entities.insert(id, entity);
        self.ids.insert(id);
        self.addresses.insert(address, entity);
        self.regions.entry(address.region()).or_default().insert(id, entity);
        if kind == Kind::Avatar {
            self.avatars.insert(id, entity);
        } else {
            self.avatars.remove(&id);
        }
        entity
    }

    pub(crate) fn remove(&mut self, entity: Entity) -> StoreRemovalTimes {
        let (id, address) = {
            let record = self.object_record(entity).expect("removal target is live");
            (record.id, record.state.binding.address)
        };
        let current_identity = self.entities.get(&id) == Some(&entity);
        #[cfg(world_bench_detailed)]
        let component_started = std::time::Instant::now();
        self.ecs.despawn(entity.0).expect("removal target is live");
        #[cfg(world_bench_detailed)]
        let component_destruction = component_started.elapsed();
        #[cfg(world_bench_detailed)]
        let indexes_started = std::time::Instant::now();
        if current_identity {
            self.entities.remove(&id);
            self.ids.remove(&id);
            self.avatars.remove(&id);
        }
        if self.addresses.get(&address) == Some(&entity) {
            self.addresses.remove(&address);
        }
        self.remove_region_id(address.region(), id, entity);
        #[cfg(world_bench_detailed)]
        let index_removal = indexes_started.elapsed();
        #[cfg(world_bench_detailed)]
        {
            StoreRemovalTimes { component_destruction, index_removal }
        }
        #[cfg(not(world_bench_detailed))]
        {
            StoreRemovalTimes {}
        }
    }

    pub(crate) fn commit(&mut self, entity: Entity, state: ObjectState, facets: Option<FacetChanges>, ordering: ObjectOrdering) {
        let (id, previous) = {
            let record = self.object_record(entity).expect("mutation target is live");
            (record.id, record.state.binding.address)
        };
        let address = state.binding.address;
        if previous != address {
            if self.addresses.get(&previous) == Some(&entity) {
                self.addresses.remove(&previous);
            }
            self.addresses.insert(address, entity);
            if previous.region() != address.region() {
                self.remove_region_id(previous.region(), id, entity);
                self.regions.entry(address.region()).or_default().insert(id, entity);
            }
        }
        {
            let (record, stored_ordering) = self
                .ecs
                .query_one_mut::<(&mut ObjectRecord, &mut ObjectOrdering)>(entity.0)
                .expect("mutation target has its required components");
            record.state = ObjectState { parent: record.state.parent, pending_since: record.state.pending_since, ..state };
            *stored_ordering = ordering;
        }
        if let Some(facets) = facets {
            facets.apply(&mut self.ecs, entity.0);
        }
    }

    fn remove_region_id(&mut self, region: Region, id: Uuid, entity: Entity) {
        let Some(ids) = self.regions.get_mut(&region) else { return };
        if ids.get(&id) == Some(&entity) {
            ids.remove(&id);
        }
        if ids.is_empty() {
            self.regions.remove(&region);
        }
    }

    pub(crate) fn object_record(&self, entity: Entity) -> Option<&ObjectRecord> {
        if entity.1 != self.world {
            return None;
        }
        self.shared_component_ref(entity.0, None)
    }

    pub(crate) fn component<T: Component>(&self, entity: hecs::Entity) -> Option<&T> {
        self.shared_component_ref(entity, None)
    }

    pub(crate) fn component_view<T: Component>(&self) -> ComponentView<'_, T> {
        ComponentView { store: self, view: self.ecs.view::<&T>() }
    }

    fn shared_component_ref<'a, T: Component>(&'a self, entity: hecs::Entity, view: Option<&ViewBorrow<'a, &'a T>>) -> Option<&'a T> {
        unsafe {
            if let Some(view) = view {
                let component = view.get_unchecked(entity)?;
                Some(&*(component as *const T))
            } else {
                self.ecs.get_unchecked::<&T>(entity).ok()
            }
        }
    }

    pub(crate) fn facets(&self, entity: hecs::Entity, updates: &FacetUpdates, reset: bool) -> FacetSet {
        FacetSet::read(self.ecs.entity(entity).expect("facet read target is live"), updates, reset)
    }

    pub(crate) fn removal_data(&mut self, entity: hecs::Entity) -> (Uuid, ObjectState, usize) {
        let (record, object_media) =
            self.ecs.query_one_mut::<(&ObjectRecord, Option<&Fenced<Arc<ObjectMedia>>>)>(entity).expect("every object has a record");
        (record.id, record.state, object_media.and_then(|component| component.value.as_deref()).map_or(0, ObjectMedia::retained_bytes))
    }

    pub(crate) fn objects(&self) -> QueryBorrow<'_, (hecs::Entity, &ObjectRecord)> {
        self.ecs.query::<(hecs::Entity, &ObjectRecord)>()
    }

    pub(crate) fn read(&self, entity: hecs::Entity) -> QueryOne<'_, (&ObjectRecord, &ObjectOrdering)> {
        self.ecs.query_one::<(&ObjectRecord, &ObjectOrdering)>(entity)
    }

    pub(crate) fn ordering(&self, entity: hecs::Entity) -> Ref<'_, ObjectOrdering> {
        self.ecs.get::<&ObjectOrdering>(entity).expect("every object has ordering")
    }

    pub(crate) fn set_parent(&mut self, entity: Entity, parent: Parent, pending_since: WorldRevision) {
        assert_eq!(entity.1, self.world, "parent mutation belongs to another World");
        let record = self.ecs.query_one_mut::<&mut ObjectRecord>(entity.0).expect("parent mutation target is live");
        record.state.parent = parent;
        record.state.pending_since = pending_since;
    }

    pub(crate) fn set_detached_pose(&mut self, entity: Entity, pose: Option<(Vec3, Quat)>) {
        assert_eq!(entity.1, self.world, "pose mutation belongs to another World");
        let record = self.ecs.query_one_mut::<&mut ObjectRecord>(entity.0).expect("pose mutation target is live");
        let generation = record.state.frame_generation.checked_add(1);
        if let Some(generation) = generation {
            record.state.frame_generation = generation;
        }
        if let Some((position, rotation)) = pose.filter(|_| generation.is_some()) {
            record.state.transform.position = Some(position);
            record.state.transform.rotation = Some(rotation);
            record.state.position_frame_generation = generation;
            record.state.rotation_frame_generation = generation;
        } else {
            record.state.position_frame_generation = None;
            record.state.rotation_frame_generation = None;
        }
        record.state.provisional_pose = None;
    }

    #[cfg(test)]
    pub(crate) fn record_mut(&mut self, entity: hecs::Entity) -> &mut ObjectRecord {
        self.ecs.query_one_mut::<&mut ObjectRecord>(entity).expect("test mutation target is live")
    }

    pub(crate) fn ordering_mut(&mut self, entity: hecs::Entity) -> &mut ObjectOrdering {
        self.ecs.query_one_mut::<&mut ObjectOrdering>(entity).expect("mutation target has ordering")
    }
}

#[cfg(test)]
pub(crate) fn object_store_entities(store: &ObjectStore) -> impl Iterator<Item = (Uuid, Entity)> + '_ {
    store.entities.iter().map(|(id, entity)| (*id, *entity))
}

#[cfg(test)]
pub(crate) fn object_store_invariants_hold(store: &ObjectStore) -> Result<(), &'static str> {
    let count = store.entities.len();
    if (store.ecs.len() as usize) < count
        || store.ecs.query::<(&ObjectRecord, &ObjectOrdering)>().iter().count() != store.ecs.len() as usize
        || store.ids.len() != count
        || store.addresses.len() > count
    {
        return Err("object storage and identity index counts differ");
    }
    if store.ids.iter().any(|id| !store.entities.contains_key(id)) {
        return Err("object ID index disagrees with entity index");
    }
    for (id, entity) in object_store_entities(store) {
        let record = store.object_record(entity).ok_or("UUID index points to a dead or foreign entity")?;
        if id.is_nil() || record.id != id || !store.ids.contains(&id) {
            return Err("object identity indexes disagree with stored state");
        }
        if store.regions.get(&record.state.binding.address.region()).and_then(|entries| entries.get(&id)) != Some(&entity) {
            return Err("object identity is missing from its region index");
        }
        if record.kind == Kind::Avatar && store.avatars.get(&id) != Some(&entity) {
            return Err("avatar identity is missing from its index");
        }
        if record.kind != Kind::Avatar && store.avatars.contains_key(&id) {
            return Err("non-avatar identity appears in the avatar index");
        }
    }
    for (address, entity) in &store.addresses {
        let record = store.object_record(*entity).ok_or("address index points to a dead entity")?;
        if record.state.binding.address != *address || store.entities.get(&record.id) != Some(entity) {
            return Err("address index disagrees with stored state");
        }
    }
    for (region, entries) in &store.regions {
        for (id, entity) in entries {
            let record = store.object_record(*entity).ok_or("region index points to a dead entity")?;
            if record.id != *id || record.state.binding.address.region() != *region {
                return Err("region index disagrees with stored state");
            }
        }
    }
    if store
        .ecs
        .query::<(hecs::Entity, &ObjectRecord)>()
        .iter()
        .any(|(entity, _)| store.ecs.entity(entity).is_ok_and(|entry| !crate::mutation::facet::components_valid(entry)))
    {
        return Err("empty facet components are stored");
    }
    for (id, entity) in &store.avatars {
        let record = store.object_record(*entity).ok_or("avatar index points to a dead entity")?;
        if record.id != *id || record.kind != Kind::Avatar || store.entities.get(id) != Some(entity) {
            return Err("avatar index disagrees with stored state");
        }
    }
    Ok(())
}
