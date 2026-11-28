use std::{
    collections::{HashMap, VecDeque},
    mem,
    net::SocketAddr,
    num::NonZeroU64,
};

use llsd::Value;
use protocol::{lludp::Message, login::ResponseData, messages::ThrottleRates};
#[cfg(test)]
use tokio::time;
use tokio::{
    sync::{mpsc, mpsc::error::TrySendError, oneshot, watch},
    task::{AbortHandle, Id, JoinError, JoinSet},
    time::Instant,
};
use url::Url;
use uuid::Uuid;

#[cfg(test)]
pub(in crate::session) use self::tests::publish_primary_terminal_event;
use super::{
    Command, SessionError, SessionOptions,
    capability_events::{decode_agent_communication, decode_enable_simulators},
    handoff::{HandoffAnnouncement, HandoffOrder, HandoffState, PromotionSetup, cancel_pending_handoff, teleport_failure_matches},
    logout::{LogoutUnavailable, PendingLogout, SessionLogout},
    routing::{
        EventQueueOutcome, MAX_PENDING_REGION_HANDSHAKE_REPLIES, MAX_PENDING_SESSION_EVENTS, PrimaryEventBacklog, primary_event_queue_size,
        queue_simulator_event,
    },
    runtime::SessionDispatcher,
    teleport::{decode_crossed_region, decode_event_teleport_failed, decode_event_teleport_finish, is_teleport_cancel},
};
#[cfg(not(test))]
use crate::circuit::admission::SESSION_APPLICATION_EVENT_CREDIT_CAPACITY;
use crate::{
    AgentSession,
    capabilities::{CapabilityBudget, Client, EventQueueExit, EventQueueRoleHandle},
    circuit::{
        HandoffCancellation, HandoffGeneration, HandoffTarget, LogoutCommand, MAX_PRECONNECT_EVENTS, MAX_SIMULATOR_CONNECTIONS,
        PrimaryState, SimulatorCommand, SimulatorEvent, SimulatorRequest, SimulatorTaskOutcome, UdpTransport,
        admission::{ConnectionRole, SimulatorMemoryBudget},
        lludp::Delivery,
        pacing::OutboundBudget,
    },
    coverage::{CoverageLossLedger, CoverageLossReason, CoverageSource, record_coverage_loss, record_event_envelope_loss},
    error::Error,
    event::{Event, EventEnvelope, SimulatorId},
};

pub(super) const MAX_TRACKED_CAPABILITY_CLIENTS: usize = MAX_SIMULATOR_CONNECTIONS;
pub(super) const NEIGHBOR_COMMAND_CAPACITY: usize = 8;

pub(super) type ConnectionTaskResult = (SimulatorId, Option<u64>, SocketAddr, SimulatorTaskOutcome);
pub(super) type EventQueueTaskResult = (SimulatorId, SocketAddr, EventQueueExit);
pub(super) type EventQueueTaskJoin = Option<Result<(Id, EventQueueTaskResult), JoinError>>;

pub(super) struct ConnectionQueueFailure {
    pub(super) error: Error,
    pub(super) completion: Option<oneshot::Sender<Result<(), Error>>>,
}

pub(super) struct SimulatorRecord {
    pub(super) endpoint: SocketAddr,
    pub(super) region_handle: Option<u64>,
    pub(super) connection_task: Option<AbortHandle>,
    pub(super) commands: mpsc::Sender<SimulatorCommand>,
    pub(super) logout: mpsc::Sender<LogoutCommand>,
    pub(super) pending_handoff_command: Option<HandoffCommand>,
    pub(super) primary_state: watch::Sender<PrimaryState>,
    pub(super) packet_memory: SimulatorMemoryBudget,
    pub(super) connection: SimulatorConnectionLifecycle,
    pub(super) capabilities: Option<SimulatorCapabilitySession>,
    pub(super) preconnect_events: VecDeque<EventEnvelope>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SimulatorConnectionLifecycle {
    Connecting,
    Connected,
    Retiring,
    Closed,
}

impl SimulatorRecord {
    pub(super) fn connection_role(&self) -> ConnectionRole {
        self.primary_state.borrow().connection_role()
    }

