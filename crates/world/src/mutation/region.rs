use crate::history::WorldChange::{RegionRegistered, RegionRetired, RegionUpdated};
use crate::update::FieldStamp;
use crate::{ApplyError, Region, RegionApplied, RegionInfo, RegionOutcome, UpdateSource};
use crate::{RegionState, World, newer_field};

impl World {
    pub fn register_region(&mut self) -> Result<Region, ApplyError> {
        if self.regions.len() >= self.limits.max_regions {
            return Err(ApplyError::RegionBudgetExhausted);
        }
        self.ensure_revision_available()?;
        let generation = self.last_region_generation.checked_add(1).ok_or(ApplyError::RegionGenerationExhausted)?;
        let region = Region::new(self.id, generation);
        self.regions.insert(region, RegionState::default());
        self.last_region_generation = generation;
        self.publish_changes(vec![RegionRegistered(region)]).expect("registering a region journals a change");
        Ok(region)
    }
    pub fn patch_region(&mut self, region: Region, info: RegionInfo) -> Result<RegionApplied, ApplyError> {
        self.require_active_region(region)?;
        self.validate_region_info(&info)?;
        let current = self.regions.get(&region).expect("active region remains registered");
        let merged = self.merge_region_info(&current.info, &info)?;
        self.commit_region_metadata(region, merged, current.name_fence)
    }
    pub fn observe_region_metadata(&mut self, source: UpdateSource, mut info: RegionInfo) -> Result<RegionApplied, ApplyError> {
        let region = source.region();
        self.require_active_region(region)?;
        self.validate_region_info(&info)?;
        let current = self.regions.get(&region).expect("active region remains registered");
        let name = info.name.take();
        let mut merged = self.merge_region_info(&current.info, &info)?;
        let mut name_fence = current.name_fence;
        if let (Some(name), Some(stamp)) = (name, source.stamp()) {
            let stamp = FieldStamp { authority: source.authority(), stamp };
            if newer_field(stamp, name_fence) {
                merged.name = Some(name);
                name_fence = Some(stamp);
            }
        }
        self.commit_region_metadata(region, merged, name_fence)
    }
    fn commit_region_metadata(
        &mut self,
        region: Region,
        info: RegionInfo,
        name_fence: Option<FieldStamp>,
    ) -> Result<RegionApplied, ApplyError> {
        let current = self.regions.get(&region).expect("metadata commit requires an active region");
        if info == current.info {
            self.regions.get_mut(&region).expect("active region remains registered").name_fence = name_fence;
            return Ok(RegionApplied::unchanged());
        }
        self.ensure_revision_available()?;
        let current = self.regions.get_mut(&region).expect("active region remains registered");
        current.info = info;
        current.name_fence = name_fence;
        Ok(self.publish_region_patch(RegionUpdated(region)))
    }
    pub fn retire_region(&mut self, region: Region) -> Result<RegionApplied, ApplyError> {
        if !self.region_was_issued(region) {
            return Err(ApplyError::StaleRegion);
        }
        if !self.regions.contains_key(&region) {
            return Ok(RegionApplied::unchanged());
        }
        self.require_active_region(region)?;
        if !self.retired_region_capacity_available() {
            return Err(ApplyError::RemovalCapacityExhausted);
        }
        self.ensure_revision_available()?;
        let removed = self.store.region_ids(region).map_or(0, |ids| ids.len());
        self.schedule_region_retirement(region)?;
        let removed_region = self.regions.remove(&region).expect("active region remains registered");
        self.retiring_regions.insert(region, removed_region);
        let revision =
            self.publish_changes(vec![RegionRetired { region, entities_removed: removed }]).expect("retiring a region journals a change");
        Ok(RegionApplied { outcome: RegionOutcome::Retired { revision, entities_removed: removed }, recovery_cancellations: Vec::new() })
    }
    fn validate_region_info(&self, info: &RegionInfo) -> Result<(), ApplyError> {
        if info.region_id.is_some_and(|id| id.is_nil()) {
            return Err(ApplyError::InvalidValue { field: "region_id" });
        }
        if info.name.as_ref().is_some_and(|name| name.len() > self.limits.max_region_name_bytes) {
            return Err(ApplyError::InvalidValue { field: "region_name" });
        }
        if info.origin.is_some_and(|origin| !origin.is_finite()) {
            return Err(ApplyError::InvalidValue { field: "region_origin" });
        }
        if info.handle.zip(info.origin).is_some_and(|(handle, origin)| handle.origin() != origin) {
            return Err(ApplyError::InvalidValue { field: "region_origin" });
        }
        Ok(())
    }
    fn merge_region_info(&self, current: &RegionInfo, incoming: &RegionInfo) -> Result<RegionInfo, ApplyError> {
        fn merge<T: Copy + PartialEq>(current: Option<T>, incoming: Option<T>, field: &'static str) -> Result<Option<T>, ApplyError> {
            match (current, incoming) {
                (Some(a), Some(b)) if a != b => Err(ApplyError::InvalidValue { field }),
                (Some(a), _) => Ok(Some(a)),
                (None, incoming) => Ok(incoming),
            }
        }
        let region_id = merge(current.region_id, incoming.region_id, "region_id")?;
        let handle = merge(current.handle, incoming.handle, "region_handle")?;
        let origin = merge(current.origin, incoming.origin, "region_origin")?;
        let name = match (&current.name, &incoming.name) {
            (Some(a), Some(b)) if a != b => return Err(ApplyError::InvalidValue { field: "region_name" }),
            (Some(a), _) => Some(a.clone()),
            (None, Some(b)) => Some(b.clone()),
            (None, None) => None,
        };
        let merged = RegionInfo { region_id, handle, name, origin };
        self.validate_region_info(&merged)?;
        Ok(merged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;

    use crate::{
        ApplyError::{RegionGenerationExhausted, RevisionExhausted},
        ApplyOutcome, DeferredReason, LocalAddress, ObjectPatch, PbrMaterialOverrides, WorldLimits, WorldRevision,
    };
    fn limits() -> WorldLimits {
        WorldLimits {
            max_regions: 2,
            max_entities: 2,
            max_entities_per_region: 2,
            max_tombstones_per_region: 2,
            max_tombstones: 2,
            max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(2),
            max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(2),
            max_journal_entries: 2,
            max_journal_changes: 4,
            max_region_name_bytes: 16,
            max_recovery_targets: 2,
        }
    }
    #[test]
    fn failed_name_commit_does_not_advance_ordering() {
        let mut world = World::new(limits());
        let region = world.register_region().unwrap();
        let source = |order| UpdateSource::new(region, order as u32, crate::OrderEvidence::Ordered(order), 0);
        let name = |name: &str| RegionInfo { name: Some(name.into()), ..RegionInfo::default() };
        let _ = world.observe_region_metadata(source(10), name("First")).unwrap();
        let revision = world.revision();
        world.journal.revision = WorldRevision(u64::MAX);
        assert_eq!(world.observe_region_metadata(source(30), name("Rejected")), Err(RevisionExhausted));
        assert_eq!(world.region(region).unwrap().name.as_deref(), Some("First"));
        world.journal.revision = revision;
        let _ = world.observe_region_metadata(source(20), name("Next")).unwrap();
        assert_eq!(world.region(region).unwrap().name.as_deref(), Some("Next"));
    }
    #[test]
    fn revision_exhaustion_keeps_region_active() {
        let mut world = World::new(limits());
        let region = world.register_region().unwrap();
        world.journal.revision = WorldRevision(u64::MAX);
        assert!(matches!(world.retire_region(region), Err(RevisionExhausted)));
        assert!(world.region(region).is_some());
    }

    #[test]
    fn registration_generation_exhaustion_is_atomic() {
        let mut world = World::new(limits());
        let existing = world.register_region().unwrap();
        let active = world.regions.len();
        let revision = world.revision();
        let journal_len = world.journal.entries.len();
        world.last_region_generation = u64::MAX;

        assert_eq!(world.register_region(), Err(RegionGenerationExhausted));
        assert_eq!(world.regions.len(), active);
        assert!(world.region(existing).is_some());
        assert_eq!(world.revision(), revision);
        assert_eq!(world.journal.entries.len(), journal_len);
        assert_eq!(world.last_region_generation, u64::MAX);
    }

    #[test]
    fn registration_revision_exhaustion_is_atomic() {
        let mut world = World::new(limits());
        let existing = world.register_region().unwrap();
        let active = world.regions.len();
        let generation = world.last_region_generation;
        let journal_len = world.journal.entries.len();
        world.journal.revision = WorldRevision(u64::MAX);

        assert_eq!(world.register_region(), Err(RevisionExhausted));
        assert_eq!(world.regions.len(), active);
        assert!(world.region(existing).is_some());
        assert_eq!(world.last_region_generation, generation);
        assert_eq!(world.journal.entries.len(), journal_len);
        assert_eq!(world.revision(), WorldRevision(u64::MAX));
    }

    #[test]
    fn retirement_clears_retained_overrides() {
        let mut world = World::new(limits());
        let region = world.register_region().unwrap();
        let address = LocalAddress::new(region, NonZeroU32::new(1).unwrap());
        let patch = |order| ObjectPatch {
            pbr_material_overrides: Some(PbrMaterialOverrides::default()),
            ..ObjectPatch::new(UpdateSource::new(region, order, crate::OrderEvidence::Ordered(u64::from(order)), 0), address.local_id())
        };
        assert_eq!(world.patch_object(patch(1)).unwrap().outcome, ApplyOutcome::Deferred(DeferredReason::UnknownObject));

        std::assert_matches!(world.retire_region(region).unwrap().outcome, RegionOutcome::Retired { entities_removed: 0, .. });
        assert_eq!(world.patch_object(patch(2)), Err(ApplyError::RegionRetired));
        assert_eq!(world.check_invariants(), Ok(()));
        while world.has_pending_removals() {
            let progress = world.reclaim_removals(crate::MAX_REMOVAL_WORK_PER_TURN);
            assert!(progress.work_units <= crate::MAX_REMOVAL_WORK_PER_TURN);
        }

        assert!(!world.retained.contains(&address));
        assert_eq!(world.check_invariants(), Ok(()));
    }
}
