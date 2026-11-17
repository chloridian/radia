use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;

use network::SimulatorId;
use protocol::messages::{
    CacheMissType::{CrcMismatch, Full},
    ObjectUpdateCached,
};
use uuid::Uuid;
use world::{LocalAddress, ObjectLookupCursor, ObjectLookupProgress, OrderEvidence, Region, UpdateSource};

use crate::session::inbound::{
    ObjectBatch, ObjectMediaRequest, ObjectOperation, decode_cached_object, decode_pbr_overrides, object_media_requests,
};
use crate::session::object_cache::storage::make_access_submission;
use crate::session::object_cache::validation::{decode_entry, set_region_data, stored_entry};
use crate::session::object_cache::{
    CacheScope, CachedOverride, EncodedEntry, LoadedRegion, MAX_REGION_BYTES, MAX_REGION_ENTRIES, MAX_REGION_OVERRIDE_BYTES,
    MAX_REGION_OVERRIDES, MAX_TOTAL_BYTES, MAX_TOTAL_ENTRIES, ObjectCacheSession, OverrideWrite, StoredScope, now_millis,
};
use crate::session::world::MAX_OBJECTS_PER_INTAKE_QUANTUM;
use crate::session::world::SessionWorld;

pub(in crate::session) struct CachedObjectContinuation {
    simulator_id: SimulatorId,
    region: Region,
    sequence: u32,
    order: OrderEvidence,
    scope: Option<CacheScope>,
    update: ObjectUpdateCached,
    batch: ObjectBatch,
    index: usize,
    stale: HashSet<u32>,
    media_requests: Vec<ObjectMediaRequest>,
    stale_overrides: Vec<OverrideWrite>,
    cached_overrides: Vec<CachedOverride>,
    invalid_fields: usize,
    pending: Option<PendingCachedObject>,
}

struct PendingCachedObject {
    index: usize,
    source: UpdateSource,
    local_id: NonZeroU32,
    object_id: Uuid,
    operation: ObjectOperation,
    media_requests: Vec<ObjectMediaRequest>,
    cursor: Option<ObjectLookupCursor>,
}

pub(in crate::session) enum CachedObjectProgress {
    Pending,
    Complete(ObjectBatch, Vec<ObjectMediaRequest>, Vec<CachedOverride>),
}

const FLAGS_USE_PHYSICS: u32 = 1;

impl ObjectCacheSession {
    pub(in crate::session) fn begin_resolve_cached(
        &self,
        simulator_id: SimulatorId,
        region: Region,
        sequence: u32,
        order: OrderEvidence,
        update: ObjectUpdateCached,
        batch: ObjectBatch,
    ) -> CachedObjectContinuation {
        let scope = self.identities.get(&simulator_id).cloned();
        CachedObjectContinuation {
            simulator_id,
            region,
            sequence,
            order,
            scope,
            update,
            batch,
            index: 0,
            stale: HashSet::new(),
            media_requests: Vec::new(),
            stale_overrides: Vec::new(),
            cached_overrides: Vec::new(),
            invalid_fields: 0,
            pending: None,
        }
    }