    pub(super) fn is_current(&self) -> bool {
        matches!(self.connection, SimulatorConnectionLifecycle::Connecting | SimulatorConnectionLifecycle::Connected)
    }

    pub(super) fn is_retiring(&self) -> bool {
        matches!(self.connection, SimulatorConnectionLifecycle::Retiring)
    }

    pub(super) fn mark_connected(&mut self) {
        if matches!(self.connection, SimulatorConnectionLifecycle::Connecting) {
            self.connection = SimulatorConnectionLifecycle::Connected;
        }
    }

    pub(super) fn abort_connection(&self) {
        if let Some(task) = &self.connection_task {
            task.abort();
        }
    }

    pub(super) fn connection_task_id(&self) -> Option<Id> {
        self.connection_task.as_ref().map(AbortHandle::id)
    }

    pub(super) fn log_disconnected(&self, simulator_id: SimulatorId, region_handle: u64) {
        tracing::info!(
            simulator = simulator_id.get(),
            region = region_handle,
            role = ?self.connection_role(),
            "simulator disconnected"
        );
    }

    pub(super) fn queue_handoff_command(&mut self, command: HandoffCommand) -> bool {
        self.pending_handoff_command = Some(command);
        self.flush_handoff_command();
        !self.commands.is_closed()
    }

    pub(super) fn flush_handoff_command(&mut self) {
        let Some(command) = self.pending_handoff_command.take() else { return };
        let command = match command {
            HandoffCommand::Prepare(target) => SimulatorCommand::PrepareHandoff(target),
            HandoffCommand::Cancel => SimulatorCommand::CancelHandoff,
        };
        match self.commands.try_send(command) {
            Ok(()) => {}
            Err(TrySendError::Full(command)) => {
                self.pending_handoff_command = Some(match command {
                    SimulatorCommand::PrepareHandoff(target) => HandoffCommand::Prepare(target),
                    SimulatorCommand::CancelHandoff => HandoffCommand::Cancel,
                    SimulatorCommand::ReplyRegionHandshake { .. } => unreachable!("handshake replies are not deferred handoff commands"),
                    SimulatorCommand::Send { .. } => unreachable!("only handoff control commands are deferred"),
                });
            }
            Err(TrySendError::Closed(_)) => {}
        }
    }
}

pub(super) enum HandoffCommand {
    Prepare(HandoffTarget),
    Cancel,
}

pub(super) struct SessionRuntimeOptions {
    pub(super) coverage_losses: CoverageLossLedger,
    pub(super) session: SessionOptions,
    pub(super) startup_disconnect: Option<watch::Receiver<bool>>,
    pub(super) root_transport: Option<UdpTransport>,
    #[cfg(test)]
    pub(super) application_event_capacity: usize,
}

impl SessionRuntimeOptions {
    pub(super) fn application_event_capacity(&self) -> usize {
        #[cfg(test)]
        {
            self.application_event_capacity
        }
        #[cfg(not(test))]
        {
            SESSION_APPLICATION_EVENT_CREDIT_CAPACITY
        }
    }
}

pub(super) struct SessionRuntime {
    pub(super) state: SessionState,
    pub(super) runtime_inputs: RuntimeInputs,
}

pub(super) struct SessionState {
    pub(super) simulator_id: SimulatorId,
    pub(super) primary_id: SimulatorId,
    pub(super) simulators: HashMap<SimulatorId, SimulatorRecord>,
    pub(super) neighbor_events: mpsc::Sender<SimulatorEvent>,
    pub(super) neighbor_requests: mpsc::Sender<SimulatorRequest>,
    pub(super) neighbor_cancellation: watch::Sender<bool>,
    pub(super) logout: SessionLogout,
    pub(super) session: AgentSession,
    pub(super) capability_budget: CapabilityBudget,
    pub(super) capability_request_waiters: JoinSet<()>,
    pub(super) login_data: ResponseData,
    pub(super) event_backlog: PrimaryEventBacklog,
    pub(super) packet_memory: SimulatorMemoryBudget,
    pub(super) coverage_losses: CoverageLossLedger,
    pub(super) outbound_budget: Option<OutboundBudget>,
    pub(super) throttle_rates: ThrottleRates,
    pub(super) handoff_generation: Option<u64>,
}

pub(super) struct RuntimeInputs {
    pub(super) neighbor_event_receiver: mpsc::Receiver<SimulatorEvent>,
    pub(super) neighbor_request_receiver: mpsc::Receiver<SimulatorRequest>,
    pub(super) root_connection_workers: JoinSet<ConnectionTaskResult>,
}

impl Drop for SessionState {
    fn drop(&mut self) {
        self.neighbor_cancellation.send_replace(true);
        for (simulator_id, simulator) in &self.simulators {
            self.coverage_losses.retire(*simulator_id, CoverageSource::Lludp);
            if let Some(capabilities) = &simulator.capabilities {
                capabilities.client.close_session();
            }
        }
    }
}

impl SessionState {
    pub(super) fn primary(&self) -> &SimulatorRecord {
        self.simulators.get(&self.primary_id).expect("primary simulator record exists")
    }

