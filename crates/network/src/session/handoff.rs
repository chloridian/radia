use std::{cmp::Ordering, collections::HashMap, mem::replace};

use super::{
    ConnectionError, EventQueueLifecycle, HandoffCommand, RunState, RuntimeAction, SessionDispatcher, SessionError, SessionState,
    SimulatorEvent, SimulatorRecord, SimulatorRequest, record_event_envelope_loss, remove_simulator_capability,
};
use crate::{
    capabilities::Client,
    circuit::{HandoffCancellation, HandoffGeneration, HandoffTarget},
    coverage::{CoverageLossLedger, CoverageLossReason},
    event::EventRetention,
    event::{Event, EventEnvelope, PacketOrder, SimulatorId},
};
use protocol::{lludp::Message, messages::AgentMovementComplete};
use tokio::sync::{mpsc, watch};

#[derive(Clone)]
pub(crate) struct PendingPromotion {
    pub(crate) source: SimulatorId,
    pub(crate) target: SimulatorId,
    pub(crate) generation: HandoffGeneration,
    pub(crate) handoff: HandoffTarget,
    pub(crate) evidence: HandoffEvidence,
}

#[derive(Default)]
pub(crate) enum HandoffState {
    #[default]
    Idle,
    Preparing(PendingPromotion),
    Conflicted(HandoffCancellation),
}

impl HandoffState {
    pub(super) fn pending(&self) -> Option<&PendingPromotion> {
        match self {
            Self::Preparing(pending) => Some(pending),
            Self::Idle | Self::Conflicted(_) => None,
        }
    }

    pub(super) fn pending_mut(&mut self) -> Option<&mut PendingPromotion> {
        match self {
            Self::Preparing(pending) => Some(pending),
            Self::Idle | Self::Conflicted(_) => None,
        }
    }

    pub(super) fn is_conflicted(&self) -> bool {
        matches!(self, Self::Conflicted(_))
    }

    pub(super) fn cancellation(&self) -> Option<HandoffCancellation> {
        match self {
            Self::Preparing(pending) => Some(handoff_cancellation(pending)),
            Self::Conflicted(cancellation) => Some(*cancellation),
            Self::Idle => None,
        }
    }

    pub(super) fn take_pending(&mut self) -> Option<PendingPromotion> {
        match replace(self, Self::Idle) {
            Self::Preparing(pending) => Some(pending),
            state => {
                *self = state;
                None
            }
        }
    }

    pub(super) fn set_pending(&mut self, pending: PendingPromotion) {
        *self = Self::Preparing(pending);
    }

    pub(super) fn conflict(&mut self, pending: &PendingPromotion) {
        *self = Self::Conflicted(handoff_cancellation(pending));
    }

    pub(super) fn cancel_matching(&mut self, cancellation: HandoffCancellation) -> Option<PendingPromotion> {
        if !matches_handoff_cancellation(self, cancellation) {
            return None;
        }
        self.take_pending()
    }

    pub(super) fn clear_conflict(&mut self, cancellation: HandoffCancellation) -> bool {
        let matches = matches!(self, Self::Conflicted(current)
            if current.source == cancellation.source
                && current.target == cancellation.target
                && current.generation == cancellation.generation);
        if matches {
            *self = Self::Idle;
        }
        matches
    }
}

