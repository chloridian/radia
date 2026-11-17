use std::{
    borrow::Cow,
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

#[cfg(test)]
use network::Event;
use network::{AgentSession, PacketOrder, SimulatorId};
use protocol::lludp::Message;
use protocol::messages::{AgentMovementComplete, ParcelProperties};
use uuid::Uuid;
use world::{
    Applied, Entity, LocalAddress, MAX_OBJECT_LOOKUP_WORK_ITEMS, ObjectLookupCursor, ObjectLookupProgress, ObjectObservation,
    OrderEvidence, OrderEvidence::Ordered, ParcelAccessSettings, ParcelBitmap, ParcelFlags, ParcelLocalId, ParcelOverlayObservation,
    ParcelPropertiesData, ParcelPropertiesObservation, RegionHandle, UpdateSource,
};

use super::{PresentationMailbox, ViewerUpdate};
use crate::session::{
    RegionLookup, display_message,
    inbound::{
        AvatarAppearanceMessage, ObjectBatch, ObjectMediaRequest, ObjectOperation, SitTargetUpdate, decode_avatar_appearance_update,
        decode_object_message, decode_sit_target_update,
    },
    object_cache::{CachedObjectContinuation, ObjectCacheSession},
    world::{IngestStatus, IngestSummary, ObjectBatchContinuation, RegistrationError, SessionWorld, ValueRejection},
};

const MAX_HANDOFF_OBSERVATIONS: usize = 32;
const MAX_PENDING_MOVEMENT_COMPLETES: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PendingWorldHandoff {
    pub(super) source: SimulatorId,
    pub(super) target: SimulatorId,
}

#[derive(Default)]
pub(super) struct WorldHandoff {
    world_primary: Option<SimulatorId>,
    promoted_target: Option<PendingWorldHandoff>,
    observations: HashMap<SimulatorId, ObjectObservation>,
    pending_movement_completes: HashMap<SimulatorId, PendingMovementComplete>,
}

struct PendingMovementComplete {
    movement: AgentMovementComplete,
    received_at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MovementCompleteOutcome {
    Applied,
    Held,
    Rejected,
}

#[derive(Clone, Copy)]
pub(super) enum SimulatorScope {
    Primary,
    Neighbor,
}

pub(super) struct SimulatorMessageOutcome {
    pub(super) summary: Option<(SimulatorId, IngestSummary)>,
    pub(super) touched_agent: bool,
    pub(super) agent_entity: Option<Entity>,
}

pub(super) enum ObjectUpdateFinishProgress {
    Pending,
    Complete(SimulatorMessageOutcome),
}

#[derive(Clone, Copy)]
enum ObjectUpdateFinishStage {
    ObservationId,
    ObservationAddress,
    CompleteObservation,
    Agent,
    Complete,
}

pub(super) struct ObjectUpdateFinishContinuation {
    simulator_id: SimulatorId,
    agent_id: Option<Uuid>,
    batch_summary: IngestSummary,
    observation: Option<ObjectObservation>,
    observation_entity: Option<Entity>,
    extra_summary: Option<IngestSummary>,
    agent_entity: Option<Entity>,
    stage: ObjectUpdateFinishStage,
    observation_id_cursor: Option<ObjectLookupCursor>,
    observation_address_cursor: Option<ObjectLookupCursor>,
    agent_cursor: Option<ObjectLookupCursor>,
}

pub(super) enum EntityUpdateProgress {
    Pending,
    Complete(SimulatorMessageOutcome),
}

pub(super) enum PromotionProgress {
    Pending,
    Complete(Option<IngestSummary>),
}

#[derive(Clone, Copy)]
enum EntityUpdateStage {
    Identity,
    Address,
    Apply,
}

#[derive(Clone, Copy)]
enum PromotionStage {
    Identity,
    Address,
    Complete,
}

pub(super) struct EntityUpdateContinuation {
    simulator_id: SimulatorId,
    update: Option<DecodedWorldUpdate>,
    entity: Option<Entity>,
    address: Option<LocalAddress>,
    entity_cursor: Option<ObjectLookupCursor>,
    address_cursor: Option<ObjectLookupCursor>,
    stage: EntityUpdateStage,
}

impl EntityUpdateContinuation {
    fn new(simulator_id: SimulatorId, update: DecodedWorldUpdate) -> Self {
        Self {
            simulator_id,
            update: Some(update),
            entity: None,
            address: None,
            entity_cursor: None,
            address_cursor: None,
            stage: EntityUpdateStage::Identity,
        }
    }
}

pub(super) struct PromotionContinuation {
    target: SimulatorId,
    observation: ObjectObservation,
    entity: Option<Entity>,
    entity_cursor: Option<ObjectLookupCursor>,
    address_cursor: Option<ObjectLookupCursor>,
    stage: PromotionStage,
}

pub(super) enum DecodedWorldUpdate {
    Object(ObjectBatch),
    AvatarAppearance { source: UpdateSource, update: AvatarAppearanceMessage },
    SitTarget { source: UpdateSource, update: SitTargetUpdate },
    ParcelOverlay { source: UpdateSource, observation: ParcelOverlayObservation },
    ParcelProperties { source: UpdateSource, observation: ParcelPropertiesObservation },
    ParcelMalformed,
    Malformed { invalid_blocks: usize, invalid_fields: usize },
}

pub(super) enum ObjectUpdatePreparation {
    Cached(CachedObjectContinuation),
    Ready { continuation: ObjectBatchContinuation, media_requests: Vec<ObjectMediaRequest> },
    Entity(EntityUpdateContinuation),
}

pub(super) fn route_recovery_targets(
    world: &SessionWorld,
    targets: impl Iterator<Item = (LocalAddress, protocol::messages::CacheMissType)>,
) -> Vec<(Option<SimulatorId>, LocalAddress, protocol::messages::CacheMissType)> {
    targets.map(|(address, cache_miss_type)| (world.simulator_for_region(address.region()), address, cache_miss_type)).collect()
}

pub(super) fn decode_simulator_world_update(
    world: &mut SessionWorld,
    simulator_id: SimulatorId,
    sequence: u32,
    packet_order: PacketOrder,
    received_at: Instant,
    ping_delay: Option<Duration>,
    message: &Message,
) -> Option<DecodedWorldUpdate> {
    let region = world.region_for(simulator_id)?;
    if let Message::RegionHandshake(handshake) = message {
        let extended = (!handshake.region_info4.is_empty())
            .then(|| handshake.region_info4.iter().fold(0_u64, |flags, block| flags | block.region_flags_extended));
        if world
            .observe_region_flags(
                UpdateSource::new(region, sequence, order_evidence(packet_order), 0),
                Some(handshake.region_info1.region_flags),
                extended,
            )
            .is_err()
        {
            record_parcel_rejection(world, simulator_id, region);
        }
        if let Err(error) = world
            .update_water_height(UpdateSource::new(region, sequence, order_evidence(packet_order), 0), handshake.region_info1.water_height)
        {
            tracing::debug!(simulator = simulator_id.get(), region = region.get(), %error, "could not update region water height");
        }
        if let Err(error) = world.update_terrain_settings(
            UpdateSource::new(region, sequence, order_evidence(packet_order), 0),
            handshake.region_info1.terrain_detail,
            handshake.region_info1.terrain_start_heights,
            handshake.region_info1.terrain_height_ranges,
        ) {
            tracing::warn!(simulator = simulator_id.get(), region = region.get(), %error, "could not update region terrain settings");
        }
        let region_id = handshake.region_info2.as_ref().map(|info| info.region_id).filter(|id| !id.is_nil());
        match world.update_handshake_metadata(
            UpdateSource::new(region, sequence, order_evidence(packet_order), 0),
            region_id,
            handshake.region_info1.sim_name.clone(),
        ) {
            Ok(()) => tracing::info!(
                simulator = simulator_id.get(),
                region = region.get(),
                name = ?handshake.region_info1.sim_name,
                "region handshake"
            ),
            Err(error) => {
                tracing::warn!(simulator = simulator_id.get(), region = region.get(), %error, "could not update region handshake metadata")
            }
        }
    }
    if let Message::RegionInfo(info) = message {
        let extended = info.region_info3.iter().filter_map(|block| block.region_flags_extended).reduce(|a, b| a | b);
        if world
            .observe_region_flags(
                UpdateSource::new(region, sequence, order_evidence(packet_order), 0),
                info.region_info.region_flags,
                extended,
            )
            .is_err()
        {
            record_parcel_rejection(world, simulator_id, region);
        }
    }

    if let Message::ParcelOverlay(message) = message {
        return Some(DecodedWorldUpdate::ParcelOverlay {
            source: UpdateSource::new(region, sequence, order_evidence(packet_order), 0),
            observation: ParcelOverlayObservation { sequence_id: message.sequence_id, data: message.data.clone() },
        });
    }
    if let Message::ParcelProperties(message) = message {
        return Some(match parcel_properties_observation(message) {
            Ok(observation) => DecodedWorldUpdate::ParcelProperties {
                source: UpdateSource::new(region, sequence, order_evidence(packet_order), 0),
                observation,
            },
            Err(()) => DecodedWorldUpdate::ParcelMalformed,
        });
    }

    if let Some(update) = decode_sit_target_update(message) {
        let source = UpdateSource::new(region, sequence, order_evidence(packet_order), 0);
        return Some(DecodedWorldUpdate::SitTarget { source, update });
    }
    match decode_avatar_appearance_update(message) {
        Ok(Some(update)) => {
            let source = UpdateSource::new(region, sequence, order_evidence(packet_order), 0);
            return Some(DecodedWorldUpdate::AvatarAppearance { source, update });
        }
        Ok(None) => {}
        Err(_) => return Some(DecodedWorldUpdate::Malformed { invalid_blocks: 1, invalid_fields: 0 }),
    }
    decode_object_message(region, sequence, order_evidence(packet_order), message).map(|mut batch| {
        batch.stamp_motion(received_at, ping_delay);
        DecodedWorldUpdate::Object(batch)
    })
}

fn record_parcel_rejection(world: &mut SessionWorld, simulator_id: SimulatorId, region: world::Region) {
    if let Some(rejected) = world.record_parcel_rejection() {
        tracing::debug!(simulator = simulator_id.get(), region = region.get(), rejected, "parcel updates were repeatedly rejected");
    }
}

#[cfg(test)]
async fn preflight_entity_address(world: &mut SessionWorld, id: Uuid) -> Option<Entity> {
    let entity = world.lookup_entity(id).await?;
    if let Some(address) = world.world().object(entity).map(|object| object.address()) {
        let _ = world.lookup_entity_at(address).await;
    }
    Some(entity)
}

fn address_for_resolved_entity(world: &SessionWorld, simulator_id: SimulatorId, entity: Option<Entity>) -> Option<LocalAddress> {
    entity
        .and_then(|entity| world.world().object(entity).map(|object| object.address()))
        .filter(|address| world.region_for(simulator_id) == Some(address.region()))
}

impl WorldHandoff {
    pub(super) fn session_ready(&mut self, world: &mut SessionWorld, simulator_id: SimulatorId, agent_id: Uuid) {
        let region = world.region_for(simulator_id).map(|region| region.get());
        world.set_self_agent(simulator_id, agent_id);
        self.world_primary = Some(simulator_id);
        match world.register_simulator(simulator_id, None) {
            Ok(_) => {}
            Err(RegistrationError::HandleChanged) => {
                tracing::error!(simulator = simulator_id.get(), region, "primary simulator handle changed during one connection lifetime");
            }
            Err(RegistrationError::Apply(error)) => {
                tracing::error!(simulator = simulator_id.get(), region, %error, "could not register primary world region")
            }
        }
    }

    pub(super) fn simulator_connected(&mut self, world: &mut SessionWorld, simulator_id: SimulatorId, region_handle: Option<u64>) {
        let region = world.region_for(simulator_id).map(|region| region.get());
        let handle = region_handle.map(RegionHandle::new);
        match world.register_simulator(simulator_id, handle) {
            Ok(_) => {}
            Err(RegistrationError::HandleChanged) => {
                tracing::warn!(simulator = simulator_id.get(), region, "simulator handle changed during one connection lifetime");
            }
            Err(RegistrationError::Apply(error)) => {
                tracing::warn!(simulator = simulator_id.get(), region, %error, "could not register simulator world region")
            }
        }
    }

    pub(super) fn movement_complete(
        &mut self,
        world: &mut SessionWorld,
        simulator_id: SimulatorId,
        session: Option<(Uuid, Uuid)>,
        movement: &AgentMovementComplete,
        received_at: Instant,
    ) -> MovementCompleteOutcome {
        let Some((agent_id, session_id)) = session else { return MovementCompleteOutcome::Rejected };
        if movement.agent_data.agent_id != agent_id || movement.agent_data.session_id != session_id {
            return MovementCompleteOutcome::Rejected;
        }
        let handle = RegionHandle::new(movement.data.region_handle);
        if self.world_primary == Some(simulator_id) || self.promoted_target.is_some_and(|handoff| handoff.target == simulator_id) {
            return if self.resolve_movement_region(world, simulator_id, handle) {
                MovementCompleteOutcome::Applied
            } else {
                MovementCompleteOutcome::Rejected
            };
        }
        if !movement_region_matches(world, simulator_id, handle) {
            return MovementCompleteOutcome::Rejected;
        }
        self.pending_movement_completes.insert(simulator_id, PendingMovementComplete { movement: movement.clone(), received_at });
        while self.pending_movement_completes.len() > MAX_PENDING_MOVEMENT_COMPLETES {
            if let Some(stale) = self.pending_movement_completes.keys().next().copied() {
                self.pending_movement_completes.remove(&stale);
            }
        }
        MovementCompleteOutcome::Held
    }

    pub(super) fn apply_promoted_movement_complete(
        &mut self,
        world: &mut SessionWorld,
        simulator_id: SimulatorId,
        session: Option<(Uuid, Uuid)>,
    ) -> Option<(AgentMovementComplete, Instant)> {
        let pending = self.pending_movement_completes.remove(&simulator_id)?;
        let (agent_id, session_id) = session?;
        if pending.movement.agent_data.agent_id != agent_id || pending.movement.agent_data.session_id != session_id {
            return None;
        }
        if self.world_primary != Some(simulator_id) && !self.promoted_target.is_some_and(|handoff| handoff.target == simulator_id) {
            return None;
        }
        self.resolve_movement_region(world, simulator_id, RegionHandle::new(pending.movement.data.region_handle))
            .then_some((pending.movement, pending.received_at))
    }

    fn resolve_movement_region(&self, world: &mut SessionWorld, simulator_id: SimulatorId, handle: RegionHandle) -> bool {
        let region = world.region_for(simulator_id).map(|region| region.get());
        match world.register_simulator(simulator_id, Some(handle)) {
            Ok(_) => true,
            Err(RegistrationError::HandleChanged) => {
                tracing::error!(simulator = simulator_id.get(), region, "primary simulator handle changed during movement completion");
                false
            }
            Err(RegistrationError::Apply(error)) => {
                tracing::warn!(simulator = simulator_id.get(), region, %error, "could not resolve primary simulator region from movement completion");
                false
            }
        }
    }

    pub(super) fn simulator_disconnected(&mut self, world: &mut SessionWorld, simulator_id: SimulatorId) {
        let region = world.region_for(simulator_id).map(|region| region.get());
        if let Err(error) = world.retire_simulator(simulator_id) {
            tracing::warn!(simulator = simulator_id.get(), region, %error, "could not retire simulator world region");
        }
        self.observations.remove(&simulator_id);
        self.pending_movement_completes.remove(&simulator_id);
        if self.promoted_target.is_some_and(|handoff| handoff.target == simulator_id) {
            self.promoted_target = None;
        }
    }

    #[cfg(test)]
    pub(super) fn apply_event(
        &mut self,
        world: &mut SessionWorld,
        event: &Event,
        agent_id: Option<Uuid>,
    ) -> Option<(SimulatorId, IngestSummary)> {
        use Event::{SessionReady, SimulatorConnected, SimulatorDisconnected, SimulatorFailed, SimulatorMessage};

        match event {
            SessionReady { simulator_id, session, .. } => {
                self.session_ready(world, *simulator_id, session.agent_id);
                None
            }
            SimulatorConnected { simulator_id, region_handle, .. } => {
                self.simulator_connected(world, *simulator_id, *region_handle);
                None
            }
            SimulatorMessage { simulator_id, sequence, packet_order, received_at, ping_delay, message, .. } => {
                let update =
                    decode_simulator_world_update(world, *simulator_id, *sequence, *packet_order, *received_at, *ping_delay, message)?;
                let entity = match &update {
                    DecodedWorldUpdate::SitTarget { update, .. } => world.world().entity(update.object_id),
                    DecodedWorldUpdate::AvatarAppearance { update, .. } => world.world().entity(update.object_id),
                    _ => None,
                };
                self.apply_update(world, *simulator_id, update, agent_id, entity)
            }
            SimulatorDisconnected { simulator_id, .. } | SimulatorFailed { simulator_id, .. } => {
                self.simulator_disconnected(world, *simulator_id);
                None
            }
            _ => None,
        }
    }

    #[cfg(test)]
    pub(super) async fn apply_message_cooperatively(
        &mut self,
        world: &mut SessionWorld,
        simulator_id: SimulatorId,
        sequence: u32,
        packet_order: PacketOrder,
        received_at: Instant,
        ping_delay: Option<Duration>,
        message: &Message,
        agent_id: Option<Uuid>,
        object_cache: &mut ObjectCacheSession,
        on_object_chunk: &mut impl FnMut(Vec<(Option<SimulatorId>, LocalAddress, protocol::messages::CacheMissType)>, Vec<LocalAddress>),
        media_requests: &mut Vec<ObjectMediaRequest>,
    ) -> Option<SimulatorMessageOutcome> {
        let update = decode_simulator_world_update(world, simulator_id, sequence, packet_order, received_at, ping_delay, message)?;
        let mut recovered_media_requests = Vec::new();
        let update = match (update, message) {
            (DecodedWorldUpdate::Object(batch), Message::ObjectUpdateCached(cached)) => {
                let region = world.region_for(simulator_id)?;
                let (batch, requests, overrides) =
                    object_cache.resolve_cached(simulator_id, region, sequence, order_evidence(packet_order), world, cached, batch).await;
                world.restore_cached_overrides(overrides);
                recovered_media_requests = requests;
                DecodedWorldUpdate::Object(batch)
            }
            (update, _) => update,
        };
        match update {
            DecodedWorldUpdate::Object(batch) => {
                let batch = self.filter_batch(world, simulator_id, agent_id, batch)?;
                let batch_summary = world
                    .apply_object_batch_cooperatively(simulator_id, batch, |targets, cancellations| {
                        on_object_chunk(targets, cancellations);
                    })
                    .await?;
                let summary = self.complete_observation_cooperatively(world, simulator_id).await;
                let agent_entity = if let Some(agent_id) = agent_id {
                    world.lookup_entity(agent_id).await.filter(|entity| {
                        batch_summary.touches_entity(*entity) || summary.as_ref().is_some_and(|summary| summary.touches_entity(*entity))
                    })
                } else {
                    None
                };
                media_requests.extend(recovered_media_requests);
                Some(SimulatorMessageOutcome {
                    summary: summary.map(|summary| (simulator_id, summary)),
                    touched_agent: agent_entity.is_some(),
                    agent_entity,
                })
            }
            update => {
                let entity = match &update {
                    DecodedWorldUpdate::SitTarget { update, .. } => preflight_entity_address(world, update.object_id).await,
                    DecodedWorldUpdate::AvatarAppearance { update, .. } => preflight_entity_address(world, update.object_id).await,
                    _ => None,
                };
                Some(SimulatorMessageOutcome {
                    summary: self.apply_update(world, simulator_id, update, agent_id, entity),
                    touched_agent: false,
                    agent_entity: None,
                })
            }
        }
    }

    pub(super) fn apply_message(
        &mut self,
        world: &mut SessionWorld,
        simulator_id: SimulatorId,
        sequence: u32,
        packet_order: PacketOrder,
        received_at: Instant,
        ping_delay: Option<Duration>,
        message: &Message,
        agent_id: Option<Uuid>,
    ) -> Option<SimulatorMessageOutcome> {
        let update = decode_simulator_world_update(world, simulator_id, sequence, packet_order, received_at, ping_delay, message)?;
        let summary = match update {
            DecodedWorldUpdate::Object(_) | DecodedWorldUpdate::SitTarget { .. } | DecodedWorldUpdate::AvatarAppearance { .. } => {
                unreachable!("object updates must enter a bounded continuation before message fallback")
            }
            update => self.apply_update(world, simulator_id, update, agent_id, None),
        };
        Some(SimulatorMessageOutcome { summary, touched_agent: false, agent_entity: None })
    }

    pub(super) fn begin_object_update(
        &mut self,
        world: &mut SessionWorld,
        simulator_id: SimulatorId,
        sequence: u32,
        packet_order: PacketOrder,
        received_at: Instant,
        ping_delay: Option<Duration>,
        message: &Message,
        agent_id: Option<Uuid>,
        object_cache: &mut ObjectCacheSession,
    ) -> Option<ObjectUpdatePreparation> {
        let update = decode_simulator_world_update(world, simulator_id, sequence, packet_order, received_at, ping_delay, message)?;
        let update = match (update, message) {
            (DecodedWorldUpdate::Object(batch), Message::ObjectUpdateCached(cached)) => {
                let region = world.region_for(simulator_id)?;
                return Some(ObjectUpdatePreparation::Cached(object_cache.begin_resolve_cached(
                    simulator_id,
                    region,
                    sequence,
                    order_evidence(packet_order),
                    cached.clone(),
                    batch,
                )));
            }
            (update, _) => update,
        };
        if matches!(update, DecodedWorldUpdate::AvatarAppearance { .. } | DecodedWorldUpdate::SitTarget { .. }) {
            return Some(ObjectUpdatePreparation::Entity(EntityUpdateContinuation::new(simulator_id, update)));
        }
        let DecodedWorldUpdate::Object(batch) = update else { return None };
        let batch = self.filter_batch(world, simulator_id, agent_id, batch)?;
        Some(ObjectUpdatePreparation::Ready { continuation: world.begin_object_batch(simulator_id, batch)?, media_requests: Vec::new() })
    }

    pub(super) fn advance_entity_update(
        &mut self,
        world: &mut SessionWorld,
        continuation: &mut EntityUpdateContinuation,
    ) -> EntityUpdateProgress {
        let mut work = 0usize;
        loop {
            if work >= MAX_OBJECT_LOOKUP_WORK_ITEMS {
                return EntityUpdateProgress::Pending;
            }
            match continuation.stage {
                EntityUpdateStage::Identity => {
                    let id = match continuation.update.as_ref() {
                        Some(DecodedWorldUpdate::AvatarAppearance { update, .. }) => update.object_id,
                        Some(DecodedWorldUpdate::SitTarget { update, .. }) => update.object_id,
                        _ => {
                            return EntityUpdateProgress::Complete(SimulatorMessageOutcome {
                                summary: None,
                                touched_agent: false,
                                agent_entity: None,
                            });
                        }
                    };
                    match world.lookup_entity_step(id, continuation.entity_cursor.take(), MAX_OBJECT_LOOKUP_WORK_ITEMS.saturating_sub(work))
                    {
                        ObjectLookupProgress::Visible { entity, work_units } => {
                            work = work.saturating_add(work_units);
                            continuation.entity = Some(entity);
                            continuation.address = world.world().object(entity).map(|object| object.address());
                            continuation.stage =
                                if continuation.address.is_some() { EntityUpdateStage::Address } else { EntityUpdateStage::Apply };
                        }
                        ObjectLookupProgress::Hidden { work_units } => {
                            work = work.saturating_add(work_units);
                            continuation.stage = EntityUpdateStage::Apply;
                        }
                        ObjectLookupProgress::Pending { cursor, .. } => {
                            continuation.entity_cursor = Some(cursor);
                            return EntityUpdateProgress::Pending;
                        }
                    }
                }
                EntityUpdateStage::Address => {
                    let Some(address) = continuation.address else {
                        continuation.stage = EntityUpdateStage::Apply;
                        continue;
                    };
                    match world.lookup_entity_at_step(
                        address,
                        continuation.address_cursor.take(),
                        MAX_OBJECT_LOOKUP_WORK_ITEMS.saturating_sub(work),
                    ) {
                        ObjectLookupProgress::Visible { work_units, .. } | ObjectLookupProgress::Hidden { work_units } => {
                            work = work.saturating_add(work_units);
                            continuation.stage = EntityUpdateStage::Apply;
                        }
                        ObjectLookupProgress::Pending { cursor, .. } => {
                            continuation.address_cursor = Some(cursor);
                            return EntityUpdateProgress::Pending;
                        }
                    }
                }
                EntityUpdateStage::Apply => {
                    let update = continuation.update.take().expect("entity update is applied once");
                    let summary = self.apply_update(world, continuation.simulator_id, update, None, continuation.entity);
                    return EntityUpdateProgress::Complete(SimulatorMessageOutcome { summary, touched_agent: false, agent_entity: None });
                }
            }
        }
    }

    pub(super) fn finish_cached_object_update(
        &mut self,
        world: &mut SessionWorld,
        simulator_id: SimulatorId,
        agent_id: Option<Uuid>,
        batch: ObjectBatch,
        media_requests: Vec<ObjectMediaRequest>,
    ) -> Option<(ObjectBatchContinuation, Vec<ObjectMediaRequest>)> {
        let batch = self.filter_batch(world, simulator_id, agent_id, batch)?;
        Some((world.begin_object_batch(simulator_id, batch)?, media_requests))
    }

    pub(super) fn begin_object_update_finish(
        &self,
        simulator_id: SimulatorId,
        agent_id: Option<Uuid>,
        batch_summary: IngestSummary,
    ) -> ObjectUpdateFinishContinuation {
        let observation = self.observations.get(&simulator_id).cloned();
        let stage = if observation.is_some() { ObjectUpdateFinishStage::ObservationId } else { ObjectUpdateFinishStage::Agent };
        ObjectUpdateFinishContinuation {
            simulator_id,
            agent_id,
            batch_summary,
            observation,
            observation_entity: None,
            extra_summary: None,
            agent_entity: None,
            stage,
            observation_id_cursor: None,
            observation_address_cursor: None,
            agent_cursor: None,
        }
    }

    pub(super) fn advance_object_update_finish(
        &mut self,
        world: &mut SessionWorld,
        continuation: &mut ObjectUpdateFinishContinuation,
    ) -> ObjectUpdateFinishProgress {
        let mut lookup_work = 0;
        loop {
            if lookup_work >= MAX_OBJECT_LOOKUP_WORK_ITEMS {
                return ObjectUpdateFinishProgress::Pending;
            }
            match continuation.stage {
                ObjectUpdateFinishStage::ObservationId => {
                    let Some(observation) = continuation.observation.as_ref() else {
                        continuation.stage = ObjectUpdateFinishStage::Agent;
                        continue;
                    };
                    let progress = world.lookup_entity_step(
                        observation.id,
                        continuation.observation_id_cursor.take(),
                        MAX_OBJECT_LOOKUP_WORK_ITEMS.saturating_sub(lookup_work),
                    );
                    match progress {
                        ObjectLookupProgress::Visible { entity, work_units } => {
                            lookup_work = lookup_work.saturating_add(work_units);
                            continuation.observation_entity = Some(entity);
                            continuation.stage = ObjectUpdateFinishStage::ObservationAddress;
                        }
                        ObjectLookupProgress::Hidden { work_units } => {
                            lookup_work = lookup_work.saturating_add(work_units);
                            continuation.observation_entity = None;
                            continuation.stage = ObjectUpdateFinishStage::ObservationAddress;
                        }
                        ObjectLookupProgress::Pending { cursor, .. } => {
                            continuation.observation_id_cursor = Some(cursor);
                            return ObjectUpdateFinishProgress::Pending;
                        }
                    }
                }
                ObjectUpdateFinishStage::ObservationAddress => {
                    let Some(observation) = continuation.observation.as_ref() else {
                        continuation.stage = ObjectUpdateFinishStage::Agent;
                        continue;
                    };
                    let progress = world.lookup_entity_at_step(
                        LocalAddress::new(observation.source.region(), observation.local_id),
                        continuation.observation_address_cursor.take(),
                        MAX_OBJECT_LOOKUP_WORK_ITEMS.saturating_sub(lookup_work),
                    );
                    match progress {
                        ObjectLookupProgress::Visible { work_units, .. } | ObjectLookupProgress::Hidden { work_units } => {
                            lookup_work = lookup_work.saturating_add(work_units);
                            continuation.stage = ObjectUpdateFinishStage::CompleteObservation;
                        }
                        ObjectLookupProgress::Pending { cursor, .. } => {
                            continuation.observation_address_cursor = Some(cursor);
                            return ObjectUpdateFinishProgress::Pending;
                        }
                    }
                }
                ObjectUpdateFinishStage::CompleteObservation => {
                    if let Some(observation) = continuation.observation.take() {
                        let (completed, applied) = self.complete_world_handoff(
                            world,
                            continuation.simulator_id,
                            continuation.observation_entity,
                            observation.clone(),
                        );
                        if completed {
                            self.observations.remove(&continuation.simulator_id);
                        }
                        continuation.extra_summary = applied.map(|applied| {
                            let mut summary = IngestSummary::new(IngestStatus::Applied);
                            summary.push_applied(applied, Some(LocalAddress::new(observation.source.region(), observation.local_id)));
                            world.record_ingest(&summary, continuation.simulator_id);
                            summary
                        });
                    }
                    continuation.stage = ObjectUpdateFinishStage::Agent;
                }
                ObjectUpdateFinishStage::Agent => {
                    let Some(agent_id) = continuation.agent_id else {
                        continuation.stage = ObjectUpdateFinishStage::Complete;
                        continue;
                    };
                    match world.lookup_entity_step(
                        agent_id,
                        continuation.agent_cursor.take(),
                        MAX_OBJECT_LOOKUP_WORK_ITEMS.saturating_sub(lookup_work),
                    ) {
                        ObjectLookupProgress::Visible { entity, work_units } => {
                            lookup_work = lookup_work.saturating_add(work_units);
                            continuation.agent_entity = Some(entity);
                            continuation.stage = ObjectUpdateFinishStage::Complete;
                        }
                        ObjectLookupProgress::Hidden { work_units } => {
                            lookup_work = lookup_work.saturating_add(work_units);
                            continuation.agent_entity = None;
                            continuation.stage = ObjectUpdateFinishStage::Complete;
                        }
                        ObjectLookupProgress::Pending { cursor, .. } => {
                            continuation.agent_cursor = Some(cursor);
                            return ObjectUpdateFinishProgress::Pending;
                        }
                    }
                }
                ObjectUpdateFinishStage::Complete => {
                    let agent_entity = continuation.agent_entity.filter(|entity| {
                        continuation.batch_summary.touches_entity(*entity)
                            || continuation.extra_summary.as_ref().is_some_and(|summary| summary.touches_entity(*entity))
                    });
                    return ObjectUpdateFinishProgress::Complete(SimulatorMessageOutcome {
                        summary: continuation.extra_summary.take().map(|summary| (continuation.simulator_id, summary)),
                        touched_agent: agent_entity.is_some(),
                        agent_entity,
                    });
                }
            }
        }
    }

    fn apply_update(
        &mut self,
        world: &mut SessionWorld,
        simulator_id: SimulatorId,
        update: DecodedWorldUpdate,
        _agent_id: Option<Uuid>,
        resolved_entity: Option<Entity>,
    ) -> Option<(SimulatorId, IngestSummary)> {
        match update {
            #[cfg(test)]
            DecodedWorldUpdate::Object(batch) => {
                let batch = self.filter_batch(world, simulator_id, _agent_id, batch)?;
                let summary = world.apply_object_batch(simulator_id, batch);
                self.complete_batch(world, simulator_id, summary)
            }
            #[cfg(not(test))]
            DecodedWorldUpdate::Object(_) => None,
            DecodedWorldUpdate::SitTarget { source, update } => {
                let address = address_for_resolved_entity(world, simulator_id, resolved_entity);
                let result = world.apply_sit_target_update(simulator_id, source, update);
                match result {
                    Some(Ok(applied)) => {
                        if !applied.rejections.is_empty() {
                            world.record_value_rejection(simulator_id, address, ValueRejection::Sit, None);
                        }
                        let mut summary = IngestSummary::new(IngestStatus::Applied);
                        summary.push_applied(applied, address);
                        Some((simulator_id, summary))
                    }
                    Some(Err(error)) => {
                        world.record_value_rejection(simulator_id, address, ValueRejection::SitError, Some(error));
                        let mut summary = IngestSummary::new(IngestStatus::Applied);
                        summary.push_error(error, address);
                        Some((simulator_id, summary))
                    }
                    None => None,
                }
            }
            DecodedWorldUpdate::AvatarAppearance { source, update } => {
                let invalid_fields = update.invalid_fields;
                let address = address_for_resolved_entity(world, simulator_id, resolved_entity);
                match world.apply_avatar_appearance(simulator_id, source, resolved_entity, update) {
                    Some(Ok(applied)) => {
                        if !applied.rejections.is_empty() {
                            world.record_value_rejection(simulator_id, address, ValueRejection::Appearance, None);
                        }
                        let mut summary = IngestSummary::new(IngestStatus::Applied);
                        summary.invalid_fields = invalid_fields;
                        summary.push_applied(applied, address);
                        Some((simulator_id, summary))
                    }
                    Some(Err(error)) => {
                        world.record_value_rejection(simulator_id, address, ValueRejection::AppearanceError, Some(error));
                        let mut summary = IngestSummary::new(IngestStatus::Applied);
                        summary.invalid_fields = invalid_fields;
                        summary.push_error(error, address);
                        Some((simulator_id, summary))
                    }
                    None => None,
                }
            }
            DecodedWorldUpdate::ParcelOverlay { source, observation } => {
                if world.observe_parcel_overlay(source, observation).is_err() {
                    record_parcel_rejection(world, simulator_id, source.region());
                }
                None
            }
            DecodedWorldUpdate::ParcelProperties { source, observation } => {
                if self.world_primary != Some(simulator_id) {
                    return None;
                }
                if world.observe_parcel_properties(source, observation).is_err() {
                    record_parcel_rejection(world, simulator_id, source.region());
                }
                None
            }
            DecodedWorldUpdate::ParcelMalformed => {
                if self.world_primary == Some(simulator_id)
                    && let Some(region) = world.region_for(simulator_id)
                {
                    record_parcel_rejection(world, simulator_id, region);
                }
                None
            }
            DecodedWorldUpdate::Malformed { invalid_blocks, invalid_fields } => {
                let mut summary = IngestSummary::new(IngestStatus::Applied);
                summary.invalid_blocks = invalid_blocks;
                summary.invalid_fields = invalid_fields;
                Some((simulator_id, summary))
            }
        }
    }

    fn filter_batch(
        &mut self,
        world: &SessionWorld,
        simulator_id: SimulatorId,
        agent_id: Option<Uuid>,
        mut batch: ObjectBatch,
    ) -> Option<ObjectBatch> {
        if self.world_primary != Some(simulator_id)
            && let Some(agent_id) = agent_id
        {
            let agent_region = world.region_for(simulator_id)?;
            let mut retained = Vec::with_capacity(batch.blocks.len());
            for operation in batch.blocks.drain(..) {
                match operation {
                    Some(ObjectOperation::Observe(observation)) if observation.id == agent_id => {
                        let replace = self
                            .observations
                            .get(&simulator_id)
                            .is_none_or(|current| should_replace_handoff_observation(current, &observation));
                        if replace {
                            self.observations.insert(simulator_id, observation);
                        }
                    }
                    Some(operation) => match self.observations.get(&simulator_id) {
                        Some(observation)
                            if operation.address() == LocalAddress::new(agent_region, observation.local_id)
                                && conflicts_with_handoff_observation(&operation, observation) =>
                        {
                            if operation_is_newer(operation.source(), observation.source) {
                                self.observations.remove(&simulator_id);
                            }
                            retained.push(Some(operation));
                        }
                        Some(observation) if operation.address() == LocalAddress::new(agent_region, observation.local_id) => {}
                        _ => retained.push(Some(operation)),
                    },
                    operation => retained.push(operation),
                }
            }
            batch.blocks = retained;
            while self.observations.len() > MAX_HANDOFF_OBSERVATIONS {
                if let Some(stale) = self.observations.keys().next().copied() {
                    self.observations.remove(&stale);
                }
            }
        }
        Some(batch)
    }

    #[cfg(test)]
    fn complete_batch(
        &mut self,
        world: &mut SessionWorld,
        simulator_id: SimulatorId,
        summary: Option<IngestSummary>,
    ) -> Option<(SimulatorId, IngestSummary)> {
        let mut summary = summary.map(|summary| (simulator_id, summary));
        if let Some(extra) = self.complete_observation(world, simulator_id) {
            let summary = summary.get_or_insert_with(|| (simulator_id, IngestSummary::new(IngestStatus::Applied)));
            summary.1.append(extra);
        }
        summary
    }

    #[cfg(test)]
    fn complete_observation(&mut self, world: &mut SessionWorld, simulator_id: SimulatorId) -> Option<IngestSummary> {
        if let Some(observation) = self.observations.get(&simulator_id).cloned() {
            let entity = world.world().entity(observation.id);
            let (completed, applied) = self.complete_world_handoff(world, simulator_id, entity, observation.clone());
            if completed {
                self.observations.remove(&simulator_id);
            }
            if let Some(applied) = applied {
                let mut summary = IngestSummary::new(IngestStatus::Applied);
                summary.push_applied(applied, Some(LocalAddress::new(observation.source.region(), observation.local_id)));
                return Some(summary);
            }
        }
        None
    }

    #[cfg(test)]
    async fn complete_observation_cooperatively(&mut self, world: &mut SessionWorld, simulator_id: SimulatorId) -> Option<IngestSummary> {
        let observation = self.observations.get(&simulator_id).cloned()?;
        let entity = world.lookup_entity(observation.id).await;
        let _ = world.lookup_entity_at(LocalAddress::new(observation.source.region(), observation.local_id)).await;
        let (completed, applied) = self.complete_world_handoff(world, simulator_id, entity, observation.clone());
        if completed {
            self.observations.remove(&simulator_id);
        }
        applied.map(|applied| {
            let mut summary = IngestSummary::new(IngestStatus::Applied);
            summary.push_applied(applied, Some(LocalAddress::new(observation.source.region(), observation.local_id)));
            summary
        })
    }

    pub(super) fn begin(&mut self, source: SimulatorId, target: SimulatorId) -> bool {
        if source == target {
            return false;
        }
        if let Some(current) = self.promoted_target
            && current.target == source
            && self.world_primary == Some(current.source)
        {
            self.promoted_target = Some(PendingWorldHandoff { source: current.source, target });
            return true;
        }
        if self.world_primary != Some(source) {
            return false;
        }
        self.promoted_target = Some(PendingWorldHandoff { source, target });
        true
    }

    #[cfg(test)]
    pub(super) fn promote(&mut self, world: &mut SessionWorld, source: SimulatorId, target: SimulatorId) -> Option<IngestSummary> {
        let previous_target = self.promoted_target.map(|handoff| handoff.target);
        if !self.begin(source, target) {
            return None;
        }
        self.pending_movement_completes.retain(|simulator, _| *simulator == target);
        if let Some(previous_target) = previous_target.filter(|previous_target| *previous_target != target) {
            self.observations.remove(&previous_target);
        }
        self.observations.get(&target).cloned().and_then(|observation| {
            let entity = world.world().entity(observation.id);
            self.complete_promoted(world, target, entity, observation).1
        })
    }

    pub(super) fn begin_promotion(&mut self, source: SimulatorId, target: SimulatorId) -> Option<PromotionContinuation> {
        let previous_target = self.promoted_target.map(|handoff| handoff.target);
        if !self.begin(source, target) {
            return None;
        }
        self.pending_movement_completes.retain(|simulator, _| *simulator == target);
        if let Some(previous_target) = previous_target.filter(|previous_target| *previous_target != target) {
            self.observations.remove(&previous_target);
        }
        let observation = self.observations.get(&target).cloned()?;
        Some(PromotionContinuation {
            target,
            observation,
            entity: None,
            entity_cursor: None,
            address_cursor: None,
            stage: PromotionStage::Identity,
        })
    }

    pub(super) fn advance_promotion(&mut self, world: &mut SessionWorld, continuation: &mut PromotionContinuation) -> PromotionProgress {
        let mut work = 0usize;
        loop {
            if work >= MAX_OBJECT_LOOKUP_WORK_ITEMS {
                return PromotionProgress::Pending;
            }
            match continuation.stage {
                PromotionStage::Identity => {
                    match world.lookup_entity_step(
                        continuation.observation.id,
                        continuation.entity_cursor.take(),
                        MAX_OBJECT_LOOKUP_WORK_ITEMS.saturating_sub(work),
                    ) {
                        ObjectLookupProgress::Visible { entity, work_units } => {
                            work = work.saturating_add(work_units);
                            continuation.entity = Some(entity);
                            continuation.stage = PromotionStage::Address;
                        }
                        ObjectLookupProgress::Hidden { work_units } => {
                            work = work.saturating_add(work_units);
                            continuation.stage = PromotionStage::Address;
                        }
                        ObjectLookupProgress::Pending { cursor, .. } => {
                            continuation.entity_cursor = Some(cursor);
                            return PromotionProgress::Pending;
                        }
                    }
                }
                PromotionStage::Address => {
                    let address = LocalAddress::new(continuation.observation.source.region(), continuation.observation.local_id);
                    match world.lookup_entity_at_step(
                        address,
                        continuation.address_cursor.take(),
                        MAX_OBJECT_LOOKUP_WORK_ITEMS.saturating_sub(work),
                    ) {
                        ObjectLookupProgress::Visible { work_units, .. } | ObjectLookupProgress::Hidden { work_units } => {
                            work = work.saturating_add(work_units);
                            continuation.stage = PromotionStage::Complete;
                        }
                        ObjectLookupProgress::Pending { cursor, .. } => {
                            continuation.address_cursor = Some(cursor);
                            return PromotionProgress::Pending;
                        }
                    }
                }
                PromotionStage::Complete => {
                    let summary =
                        self.complete_promoted(world, continuation.target, continuation.entity, continuation.observation.clone()).1;
                    return PromotionProgress::Complete(summary);
                }
            }
        }
    }

    pub(super) fn complete_promoted(
        &mut self,
        world: &mut SessionWorld,
        target: SimulatorId,
        resolved_entity: Option<Entity>,
        observation: ObjectObservation,
    ) -> (bool, Option<IngestSummary>) {
        let address = LocalAddress::new(observation.source.region(), observation.local_id);
        let (completed, applied) = self.complete_world_handoff(world, target, resolved_entity, observation);
        let summary = applied.map(|applied| {
            let mut summary = IngestSummary::new(IngestStatus::Applied);
            summary.push_applied(applied, Some(address));
            world.record_ingest(&summary, target);
            summary
        });
        (completed, summary)
    }

    fn complete_world_handoff(
        &mut self,
        world: &mut SessionWorld,
        target: SimulatorId,
        resolved_entity: Option<Entity>,
        observation: ObjectObservation,
    ) -> (bool, Option<Applied>) {
        let Some(handoff) = self.promoted_target else { return (false, None) };
        if handoff.target != target || self.world_primary != Some(handoff.source) {
            return (false, None);
        }
        let (completed, applied) = world.complete_agent_handoff(handoff.source, target, resolved_entity, observation);
        if completed {
            self.world_primary = Some(target);
            self.promoted_target = None;
        }
        (completed, applied)
    }
}

fn movement_region_matches(world: &SessionWorld, simulator_id: SimulatorId, handle: RegionHandle) -> bool {
    world
        .region_for(simulator_id)
        .and_then(|region| world.world().region(region))
        .is_some_and(|info| info.handle.is_none_or(|known| known == handle))
}

fn parcel_properties_observation(message: &ParcelProperties) -> Result<ParcelPropertiesObservation, ()> {
    let parcel = &message.parcel_data;
    let request_result = parcel.request_result.ok_or(())?;
    let sequence_id = parcel.sequence_id.ok_or(())?;
    if request_result == -1 || sequence_id < 0 {
        return Ok(ParcelPropertiesObservation { request_result, sequence_id, properties: None });
    }
    let local_id = ParcelLocalId::new(parcel.local_id.ok_or(())?).ok_or(())?;
    let name = wire_string(parcel.name.as_deref().ok_or(())?, world::MAX_PARCEL_NAME_BYTES)?;
    let description = wire_string(parcel.description.as_deref().ok_or(())?, world::MAX_PARCEL_DESCRIPTION_BYTES)?;
    let music_url = wire_string(parcel.music_url.as_deref().ok_or(())?, world::MAX_PARCEL_URL_BYTES)?;
    let media_url = wire_string(parcel.media_url.as_deref().ok_or(())?, world::MAX_PARCEL_URL_BYTES)?;
    let bitmap = ParcelBitmap::from_bytes(parcel.bitmap.as_deref().ok_or(())?).map_err(|_| ())?;
    let area = u32::try_from(parcel.area.ok_or(())?).map_err(|_| ())?;
    let pass_price = parcel.pass_price.map(u32::try_from).transpose().map_err(|_| ())?;
    let properties = ParcelPropertiesData {
        local_id,
        name,
        description,
        owner_id: parcel.owner_id,
        group_id: parcel.group_id,
        group_owned: parcel.is_group_owned.ok_or(())?,
        area,
        flags: ParcelFlags::from_bits(parcel.parcel_flags.ok_or(())?),
        bitmap,
        landing_point: parcel.user_location,
        landing_look_at: parcel.user_look_at,
        landing_type: parcel.landing_type,
        music_url,
        media_url,
        access: ParcelAccessSettings {
            region_push_override: parcel.region_push_override,
            region_deny_anonymous: parcel.region_deny_anonymous,
            region_deny_identified: parcel.region_deny_identified,
            region_deny_transacted: parcel.region_deny_transacted,
            region_deny_age_unverified: message.age_verification_block.region_deny_age_unverified,
            region_allow_access_override: message.region_allow_access_block.region_allow_access_override,
            pass_price,
            pass_hours: parcel.pass_hours,
        },
    };
    Ok(ParcelPropertiesObservation { request_result, sequence_id, properties: Some(properties) })
}

fn wire_string(bytes: &[u8], limit: usize) -> Result<Arc<str>, ()> {
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    if bytes.len() > limit {
        return Err(());
    }
    let decoded: Cow<'_, str> = String::from_utf8_lossy(bytes);
    Ok(Arc::from(decoded.as_ref()))
}

pub(super) fn conflicts_with_handoff_observation(operation: &ObjectOperation, observation: &ObjectObservation) -> bool {
    match operation {
        ObjectOperation::Observe(candidate) => candidate.id != observation.id,
        ObjectOperation::Remove { .. } => true,
        ObjectOperation::Patch(_) | ObjectOperation::Cached { .. } => false,
    }
}

pub(super) fn operation_is_newer(candidate: UpdateSource, current: UpdateSource) -> bool {
    match (candidate.order(), current.order()) {
        (Ordered(candidate_order), Ordered(current_order)) => {
            (candidate_order, candidate.block_ordinal()) > (current_order, current.block_ordinal())
        }
        _ => false,
    }
}

pub(super) fn should_replace_handoff_observation(current: &ObjectObservation, candidate: &ObjectObservation) -> bool {
    if current.source.region() != candidate.source.region() {
        return true;
    }
    match (current.source.order(), candidate.source.order()) {
        (OrderEvidence::Ordered(current_order), OrderEvidence::Ordered(candidate_order)) => {
            (candidate_order, candidate.source.block_ordinal()) > (current_order, current.source.block_ordinal())
        }
        (OrderEvidence::Ambiguous | OrderEvidence::Degraded, OrderEvidence::Ordered(_)) => true,
        (OrderEvidence::Ordered(_), OrderEvidence::Ambiguous | OrderEvidence::Degraded)
        | (OrderEvidence::Ambiguous | OrderEvidence::Degraded, OrderEvidence::Ambiguous | OrderEvidence::Degraded) => false,
    }
}

pub(super) fn order_evidence(packet_order: PacketOrder) -> OrderEvidence {
    match packet_order {
        PacketOrder::Ordered(order) => OrderEvidence::Ordered(order),
        PacketOrder::Ambiguous => OrderEvidence::Ambiguous,
        PacketOrder::Degraded => OrderEvidence::Degraded,
    }
}

pub(super) fn handle_simulator_message(
    scope: SimulatorScope,
    source: SocketAddr,
    message: &Message,
    session: Option<&AgentSession>,
    region_lookup: Option<&RegionLookup>,
    region_name: Option<&str>,
    mailbox: &PresentationMailbox,
) {
    use Message::{ChatFromSimulator, RegionHandshake};

    if matches!(scope, SimulatorScope::Primary) {
        if let Some(region_lookup) = region_lookup {
            region_lookup.handle_message(source, message);
        }
        if matches!(&message, RegionHandshake(_))
            && let Some(name) = region_name
        {
            mailbox.publish(ViewerUpdate::RegionName(name.to_owned()));
        }
    }
    if let ChatFromSimulator(message) = message
        && let Some(chat_message) = display_message(message.chat_data.clone(), session)
    {
        mailbox.publish(ViewerUpdate::ChatMessage(chat_message));
    }
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;