    #[cfg(test)]
    pub(super) fn primary_mut(&mut self) -> &mut SimulatorRecord {
        self.simulators.get_mut(&self.primary_id).expect("primary simulator record exists")
    }

    pub(super) fn mark_connection_task_joined(
        &mut self,
        task_id: Id,
    ) -> Option<(SimulatorId, Option<u64>, SocketAddr, SimulatorConnectionLifecycle)> {
        let simulator_id = self
            .simulators
            .iter()
            .find_map(|(simulator_id, simulator)| (simulator.connection_task_id() == Some(task_id)).then_some(*simulator_id))?;
        let simulator = self.simulators.get_mut(&simulator_id)?;
        simulator.connection_task.take()?;
        let joined = (simulator_id, simulator.region_handle, simulator.endpoint, simulator.connection);
        if simulator.connection == SimulatorConnectionLifecycle::Closed {
            self.simulators.remove(&simulator_id);
        }
        Some(joined)
    }

    pub(super) fn finish_event_queue_task(
        &mut self,
        task_id: Id,
        exit: EventQueueExit,
    ) -> Option<(SimulatorId, SocketAddr, EventQueueExit)> {
        let simulator_id = self.simulators.iter().find_map(|(simulator_id, simulator)| {
            simulator
                .capabilities
                .as_ref()
                .is_some_and(|capabilities| capabilities.event_queue.task_id() == Some(task_id))
                .then_some(*simulator_id)
        })?;
        let simulator = self.simulators.get_mut(&simulator_id)?;
        let capabilities = simulator.capabilities.as_mut()?;
        if !capabilities.event_queue.finish(task_id, exit) {
            return None;
        }
        Some((simulator_id, simulator.endpoint, capabilities.event_queue.exit()?))
    }

    pub(super) fn promote_primary(&mut self, simulator_id: SimulatorId, role_generation: u64) {
        let old_id = self.primary_id;
        let old_memory = self.simulators.get(&old_id).map(|record| record.packet_memory.clone());
        if let Some(old) = self.simulators.get_mut(&old_id) {
            old.primary_state.send_replace(PrimaryState::Neighbor { generation: role_generation });
        }
        let Some(next) = self.simulators.get_mut(&simulator_id) else { return };
        if let Some(memory) = old_memory {
            memory.promote_to(&next.packet_memory);
        }
        next.primary_state.send_replace(PrimaryState::Ready { generation: role_generation });
        self.primary_id = simulator_id;
    }

    pub(super) fn neighbor_connection_tasks(&self) -> usize {
        self.simulators
            .iter()
            .filter(|(simulator_id, simulator)| **simulator_id != self.simulator_id && simulator.connection_task.is_some())
            .count()
    }

