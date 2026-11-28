use std::{
    collections::{HashMap, VecDeque},
    mem,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};

use protocol::lludp::Message;
use tokio::{
    sync::{
        mpsc,
        mpsc::error::{SendError, TrySendError},
        watch,
    },
    task::{Id, JoinError},
};
use tracing::{Instrument, field};
use url::Url;

use super::{
    ConnectionError, ConnectionTaskResult, HandoffCommand, HandoffTarget, NEIGHBOR_COMMAND_CAPACITY, PrimaryState, RunState, RuntimeAction,
    SessionDispatcher, SessionError, SessionState, SimulatorCapabilitySession, SimulatorCommand, SimulatorConnection,
    SimulatorConnectionLifecycle, SimulatorConnectionOptions, SimulatorEvent, SimulatorRecord, SimulatorRequest, SimulatorTaskOutcome,
    handoff::{PromotionSetup, matches_pending_promotion},
    is_current_simulator, record_event_envelope_loss, remove_simulator_capability,
    simulators::EventQueueLifecycle,
    teleport::is_handoff_message,
};
use crate::{
    capabilities::{EVENT_QUEUE_NAME, EventQueueExit},
    circuit::{MAX_NEIGHBOR_CONNECTION_TASKS, MAX_NEIGHBOR_SIMULATORS},
    coverage::{CoverageLossReason, CoverageWatermark},
    event::{Event, EventEnvelope, EventRetention, SimulatorId},
};

impl SessionState {
    pub(super) async fn start_neighbor(
        &mut self,
        origin: SimulatorId,
        region_handle: u64,
        endpoint: SocketAddr,
        handoff: Option<HandoffTarget>,
        setup: &mut PromotionSetup<'_>,
    ) -> Result<Option<SimulatorId>, SessionError> {
        if origin != self.primary_id || !is_current_simulator(origin, &self.simulators) {
            return Ok(None);
        }
        if handoff.as_ref().is_some_and(|target| target.region_handle != region_handle || target.endpoint != endpoint) {
            return Ok(None);
        }
        let simulator_id = SimulatorId::allocate().ok_or(SessionError::SimulatorIdSpaceExhausted)?;
        if endpoint.port() == 0
            || endpoint.ip().is_unspecified()
            || endpoint.ip().is_multicast()
            || endpoint.ip() == IpAddr::V4(Ipv4Addr::BROADCAST)
        {
            return self
                .reject_neighbor_start(
                    simulator_id,
                    region_handle,
                    endpoint,
                    "The simulator announced an invalid neighbor endpoint.",
                    setup,
                )
                .await;
        }
        if let Some(simulator_id) = Self::reuse_neighbor(region_handle, endpoint, handoff.as_ref(), &mut self.simulators) {
            return Ok(Some(simulator_id));
        }
        if self.simulators.get(&self.primary_id).is_some_and(|current| current.region_handle == Some(region_handle)) {
            return self
                .reject_neighbor_start(
                    simulator_id,
                    region_handle,
                    endpoint,
                    "The original simulator connection cannot be replaced before its handoff completes.",
                    setup,
                )
                .await;
        }
        let primary_endpoint = self.simulators.get(&self.primary_id).map_or(self.session.simulator_address, |primary| primary.endpoint);
        if (endpoint == primary_endpoint && handoff.is_none())
            || self.simulators.iter().any(|(active_id, active)| {
                active.is_current()
                    && active.region_handle != Some(region_handle)
                    && active.endpoint == endpoint
                    && (active_id != &self.primary_id || handoff.is_none())
            })
        {
            return self
                .reject_neighbor_start(
                    simulator_id,
                    region_handle,
                    endpoint,
                    "The simulator announced an endpoint that is already in use.",
                    setup,
                )
                .await;
        }
        if self.neighbor_connection_tasks() >= MAX_NEIGHBOR_CONNECTION_TASKS {
            return self
                .reject_neighbor_start(simulator_id, region_handle, endpoint, "The neighbor connection task limit has been reached.", setup)
                .await;
        }
        self.retire_neighbor(region_handle, setup.state);
        let neighbor_count =
            self.simulators.iter().filter(|(active_id, active)| **active_id != self.simulator_id && active.is_current()).count();
        if neighbor_count >= MAX_NEIGHBOR_SIMULATORS {
            return self
                .reject_neighbor_start(
                    simulator_id,
                    region_handle,
                    endpoint,
                    "The simulator announced more neighbor regions than the client can track.",
                    setup,
                )
                .await;
        }

        self.spawn_neighbor(simulator_id, region_handle, endpoint, handoff, setup);
        Ok(Some(simulator_id))
    }