    pub(in crate::session) fn advance_resolve_cached(
        &mut self,
        world: &mut SessionWorld,
        continuation: &mut CachedObjectContinuation,
    ) -> CachedObjectProgress {
        let mut processed = usize::from(continuation.pending.is_some());
        let mut lookup_work = 0;
        loop {
            if processed >= MAX_OBJECTS_PER_INTAKE_QUANTUM || lookup_work >= world::MAX_OBJECT_LOOKUP_WORK_ITEMS {
                return CachedObjectProgress::Pending;
            }

            if let Some(mut pending) = continuation.pending.take() {
                match world.lookup_entity_at_step(
                    LocalAddress::new(continuation.region, pending.local_id),
                    pending.cursor.take(),
                    world::MAX_OBJECT_LOOKUP_WORK_ITEMS.saturating_sub(lookup_work),
                ) {
                    ObjectLookupProgress::Visible { entity, work_units } => {
                        lookup_work = lookup_work.saturating_add(work_units);
                        let current_id = world.world().object(entity).map(|object| object.id());
                        if current_id != Some(pending.object_id) {
                            self.resolve_stored_override(continuation, pending.source, pending.local_id, Some(pending.object_id));
                        }
                        continuation.batch.blocks[pending.index] = match current_id {
                            Some(current_id) if current_id == pending.object_id => None,
                            Some(_) => {
                                Some(ObjectOperation::Cached { source: pending.source, local_id: pending.local_id, cache_miss_type: Full })
                            }
                            None => {
                                continuation.media_requests.append(&mut pending.media_requests);
                                Some(pending.operation)
                            }
                        };
                    }
                    ObjectLookupProgress::Hidden { work_units } => {
                        lookup_work = lookup_work.saturating_add(work_units);
                        self.resolve_stored_override(continuation, pending.source, pending.local_id, Some(pending.object_id));
                        continuation.media_requests.append(&mut pending.media_requests);
                        continuation.batch.blocks[pending.index] = Some(pending.operation);
                    }
                    ObjectLookupProgress::Pending { cursor, .. } => {
                        pending.cursor = Some(cursor);
                        continuation.pending = Some(pending);
                        return CachedObjectProgress::Pending;
                    }
                }
                continue;
            }

            if continuation.index == continuation.batch.blocks.len() {
                continuation.batch.invalid_fields = continuation.batch.invalid_fields.saturating_add(continuation.invalid_fields);
                if let Some(scope) = continuation.scope.as_ref()
                    && self.loaded_regions.get(&continuation.simulator_id).is_some_and(|region| &region.scope == scope)
                {
                    self.touch_loaded_region(continuation.simulator_id, scope);
                    self.remove_loaded_entries(continuation.simulator_id, &continuation.stale);
                    if !continuation.stale.is_empty() {
                        self.enqueue(make_access_submission(scope.clone(), std::mem::take(&mut continuation.stale), now_millis()));
                    }
                    if !continuation.stale_overrides.is_empty() {
                        self.write_overrides(continuation.simulator_id, scope.clone(), std::mem::take(&mut continuation.stale_overrides));
                    }
                }
                return CachedObjectProgress::Complete(
                    std::mem::replace(
                        &mut continuation.batch,
                        ObjectBatch {
                            region: continuation.region,
                            region_handle: None,
                            blocks: Vec::new(),
                            invalid_blocks: 0,
                            invalid_fields: 0,
                        },
                    ),
                    std::mem::take(&mut continuation.media_requests),
                    std::mem::take(&mut continuation.cached_overrides),
                );
            }

            processed += 1;
            let index = continuation.index;
            continuation.index += 1;
            let Some(ObjectOperation::Cached { source, local_id, .. }) = continuation.batch.blocks[index].as_ref() else {
                continue;
            };
            let source = *source;
            let local_id = *local_id;
            let reference = continuation.update.objects.get(index).copied();
            let entry = self
                .loaded_regions
                .get(&continuation.simulator_id)
                .filter(|region| continuation.scope.as_ref() == Some(&region.scope))
                .and_then(|region| region.entries.get(&local_id.get()));
            let mut cache_miss_type = Full;
            let decoded = reference.zip(entry).and_then(|(reference, entry)| {
                let Some(crc) = u32::try_from(entry.crc).ok() else {
                    continuation.stale.insert(local_id.get());
                    return None;
                };
                if crc != reference.crc {
                    continuation.stale.insert(local_id.get());
                    cache_miss_type = CrcMismatch;
                    return None;
                }
                let Some(mut payload) = decode_entry(entry, local_id.get()) else {
                    continuation.stale.insert(local_id.get());
                    return None;
                };
                set_region_data(&mut payload, continuation.update.region_data);
                let ordinal = u32::try_from(index).ok()?;
                let Some(operation) = decode_cached_object(
                    continuation.region,
                    continuation.sequence,
                    continuation.order,
                    ordinal,
                    reference,
                    &payload,
                    &mut continuation.invalid_fields,
                ) else {
                    continuation.stale.insert(local_id.get());
                    return None;
                };
                Some((reference, payload, operation))
            });

            let Some((reference, payload, mut operation)) = decoded else {
                self.resolve_stored_override(continuation, source, local_id, None);
                continuation.batch.blocks[index] = Some(ObjectOperation::Cached { source, local_id, cache_miss_type });
                continue;
            };
            let object_id = match &operation {
                ObjectOperation::Observe(observation) => observation.id,
                _ => {
                    continuation.stale.insert(local_id.get());
                    self.resolve_stored_override(continuation, source, local_id, None);
                    continuation.batch.blocks[index] = Some(ObjectOperation::Cached { source, local_id, cache_miss_type });
                    continue;
                }
            };
            if let Some(received_at) = source.motion_received_at() {
                operation.set_source(operation.source().with_motion_timing(received_at, source.motion_ping_delay()));
            }
            operation.set_is_physical(reference.update_flags & FLAGS_USE_PHYSICS != 0);
            let Some(ordinal) = u32::try_from(index).ok() else {
                continuation.stale.insert(local_id.get());
                self.resolve_stored_override(continuation, source, local_id, None);
                continuation.batch.blocks[index] = Some(ObjectOperation::Cached { source, local_id, cache_miss_type });
                continue;
            };
            let mut media_requests = object_media_requests(continuation.region, continuation.sequence, continuation.order, &payload);
            for request in &mut media_requests {
                request.source = UpdateSource::new(continuation.region, continuation.sequence, continuation.order, ordinal);
            }
            continuation.pending =
                Some(PendingCachedObject { index, source, local_id, object_id, operation, media_requests, cursor: None });
        }
    }

