use std::collections::{HashMap, HashSet};

use network::SimulatorId;
use world::{LocalAddress, MAX_WORLD_CHANGE_PAGE_ITEMS, World, WorldChange, WorldChangeClasses, WorldEntityRemovalReason};

use super::storage::make_override_submission;
use super::{
    CacheScope, MAX_REGION_OVERRIDE_BYTES, MAX_REGION_OVERRIDES, MAX_TOTAL_BYTES, MAX_TOTAL_ENTRIES, ObjectCacheSession, OverrideWrite,
    StoredOverride, now_millis,
};
use crate::session::inbound::encode_pbr_overrides;
use crate::session::world::SessionWorld;

impl ObjectCacheSession {
    pub(in crate::session) fn persist_overrides(&mut self, session_world: &SessionWorld) {
        let world = session_world.world();
        let barrier = world.read_barrier().changes;
        let Some(mut cursor) = self.override_cursor.replace(barrier) else { return };
        if self.disabled {
            return;
        }
        let mut touched = HashSet::new();
        loop {
            let page = match world.change_page(cursor, MAX_WORLD_CHANGE_PAGE_ITEMS) {
                Ok(page) => page,
                Err(error) => {
                    tracing::warn!(?error, "PBR override persistence missed World changes");
                    break;
                }
            };
            for record in &page.records {
                touched.extend(touched_addresses(world, record.change));
            }
            cursor = page.cursor;
            if !page.has_more {
                break;
            }
        }
        let mut writes = HashMap::<SimulatorId, Vec<OverrideWrite>>::new();
        for address in touched {
            let Some(simulator_id) = session_world.simulator_for_region(address.region()) else { continue };
            if let Some(write) = self.override_write(world, simulator_id, address) {
                writes.entry(simulator_id).or_default().push(write);
            }
        }
        for (simulator_id, mut writes) in writes {
            writes.sort_unstable_by_key(OverrideWrite::local_id);
            if let Some(scope) = self.identities.get(&simulator_id).cloned() {
                self.write_overrides(simulator_id, scope, writes);
            }
        }
    }

    pub(super) fn write_overrides(&mut self, simulator_id: SimulatorId, scope: CacheScope, writes: Vec<OverrideWrite>) {
        self.ensure_loaded_region(simulator_id, scope.clone());
        let deleted =
            writes.iter().filter_map(|write| matches!(write, OverrideWrite::Delete(_)).then_some(write.local_id())).collect::<HashSet<_>>();
        self.invalidate_in_flight_load(simulator_id, &scope, &deleted);
        for write in &writes {
            self.remember_override(simulator_id, write);
        }
        self.enqueue(make_override_submission(scope, writes, now_millis()));
    }

    fn override_write(&self, world: &World, simulator_id: SimulatorId, address: LocalAddress) -> Option<OverrideWrite> {
        let scope = self.identities.get(&simulator_id)?;
        let local_id = address.local_id();
        let known = self
            .loaded_regions
            .get(&simulator_id)
            .filter(|region| region.scope == *scope)
            .and_then(|region| region.overrides.get(&local_id.get()));
        let current = world.entity_at(address).and_then(|entity| world.object(entity)).and_then(|object| {
            let payload = encode_pbr_overrides(local_id, object.pbr_material_overrides()?)?;
            Some(StoredOverride { object_id: object.id(), payload: payload.into() })
        });
        match current {
            Some(current) if known != Some(&current) => Some(OverrideWrite::Upsert(local_id.get(), current)),
            None if known.is_some() => Some(OverrideWrite::Delete(local_id.get())),
            Some(_) | None => None,
        }
    }

    fn remember_override(&mut self, simulator_id: SimulatorId, write: &OverrideWrite) {
        let Some(region) = self.loaded_regions.get_mut(&simulator_id) else { return };
        let local_id = write.local_id();
        if let Some(removed) = region.overrides.remove(&local_id) {
            region.bytes = region.bytes.saturating_sub(removed.payload.len());
            self.loaded_entries = self.loaded_entries.saturating_sub(1);
            self.loaded_bytes = self.loaded_bytes.saturating_sub(removed.payload.len());
        }
        let OverrideWrite::Upsert(_, stored) = write else { return };
        let override_bytes = region.overrides.values().map(|stored| stored.payload.len()).sum::<usize>();
        if region.overrides.len() >= MAX_REGION_OVERRIDES || override_bytes.saturating_add(stored.payload.len()) > MAX_REGION_OVERRIDE_BYTES
        {
            return;
        }
        self.make_loaded_room(simulator_id, 1, stored.payload.len());
        if self.loaded_entries >= MAX_TOTAL_ENTRIES || self.loaded_bytes.saturating_add(stored.payload.len()) > MAX_TOTAL_BYTES {
            return;
        }
        let last_used = self.next_use();
        let Some(region) = self.loaded_regions.get_mut(&simulator_id) else { return };
        region.bytes = region.bytes.saturating_add(stored.payload.len());
        region.last_used = last_used;
        region.overrides.insert(local_id, stored.clone());
        self.loaded_entries += 1;
        self.loaded_bytes += stored.payload.len();
    }
}