    fn reuse_neighbor(
        region_handle: u64,
        endpoint: SocketAddr,
        handoff: Option<&HandoffTarget>,
        connections: &mut HashMap<SimulatorId, SimulatorRecord>,
    ) -> Option<SimulatorId> {
        let (simulator_id, current) = connections
            .iter_mut()
            .find(|(_, current)| current.is_current() && current.region_handle == Some(region_handle) && current.endpoint == endpoint)?;
        handoff.is_none_or(|handoff| current.queue_handoff_command(HandoffCommand::Prepare(handoff.clone()))).then_some(*simulator_id)
    }

    async fn reject_neighbor_start(
        &mut self,
        simulator_id: SimulatorId,
        region_handle: u64,
        endpoint: SocketAddr,
        reason: &'static str,
        setup: &mut PromotionSetup<'_>,
    ) -> Result<Option<SimulatorId>, SessionError> {
        tracing::warn!(simulator = simulator_id.get(), region = region_handle, %reason, "neighbor simulator connection was rejected");
        self.publish_event(&setup.application_events, setup.state, Event::SimulatorConnectFailed { simulator_id, region_handle, endpoint })
            .await?;
        Ok(None)
    }

    fn retire_neighbor(&mut self, region_handle: u64, state: &mut RunState) {
        let Some(simulator_id) = self.simulators.iter().find_map(|(simulator_id, current)| {
            (current.is_current() && current.region_handle == Some(region_handle)).then_some(*simulator_id)
        }) else {
            return;
        };
        let (queued, event_queue_task) = if let Some(current) = self.simulators.get_mut(&simulator_id) {
            if current.connection == SimulatorConnectionLifecycle::Connected
                && current.connection_task.as_ref().is_some_and(|task| !task.is_finished())
            {
                current.log_disconnected(simulator_id, region_handle);
            }
            current.connection = SimulatorConnectionLifecycle::Retiring;
            current.abort_connection();
            let event_queue_task = current.capabilities.as_ref().and_then(|capability| capability.event_queue.task());
            (mem::take(&mut current.preconnect_events), event_queue_task)
        } else {
            return;
        };
        if let Some(task) = event_queue_task {
            task.abort();
        }
        remove_simulator_capability(&mut self.simulators, simulator_id);
        state.preconnect_events_len = state.preconnect_events_len.saturating_sub(queued.len());
        for event in queued {
            record_event_envelope_loss(&self.coverage_losses, event, CoverageLossReason::EventAdmission);
        }
    }