    fn resolve_stored_override(
        &self,
        continuation: &mut CachedObjectContinuation,
        source: UpdateSource,
        local_id: NonZeroU32,
        observed_id: Option<Uuid>,
    ) {
        let Some(stored) = self
            .loaded_regions
            .get(&continuation.simulator_id)
            .filter(|region| continuation.scope.as_ref() == Some(&region.scope))
            .and_then(|region| region.overrides.get(&local_id.get()))
        else {
            return;
        };
        match decode_pbr_overrides(&stored.payload) {
            Ok(decoded) if decoded.local_id == local_id && observed_id.is_none_or(|id| id == stored.object_id) => {
                continuation.cached_overrides.push(CachedOverride {
                    source,
                    local_id,
                    expected_id: stored.object_id,
                    overrides: decoded.overrides,
                });
            }
            _ => continuation.stale_overrides.push(OverrideWrite::Delete(local_id.get())),
        }
    }

    #[cfg(test)]
    pub(in crate::session) async fn resolve_cached(
        &mut self,
        simulator_id: SimulatorId,
        region: Region,
        sequence: u32,
        order: OrderEvidence,
        world: &mut SessionWorld,
        update: &ObjectUpdateCached,
        batch: ObjectBatch,
    ) -> (ObjectBatch, Vec<ObjectMediaRequest>, Vec<CachedOverride>) {
        let mut continuation = self.begin_resolve_cached(simulator_id, region, sequence, order, update.clone(), batch);
        loop {
            match self.advance_resolve_cached(world, &mut continuation) {
                CachedObjectProgress::Pending => tokio::task::yield_now().await,
                CachedObjectProgress::Complete(batch, media_requests, overrides) => return (batch, media_requests, overrides),
            }
        }
    }

    pub(super) fn remember_entries(&mut self, simulator_id: SimulatorId, scope: CacheScope, entries: &[EncodedEntry]) {
        self.ensure_loaded_region(simulator_id, scope);
        for entry in entries {
            let current_bytes = self
                .loaded_regions
                .get(&simulator_id)
                .and_then(|region| region.entries.get(&entry.local_id))
                .map_or(0, |old| usize::try_from(old.payload_bytes).unwrap_or(usize::MAX));
            let replacing = self.loaded_regions.get(&simulator_id).is_some_and(|region| region.entries.contains_key(&entry.local_id));
            let Some(bytes_after) = self
                .loaded_regions
                .get(&simulator_id)
                .and_then(|region| region.bytes.checked_sub(current_bytes))
                .and_then(|bytes| bytes.checked_add(entry.payload.len()))
            else {
                continue;
            };
            let region_entries = self.loaded_regions.get(&simulator_id).map_or(0, |region| region.entries.len());
            if (!replacing && region_entries >= MAX_REGION_ENTRIES) || bytes_after > MAX_REGION_BYTES {
                continue;
            }
            let row_increment = usize::from(!replacing);
            let byte_increment = entry.payload.len().saturating_sub(current_bytes);
            self.make_loaded_room(simulator_id, row_increment, byte_increment);
            let rows_after = self.loaded_entries.saturating_add(row_increment);
            let bytes_after_global = self.loaded_bytes.saturating_sub(current_bytes).saturating_add(entry.payload.len());
            if rows_after > MAX_TOTAL_ENTRIES || bytes_after_global > MAX_TOTAL_BYTES {
                continue;
            }
            let last_used = self.next_use();
            if let Some(region) = self.loaded_regions.get_mut(&simulator_id) {
                region.bytes = bytes_after;
                region.last_used = last_used;
                region.entries.insert(entry.local_id, stored_entry(entry));
            }
            self.loaded_entries = rows_after;
            self.loaded_bytes = bytes_after_global;
        }
    }

