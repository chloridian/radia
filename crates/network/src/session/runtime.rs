use std::{collections::VecDeque, future::pending, pin::Pin, time::Duration};

use tokio::{
    sync::{mpsc, watch},
    time::{self, Instant, Interval, Sleep},
};

use super::{
    CLIENT_EVENT_CAPACITY, RunState, RuntimeInputs, SessionError, SessionStartup, SessionState, SimulatorEvent, SimulatorRequest,
    handoff::HandoffState, simulators::EventQueueLifecycle,
};
use crate::{capabilities::EventQueueTask, event::EventEnvelope, session::Command};

const SESSION_TIMEOUT: Duration = Duration::from_secs(40);
pub(super) const APPLICATION_EVENT_PRESSURE_GRACE_PERIOD: Duration = Duration::from_secs(30);
pub(super) const HANDOFF_COMMAND_RETRY_INTERVAL: Duration = Duration::from_millis(10);

pub(super) struct SessionDispatcher<'a> {
    pub(super) state: &'a mut SessionState,
    pub(super) commands: mpsc::Receiver<Command>,
    pub(super) events: mpsc::Sender<EventEnvelope>,
    pub(super) disconnect_requested: watch::Receiver<bool>,
    pub(super) capability_events: mpsc::Sender<EventEnvelope>,
    pub(super) capability_event_rx: mpsc::Receiver<EventEnvelope>,
    pub(super) neighbor_events: mpsc::Sender<SimulatorEvent>,
    pub(super) neighbor_event_rx: mpsc::Receiver<SimulatorEvent>,
    pub(super) neighbor_requests: mpsc::Sender<SimulatorRequest>,
    pub(super) neighbor_request_rx: mpsc::Receiver<SimulatorRequest>,
    pub(super) capability_events_closed: bool,
    pub(super) neighbor_events_closed: bool,
    pub(super) startup: SessionStartup,
    pub(super) run_state: RunState,
    pub(super) pending_neighbor_finishes: VecDeque<SimulatorEvent>,
    pub(super) handoff_state: HandoffState,
    pub(super) handoff_command_retry: Interval,
    pub(super) session_deadline: Pin<Box<Sleep>>,
}

pub(super) enum RuntimeAction {
    Continue,
    Exit(Result<(), SessionError>),
}

impl<'a> SessionDispatcher<'a> {
    pub(super) async fn start(
        state: &'a mut SessionState,
        runtime_inputs: RuntimeInputs,
        commands: mpsc::Receiver<Command>,
        events: mpsc::Sender<EventEnvelope>,
        disconnect_requested: watch::Receiver<bool>,
    ) -> Result<Self, SessionError> {
        let (capability_events, capability_event_rx) = mpsc::channel(CLIENT_EVENT_CAPACITY);
        let RuntimeInputs { neighbor_event_receiver, neighbor_request_receiver, root_connection_workers } = runtime_inputs;
        let neighbor_events = state.neighbor_events.clone();
        let neighbor_requests = state.neighbor_requests.clone();
        let mut run_state = RunState::new();
        run_state.connections = root_connection_workers;
        let root_capability = state
            .simulators
            .get(&state.simulator_id)
            .and_then(|simulator| simulator.capabilities.as_ref())
            .expect("initial simulator capabilities exist");
        let event_queue: EventQueueTask = match root_capability.client.start_primary_event_queue(
            state.simulator_id,
            state.session.simulator_address,
            capability_events.clone(),
            disconnect_requested.clone(),
            state.coverage_losses.clone(),
        ) {
            Ok(event_queue) => event_queue,
            Err(error) => {
                state.neighbor_cancellation.send_replace(true);
                run_state.connections.shutdown().await;
                return Err(error.into());
            }
        };
        let event_queue_role = event_queue.role_handle();
        let event_queue_endpoint = state.session.simulator_address;
        let initial_simulator_id = state.simulator_id;
        let task = run_state.event_queues.spawn(async move {
            let exit = event_queue.wait().await;
            (initial_simulator_id, event_queue_endpoint, exit)
        });
        if let Some(capability) = state.simulators.get_mut(&state.simulator_id).and_then(|simulator| simulator.capabilities.as_mut()) {
            capability.event_queue = EventQueueLifecycle::Running { role: event_queue_role, task };
        }
        let mut handoff_command_retry = time::interval(HANDOFF_COMMAND_RETRY_INTERVAL);
        handoff_command_retry.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

        Ok(Self {
            state,
            commands,
            events,
            disconnect_requested,
            capability_events,
            capability_event_rx,
            neighbor_events,
            neighbor_event_rx: neighbor_event_receiver,
            neighbor_requests,
            neighbor_request_rx: neighbor_request_receiver,
            capability_events_closed: false,
            neighbor_events_closed: false,
            startup: SessionStartup::default(),
            run_state,
            pending_neighbor_finishes: VecDeque::new(),
            handoff_state: HandoffState::default(),
            handoff_command_retry,
            session_deadline: Box::pin(time::sleep(SESSION_TIMEOUT)),
        })
    }