    fn spawn_neighbor(
        &mut self,
        simulator_id: SimulatorId,
        region_handle: u64,
        endpoint: SocketAddr,
        handoff: Option<HandoffTarget>,
        setup: &mut PromotionSetup<'_>,
    ) {
        let neighbor_requests = setup.neighbor_requests.clone();
        let neighbor_events = setup.neighbor_events.clone();
        let shutdown = self.neighbor_cancellation.subscribe();
        let state = &mut *setup.state;
        let RunState { connections, .. } = state;
        let mut session = self.session.clone();
        session.simulator_address = endpoint;
        if let Some(handoff) = &handoff {
            session.seed_capability = handoff.seed_capability.clone();
        }
        let coverage_losses = self.coverage_losses.clone();
        let packet_memory = self.packet_memory.for_neighbor();
        let connection_packet_memory = packet_memory.clone();
        let outbound_budget = self.outbound_budget.as_ref().expect("session pacing budget exists").handle();
        let throttle_rates = self.throttle_rates;
        let (primary_state, primary_state_receiver) = watch::channel(PrimaryState::Neighbor { generation: 0 });
        let (command_sender, command_receiver) = mpsc::channel(NEIGHBOR_COMMAND_CAPACITY);
        let (logout_sender, logout_receiver) = mpsc::channel(1);
        if let Some(handoff) = handoff {
            command_sender.try_send(SimulatorCommand::PrepareHandoff(handoff)).expect("new neighbor command queue has capacity");
        }
        let finish_events = neighbor_events.clone();
        let task = connections.spawn(
            async move {
                let outcome = match SimulatorConnection::connect(
                    session,
                    SimulatorConnectionOptions {
                        simulator_id,
                        region_handle: Some(region_handle),
                        coverage_losses,
                        primary_state: Some(primary_state_receiver),
                        budget: outbound_budget,
                        owned_budget: None,
                        transport: None,
                        throttle_rates,
                    },
                )
                .await
                {
                    Ok(connection) => SimulatorTaskOutcome::Finished(
                        connection
                            .run(neighbor_events, shutdown, neighbor_requests, command_receiver, logout_receiver, packet_memory)
                            .await,
                    ),
                    Err(error) => SimulatorTaskOutcome::ConnectFailed(error),
                };
                finish_events
                    .send(SimulatorEvent::Finished { simulator_id, region_handle: Some(region_handle), endpoint, outcome: outcome.clone() })
                    .await
                    .ok();
                (simulator_id, Some(region_handle), endpoint, outcome)
            }
            .in_current_span(),
        );
        self.simulators.insert(
            simulator_id,
            SimulatorRecord {
                endpoint,
                region_handle: Some(region_handle),
                connection_task: Some(task),
                commands: command_sender,
                logout: logout_sender,
                pending_handoff_command: None,
                primary_state,
                packet_memory: connection_packet_memory,
                connection: SimulatorConnectionLifecycle::Connecting,
                capabilities: None,
                preconnect_events: VecDeque::new(),
            },
        );
    }

    pub(super) async fn start_neighbor_capabilities(
        &mut self,
        simulator_id: SimulatorId,
        endpoint: SocketAddr,
        seed: Url,
        setup: &mut PromotionSetup<'_>,
    ) -> Result<(), SessionError> {
        let client = match self.new_capability_client(seed.clone()) {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!(simulator = simulator_id.get(), %error, "could not start neighbor capability requests");
                return Ok(());
            }
        };
        let event_queue = match client.start_event_queue(
            simulator_id,
            endpoint,
            setup.capability_events.clone(),
            setup.shutdown.clone(),
            self.coverage_losses.clone(),
        ) {
            Ok(event_queue) => event_queue,
            Err(error) => {
                client.close_session();
                tracing::warn!(simulator = simulator_id.get(), %error, "could not start neighbor event queue");
                return Ok(());
            }
        };
        let event_queue_role = event_queue.role_handle();
        let task = setup.state.event_queues.spawn(async move {
            let exit = event_queue.wait().await;
            (simulator_id, endpoint, exit)
        });
        if let Some(simulator) = self.simulators.get_mut(&simulator_id) {
            simulator.capabilities = Some(SimulatorCapabilitySession {
                seed_capability: seed,
                client: client.clone(),
                event_queue: EventQueueLifecycle::Running { role: event_queue_role, task },
            });
        }
        self.publish_simulator_event(
            simulator_id,
            Event::SimulatorCapabilities { simulator_id, source: endpoint, capabilities: client }.into(),
            &setup.application_events,
            setup.state,
        )
        .await
    }
}