#[derive(Clone, Copy)]
pub(super) enum HandoffAnnouncement {
    CrossedRegion,
    TeleportFinish(HandoffOrder),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HandoffOrder {
    Packet(PacketOrder),
    EventQueue { incarnation: u64, batch: u64, index: usize },
    Unavailable,
}

impl HandoffOrder {
    fn finish_order(self) -> FinishOrder {
        match self {
            Self::Packet(PacketOrder::Ordered(sequence)) => FinishOrder::Packet(sequence),
            Self::EventQueue { incarnation, batch, index } => FinishOrder::EventQueue { incarnation, batch, index },
            Self::Packet(PacketOrder::Ambiguous | PacketOrder::Degraded) | Self::Unavailable => FinishOrder::Unusable,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FinishOrder {
    Packet(u64),
    EventQueue { incarnation: u64, batch: u64, index: usize },
    Unusable,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct HandoffEvidence {
    pub(super) crossed_region: bool,
    pub(super) teleport_finish: Option<FinishOrder>,
}

impl HandoffEvidence {
    fn record(&mut self, announcement: HandoffAnnouncement) {
        match announcement {
            HandoffAnnouncement::CrossedRegion => self.crossed_region = true,
            HandoffAnnouncement::TeleportFinish(order) => {
                let incoming = order.finish_order();
                self.teleport_finish = Some(match (self.teleport_finish, incoming) {
                    (None, incoming) => incoming,
                    (Some(current), incoming) => match compare_finish_order(current, incoming) {
                        Some(Ordering::Greater) => incoming,
                        Some(Ordering::Less | Ordering::Equal) => current,
                        None => FinishOrder::Unusable,
                    },
                });
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HandoffDecision {
    SameDestination,
    ReplaceNewer,
    IgnoreStale,
    Conflict,
}

pub(super) struct PromotionSetup<'a> {
    pub(super) state: &'a mut RunState,
    pub(super) neighbor_requests: mpsc::Sender<SimulatorRequest>,
    pub(super) application_events: mpsc::Sender<EventEnvelope>,
    pub(super) neighbor_events: mpsc::Sender<SimulatorEvent>,
    pub(super) shutdown: watch::Receiver<bool>,
    pub(super) capability_events: mpsc::Sender<EventEnvelope>,
}

impl SessionState {
    fn ensure_handoff_capabilities(
        &mut self,
        simulator_id: SimulatorId,
        handoff: &HandoffTarget,
        capability_events: mpsc::Sender<EventEnvelope>,
        shutdown: watch::Receiver<bool>,
        run_state: &mut RunState,
    ) -> Result<Client, SessionError> {
        if let Some(capability) = self.simulators.get(&simulator_id).and_then(|simulator| simulator.capabilities.as_ref())
            && self.simulators.get(&simulator_id).is_some_and(|simulator| simulator.endpoint == handoff.endpoint)
            && capability.seed_capability == handoff.seed_capability
            && capability.event_queue.is_running()
        {
            return Ok(capability.client.clone());
        }
        remove_simulator_capability(&mut self.simulators, simulator_id);
        let client = self.new_capability_client(handoff.seed_capability.clone())?;
        let event_queue =
            match client.start_event_queue(simulator_id, handoff.endpoint, capability_events, shutdown, self.coverage_losses.clone()) {
                Ok(queue) => queue,
                Err(error) => {
                    client.close_session();
                    return Err(error.into());
                }
            };
        let endpoint = handoff.endpoint;
        let event_queue_role = event_queue.role_handle();
        let task = run_state.event_queues.spawn(async move {
            let exit = event_queue.wait().await;
            (simulator_id, endpoint, exit)
        });
        if let Some(simulator) = self.simulators.get_mut(&simulator_id) {
            simulator.capabilities = Some(super::simulators::SimulatorCapabilitySession {
                seed_capability: handoff.seed_capability.clone(),
                client: client.clone(),
                event_queue: EventQueueLifecycle::Running { role: event_queue_role, task },
            });
        }
        Ok(client)
    }

    pub(super) async fn request_handoff(
        &mut self,
        source: SimulatorId,
        handoff: HandoffTarget,
        announcement: HandoffAnnouncement,
        setup: &mut PromotionSetup<'_>,
        state: &mut HandoffState,
    ) -> Result<(), SessionError> {
        if source != self.primary_id || state.is_conflicted() {
            return Ok(());
        }
        if self.primary().region_handle == Some(handoff.region_handle) && self.primary().endpoint == handoff.endpoint {
            return Ok(());
        }

        let mut evidence = HandoffEvidence::default();
        if !self.resolve_pending_handoff(source, &handoff, announcement, setup, state, &mut evidence) {
            return Ok(());
        }
        evidence.record(announcement);
        let Some(generation) = self.next_handoff_generation() else {
            return Ok(());
        };

        let source = self.primary_id;
        let Some(simulator_id) = self.start_neighbor(source, handoff.region_handle, handoff.endpoint, Some(handoff.clone()), setup).await?
        else {
            return Ok(());
        };
        if let Err(error) =
            self.ensure_handoff_capabilities(simulator_id, &handoff, setup.capability_events.clone(), setup.shutdown.clone(), setup.state)
        {
            tracing::warn!(
                simulator = simulator_id.get(),
                region = handoff.region_handle,
                reason = "destination capabilities failed",
                %error,
                "simulator handoff failed"
            );
            if let Some(connection) = self.simulators.get_mut(&simulator_id) {
                connection.queue_handoff_command(HandoffCommand::Cancel);
            }
            return Ok(());
        }
        let region_handle = handoff.region_handle;
        state.set_pending(PendingPromotion { source, target: simulator_id, generation, handoff, evidence });
        tracing::info!(simulator = simulator_id.get(), region = region_handle, "simulator promotion started");
        Ok(())
    }

    fn resolve_pending_handoff(
        &mut self,
        source: SimulatorId,
        handoff: &HandoffTarget,
        announcement: HandoffAnnouncement,
        setup: &mut PromotionSetup<'_>,
        state: &mut HandoffState,
        evidence: &mut HandoffEvidence,
    ) -> bool {
        let Some(current) = state.pending().cloned() else {
            return true;
        };
        match arbitrate_handoff(&current, handoff, announcement) {
            HandoffDecision::SameDestination => {
                let Some(current) = state.pending_mut() else { unreachable!("a cloned pending handoff remains active") };
                current.evidence.record(announcement);
                let same_live_target = current.source == source
                    && self.simulators.get(&current.target).is_some_and(|connection| {
                        connection.is_current()
                            && connection.region_handle == Some(current.handoff.region_handle)
                            && connection.endpoint == current.handoff.endpoint
                    });
                if same_live_target {
                    let Some(generation) = self.next_handoff_generation() else {
                        return false;
                    };
                    current.generation = generation;
                    return false;
                }
                *evidence = current.evidence;
                let current = state.take_pending().expect("same-destination handoff remains pending");
                cancel_pending_handoff(
                    current,
                    "destination connection is no longer active",
                    self.simulator_id,
                    &mut self.simulators,
                    setup.state,
                    &self.coverage_losses,
                );
            }
            HandoffDecision::ReplaceNewer => {
                let current = state.take_pending().expect("replaceable handoff remains pending");
                cancel_pending_handoff(
                    current,
                    "newer teleport finish received",
                    self.simulator_id,
                    &mut self.simulators,
                    setup.state,
                    &self.coverage_losses,
                );
            }
            HandoffDecision::IgnoreStale => return false,
            HandoffDecision::Conflict => {
                state.conflict(&current);
                cancel_pending_handoff(
                    current,
                    "conflicting handoff announcement",
                    self.simulator_id,
                    &mut self.simulators,
                    setup.state,
                    &self.coverage_losses,
                );
                tracing::warn!(
                    simulator = self.primary_id.get(),
                    region = handoff.region_handle,
                    "conflicting simulator handoff announcements; waiting for TeleportCancel"
                );
                return false;
            }
        }
        true
    }

    pub(super) async fn handle_handoff_message(
        &mut self,
        source: SimulatorId,
        message: &Message,
        packet_order: PacketOrder,
        setup: &mut PromotionSetup<'_>,
        state: &mut HandoffState,
    ) -> Result<(), SessionError> {
        if source != self.primary_id || state.is_conflicted() {
            return Ok(());
        }
        match message {
            Message::TeleportFinish(_) => {
                match super::teleport::decode_teleport_finish(message, self.session.agent_id, self.session.session_id) {
                    Ok(handoff) => {
                        self.request_handoff(
                            source,
                            handoff,
                            HandoffAnnouncement::TeleportFinish(HandoffOrder::Packet(packet_order)),
                            setup,
                            state,
                        )
                        .await
                    }
                    Err(reason) => {
                        tracing::debug!(simulator = source.get(), announcement = "TeleportFinish", reason, "handoff announcement rejected");
                        Ok(())
                    }
                }
            }
            Message::TeleportFailed(failed) if failed.agent_id == self.session.agent_id => {
                if teleport_failure_matches(state.pending(), HandoffOrder::Packet(packet_order))
                    && let Some(pending) = state.take_pending()
                {
                    cancel_pending_handoff(
                        pending,
                        "teleport failed",
                        self.simulator_id,
                        &mut self.simulators,
                        setup.state,
                        &self.coverage_losses,
                    );
                }
                Ok(())
            }
            Message::TeleportFailed(_) => Ok(()),
            _ => Ok(()),
        }
    }
}

impl SessionDispatcher<'_> {
    pub(super) fn handle_handoff_command_retry(&mut self) -> RuntimeAction {
        self.flush_pending_handshake_replies();
        for connection in self.state.simulators.values_mut() {
            connection.flush_handoff_command();
        }
        RuntimeAction::Continue
    }

    pub(super) async fn handle_send_completed(
        &mut self,
        simulator_id: SimulatorId,
        cancel_target: Option<HandoffCancellation>,
        error: Option<ConnectionError>,
        report_failure: bool,
    ) -> Result<RuntimeAction, SessionError> {
        if let Some(error) = error {
            if report_failure {
                self.state
                    .publish_event(
                        &self.events,
                        &mut self.run_state,
                        Event::SendFailed(crate::circuit::bound_send_error(error.to_string())),
                    )
                    .await?;
            }
        } else if simulator_id == self.state.primary_id
            && let Some(cancel) = cancel_target
        {
            if self.handoff_state.clear_conflict(cancel) {
                return Ok(RuntimeAction::Continue);
            }
            if matches_handoff_cancellation(&self.handoff_state, cancel)
                && let Some(pending) = self.handoff_state.cancel_matching(cancel)
            {
                cancel_pending_handoff(
                    pending,
                    "teleport cancelled",
                    self.state.simulator_id,
                    &mut self.state.simulators,
                    &mut self.run_state,
                    &self.state.coverage_losses,
                );
            }
        }
        Ok(RuntimeAction::Continue)
    }

    pub(super) async fn promote_primary(
        &mut self,
        simulator_id: SimulatorId,
        event: Event,
        retained_bytes: Option<EventRetention>,
        coverage_watermark: Option<crate::coverage::CoverageWatermark>,
    ) -> Result<(), SessionError> {
        let promotion = self.handoff_state.take_pending().expect("matching promotion is pending");
        let Some(_connection) = self.state.simulators.get(&promotion.target).filter(|connection| {
            connection.is_current()
                && connection.region_handle == Some(promotion.handoff.region_handle)
                && connection.endpoint == promotion.handoff.endpoint
        }) else {
            tracing::debug!(
                simulator = promotion.target.get(),
                region = promotion.handoff.region_handle,
                reason = "destination circuit is stale",
                "simulator handoff failed"
            );
            return Ok(());
        };
        let Some((capabilities, promoted_event_queue)) = self
            .state
            .simulators
            .get(&promotion.target)
            .and_then(|simulator| simulator.capabilities.as_ref())
            .filter(|capabilities| capabilities.seed_capability == promotion.handoff.seed_capability)
            .and_then(|capabilities| capabilities.event_queue.role().cloned().map(|role| (capabilities.client.clone(), role)))
        else {
            tracing::warn!(
                simulator = promotion.target.get(),
                region = promotion.handoff.region_handle,
                reason = "destination capabilities are unavailable",
                "simulator handoff failed"
            );
            return Ok(());
        };
        let previous_simulator_id = self.state.primary_id;
        self.state
            .publish_simulator_event(
                simulator_id,
                EventEnvelope::from_parts(event, retained_bytes, coverage_watermark),
                &self.events,
                &mut self.run_state,
            )
            .await?;
        if self.state.logout.deadline().is_some() {
            return Ok(());
        }
        let mut session = self.state.session.clone();
        session.simulator_address = promotion.handoff.endpoint;
        session.seed_capability = promotion.handoff.seed_capability;
        let role_generation = promotion.generation.get();
        self.state.promote_primary(promotion.target, role_generation);
        tracing::info!(simulator = promotion.target.get(), region = promotion.handoff.region_handle, "simulator promoted");
        if let Some(previous_role) = self
            .state
            .simulators
            .get(&previous_simulator_id)
            .and_then(|simulator| simulator.capabilities.as_ref())
            .and_then(|capabilities| capabilities.event_queue.role())
        {
            previous_role.set_primary(false);
        }
        promoted_event_queue.set_primary(true);
        self.state
            .publish_event(
                &self.events,
                &mut self.run_state,
                Event::SimulatorPromoted {
                    previous_simulator_id,
                    simulator_id: promotion.target,
                    session: Box::new(session),
                    capabilities,
                },
            )
            .await
    }
}

pub(crate) fn handoff_cancellation(pending: &PendingPromotion) -> HandoffCancellation {
    HandoffCancellation { source: pending.source, target: pending.target, generation: pending.generation }
}

pub(crate) fn matches_handoff_cancellation(state: &HandoffState, cancellation: HandoffCancellation) -> bool {
    state.cancellation().is_some_and(|current| {
        current.source == cancellation.source && current.target == cancellation.target && current.generation == cancellation.generation
    })
}

fn compare_finish_order(current: FinishOrder, incoming: FinishOrder) -> Option<Ordering> {
    match (current, incoming) {
        (FinishOrder::Packet(current), FinishOrder::Packet(incoming)) => Some(incoming.cmp(&current)),
        (
            FinishOrder::EventQueue { incarnation: current_incarnation, batch: current_batch, index: current_index },
            FinishOrder::EventQueue { incarnation: incoming_incarnation, batch: incoming_batch, index: incoming_index },
        ) if current_incarnation == incoming_incarnation => Some((incoming_batch, incoming_index).cmp(&(current_batch, current_index))),
        _ => None,
    }
}

pub(super) fn same_handoff_destination(left: &HandoffTarget, right: &HandoffTarget) -> bool {
    left.agent_id == right.agent_id
        && left.session_id == right.session_id
        && left.region_handle == right.region_handle
        && left.endpoint == right.endpoint
}

pub(super) fn arbitrate_handoff(
    current: &PendingPromotion,
    incoming: &HandoffTarget,
    announcement: HandoffAnnouncement,
) -> HandoffDecision {
    if same_handoff_destination(&current.handoff, incoming) {
        return HandoffDecision::SameDestination;
    }
    if current.evidence.crossed_region {
        return HandoffDecision::Conflict;
    }
    let Some(current_order) = current.evidence.teleport_finish else {
        return HandoffDecision::Conflict;
    };
    let HandoffAnnouncement::TeleportFinish(incoming) = announcement else {
        return HandoffDecision::Conflict;
    };
    let incoming_order = incoming.finish_order();
    let Some(order) = compare_finish_order(current_order, incoming_order) else { return HandoffDecision::Conflict };
    match order {
        Ordering::Greater => HandoffDecision::ReplaceNewer,
        Ordering::Less => HandoffDecision::IgnoreStale,
        Ordering::Equal => HandoffDecision::Conflict,
    }
}

pub(super) fn teleport_failure_matches(pending: Option<&PendingPromotion>, order: HandoffOrder) -> bool {
    let Some(pending) = pending else { return false };
    if pending.evidence.crossed_region {
        return false;
    }
    pending.evidence.teleport_finish.is_some_and(|finish| compare_finish_order(finish, order.finish_order()) == Some(Ordering::Greater))
}

pub(super) fn cancel_pending_handoff(
    current: PendingPromotion,
    reason: &'static str,
    root_simulator_id: SimulatorId,
    simulators: &mut HashMap<SimulatorId, SimulatorRecord>,
    run_state: &mut RunState,
    coverage_losses: &CoverageLossLedger,
) {
    tracing::info!(simulator = current.target.get(), region = current.handoff.region_handle, reason, "simulator handoff cancelled");
    let Some(connection) = simulators.get_mut(&current.target).filter(|connection| {
        connection.region_handle == Some(current.handoff.region_handle) && connection.endpoint == current.handoff.endpoint
    }) else {
        return;
    };
    if current.target == root_simulator_id {
        connection.queue_handoff_command(HandoffCommand::Cancel);
        return;
    }
    if connection.connection == super::simulators::SimulatorConnectionLifecycle::Connected
        && connection.connection_task.as_ref().is_some_and(|task| !task.is_finished())
    {
        connection.log_disconnected(current.target, current.handoff.region_handle);
    }
    connection.connection = super::simulators::SimulatorConnectionLifecycle::Retiring;
    connection.abort_connection();
    let event_queue_task = connection.capabilities.as_ref().and_then(|capabilities| capabilities.event_queue.task());
    let queued = std::mem::take(&mut connection.preconnect_events);
    connection.capabilities.take();
    run_state.preconnect_events_len = run_state.preconnect_events_len.saturating_sub(queued.len());
    if let Some(task) = event_queue_task {
        task.abort();
    }
    for event in queued {
        record_event_envelope_loss(coverage_losses, event, CoverageLossReason::EventAdmission);
    }
}

pub(crate) fn matches_pending_promotion(
    pending: Option<&PendingPromotion>,
    active_primary: SimulatorId,
    simulator_id: SimulatorId,
    movement: &AgentMovementComplete,
) -> bool {
    pending.is_some_and(|pending| {
        pending.source == active_primary
            && pending.target == simulator_id
            && movement.agent_data.agent_id == pending.handoff.agent_id
            && movement.agent_data.session_id == pending.handoff.session_id
            && movement.data.region_handle == pending.handoff.region_handle
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use protocol::login::ResponseData;
    use tokio::{net::UdpSocket, time::timeout};

    use super::*;
    use crate::{
        Error,
        session::{SessionRuntime, tests::login_session},
    };

    #[test]
    fn evidence_keeps_newest_packet_finish() {
        let mut evidence = HandoffEvidence::default();
        evidence.record(HandoffAnnouncement::TeleportFinish(HandoffOrder::Packet(PacketOrder::Ordered(10))));
        evidence.record(HandoffAnnouncement::TeleportFinish(HandoffOrder::Packet(PacketOrder::Ordered(8))));
        assert_eq!(evidence.teleport_finish, Some(FinishOrder::Packet(10)));

        evidence.record(HandoffAnnouncement::TeleportFinish(HandoffOrder::Packet(PacketOrder::Ordered(12))));
        assert_eq!(evidence.teleport_finish, Some(FinishOrder::Packet(12)));
    }

    #[test]
    fn evidence_keeps_newest_queue_finish() {
        let mut evidence = HandoffEvidence::default();
        evidence.record(HandoffAnnouncement::TeleportFinish(HandoffOrder::EventQueue { incarnation: 4, batch: 7, index: 3 }));
        evidence.record(HandoffAnnouncement::TeleportFinish(HandoffOrder::EventQueue { incarnation: 4, batch: 7, index: 2 }));
        assert_eq!(evidence.teleport_finish, Some(FinishOrder::EventQueue { incarnation: 4, batch: 7, index: 3 }));

        evidence.record(HandoffAnnouncement::TeleportFinish(HandoffOrder::EventQueue { incarnation: 4, batch: 8, index: 0 }));
        assert_eq!(evidence.teleport_finish, Some(FinishOrder::EventQueue { incarnation: 4, batch: 8, index: 0 }));
    }

    #[test]
    fn evidence_discards_ordering_across_sources() {
        let mut evidence = HandoffEvidence::default();
        evidence.record(HandoffAnnouncement::TeleportFinish(HandoffOrder::Packet(PacketOrder::Ordered(10))));
        evidence.record(HandoffAnnouncement::TeleportFinish(HandoffOrder::EventQueue { incarnation: 4, batch: 7, index: 3 }));
        assert_eq!(evidence.teleport_finish, Some(FinishOrder::Unusable));
    }

    #[tokio::test]
    async fn handoff_targets_admitted_id() {
        let root = UdpSocket::bind("127.0.0.1:0").await.expect("bind root simulator");
        let destination = UdpSocket::bind("127.0.0.1:0").await.expect("bind destination simulator");
        let session = login_session(root.local_addr().expect("root endpoint"));
        let target = HandoffTarget {
            agent_id: session.agent_id,
            session_id: session.session_id,
            region_handle: 42,
            endpoint: destination.local_addr().expect("destination endpoint"),
            seed_capability: session.seed_capability.clone(),
        };
        let mut connection = SessionRuntime::connect(session, ResponseData::default(), crate::session::tests::test_options())
            .await
            .expect("session startup")
            .expect("primary connection starts");
        let (application_events, _application_receiver) = mpsc::channel(4);
        let (neighbor_events, _neighbor_event_receiver) = mpsc::channel(4);
        let (neighbor_requests, _neighbor_request_receiver) = mpsc::channel(4);
        let (capability_events, _capability_event_receiver) = mpsc::channel(4);
        let (shutdown_signal, shutdown) = watch::channel(false);
        let mut run_state = RunState::new();
        let mut setup = PromotionSetup {
            state: &mut run_state,
            neighbor_requests,
            application_events,
            neighbor_events,
            shutdown: shutdown.clone(),
            capability_events,
        };

        let old_id = connection
            .state
            .start_neighbor(connection.state.primary_id, target.region_handle, target.endpoint, None, &mut setup)
            .await
            .expect("start first destination attempt")
            .expect("first attempt is admitted");
        let old = connection.state.simulators.get_mut(&old_id).expect("first attempt record");
        old.connection = crate::session::simulators::SimulatorConnectionLifecycle::Retiring;
        old.abort_connection();

        let replacement_id = connection
            .state
            .start_neighbor(connection.state.primary_id, target.region_handle, target.endpoint, None, &mut setup)
            .await
            .expect("start retry before joining old attempt")
            .expect("replacement is admitted");
        assert_ne!(old_id, replacement_id);
        assert!(connection.state.simulators.get(&old_id).is_some_and(|simulator| simulator.is_retiring()));
        assert!(connection.state.simulators.get(&replacement_id).is_some_and(|simulator| simulator.is_current()));

        let mut handoff_state = HandoffState::default();
        connection
            .state
            .request_handoff(
                connection.state.primary_id,
                target.clone(),
                HandoffAnnouncement::TeleportFinish(HandoffOrder::Packet(PacketOrder::Ordered(60))),
                &mut setup,
                &mut handoff_state,
            )
            .await
            .expect("prepare replacement handoff");
        assert_eq!(handoff_state.pending().map(|pending| pending.target), Some(replacement_id));

        for simulator in connection.state.simulators.values() {
            if let Some(capability) = &simulator.capabilities {
                capability.client.close_session();
            }
        }
        shutdown_signal.send_replace(true);
        setup.state.connections.shutdown().await;
        setup.state.event_queues.shutdown().await;
        connection.state.finish_capability_request_waiters().await;
    }

    #[tokio::test]
    async fn retired_capability_requests_are_awaited() {
        let root = UdpSocket::bind("127.0.0.1:0").await.expect("bind root simulator");
        let destination = UdpSocket::bind("127.0.0.1:0").await.expect("bind destination");
        let session = login_session(root.local_addr().expect("root endpoint"));
        let target = HandoffTarget {
            agent_id: session.agent_id,
            session_id: session.session_id,
            region_handle: 42,
            endpoint: destination.local_addr().expect("destination endpoint"),
            seed_capability: session.seed_capability.clone(),
        };
        let mut connection = SessionRuntime::connect(session, ResponseData::default(), crate::session::tests::test_options())
            .await
            .expect("session startup")
            .expect("primary connection starts");
        let (application_events, _application_receiver) = mpsc::channel(4);
        let (neighbor_events, _neighbor_event_receiver) = mpsc::channel(4);
        let (neighbor_requests, _neighbor_request_receiver) = mpsc::channel(4);
        let (capability_events, _capability_event_receiver) = mpsc::channel(4);
        let (_shutdown_signal, shutdown) = watch::channel(false);
        let mut run_state = RunState::new();
        let mut setup = PromotionSetup {
            state: &mut run_state,
            neighbor_requests,
            application_events,
            neighbor_events,
            shutdown: shutdown.clone(),
            capability_events: capability_events.clone(),
        };
        connection
            .state
            .start_neighbor(connection.state.primary_id, target.region_handle, target.endpoint, None, &mut setup)
            .await
            .expect("start destination connection");
        let id = connection
            .state
            .simulators
            .iter()
            .find_map(|(id, simulator)| (simulator.region_handle == Some(target.region_handle)).then_some(*id))
            .expect("destination simulator record");
        let client = connection
            .state
            .ensure_handoff_capabilities(id, &target, capability_events, shutdown, setup.state)
            .expect("start destination capability");
        assert_eq!(connection.state.capability_request_waiters.len(), 2);
        remove_simulator_capability(&mut connection.state.simulators, id);
        assert!(connection.state.simulators.get(&id).is_some_and(|simulator| simulator.capabilities.is_none()));
        timeout(Duration::from_secs(2), connection.state.finish_capability_request_waiters())
            .await
            .expect("wait for root and retired destination capability requests");
        assert!(connection.state.capability_request_waiters.is_empty());
        drop(setup);
        while timeout(Duration::from_secs(2), run_state.event_queues.join_next()).await.expect("join retired EventQueue").is_some() {}
        run_state.connections.shutdown().await;
        std::assert_matches!(client.request_capabilities(&["TestCapability"]).await, Err(Error::CapabilitySessionClosed));
    }
}