    pub(super) fn next_handoff_generation(&mut self) -> Option<HandoffGeneration> {
        let Some(generation) = self.handoff_generation.and_then(|generation| generation.checked_add(1)) else {
            if self.handoff_generation.is_some() {
                self.handoff_generation = None;
                tracing::error!(simulator = self.primary_id.get(), "simulator handoff generation space is exhausted");
            }
            return None;
        };
        self.handoff_generation = Some(generation);
        NonZeroU64::new(generation).map(HandoffGeneration::new)
    }
}

impl SessionRuntime {
    pub(super) async fn run(
        self,
        commands: mpsc::Receiver<Command>,
        events: mpsc::Sender<EventEnvelope>,
        disconnect_requested: watch::Receiver<bool>,
        terminal_event_permit: mpsc::OwnedPermit<EventEnvelope>,
    ) -> Result<(), SessionError> {
        let Self { mut state, runtime_inputs } = self;
        let result = match SessionDispatcher::start(&mut state, runtime_inputs, commands, events, disconnect_requested).await {
            Ok(mut runtime) => {
                let result = runtime.run().await;
                runtime.shutdown().await;
                result
            }
            Err(error) => Err(error),
        };
        if let Some(outbound_budget) = state.outbound_budget.take() {
            outbound_budget.shutdown().await;
        }
        state.finish_capability_request_waiters().await;
        let primary = state.primary();
        SessionState::publish_primary_terminal_event(
            terminal_event_permit,
            state.primary_id,
            primary.region_handle,
            primary.endpoint,
            &result,
        );
        result
    }
}

impl SessionState {
    pub(super) fn new_capability_client(&mut self, seed: Url) -> Result<Client, SessionError> {
        if self.capability_request_waiters.len() >= MAX_TRACKED_CAPABILITY_CLIENTS {
            return Err(SessionError::CapabilityClientLimit);
        }
        let client = Client::new_with_budget(seed, self.capability_budget.clone())?;
        track_capability_requests(&mut self.capability_request_waiters, &client);
        Ok(client)
    }

    pub(super) async fn finish_capability_request_waiters(&mut self) {
        for simulator in self.simulators.values() {
            if let Some(capabilities) = &simulator.capabilities {
                capabilities.client.close_session();
            }
        }
        while let Some(result) = self.capability_request_waiters.join_next().await {
            if result.is_err() {
                tracing::error!("Capability request waiter task failed");
            }
        }
    }

    fn publish_primary_terminal_event(
        permit: mpsc::OwnedPermit<EventEnvelope>,
        simulator_id: SimulatorId,
        region_handle: Option<u64>,
        endpoint: SocketAddr,
        result: &Result<(), SessionError>,
    ) {
        let event = match result {
            Ok(()) => Event::SimulatorDisconnected { simulator_id, region_handle, endpoint },
            Err(reason) => Event::SimulatorFailed { simulator_id, region_handle, endpoint, reason: reason.to_string() },
        };
        permit.send(EventEnvelope::new(event));
    }