    pub(super) fn ensure_loaded_region(&mut self, simulator_id: SimulatorId, scope: CacheScope) {
        if self.loaded_regions.get(&simulator_id).is_some_and(|region| region.scope == scope) {
            return;
        }
        self.remove_loaded_region(simulator_id);
        let last_used = self.next_use();
        self.loaded_regions.insert(
            simulator_id,
            LoadedRegion { scope, entries: HashMap::new(), overrides: HashMap::new(), bytes: 0, last_used, complete: false },
        );
    }

    pub(super) fn merge_partial_region(&mut self, simulator_id: SimulatorId, scope: &CacheScope, stored: &mut StoredScope) {
        let Some(partial) = self.loaded_regions.remove(&simulator_id) else { return };
        self.loaded_entries = self.loaded_entries.saturating_sub(partial.entries.len() + partial.overrides.len());
        self.loaded_bytes = self.loaded_bytes.saturating_sub(partial.bytes);
        if partial.scope != *scope {
            return;
        }
        let mut override_bytes = stored.overrides.values().map(|stored| stored.payload.len()).sum::<usize>();
        for (local_id, latest) in partial.overrides {
            let previous_bytes = stored.overrides.get(&local_id).map_or(0, |previous| previous.payload.len());
            let bytes_after = override_bytes.saturating_sub(previous_bytes).saturating_add(latest.payload.len());
            if (previous_bytes > 0 || stored.overrides.len() < MAX_REGION_OVERRIDES) && bytes_after <= MAX_REGION_OVERRIDE_BYTES {
                stored.overrides.insert(local_id, latest);
                override_bytes = bytes_after;
            }
        }
        let entries = &mut stored.entries;
        let mut bytes = entries
            .values()
            .map(|entry| usize::try_from(entry.payload_bytes).unwrap_or(usize::MAX))
            .try_fold(0_usize, usize::checked_add)
            .unwrap_or(usize::MAX);
        for (local_id, latest) in partial.entries {
            let previous = entries.remove(&local_id);
            let previous_bytes = previous.as_ref().map_or(0, |entry| usize::try_from(entry.payload_bytes).unwrap_or(usize::MAX));
            let latest_bytes = usize::try_from(latest.payload_bytes).unwrap_or(usize::MAX);
            let Some(bytes_after) = bytes.checked_sub(previous_bytes).and_then(|bytes| bytes.checked_add(latest_bytes)) else { continue };
            if (previous.is_some() || entries.len() < MAX_REGION_ENTRIES) && bytes_after <= MAX_REGION_BYTES {
                entries.insert(local_id, latest);
                bytes = bytes_after;
            } else if let Some(previous) = previous {
                entries.insert(local_id, previous);
            }
        }
    }

    pub(super) fn install_loaded_region(&mut self, simulator_id: SimulatorId, scope: CacheScope, stored: StoredScope) {
        let StoredScope { entries, overrides } = stored;
        if entries.len() > MAX_REGION_ENTRIES || overrides.len() > MAX_REGION_OVERRIDES {
            return;
        }
        let entry_bytes =
            entries.values().map(|entry| usize::try_from(entry.payload_bytes).unwrap_or(usize::MAX)).try_fold(0_usize, usize::checked_add);
        let Some(entry_bytes) = entry_bytes.filter(|bytes| *bytes <= MAX_REGION_BYTES) else { return };
        let override_bytes = overrides.values().map(|stored| stored.payload.len()).sum::<usize>();
        if override_bytes > MAX_REGION_OVERRIDE_BYTES {
            return;
        }
        let rows = entries.len() + overrides.len();
        let bytes = entry_bytes + override_bytes;
        self.make_loaded_room(simulator_id, rows, bytes);
        if rows > MAX_TOTAL_ENTRIES
            || bytes > MAX_TOTAL_BYTES
            || self.loaded_entries.saturating_add(rows) > MAX_TOTAL_ENTRIES
            || self.loaded_bytes.saturating_add(bytes) > MAX_TOTAL_BYTES
        {
            return;
        }
        let last_used = self.next_use();
        self.loaded_entries += rows;
        self.loaded_bytes += bytes;
        self.loaded_regions.insert(simulator_id, LoadedRegion { scope, entries, overrides, bytes, last_used, complete: true });
    }