impl SessionDispatcher<'_> {
    pub(super) async fn handle_neighbor_event(&mut self, neighbor_event: Option<SimulatorEvent>) -> Result<RuntimeAction, SessionError> {
        match neighbor_event {
            Some(SimulatorEvent::Simulator(event)) => self.handle_simulator_envelope(event).await,
            Some(SimulatorEvent::SendCompleted { simulator_id, cancel_target, error, report_failure }) => {
                self.handle_send_completed(simulator_id, cancel_target, error, report_failure).await
            }
            Some(SimulatorEvent::Finished { simulator_id, region_handle, endpoint, outcome }) => {
                self.handle_simulator_finished(simulator_id, region_handle, endpoint, outcome).await
            }
            None => {
                self.neighbor_events_closed = true;
                Ok(RuntimeAction::Continue)
            }
        }
    }

    async fn handle_simulator_envelope(&mut self, envelope: EventEnvelope) -> Result<RuntimeAction, SessionError> {
        let (event, retained_bytes, coverage_watermark) = envelope.into_parts();
        if let Event::SimulatorMessage { simulator_id, message: Message::AgentMovementComplete(movement), .. } = &event
            && *simulator_id == self.state.simulator_id
            && movement.agent_data.agent_id == self.state.session.agent_id
            && movement.agent_data.session_id == self.state.session.session_id
        {
            self.state.register_root_connection(movement.data.region_handle);
        }
        let is_current = match &event {
            Event::SimulatorConnected { simulator_id, .. }
            | Event::SimulatorMotionStatus { simulator_id, .. }
            | Event::SimulatorMessage { simulator_id, .. } => is_current_simulator(*simulator_id, &self.state.simulators),
            _ => false,
        };
        if !is_current {
            return Ok(RuntimeAction::Continue);
        }
        self.handle_current_simulator_event(event, retained_bytes, coverage_watermark).await
    }

    async fn handle_current_simulator_event(
        &mut self,
        event: Event,
        retained_bytes: Option<EventRetention>,
        coverage_watermark: Option<CoverageWatermark>,
    ) -> Result<RuntimeAction, SessionError> {
        if let Event::SimulatorMessage { simulator_id, message, .. } = &event
            && *simulator_id == self.state.simulator_id
            && self.startup.observe_primary_message(message, self.state.session.agent_id, self.state.session.session_id)
        {
            let primary = self.state.primary();
            tracing::info!(
                simulator = self.state.simulator_id.get(),
                region = primary.region_handle,
                endpoint = %primary.endpoint,
                "simulator session ready"
            );
            self.state.primary().primary_state.send_replace(PrimaryState::Ready { generation: 0 });
            if self.startup.should_publish_ready() {
                self.state.publish_session_ready(&self.events, &mut self.run_state).await?;
                self.startup.mark_ready_published();
            }
        }
        if let Event::SimulatorMessage { simulator_id, packet_order, message, .. } = &event
            && is_handoff_message(message)
            && self.state.logout.deadline().is_none()
        {
            let mut setup = PromotionSetup {
                state: &mut self.run_state,
                neighbor_requests: self.neighbor_requests.clone(),
                application_events: self.events.clone(),
                neighbor_events: self.neighbor_events.clone(),
                shutdown: self.disconnect_requested.clone(),
                capability_events: self.capability_events.clone(),
            };
            self.state.handle_handoff_message(*simulator_id, message, *packet_order, &mut setup, &mut self.handoff_state).await?;
        }
        if let Event::SimulatorMessage { simulator_id, message: Message::AgentMovementComplete(movement), .. } = &event
            && self.state.logout.deadline().is_none()
            && matches_pending_promotion(self.handoff_state.pending(), self.state.primary_id, *simulator_id, movement)
        {
            self.promote_primary(*simulator_id, event, retained_bytes, coverage_watermark).await?;
            return Ok(RuntimeAction::Continue);
        }
        if let Event::SimulatorConnected { simulator_id, .. } = &event {
            let simulator_id = *simulator_id;
            if simulator_id == self.state.simulator_id {
                self.startup.mark_primary_announced();
            }
            self.state
                .publish_simulator_connected(
                    simulator_id,
                    EventEnvelope::from_parts(event, retained_bytes, coverage_watermark),
                    &self.events,
                    &mut self.run_state,
                )
                .await?;
            if simulator_id == self.state.simulator_id && self.startup.should_publish_ready() {
                self.state.publish_session_ready(&self.events, &mut self.run_state).await?;
                self.startup.mark_ready_published();
            }
        } else if let Event::SimulatorMessage { simulator_id, .. } | Event::SimulatorMotionStatus { simulator_id, .. } = &event {
            self.state
                .publish_simulator_event(
                    *simulator_id,
                    EventEnvelope::from_parts(event, retained_bytes, coverage_watermark),
                    &self.events,
                    &mut self.run_state,
                )
                .await?;
        }
        Ok(RuntimeAction::Continue)
    }

    pub(super) fn handle_pending_neighbor_finish(
        &mut self,
        permit: Result<mpsc::OwnedPermit<SimulatorEvent>, SendError<()>>,
    ) -> RuntimeAction {
        match permit {
            Ok(permit) => {
                if let Some(finish) = self.pending_neighbor_finishes.pop_front() {
                    permit.send(finish);
                }
            }
            Err(_) => self.pending_neighbor_finishes.clear(),
        }
        RuntimeAction::Continue
    }

    pub(super) async fn handle_neighbor_request(&mut self, request: Option<SimulatorRequest>) -> Result<RuntimeAction, SessionError> {
        if let Some(SimulatorRequest::Enable { simulator_id, region_handle, endpoint }) = request
            && is_current_simulator(simulator_id, &self.state.simulators)
        {
            let mut setup = PromotionSetup {
                state: &mut self.run_state,
                neighbor_requests: self.neighbor_requests.clone(),
                application_events: self.events.clone(),
                neighbor_events: self.neighbor_events.clone(),
                shutdown: self.disconnect_requested.clone(),
                capability_events: self.capability_events.clone(),
            };
            Box::pin(self.state.start_neighbor(simulator_id, region_handle, endpoint, None, &mut setup)).await?;
        }
        Ok(RuntimeAction::Continue)
    }

    pub(super) fn handle_connection_task_join(&mut self, joined: Option<Result<(Id, ConnectionTaskResult), JoinError>>) -> RuntimeAction {
        match joined {
            Some(Ok((task_id, _))) => {
                self.state.mark_connection_task_joined(task_id);
            }
            Some(Err(error)) => {
                let owner = self.state.mark_connection_task_joined(error.id());
                if let Some((simulator_id, region_handle, endpoint, lifecycle)) = owner
                    && lifecycle != SimulatorConnectionLifecycle::Closed
                {
                    let retiring = lifecycle == SimulatorConnectionLifecycle::Retiring;
                    let outcome = if error.is_cancelled() && retiring {
                        SimulatorTaskOutcome::Finished(Ok(()))
                    } else {
                        let reason = if simulator_id == self.state.primary_id {
                            ConnectionError::PrimaryTaskFailed(Arc::new(error))
                        } else {
                            ConnectionError::NeighborTaskFailed(Arc::new(error))
                        };
                        SimulatorTaskOutcome::Finished(Err(reason))
                    };
                    let finish = SimulatorEvent::Finished { simulator_id, region_handle, endpoint, outcome };
                    match self.neighbor_events.try_send(finish) {
                        Ok(()) => {}
                        Err(TrySendError::Full(finish)) => self.pending_neighbor_finishes.push_back(finish),
                        Err(TrySendError::Closed(_)) => {}
                    }
                }
            }
            None => {}
        }
        RuntimeAction::Continue
    }

    pub(super) async fn handle_simulator_finished(
        &mut self,
        simulator_id: SimulatorId,
        region_handle: Option<u64>,
        endpoint: SocketAddr,
        outcome: SimulatorTaskOutcome,
    ) -> Result<RuntimeAction, SessionError> {
        let Some(connection) = self.state.simulators.get(&simulator_id) else { return Ok(RuntimeAction::Continue) };
        if connection.endpoint != endpoint || (!connection.is_current() && !connection.is_retiring()) {
            return Ok(RuntimeAction::Continue);
        }
        let error = match &outcome {
            SimulatorTaskOutcome::Finished(Err(error)) | SimulatorTaskOutcome::ConnectFailed(error) => Some(error),
            SimulatorTaskOutcome::Finished(Ok(())) => None,
        };
        let handoff_failed = if let Some(pending) = self.handoff_state.pending().filter(|pending| pending.target == simulator_id) {
            let reason = match &outcome {
                SimulatorTaskOutcome::Finished(Err(_)) => "destination connection ended with an error",
                SimulatorTaskOutcome::Finished(Ok(())) => "destination connection ended before promotion",
                SimulatorTaskOutcome::ConnectFailed(_) => "destination connection failed",
            };
            tracing::warn!(
                simulator = simulator_id.get(),
                region = pending.handoff.region_handle,
                %endpoint,
                reason,
                error = error.map(field::display),
                "simulator handoff failed"
            );
            self.handoff_state.take_pending();
            true
        } else {
            false
        };
        if !handoff_failed
            && simulator_id != self.state.primary_id
            && let Some(error) = error
        {
            tracing::warn!(simulator = simulator_id.get(), region = ?region_handle, %endpoint, %error, "neighbor simulator connection failed");
        }
        let connection_region = region_handle.or(connection.region_handle);
        let was_connected = connection.connection == SimulatorConnectionLifecycle::Connected;
        if was_connected && connection.connection_task.as_ref().is_some_and(|task| !task.is_finished()) {
            connection.log_disconnected(simulator_id, connection_region.unwrap_or_default());
        }
        let event = neighbor_completion_event(simulator_id, region_handle, endpoint, &outcome);
        if simulator_id == self.state.primary_id {
            if *self.disconnect_requested.borrow() {
                self.state.neighbor_cancellation.send_replace(true);
                return Ok(RuntimeAction::Exit(Ok(())));
            }
            return Ok(RuntimeAction::Exit(match outcome {
                SimulatorTaskOutcome::Finished(Ok(())) => Ok(()),
                SimulatorTaskOutcome::Finished(Err(reason)) | SimulatorTaskOutcome::ConnectFailed(reason) => {
                    Err(SessionError::Connection(reason))
                }
            }));
        }
        let closed = if let Some(connection) = self.state.simulators.get_mut(&simulator_id) {
            connection.connection = SimulatorConnectionLifecycle::Closed;
            let event_queue_task = connection.capabilities.as_ref().and_then(|capabilities| capabilities.event_queue.task());
            if let Some(task) = event_queue_task {
                task.abort();
            }
            let queued = mem::take(&mut connection.preconnect_events);
            self.run_state.preconnect_events_len = self.run_state.preconnect_events_len.saturating_sub(queued.len());
            connection.capabilities.take();
            for event in queued {
                record_event_envelope_loss(&self.state.coverage_losses, event, CoverageLossReason::EventAdmission);
            }
            connection.connection_task.is_none()
        } else {
            false
        };
        if closed {
            self.state.simulators.remove(&simulator_id);
        }
        self.state.publish_event(&self.events, &mut self.run_state, event).await?;
        Ok(RuntimeAction::Continue)
    }
}