    pub(super) async fn publish_event(
        &mut self,
        events: &mpsc::Sender<EventEnvelope>,
        state: &mut RunState,
        event: impl Into<EventEnvelope>,
    ) -> Result<(), SessionError> {
        let RunState { pending_events, pending_lifecycle_events, .. } = state;
        let mut event = event.into();
        if self.logout.deadline().is_some() && event.event.is_application() {
            if let Some(watermark) = event.coverage_watermark() {
                record_coverage_loss(&self.coverage_losses, watermark.simulator_id, watermark.source, CoverageLossReason::EventAdmission);
            }
            return Ok(());
        }
        let is_application = event.event.is_application();
        let acquired_application_credit = is_application && !event.has_application_credit();
        if acquired_application_credit {
            let Some(permit) = self.packet_memory.application_events().try_acquire() else {
                if self.packet_memory.application_events().note_failure() {
                    tracing::warn!("application event intake is under pressure");
                }
                if let Some(watermark) = event.coverage_watermark() {
                    record_coverage_loss(
                        &self.coverage_losses,
                        watermark.simulator_id,
                        watermark.source,
                        CoverageLossReason::EventAdmission,
                    );
                }
                return Ok(());
            };
            event.attach_application_credit(permit);
        }
        let watermark = event.coverage_watermark();
        let outcome = queue_simulator_event(events, pending_events, pending_lifecycle_events, event);
        let should_logout = match outcome {
            EventQueueOutcome::Queued => false,
            EventQueueOutcome::ReceiverClosed => {
                if let Some(watermark) = watermark {
                    record_coverage_loss(
                        &self.coverage_losses,
                        watermark.simulator_id,
                        watermark.source,
                        CoverageLossReason::EventAdmission,
                    );
                }
                for queued in pending_events.drain(..) {
                    record_event_envelope_loss(&self.coverage_losses, queued, CoverageLossReason::EventAdmission);
                }
                *pending_lifecycle_events = 0;
                true
            }
            EventQueueOutcome::BacklogFull => {
                if let Some(watermark) = watermark {
                    record_coverage_loss(
                        &self.coverage_losses,
                        watermark.simulator_id,
                        watermark.source,
                        CoverageLossReason::EventAdmission,
                    );
                }
                return Err(SessionError::ApplicationEventBacklogExceeded);
            }
            EventQueueOutcome::LifecycleBacklogFull => {
                return Err(SessionError::LifecycleEventBacklogFull);
            }
        };

        if acquired_application_credit && !should_logout {
            self.packet_memory.application_events().note_success();
        }

        let now = Instant::now();
        if let Some(backlog_duration_ms) = self.event_backlog.observe(pending_events.len(), now) {
            let (queue_occupancy, queue_capacity) = primary_event_queue_size(events, pending_events.len());
            tracing::warn!(
                queue_occupancy,
                queue_capacity,
                backlog_duration_ms,
                "primary simulator application-event queue is under pressure"
            );
        }

        if should_logout && self.logout.deadline().is_none() {
            let (command, pending) = PendingLogout::new();
            let deadline = Some(pending.deadline());
            match self.primary().logout.try_send(command) {
                Ok(()) => {
                    self.logout = SessionLogout::Sending { pending };
                }
                Err(TrySendError::Full(_command)) => {
                    self.record_logout_unavailable(LogoutUnavailable::QueueFull, deadline);
                }
                Err(TrySendError::Closed(_command)) => {
                    self.record_logout_unavailable(LogoutUnavailable::ConnectionClosed, deadline);
                }
            }
        }
        Ok(())
    }

    pub(super) async fn publish_simulator_event(
        &mut self,
        simulator_id: SimulatorId,
        event: EventEnvelope,
        events: &mpsc::Sender<EventEnvelope>,
        state: &mut RunState,
    ) -> Result<(), SessionError> {
        let watermark = event.coverage_watermark();
        let Some(simulator) = self.simulators.get_mut(&simulator_id) else { return Ok(()) };
        if matches!(simulator.connection, SimulatorConnectionLifecycle::Connected) {
            self.publish_event(events, state, event).await
        } else if matches!(simulator.connection, SimulatorConnectionLifecycle::Connecting)
            && state.preconnect_events_len < MAX_PRECONNECT_EVENTS
        {
            simulator.preconnect_events.push_back(event);
            state.preconnect_events_len += 1;
            Ok(())
        } else {
            if let Some(watermark) = watermark {
                record_coverage_loss(&self.coverage_losses, watermark.simulator_id, watermark.source, CoverageLossReason::EventAdmission);
            }
            Ok(())
        }
    }

    pub(super) async fn publish_simulator_connected(
        &mut self,
        simulator_id: SimulatorId,
        event: impl Into<EventEnvelope>,
        events: &mpsc::Sender<EventEnvelope>,
        state: &mut RunState,
    ) -> Result<(), SessionError> {
        if let Some(simulator) = self.simulators.get_mut(&simulator_id) {
            simulator.mark_connected();
        }
        self.publish_event(events, state, event).await?;
        let queued = if let Some(simulator) = self.simulators.get_mut(&simulator_id) {
            mem::take(&mut simulator.preconnect_events)
        } else {
            VecDeque::new()
        };
        state.preconnect_events_len = state.preconnect_events_len.saturating_sub(queued.len());
        for event in queued {
            self.publish_event(events, state, event).await?;
        }
        Ok(())
    }