    pub(super) fn remove_loaded_region(&mut self, simulator_id: SimulatorId) {
        if let Some(region) = self.loaded_regions.remove(&simulator_id) {
            self.loaded_entries = self.loaded_entries.saturating_sub(region.entries.len() + region.overrides.len());
            self.loaded_bytes = self.loaded_bytes.saturating_sub(region.bytes);
        }
    }

    pub(super) fn remove_loaded_entries(&mut self, simulator_id: SimulatorId, local_ids: &HashSet<u32>) {
        let Some(region) = self.loaded_regions.get_mut(&simulator_id) else { return };
        for local_id in local_ids {
            if let Some(entry) = region.entries.remove(local_id) {
                let bytes = usize::try_from(entry.payload_bytes).unwrap_or(usize::MAX);
                region.bytes = region.bytes.saturating_sub(bytes);
                self.loaded_entries = self.loaded_entries.saturating_sub(1);
                self.loaded_bytes = self.loaded_bytes.saturating_sub(bytes);
            }
        }
    }

    fn touch_loaded_region(&mut self, simulator_id: SimulatorId, scope: &CacheScope) {
        let last_used = self.next_use();
        if let Some(region) = self.loaded_regions.get_mut(&simulator_id).filter(|region| &region.scope == scope) {
            region.last_used = last_used;
        }
    }

    pub(super) fn make_loaded_room(&mut self, current: SimulatorId, rows: usize, bytes: usize) {
        while (self.loaded_entries.saturating_add(rows) > MAX_TOTAL_ENTRIES || self.loaded_bytes.saturating_add(bytes) > MAX_TOTAL_BYTES)
            && let Some(oldest) = self
                .loaded_regions
                .iter()
                .filter(|(simulator_id, _)| **simulator_id != current)
                .min_by_key(|(_, region)| region.last_used)
                .map(|(simulator_id, _)| *simulator_id)
        {
            self.remove_loaded_region(oldest);
        }
    }

    pub(super) fn next_use(&mut self) -> u64 {
        self.use_clock = self.use_clock.saturating_add(1);
        self.use_clock
    }
}

#[cfg(test)]
mod tests {
    use network::Grid;
    use protocol::{lludp::Message, messages::CachedObjectReference};
    use rusqlite::params;
    use storage::sqlite::SqliteBatch;
    use uuid::Uuid;

