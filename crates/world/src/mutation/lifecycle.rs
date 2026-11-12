use std::num::NonZeroU32;

use super::Effects;
use crate::UpdateAuthority::Untracked;
use crate::removal::RemovalCause;
use crate::update::{FieldStamp, Stamp};
use crate::{Applied, ApplyError, ApplyOutcome, DeferredReason, Entity, LocalAddress, Region, UpdateAuthority, UpdateSource};
use crate::{World, newer_field};

impl World {
    pub fn remove_object(&mut self, source: UpdateSource, local_id: NonZeroU32) -> Result<Applied, ApplyError> {
        self.require_active_region(source.region)?;
        if source.authority() == Untracked {
            return Ok(Applied::unchanged());
        }
        let address = LocalAddress::new(source.region, local_id);
        let live = self.entity_at(address).map(|entity| {
            let record = self.store.object_record(entity).expect("address index contains a live entity");
            (entity, record.id, record.state.binding.fence())
        });
        if live.is_some_and(|(_, id, _)| self.self_agent == Some(id)) {
            return Ok(Applied::unchanged());
        }
        let Some(stamp) = source.stamp() else {
            return Ok(self.defer(DeferredReason::AmbiguousOrder, address));
        };
        if let Some((entity, id, binding_fence)) = live {
            if stamp <= binding_fence {
                return Ok(Applied::unchanged());
            }
            self.ensure_revision_available()?;
            let mut effects = Effects::default();
            self.schedule_removal(entity, RemovalCause::Killed(stamp), None, &mut effects.changes, &mut effects.recovery_cancellations)?;
            return Ok(self.finish(ApplyOutcome::Removed(id), effects));
        }
        if self.removal_cause_at(address).and_then(RemovalCause::stamp).is_some_and(|fence| stamp <= fence) {
            return Ok(Applied::unchanged());
        }
        if let Some(tombstone) = self.tombstones.get(&address) {
            if stamp <= *tombstone {
                return Ok(Applied::unchanged());
            }
        } else if self.floors.get(&source.region).is_some_and(|floor| stamp <= *floor) {
            return Ok(self.defer(DeferredReason::AmbiguousOrder, address));
        }
        self.retained.remove_older(address, stamp);
        if self.tombstone_would_change(address, stamp) && self.insert_tombstone(address, stamp) {
            return Ok(Applied::new(ApplyOutcome::TombstoneRecorded(address)));
        }
        Ok(Applied::unchanged())
    }

    pub(super) fn remove_entity(
        &mut self,
        entity: Entity,
        cause: RemovalCause,
        preserve_child: Option<Entity>,
        effects: &mut Effects,
    ) -> Result<(), ApplyError> {
        self.schedule_removal(entity, cause, preserve_child, &mut effects.changes, &mut effects.recovery_cancellations)
    }

    pub(crate) fn advance_parent_fence(&mut self, child: Entity, invalidation: Option<Stamp>, invalidation_region: Region) {
        let Some(stamp) = invalidation
            .filter(|_| self.store.object_record(child).is_some_and(|record| record.state.binding.address.region == invalidation_region))
        else {
            return;
        };
        let ordering = self.store.ordering_mut(child.0);
        let field_stamp = FieldStamp { authority: UpdateAuthority::Tracked, stamp };
        if newer_field(field_stamp, ordering.fences.parent) {
            ordering.fences.parent = Some(field_stamp);
        }
    }
}