    pub(super) async fn publish_session_ready(
        &mut self,
        events: &mpsc::Sender<EventEnvelope>,
        state: &mut RunState,
    ) -> Result<(), SessionError> {
        self.publish_event(
            events,
            state,
            Event::SessionReady {
                simulator_id: self.simulator_id,
                session: Box::new(self.session.clone()),
                capabilities: self
                    .simulators
                    .get(&self.simulator_id)
                    .and_then(|simulator| simulator.capabilities.as_ref())
                    .expect("initial simulator capabilities exist")
                    .client
                    .clone(),
                login_data: self.login_data.clone(),
            },
        )
        .await
    }

    pub(super) async fn process_capability_event(
        &mut self,
        simulator_id: SimulatorId,
        message: &str,
        body: &Value,
        order: Option<HandoffOrder>,
        setup: &mut PromotionSetup<'_>,
        handoff_state: &mut HandoffState,
    ) -> Result<(), SessionError> {
        if self.logout.deadline().is_some() {
            return Ok(());
        }
        match message {
            "EnableSimulator" => self.process_enable_simulators(simulator_id, body, setup).await,
            "CrossedRegion" => self.process_crossed_region(simulator_id, body, setup, handoff_state).await,
            "TeleportFinish" => self.process_teleport_finish(simulator_id, body, order, setup, handoff_state).await,
            "TeleportFailed" => self.process_teleport_failed(body, order, setup, handoff_state),
            "EstablishAgentCommunication" => self.process_agent_communication(body, setup).await,
            _ => Ok(()),
        }
    }

    pub(super) async fn process_enable_simulators(
        &mut self,
        simulator_id: SimulatorId,
        body: &Value,
        setup: &mut PromotionSetup<'_>,
    ) -> Result<(), SessionError> {
        match decode_enable_simulators(body) {
            Ok(simulators) => {
                for (region_handle, endpoint) in simulators {
                    if self.logout.deadline().is_some() {
                        break;
                    }
                    self.start_neighbor(simulator_id, region_handle, endpoint, None, setup).await?;
                }
            }
            Err(_reason) => {}
        }
        Ok(())
    }

    pub(super) async fn process_crossed_region(
        &mut self,
        simulator_id: SimulatorId,
        body: &Value,
        setup: &mut PromotionSetup<'_>,
        handoff_state: &mut HandoffState,
    ) -> Result<(), SessionError> {
        match decode_crossed_region(body, self.session.agent_id, self.session.session_id) {
            Ok(handoff) => self.request_handoff(simulator_id, handoff, HandoffAnnouncement::CrossedRegion, setup, handoff_state).await,
            Err(reason) => {
                tracing::debug!(simulator = simulator_id.get(), announcement = "CrossedRegion", reason, "handoff announcement rejected");
                Ok(())
            }
        }
    }

    pub(super) async fn process_teleport_finish(
        &mut self,
        simulator_id: SimulatorId,
        body: &Value,
        order: Option<HandoffOrder>,
        setup: &mut PromotionSetup<'_>,
        handoff_state: &mut HandoffState,
    ) -> Result<(), SessionError> {
        match decode_event_teleport_finish(body, self.session.agent_id, self.session.session_id) {
            Ok(handoff) => {
                self.request_handoff(
                    simulator_id,
                    handoff,
                    HandoffAnnouncement::TeleportFinish(order.unwrap_or(HandoffOrder::Unavailable)),
                    setup,
                    handoff_state,
                )
                .await
            }
            Err(reason) => {
                tracing::debug!(simulator = simulator_id.get(), announcement = "TeleportFinish", reason, "handoff announcement rejected");
                Ok(())
            }
        }
    }