pub(super) fn neighbor_completion_event(
    simulator_id: SimulatorId,
    region_handle: Option<u64>,
    endpoint: SocketAddr,
    outcome: &SimulatorTaskOutcome,
) -> Event {
    match outcome {
        SimulatorTaskOutcome::Finished(Ok(())) => Event::SimulatorDisconnected { simulator_id, region_handle, endpoint },
        SimulatorTaskOutcome::Finished(Err(reason)) => {
            Event::SimulatorFailed { simulator_id, region_handle, endpoint, reason: reason.to_string() }
        }
        SimulatorTaskOutcome::ConnectFailed(reason) => match region_handle {
            Some(region_handle) => Event::SimulatorConnectFailed { simulator_id, region_handle, endpoint },
            None => Event::SimulatorFailed { simulator_id, region_handle: None, endpoint, reason: reason.to_string() },
        },
    }
}

pub(super) fn neighbor_event_queue_failure(
    simulator_id: SimulatorId,
    source: SocketAddr,
    exit: EventQueueExit,
    connections: &HashMap<SimulatorId, SimulatorRecord>,
) -> Option<Event> {
    let reason = match exit {
        EventQueueExit::Stopped => return None,
        EventQueueExit::StoppedWithCoverageLoss => "The neighbor simulator EventQueueGet stopped after losing event coverage.",
        EventQueueExit::Unavailable => "The neighbor simulator does not have a usable EventQueueGet capability.",
        EventQueueExit::RetryLimitReached => "The neighbor simulator EventQueueGet stopped after repeated errors.",
        EventQueueExit::TaskFailed => "The neighbor simulator EventQueueGet task ended unexpectedly.",
    };
    if !is_current_simulator(simulator_id, connections) {
        return None;
    }
    tracing::warn!(simulator = simulator_id.get(), source = %source, ?exit, reason, "neighbor simulator event queue stopped");
    Some(Event::CapabilityError { simulator_id, source, capability: EVENT_QUEUE_NAME.to_owned(), reason: reason.into() })
}