    use super::*;
    use crate::session::object_cache::test_support::*;
    #[tokio::test(start_paused = true)]
    async fn payloads_survive_restart_as_hits() {
        let directory = tempfile::tempdir().expect("create isolated cache directory");
        let path = directory.path().join("objects.sqlite");
        let mut cache = open_cache(&path);
        assert_eq!(load_region(&mut cache).await, 7);
        assert_eq!(cache.region_handshake_flags(SIMULATOR), 7);

        for fixture in [vectors::OBJECT_UPDATE, vectors::OBJECT_UPDATE_COMPRESSED_TYPED] {
            let mut update = fixture_message(fixture);
            if let Message::ObjectUpdateCompressed(update) = &mut update {
                for object in &mut update.objects {
                    object.local_id = object.local_id.saturating_add(10_000);
                }
            }
            let (_, references) = cached_references(&update);
            cache.record_update(SIMULATOR, update.clone());
            flush(&mut cache).await;
            assert_eq!(cache.region_handshake_flags(SIMULATOR), 5);
            let cached = cached_message(cached_references(&update).0, references);
            let resolved = resolve(&mut cache, &cached, 40).await;
            assert_eq!(resolved.blocks.len(), cached.objects.len());
            assert!(
                resolved
                    .blocks
                    .iter()
                    .all(|operation| matches!(operation, Some(super::ObjectOperation::Observe(_) | super::ObjectOperation::Patch(_))))
            );
        }
        flush(&mut cache).await;
        cache.close().await;

        let mut reopened = open_cache(&path);
        assert_eq!(reopened.region_handshake_flags(SIMULATOR), 7);
        assert_eq!(load_region(&mut reopened).await, 5);
        assert_eq!(reopened.region_handshake_flags(SIMULATOR), 5);
        for fixture in [vectors::OBJECT_UPDATE, vectors::OBJECT_UPDATE_COMPRESSED_TYPED] {
            let mut update = fixture_message(fixture);
            if let Message::ObjectUpdateCompressed(update) = &mut update {
                for object in &mut update.objects {
                    object.local_id = object.local_id.saturating_add(10_000);
                }
            }
            let (region_data, references) = cached_references(&update);
            let resolved = resolve(&mut reopened, &cached_message(region_data, references), 41).await;
            assert!(
                resolved
                    .blocks
                    .iter()
                    .all(|operation| { matches!(operation, Some(super::ObjectOperation::Observe(_) | super::ObjectOperation::Patch(_))) })
            );
        }
        reopened.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn cached_hits_restore_media_requests() {
        let directory = tempfile::tempdir().expect("create isolated cache directory");
        let mut cache = open_cache(directory.path().join("objects.sqlite"));
        assert_eq!(load_region(&mut cache).await, 7);
        let object_id = Uuid::from_u128(0xabcde);
        let mut world = region_world();
        let region = world.region_for(SIMULATOR).expect("cached simulator has a registered region");

        for (index, fixture) in [vectors::OBJECT_UPDATE, vectors::OBJECT_UPDATE_COMPRESSED_TYPED].into_iter().enumerate() {
            let mut full = fixture_message(fixture);
            let local_id = 20_000 + u32::try_from(index).expect("two fixture indexes fit u32");
            match &mut full {
                Message::ObjectUpdate(update) => {
                    update.objects.truncate(1);
                    update.objects[0].pcode = 9;
                    update.objects[0].id = object_id;
                    update.objects[0].local_id = local_id;
                    update.objects[0].media_url = "x-mv:12/00000000-0000-0000-0000-000000000001".into();
                }
                Message::ObjectUpdateCompressed(update) => {
                    update.objects.truncate(1);
                    update.objects[0].pcode = 9;
                    update.objects[0].id = object_id;
                    update.objects[0].local_id = local_id;
                    update.objects[0].media_url = Some("x-mv:12/00000000-0000-0000-0000-000000000001".into());
                }
                _ => panic!("object update fixture should be full or compressed"),
            }
            let (region_data, references) = cached_references(&full);
            cache.record_update(SIMULATOR, full.clone());
            flush(&mut cache).await;

            let cached = cached_message(region_data, references);
            let sequence = 90 + u32::try_from(index).expect("two fixture indexes fit u32");
            let batch = decoded_cached_batch(region, &cached, sequence);
            let (resolved, requests, _) = cache
                .resolve_cached(SIMULATOR, region, sequence, OrderEvidence::Ordered(u64::from(sequence)), &mut world, &cached, batch)
                .await;

            assert_eq!(resolved.blocks.len(), 1);
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].object_id, object_id);
            assert_eq!(requests[0].local_id.get(), local_id);
            assert_eq!(requests[0].version, 12);
        }
        cache.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn hit_uses_reference_physics() {
        let directory = tempfile::tempdir().expect("create isolated cache directory");
        let mut cache = open_cache(directory.path().join("objects.sqlite"));
        assert_eq!(load_region(&mut cache).await, 7);
        let mut full = fixture_message(vectors::OBJECT_UPDATE);
        let Message::ObjectUpdate(update) = &mut full else { panic!("fixture should be a full object update") };
        update.objects.truncate(1);
        update.objects[0].update_flags |= FLAGS_USE_PHYSICS;
        update.region_data.region_handle = REGION_HANDLE;
        let object_id = update.objects[0].id;
        let (region_data, mut references) = cached_references(&full);
        references[0].update_flags &= !FLAGS_USE_PHYSICS;
        cache.record_update(SIMULATOR, full);
        flush(&mut cache).await;

        let cached = cached_message(region_data, references);
        let mut world = region_world();
        let region = world.region_for(SIMULATOR).expect("cached simulator has a registered region");
        let batch = decoded_cached_batch(region, &cached, 80);
        let (batch, _, _) = cache.resolve_cached(SIMULATOR, region, 80, OrderEvidence::Ordered(80), &mut world, &cached, batch).await;
        let summary = world.apply_object_batch(SIMULATOR, batch).expect("registered simulator applies the cached hit");
        assert!(summary.blocks.iter().all(|block| block.applied().is_some()));
        let entity = world.world().entity(object_id).expect("cached object was restored");
        assert!(!world.world().object(entity).expect("restored object is live").is_physical());
        cache.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn mismatch_and_miss_request_full() {
        let directory = tempfile::tempdir().expect("create isolated cache directory");
        let mut cache = open_cache(directory.path().join("objects.sqlite"));
        assert_eq!(load_region(&mut cache).await, 7);
        let full = fixture_message(vectors::OBJECT_UPDATE);
        cache.record_update(SIMULATOR, full.clone());
        flush(&mut cache).await;
        let (region_data, mut references) = cached_references(&full);
        let expected_crc = references[0].crc;
        references[0].crc = expected_crc.wrapping_add(1);
        references.push(CachedObjectReference { local_id: 0x7fff_fffe, crc: 1, update_flags: 0 });

        let resolved = resolve(&mut cache, &cached_message(region_data, references), 51).await;
        std::assert_matches!(
            resolved.blocks[0],
            Some(super::ObjectOperation::Cached { cache_miss_type: protocol::messages::CacheMissType::CrcMismatch, .. })
        );
        std::assert_matches!(
            resolved.blocks.last(),
            Some(Some(super::ObjectOperation::Cached { cache_miss_type: protocol::messages::CacheMissType::Full, .. }))
        );

        flush(&mut cache).await;
        let (region_data, mut references) = cached_references(&full);
        references.truncate(1);
        let resolved = resolve(&mut cache, &cached_message(region_data, references), 52).await;
        std::assert_matches!(
            resolved.blocks[0],
            Some(super::ObjectOperation::Cached { cache_miss_type: protocol::messages::CacheMissType::Full, .. })
        );
        cache.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn early_probes_are_misses() {
        let directory = tempfile::tempdir().expect("create isolated cache directory");
        let path = directory.path().join("objects.sqlite");
        let mut writer = open_cache(&path);
        let full = fixture_message(vectors::OBJECT_UPDATE);
        writer.record_update(SIMULATOR, full.clone());
        flush(&mut writer).await;
        writer.close().await;

        let mut cache = open_cache(&path);
        let (region_data, references) = cached_references(&full);
        let update = cached_message(region_data, references);
        let before_load = resolve(&mut cache, &update, 55).await;
        assert!(before_load.blocks.iter().all(|operation| {
            matches!(operation, Some(super::ObjectOperation::Cached { cache_miss_type: protocol::messages::CacheMissType::Full, .. }))
        }));

        assert_eq!(load_region(&mut cache).await, 5);
        let after_load = resolve(&mut cache, &update, 56).await;
        assert!(
            after_load
                .blocks
                .iter()
                .all(|operation| { matches!(operation, Some(super::ObjectOperation::Observe(_) | super::ObjectOperation::Patch(_))) })
        );
        cache.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn corrupt_entries_miss_and_delete() {
        let directory = tempfile::tempdir().expect("create isolated cache directory");
        let path = directory.path().join("objects.sqlite");
        let mut cache = open_cache(&path);
        let full = fixture_message(vectors::OBJECT_UPDATE);
        cache.record_update(SIMULATOR, full.clone());
        flush(&mut cache).await;
        cache.close().await;

        let mut cache = open_cache(&path);
        let (region_data, references) = cached_references(&full);
        let corrupt_local_id = references[0].local_id;
        let corrupt = SqliteBatch::new(1, 64, move |transaction| {
            transaction.execute(
                "UPDATE object_entries SET payload = zeroblob(length(payload))
             WHERE grid_id = ?1 AND region_id = ?2 AND cache_id = ?3 AND local_id = ?4",
                params![Grid::Agni.id(), REGION_ID.as_bytes().as_slice(), CACHE_ID.as_bytes().as_slice(), i64::from(corrupt_local_id)],
            )?;
            Ok(())
        })
        .expect("corruption batch fits SQLite limits");
        cache.db.as_ref().expect("cache database is open").submit_batches(vec![corrupt]).expect("queue corrupt fixture");
        flush(&mut cache).await;
        assert_eq!(load_region(&mut cache).await, 5);

        let first = cached_message(region_data, references[..1].to_vec());
        let resolved = resolve(&mut cache, &first, 60).await;
        std::assert_matches!(
            resolved.blocks[0],
            Some(super::ObjectOperation::Cached { cache_miss_type: protocol::messages::CacheMissType::Full, .. })
        );
        flush(&mut cache).await;
        assert_eq!(cache_count(cache.db.as_ref().expect("cache database is open")).await, references.len() - 1);
        cache.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn semantically_invalid_entries_are_removed() {
        let directory = tempfile::tempdir().expect("create isolated cache directory");
        let mut cache = open_cache(directory.path().join("objects.sqlite"));
        let mut full = fixture_message(vectors::OBJECT_UPDATE);
        let Message::ObjectUpdate(update) = &mut full else { panic!("object fixture should be a full update") };
        update.objects[0].id = Uuid::nil();
        let (region_data, references) = cached_references(&full);
        cache.record_update(SIMULATOR, full);
        flush(&mut cache).await;
        assert_eq!(load_region(&mut cache).await, 5);

        let resolved = resolve(&mut cache, &cached_message(region_data, references.clone()), 61).await;

        std::assert_matches!(
            resolved.blocks[0],
            Some(super::ObjectOperation::Cached { cache_miss_type: protocol::messages::CacheMissType::Full, .. })
        );
        flush(&mut cache).await;
        assert_eq!(cache_count(cache.db.as_ref().expect("cache database is open")).await, references.len() - 1);
        cache.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn terse_updates_evict_cached_entries() {
        let directory = tempfile::tempdir().expect("create isolated cache directory");
        let mut cache = open_cache(directory.path().join("objects.sqlite"));
        let mut full = fixture_message(vectors::OBJECT_UPDATE);
        let Message::ObjectUpdate(update) = &mut full else { panic!("object fixture should be a full update") };
        update.objects.truncate(1);
        let local_id = update.objects[0].local_id;
        let (region_data, references) = cached_references(&full);
        cache.record_update(SIMULATOR, full);
        flush(&mut cache).await;
        assert_eq!(cache_count(cache.db.as_ref().expect("cache database is open")).await, 1);
        assert_eq!(load_region(&mut cache).await, 5);

        let mut terse = fixture_message(vectors::IMPROVED_TERSE_OBJECT_UPDATE);
        let Message::ImprovedTerseObjectUpdate(update) = &mut terse else { panic!("terse fixture should be a terse update") };
        update.region_data.region_handle = region_data.region_handle;
        update.objects[0].as_decoded_mut().expect("terse fixture object is valid").local_id = local_id;
        cache.record_update(SIMULATOR, terse);
        flush(&mut cache).await;

        assert_eq!(cache_count(cache.db.as_ref().expect("cache database is open")).await, 0);
        let resolved = resolve(&mut cache, &cached_message(region_data, references), 62).await;
        std::assert_matches!(
            resolved.blocks[0],
            Some(super::ObjectOperation::Cached { cache_miss_type: protocol::messages::CacheMissType::Full, .. })
        );
        cache.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn cache_id_change_invalidates_scope() {
        let directory = tempfile::tempdir().expect("create isolated cache directory");
        let mut cache = open_cache(directory.path().join("objects.sqlite"));
        assert_eq!(load_region(&mut cache).await, 7);
        let full = fixture_message(vectors::OBJECT_UPDATE);
        cache.record_update(SIMULATOR, full.clone());
        flush(&mut cache).await;
        assert_eq!(cache.region_handshake_flags(SIMULATOR), 5);

        cache.set_region_identity(SIMULATOR, &region_handshake(NEXT_CACHE_ID));
        assert_eq!(cache.region_handshake_flags(SIMULATOR), 7);
        flush(&mut cache).await;
        cache.set_region_identity(SIMULATOR, &region_handshake(CACHE_ID));
        assert_eq!(load_region(&mut cache).await, 7);
        let (region_data, references) = cached_references(&full);
        let result = resolve(&mut cache, &cached_message(region_data, references[..1].to_vec()), 71).await;
        std::assert_matches!(
            result.blocks[0],
            Some(super::ObjectOperation::Cached { cache_miss_type: protocol::messages::CacheMissType::Full, .. })
        );
        cache.close().await;
    }
}