    pub(super) fn process_teleport_failed(
        &mut self,
        body: &Value,
        order: Option<HandoffOrder>,
        setup: &mut PromotionSetup<'_>,
        handoff_state: &mut HandoffState,
    ) -> Result<(), SessionError> {
        match decode_event_teleport_failed(body, self.session.agent_id) {
            Ok(()) if teleport_failure_matches(handoff_state.pending(), order.unwrap_or(HandoffOrder::Unavailable)) => {
                if let Some(pending) = handoff_state.take_pending() {
                    cancel_pending_handoff(
                        pending,
                        "teleport failed",
                        self.simulator_id,
                        &mut self.simulators,
                        setup.state,
                        &self.coverage_losses,
                    );
                }
            }
            Ok(()) => {}
            Err(_reason) => {}
        }
        Ok(())
    }

    pub(super) async fn process_agent_communication(&mut self, body: &Value, setup: &mut PromotionSetup<'_>) -> Result<(), SessionError> {
        let (endpoint, seed) = match decode_agent_communication(body, self.session.agent_id) {
            Ok(event) => event,
            Err(_reason) => {
                return Ok(());
            }
        };
        if endpoint == self.primary().endpoint {
            return Ok(());
        }
        let Some(target_id) = self
            .simulators
            .iter()
            .find_map(|(simulator_id, record)| (record.endpoint == endpoint && record.is_current()).then_some(*simulator_id))
        else {
            return Ok(());
        };
        if self.simulators.get(&target_id).is_some_and(|record| record.capabilities.is_some()) {
            return Ok(());
        }
        self.start_neighbor_capabilities(target_id, endpoint, seed, setup).await
    }

    pub(super) fn note_primary_event_queue_exit(&self, simulator_id: SimulatorId, exit: EventQueueExit) {
        let reason = match exit {
            EventQueueExit::Stopped => return,
            EventQueueExit::StoppedWithCoverageLoss => "The primary simulator EventQueueGet stopped after losing event coverage.",
            EventQueueExit::Unavailable => "The primary simulator does not have a usable EventQueueGet capability.",
            EventQueueExit::RetryLimitReached => "The primary simulator EventQueueGet stopped after repeated errors.",
            EventQueueExit::TaskFailed => "The primary simulator EventQueueGet task ended unexpectedly.",
        };

        tracing::warn!(
            simulator = simulator_id.get(),
            ?exit,
            reason,
            "primary simulator event queue stopped; keeping the authenticated session active"
        );
    }
}

pub(super) struct SimulatorCapabilitySession {
    pub(super) seed_capability: Url,
    pub(super) client: Client,
    pub(super) event_queue: EventQueueLifecycle,
}

pub(super) enum EventQueueLifecycle {
    NotStarted,
    Running { role: EventQueueRoleHandle, task: AbortHandle },
    Finished { role: EventQueueRoleHandle, exit: EventQueueExit },
}

impl EventQueueLifecycle {
    pub(super) fn is_running(&self) -> bool {
        matches!(self, Self::Running { task, .. } if !task.is_finished())
    }

    pub(super) fn role(&self) -> Option<&EventQueueRoleHandle> {
        match self {
            Self::NotStarted => None,
            Self::Running { role, .. } | Self::Finished { role, .. } => Some(role),
        }
    }

    pub(super) fn task(&self) -> Option<AbortHandle> {
        match self {
            Self::Running { task, .. } => Some(task.clone()),
            Self::NotStarted | Self::Finished { .. } => None,
        }
    }

    pub(super) fn task_id(&self) -> Option<Id> {
        match self {
            Self::Running { task, .. } => Some(task.id()),
            Self::NotStarted | Self::Finished { .. } => None,
        }
    }

    pub(super) fn exit(&self) -> Option<EventQueueExit> {
        match self {
            Self::Finished { exit, .. } => Some(*exit),
            Self::NotStarted | Self::Running { .. } => None,
        }
    }