fn touched_addresses(world: &World, change: WorldChange) -> Vec<LocalAddress> {
    match change {
        WorldChange::EntityAdded { address, .. } => vec![address],
        WorldChange::EntityUpdated { entity, classes, .. } if classes.contains(WorldChangeClasses::PBR_MATERIAL_OVERRIDES) => {
            world.object(entity).map(|object| object.address()).into_iter().collect()
        }
        WorldChange::EntityRebound { from, to, .. } | WorldChange::EntityTransferred { from, to, .. } => vec![from, to],
        WorldChange::EntityRemoved { address, reason, .. } if reason != WorldEntityRemovalReason::RegionRetired => vec![address],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroU32, path::Path};

    use protocol::{
        lludp::Message,
        messages::{GenericStreamingMessage, ObjectUpdateCached, object::kill_object},
    };
    use uuid::Uuid;
    use world::{OrderEvidence, PbrMaterialOverride, PbrMaterialOverrides, Region};

    use super::*;
    use crate::session::inbound::decode_object_message;
    use crate::session::object_cache::test_support::*;

    const LOCAL_ID: u32 = 4_242;
    const OBJECT_ID: Uuid = Uuid::from_u128(0x0bec7);
    const OTHER_OBJECT_ID: Uuid = Uuid::from_u128(0x0bec8);

    struct Viewer {
        world: SessionWorld,
        cache: ObjectCacheSession,
        region: Region,
        sequence: u32,
    }

    impl Viewer {
        async fn open(path: &Path) -> Self {
            let mut cache = open_cache(path);
            load_region(&mut cache).await;
            let world = region_world();
            let region = world.region_for(SIMULATOR).expect("synthetic simulator has a registered region");
            cache.persist_overrides(&world);
            Self { world, cache, region, sequence: 0 }
        }

        fn next_sequence(&mut self) -> u32 {
            self.sequence += 1;
            self.sequence
        }

        async fn ingest(&mut self, message: Message) {
            let sequence = self.next_sequence();
            let batch = decode_object_message(self.region, sequence, OrderEvidence::Ordered(u64::from(sequence)), &message)
                .expect("message decodes into World operations");
            self.world.apply_object_batch(SIMULATOR, batch).expect("registered simulator applies the batch");
            self.cache.record_update(SIMULATOR, message);
            self.settle().await;
        }

        async fn replay(&mut self, update: &ObjectUpdateCached) {
            let sequence = self.next_sequence();
            let batch = decoded_cached_batch(self.region, update, sequence);
            let world = &self.world;
            let (batch, _, overrides) = self
                .cache
                .resolve_cached(
                    SIMULATOR,
                    self.region,
                    sequence,
                    OrderEvidence::Ordered(u64::from(sequence)),
                    |address| {
                        let world = world.world();
                        world.entity_at(address).and_then(|entity| world.object(entity).map(|object| object.id()))
                    },
                    update,
                    batch,
                )
                .await;
            self.world.retain_cached_overrides(overrides).expect("cached overrides target an active region");
            self.world.apply_object_batch(SIMULATOR, batch).expect("registered simulator applies the batch");
            self.settle().await;
        }

        async fn settle(&mut self) {
            self.cache.persist_overrides(&self.world);
            flush(&mut self.cache).await;
        }

        fn roughness(&self, object_id: Uuid) -> Option<f32> {
            let world = self.world.world();
            let object = world.entity(object_id).and_then(|entity| world.object(entity)).expect("object is live");
            object.pbr_material_overrides().and_then(|overrides| overrides.face(0)).and_then(|face| face.roughness)
        }

        async fn rows(&self) -> Vec<(u32, Uuid)> {
            override_rows(self.cache.db.as_ref().expect("cache database is open")).await
        }
    }

    fn full_update(object_id: Uuid, crc: u32) -> Message {
        let mut message = fixture_message(vectors::OBJECT_UPDATE);
        let Message::ObjectUpdate(update) = &mut message else { panic!("fixture should be a full object update") };
        update.objects.truncate(1);
        update.region_data.region_handle = REGION_HANDLE;
        update.objects[0].local_id = LOCAL_ID;
        update.objects[0].id = object_id;
        update.objects[0].crc = crc;
        message
    }

    fn override_message(roughness: Option<f32>) -> Message {
        let faces = roughness.map(|roughness| (0, PbrMaterialOverride { roughness: Some(roughness), ..Default::default() }));
        let overrides = PbrMaterialOverrides::new(faces.into_iter().collect()).expect("valid overrides");
        let local_id = NonZeroU32::new(LOCAL_ID).expect("fixture local ID is nonzero");
        let data = encode_pbr_overrides(local_id, &overrides).expect("overrides fit a message");
        Message::GenericStreamingMessage(GenericStreamingMessage { method: 0x4175, data })
    }

    fn kill() -> Message {
        Message::KillObject(kill_object::KillObject { object_data: vec![kill_object::ObjectData { local_id: LOCAL_ID }] })
    }

    fn cached(message: &Message) -> ObjectUpdateCached {
        let (region_data, references) = cached_references(message);
        cached_message(region_data, references)
    }

    async fn persisted_session(path: &Path, object_id: Uuid) {
        let mut viewer = Viewer::open(path).await;
        viewer.ingest(full_update(object_id, 11)).await;
        viewer.ingest(override_message(Some(0.25))).await;
        assert_eq!(viewer.rows().await, vec![(LOCAL_ID, object_id)]);
        viewer.cache.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn restart_restores_overrides_on_a_cache_hit() {
        let directory = tempfile::tempdir().expect("create isolated cache directory");
        let path = directory.path().join("objects.sqlite");
        persisted_session(&path, OBJECT_ID).await;

        let mut viewer = Viewer::open(&path).await;
        viewer.replay(&cached(&full_update(OBJECT_ID, 11))).await;

        assert_eq!(viewer.roughness(OBJECT_ID), Some(0.25));
        assert_eq!(viewer.rows().await, vec![(LOCAL_ID, OBJECT_ID)]);
        viewer.cache.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn terse_eviction_keeps_overrides_for_a_miss_and_later_full_update() {
        let directory = tempfile::tempdir().expect("create isolated cache directory");
        let path = directory.path().join("objects.sqlite");
        let mut viewer = Viewer::open(&path).await;
        viewer.ingest(full_update(OBJECT_ID, 11)).await;
        viewer.ingest(override_message(Some(0.25))).await;
        let mut terse = fixture_message(vectors::IMPROVED_TERSE_OBJECT_UPDATE);
        let Message::ImprovedTerseObjectUpdate(update) = &mut terse else { panic!("fixture should be a terse update") };
        update.region_data.region_handle = REGION_HANDLE;
        update.objects[0].as_decoded_mut().expect("terse fixture object is valid").local_id = LOCAL_ID;
        viewer.cache.record_update(SIMULATOR, terse);
        viewer.settle().await;

        assert_eq!(cache_count(viewer.cache.db.as_ref().expect("cache database is open")).await, 0);
        assert_eq!(viewer.rows().await, vec![(LOCAL_ID, OBJECT_ID)]);
        viewer.cache.close().await;

        let mut viewer = Viewer::open(&path).await;
        viewer.replay(&cached(&full_update(OBJECT_ID, 11))).await;
        assert!(viewer.world.world().entity(OBJECT_ID).is_none());
        viewer.ingest(full_update(OBJECT_ID, 11)).await;

        assert_eq!(viewer.roughness(OBJECT_ID), Some(0.25));
        assert_eq!(viewer.rows().await, vec![(LOCAL_ID, OBJECT_ID)]);
        viewer.cache.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn empty_overrides_and_kill_delete_the_row() {
        let directory = tempfile::tempdir().expect("create isolated cache directory");
        let mut viewer = Viewer::open(&directory.path().join("objects.sqlite")).await;
        viewer.ingest(full_update(OBJECT_ID, 11)).await;
        viewer.ingest(override_message(Some(0.25))).await;

        viewer.ingest(override_message(None)).await;
        assert_eq!(viewer.rows().await, Vec::new());

        viewer.ingest(override_message(Some(0.5))).await;
        assert_eq!(viewer.rows().await, vec![(LOCAL_ID, OBJECT_ID)]);
        viewer.ingest(kill()).await;
        assert_eq!(viewer.rows().await, Vec::new());
        viewer.cache.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn cache_id_change_wipes_overrides() {
        let directory = tempfile::tempdir().expect("create isolated cache directory");
        let mut viewer = Viewer::open(&directory.path().join("objects.sqlite")).await;
        viewer.ingest(full_update(OBJECT_ID, 11)).await;
        viewer.ingest(override_message(Some(0.25))).await;

        viewer.cache.set_region_identity(SIMULATOR, &region_handshake(NEXT_CACHE_ID));
        flush(&mut viewer.cache).await;

        assert_eq!(viewer.rows().await, Vec::new());
        let (rows, bytes, total_rows, total_bytes) = cache_totals(viewer.cache.db.as_ref().expect("cache database is open")).await;
        assert_eq!((rows, bytes), (total_rows, total_bytes));
        viewer.cache.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn mismatched_object_does_not_receive_overrides_and_drops_the_row() {
        for hit in [true, false] {
            let directory = tempfile::tempdir().expect("create isolated cache directory");
            let path = directory.path().join("objects.sqlite");
            persisted_session(&path, OBJECT_ID).await;

            let mut viewer = Viewer::open(&path).await;
            let replacement = full_update(OTHER_OBJECT_ID, 12);
            if hit {
                viewer.cache.record_update(SIMULATOR, replacement.clone());
                flush(&mut viewer.cache).await;
                viewer.replay(&cached(&replacement)).await;
            } else {
                viewer.replay(&cached(&full_update(OBJECT_ID, 99))).await;
                viewer.ingest(replacement).await;
            }

            assert_eq!(viewer.roughness(OTHER_OBJECT_ID), None, "hit: {hit}");
            assert_eq!(viewer.rows().await, Vec::new(), "hit: {hit}");
            viewer.cache.close().await;
        }
    }
}