    pub(super) async fn shutdown(mut self) {
        for simulator in self.state.simulators.values() {
            if let Some(capability) = &simulator.capabilities {
                capability.client.close_session();
            }
        }
        self.state.neighbor_cancellation.send_replace(true);
        self.run_state.connections.shutdown().await;
        self.run_state.event_queues.shutdown().await;
        for simulator in self.state.simulators.values() {
            if let Some(capability) = &simulator.capabilities {
                capability.client.close_session_and_wait().await;
            }
        }
    }

    #[tracing::instrument(name = "simulator.session", level = "info", skip_all)]
    pub(super) async fn run(&mut self) -> Result<(), SessionError> {
        if *self.disconnect_requested.borrow() {
            self.state.graceful_disconnect().await?;
            return Ok(());
        }
        let mut application_pressure_changed = self.state.packet_memory.application_events().subscribe_pressure();
        loop {
            self.flush_pending_handshake_replies();
            let logout_deadline = self.state.logout.deadline();
            let logout_pending = self.state.logout.pending_mut().is_some();
            let pressure_deadline = (*application_pressure_changed.borrow()).map(|since| since + APPLICATION_EVENT_PRESSURE_GRACE_PERIOD);
            let application_pressure = async {
                if let Some(deadline) = pressure_deadline {
                    time::sleep_until(deadline).await;
                } else {
                    pending::<()>().await;
                }
            };
            let action = tokio::select! {
                _ = application_pressure => {
                    if self.state.packet_memory.application_events().pressure_expired(Instant::now(), APPLICATION_EVENT_PRESSURE_GRACE_PERIOD) {
                    self.state.graceful_disconnect().await?;
                        return Err(SessionError::SustainedApplicationEventPressure);
                    }
                    RuntimeAction::Continue
                }
                _ = application_pressure_changed.changed() => RuntimeAction::Continue,
                _ = self.disconnect_requested.changed() => self.handle_disconnect_changed().await?,
                result = async {
                    self.state.logout.pending_mut()
                        .expect("pending logout branch requires an active request")
                        .wait()
                        .await
                }, if logout_pending => self.handle_primary_logout_completed(result)?,
                _ = &mut self.session_deadline, if !self.startup.is_ready() && !*self.disconnect_requested.borrow() => {
                    self.handle_session_startup_timeout()
                }
                _ = self.events.closed(), if logout_deadline.is_none() => self.handle_event_receiver_closed().await?,
                _ = self.handoff_command_retry.tick(),
                    if !self.run_state.pending_region_handshake_replies.is_empty()
                        || self.state.simulators.values().any(|simulator| simulator.pending_handoff_command.is_some()) => {
                        self.handle_handoff_command_retry()
                    }
                capability_event = self.capability_event_rx.recv(), if !self.capability_events_closed => self.handle_capability_event(capability_event).await?,
                neighbor_event = self.neighbor_event_rx.recv(), if !self.neighbor_events_closed => self.handle_neighbor_event(neighbor_event).await?,
                permit = self.neighbor_events.clone().reserve_owned(), if !self.pending_neighbor_finishes.is_empty() && !self.neighbor_events.is_closed() => {
                    self.handle_pending_neighbor_finish(permit)
                }
                joined = self.run_state.event_queues.join_next_with_id(), if !self.run_state.event_queues.is_empty() => {
                    self.handle_event_queue_join(joined).await?
                }
                permit = self.events.clone().reserve_owned(), if !self.run_state.pending_events.is_empty() && !self.events.is_closed() => {
                    self.handle_pending_application_event(permit).await?
                }
                request = self.neighbor_request_rx.recv(), if logout_deadline.is_none() => self.handle_neighbor_request(request).await?,
                joined = self.state.capability_request_waiters.join_next(), if !self.state.capability_request_waiters.is_empty() => {
                    self.handle_capability_request_join(joined)
                }
                joined = self.run_state.connections.join_next_with_id(), if !self.run_state.connections.is_empty() => {
                    self.handle_connection_task_join(joined)
                }
                command = self.commands.recv(), if logout_deadline.is_none() => self.handle_command(command).await?,
            };
            match action {
                RuntimeAction::Continue => {}
                RuntimeAction::Exit(result) => return result,
            }
        }
    }
}