    pub(super) fn finish(&mut self, task: Id, exit: EventQueueExit) -> bool {
        let Self::Running { role, task: current_task } = self else { return false };
        if current_task.id() != task {
            return false;
        }
        let role = role.clone();
        *self = Self::Finished { role, exit };
        true
    }
}

impl Drop for SimulatorCapabilitySession {
    fn drop(&mut self) {
        self.client.close_session();
    }
}

pub(super) struct RunState {
    pub(super) connections: JoinSet<ConnectionTaskResult>,
    pub(super) event_queues: JoinSet<EventQueueTaskResult>,
    pub(super) preconnect_events_len: usize,
    pub(super) pending_events: VecDeque<EventEnvelope>,
    pub(super) pending_lifecycle_events: usize,
    pub(super) pending_region_handshake_replies: VecDeque<(SimulatorId, SimulatorCommand)>,
}

impl RunState {
    pub(super) fn new() -> Self {
        Self {
            connections: JoinSet::new(),
            event_queues: JoinSet::new(),
            preconnect_events_len: 0,
            pending_events: VecDeque::with_capacity(MAX_PENDING_SESSION_EVENTS),
            pending_lifecycle_events: 0,
            pending_region_handshake_replies: VecDeque::with_capacity(MAX_PENDING_REGION_HANDSHAKE_REPLIES),
        }
    }
}

#[cfg(test)]
pub(super) async fn retry_pending_handoff_commands(retry: &mut time::Interval, connections: &mut HashMap<SimulatorId, SimulatorRecord>) {
    retry.tick().await;
    for connection in connections.values_mut() {
        connection.flush_handoff_command();
    }
}

pub(super) fn queue_connection_message(
    commands: &mpsc::Sender<SimulatorCommand>,
    message: Message,
    delivery: Delivery,
    completed: Option<oneshot::Sender<Result<(), Error>>>,
    cancel_target: Option<HandoffCancellation>,
    report_failure: bool,
) -> Result<(), ConnectionQueueFailure> {
    let command = SimulatorCommand::Send { message, delivery, completed, cancel_target, report_failure };
    match commands.try_send(command) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(SimulatorCommand::Send { completed, .. })) => {
            Err(ConnectionQueueFailure { error: Error::SimulatorCommandQueueFull, completion: completed })
        }
        Err(TrySendError::Closed(SimulatorCommand::Send { completed, .. })) => {
            Err(ConnectionQueueFailure { error: Error::SimulatorConnectionClosed, completion: completed })
        }
        Err(TrySendError::Full(_)) | Err(TrySendError::Closed(_)) => {
            unreachable!("only send commands are queued through this helper")
        }
    }
}

pub(super) fn queue_primary_message(
    commands: &mpsc::Sender<SimulatorCommand>,
    agent_id: Uuid,
    session_id: Uuid,
    message: Message,
    delivery: Delivery,
    cancellation: Option<HandoffCancellation>,
) -> Result<(), ConnectionQueueFailure> {
    let cancel_handoff = is_teleport_cancel(&message, agent_id, session_id);
    let cancel_target = cancel_handoff.then_some(cancellation).flatten();
    queue_connection_message(commands, message, delivery, None, cancel_target, true)
}

pub(super) fn track_capability_requests(workers: &mut JoinSet<()>, client: &Client) {
    let client = client.clone();
    workers.spawn(async move { client.wait_for_requests().await });
}

pub(super) fn remove_simulator_capability(simulators: &mut HashMap<SimulatorId, SimulatorRecord>, simulator_id: SimulatorId) {
    if let Some(simulator) = simulators.get_mut(&simulator_id) {
        simulator.capabilities.take();
    }
}

pub(super) fn is_current_simulator(simulator_id: SimulatorId, connections: &HashMap<SimulatorId, SimulatorRecord>) -> bool {
    connections.get(&simulator_id).is_some_and(SimulatorRecord::is_current)
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use tokio::sync::mpsc;

    use super::SessionState;
    use crate::{
        event::{EventEnvelope, SimulatorId},
        session::error::SessionError,
    };

    pub(in crate::session) fn publish_primary_terminal_event(
        permit: mpsc::OwnedPermit<EventEnvelope>,
        simulator_id: SimulatorId,
        region_handle: Option<u64>,
        endpoint: SocketAddr,
        result: &Result<(), SessionError>,
    ) {
        SessionState::publish_primary_terminal_event(permit, simulator_id, region_handle, endpoint, result);
    }
}
