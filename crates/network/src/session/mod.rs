mod capability_events;
mod error;
mod handoff;
mod logout;
mod neighbors;
mod routing;
mod runtime;
mod simulators;
mod startup;
mod teleport;
mod thread;

mod outbound_message {
    pub trait Sealed {}
}

use std::{
    collections::{HashMap, HashSet},
    fmt,
    num::NonZeroU64,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use protocol::{
    lludp::Message,
    messages::{
        AgentUpdate, ChatFromViewer, MapBlockRequest, MoveInventoryFolder, MoveInventoryItem, RegionHandleRequest, RequestMultipleObjects,
        SetAlwaysRun, TeleportCancel,
    },
};
use tokio::sync::{
    mpsc::{
        Receiver, Sender,
        error::{TryRecvError, TrySendError},
    },
    oneshot, watch,
};

pub(crate) use self::error::SessionError;
#[cfg(test)]
pub(crate) use self::handoff::{HandoffEvidence, HandoffState, PendingPromotion, handoff_cancellation, matches_handoff_cancellation};
#[cfg(test)]
use self::neighbors::neighbor_completion_event;
#[cfg(test)]
use self::routing::{EventQueueOutcome, queue_simulator_event};
#[cfg(test)]
use self::runtime::{APPLICATION_EVENT_PRESSURE_GRACE_PERIOD, HANDOFF_COMMAND_RETRY_INTERVAL};
#[cfg(test)]
use self::simulators::retry_pending_handoff_commands;
#[cfg(test)]
pub(crate) use self::tests::{connect_test_connection, connect_test_primary_with_state, run_session};
#[cfg(test)]
use self::thread::{Channels, run_client, wait_for_login};
use self::{
    runtime::{RuntimeAction, SessionDispatcher},
    simulators::{
        ConnectionQueueFailure, ConnectionTaskResult, EventQueueLifecycle, HandoffCommand, NEIGHBOR_COMMAND_CAPACITY, RunState,
        RuntimeInputs, SessionRuntime, SessionRuntimeOptions, SessionState, SimulatorCapabilitySession, SimulatorConnectionLifecycle,
        SimulatorRecord, is_current_simulator, queue_connection_message, queue_primary_message, remove_simulator_capability,
        track_capability_requests,
    },
    startup::SessionStartup,
};
use crate::{
    LoginCredentials,
    circuit::{
        ConnectionError, HandoffTarget, PrimaryState, SimulatorCommand, SimulatorConnection, SimulatorConnectionOptions, SimulatorEvent,
        SimulatorRequest, SimulatorTaskOutcome, admission::SimulatorMemoryBudget, lludp::Delivery,
    },
    coverage,
    coverage::{
        CoverageLoss, CoverageLossLedger, CoverageLossRecord, CoverageSnapshot, CoverageSource, CoverageWatermark,
        record_event_envelope_loss,
    },
    error::Error,
    event::{CLIENT_EVENT_CAPACITY, Event, EventEnvelope, SimulatorId},
};

pub(crate) const COMMAND_CAPACITY: usize = 256;
const NETWORK_THREAD_STACK_SIZE: usize = 8 * 1024 * 1024;
pub const DEFAULT_UPSTREAM_BYTES_PER_SECOND: NonZeroU64 = NonZeroU64::new(64_000).unwrap();
const MAX_OUTBOUND_CHAT_MESSAGE_BYTES: usize = 8 * 1024 - 6 - 4 - 40 - (1 + u8::MAX as usize * 4);
const MAX_OUTBOUND_OBJECT_REQUESTS: usize = u8::MAX as usize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionOptions {
    upstream_bytes_per_second: NonZeroU64,
}

impl SessionOptions {
    pub const fn new(upstream_bytes_per_second: NonZeroU64) -> Self {
        Self { upstream_bytes_per_second }
    }

    pub const fn upstream_bytes_per_second(self) -> NonZeroU64 {
        self.upstream_bytes_per_second
    }
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self::new(DEFAULT_UPSTREAM_BYTES_PER_SECOND)
    }
}

pub trait OutboundMessage: outbound_message::Sealed + Into<Message> {}

impl<T: outbound_message::Sealed + Into<Message>> OutboundMessage for T {}

pub struct MfaChallenge {
    commands: Sender<Command>,
    completed: AtomicBool,
}

impl MfaChallenge {
    fn new(commands: Sender<Command>) -> Self {
        Self { commands, completed: AtomicBool::new(false) }
    }

    pub fn complete(&self, code: &str) -> Result<(), Error> {
        let code = code.trim();
        if code.is_empty() {
            return Err(Error::EmptyMfaCode);
        }
        if self.completed.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Err(Error::MfaChallengeCompleted);
        }
        match try_send_command(&self.commands, Command::SubmitMfaCode(code.to_owned()), Error::LoginEnded) {
            Err(error @ Error::CommandQueueFull) => {
                self.completed.store(false, Ordering::Release);
                Err(error)
            }
            result => result,
        }
    }
}

impl fmt::Debug for MfaChallenge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MfaChallenge")
    }
}

pub struct Session {
    handle: SessionHandle,
    events: Receiver<EventEnvelope>,
    coverage_losses: CoverageLossLedger,
    coverage_changed: watch::Receiver<u64>,
    pending_event: Option<EventEnvelope>,
    surfaced_coverage: HashMap<(SimulatorId, CoverageSource), u64>,
    surfaced_overflow_generation: u64,
    last_event_watermark: Option<CoverageWatermark>,
}

impl Session {
    pub fn start(credentials: LoginCredentials) -> Result<Self, Error> {
        Self::start_with_options(credentials, SessionOptions::default())
    }

    pub fn start_with_options(credentials: LoginCredentials, options: SessionOptions) -> Result<Self, Error> {
        let login_url = credentials.grid.login_url().to_owned();
        thread::start_at(credentials, options, login_url)
    }
}

impl Session {
    pub fn handle(&self) -> SessionHandle {
        self.handle.clone()
    }

    pub async fn recv_event(&mut self) -> Option<Event> {
        self.last_event_watermark = None;
        loop {
            if let Some(envelope) = self.pending_event.take() {
                let watermark = envelope.coverage_watermark();
                if let Some(loss) = self.next_coverage_loss(watermark) {
                    self.pending_event = Some(envelope);
                    return Some(Event::CoverageLost { loss });
                }
                if self.has_unacknowledged_coverage_loss(watermark) {
                    self.pending_event = Some(envelope);
                    if self.coverage_changed.changed().await.is_err() {
                        return self.pending_event.take().map(|envelope| self.deliver(envelope));
                    }
                    continue;
                }
                return Some(self.deliver(envelope));
            }

            match self.events.try_recv() {
                Ok(envelope) => {
                    self.pending_event = Some(envelope);
                    continue;
                }
                Err(TryRecvError::Disconnected) => {
                    if let Some(loss) = self.next_coverage_loss(None) {
                        return Some(Event::CoverageLost { loss });
                    }
                    return None;
                }
                Err(TryRecvError::Empty) => {}
            }

            if let Some(loss) = self.next_coverage_loss(None) {
                return Some(Event::CoverageLost { loss });
            }

            tokio::select! {
                biased;
                envelope = self.events.recv() => match envelope {
                    Some(envelope) => self.pending_event = Some(envelope),
                    None => {
                        if let Some(loss) = self.next_coverage_loss(None) {
                            return Some(Event::CoverageLost { loss });
                        }
                        return None;
                    }
                },
                changed = self.coverage_changed.changed() => {
                    if changed.is_ok()
                        && let Some(loss) = self.next_coverage_loss(None)
                    {
                        return Some(Event::CoverageLost { loss });
                    }
                }
            }
        }
    }

    pub fn coverage_losses(&self) -> CoverageSnapshot {
        self.coverage_losses.snapshot()
    }

    pub fn report_current_event_loss(&mut self, reason: crate::CoverageLossReason) -> Option<CoverageWatermark> {
        let watermark = self.last_event_watermark.take()?;
        coverage::record_coverage_loss(&self.coverage_losses, watermark.simulator_id, watermark.source, reason);
        Some(watermark)
    }

    pub fn acknowledge_coverage_loss(&mut self, watermark: CoverageWatermark) {
        let scope = (watermark.simulator_id, watermark.source);
        if self.surfaced_coverage.get(&scope).is_some_and(|generation| watermark.generation <= *generation) {
            self.coverage_losses.acknowledge(watermark);
            self.prune_surfaced_coverage();
        }
    }

    pub fn acknowledge_coverage_overflow(&mut self, generation: u64) {
        if generation > 0 && generation <= self.surfaced_overflow_generation {
            self.coverage_losses.acknowledge_overflow(generation);
            self.prune_surfaced_coverage();
        }
    }

    fn next_coverage_loss(&mut self, watermark: Option<CoverageWatermark>) -> Option<CoverageLoss> {
        let snapshot = self.coverage_losses.snapshot();
        if let Some(latest_generation) = snapshot.overflow_generation {
            let first_pending = snapshot.acknowledged_overflow_generation.saturating_add(1);
            let eligible_generation = watermark.map_or(Some(latest_generation), |watermark| {
                (watermark.overflow_generation >= first_pending).then_some(latest_generation.min(watermark.overflow_generation))
            });
            if let Some(generation) = eligible_generation
                && generation > self.surfaced_overflow_generation
            {
                self.surfaced_overflow_generation = generation;
                return Some(CoverageLoss::SessionOverflow { generation });
            }
        }

        for loss in snapshot.losses {
            let CoverageLoss::Source(record) = loss else { continue };
            let key = (record.watermark.simulator_id, record.watermark.source);
            let surfaced = self.surfaced_coverage.get(&key).copied().unwrap_or_default();
            let eligible_generation = watermark.map_or(Some(record.watermark.generation), |watermark| {
                (watermark.simulator_id == key.0 && watermark.source == key.1 && watermark.generation >= record.pending_from_generation)
                    .then_some(record.watermark.generation.min(watermark.generation))
            });
            let Some(generation) = eligible_generation else { continue };
            if generation <= surfaced {
                continue;
            }
            self.surfaced_coverage.insert(key, generation);
            let overflow_generation = watermark.map_or(record.watermark.overflow_generation, |watermark| {
                record.watermark.overflow_generation.min(watermark.overflow_generation)
            });
            return Some(CoverageLoss::Source(CoverageLossRecord {
                watermark: CoverageWatermark { generation, overflow_generation, ..record.watermark },
                ..record
            }));
        }
        None
    }

    fn deliver(&mut self, envelope: EventEnvelope) -> Event {
        let (event, retained, watermark) = envelope.into_parts();
        self.last_event_watermark = watermark;
        drop(retained);
        event
    }

    fn has_unacknowledged_coverage_loss(&self, watermark: Option<CoverageWatermark>) -> bool {
        let snapshot = self.coverage_losses.snapshot();
        if let Some(generation) = snapshot.overflow_generation {
            let first_pending = snapshot.acknowledged_overflow_generation.saturating_add(1);
            if watermark.is_none_or(|watermark| watermark.overflow_generation >= first_pending)
                && (snapshot.overflow_exhausted || generation > snapshot.acknowledged_overflow_generation)
            {
                return true;
            }
        }

        snapshot.losses.iter().any(|loss| {
            let CoverageLoss::Source(record) = loss else { return false };
            watermark.is_none_or(|watermark| {
                watermark.simulator_id == record.watermark.simulator_id
                    && watermark.source == record.watermark.source
                    && watermark.generation >= record.pending_from_generation
            })
        })
    }

    fn prune_surfaced_coverage(&mut self) {
        let pending = self.coverage_losses.snapshot();
        let pending_sources = pending
            .losses
            .iter()
            .filter_map(|loss| match loss {
                CoverageLoss::Source(record) => Some((record.watermark.simulator_id, record.watermark.source)),
                CoverageLoss::SessionOverflow { .. } => None,
            })
            .collect::<HashSet<_>>();
        self.surfaced_coverage.retain(|scope, _| pending_sources.contains(scope));
        if pending.overflow_generation.is_none() {
            self.surfaced_overflow_generation = 0;
        }
    }

    pub fn send(&self, message: impl OutboundMessage, delivery: Delivery) -> Result<(), Error> {
        self.handle.send(message, delivery)
    }

    pub async fn send_to(&self, simulator_id: SimulatorId, message: impl OutboundMessage, delivery: Delivery) -> Result<(), Error> {
        self.handle.send_to(simulator_id, message, delivery).await
    }

    pub async fn disconnect(&self) {
        self.handle.disconnect().await;
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.handle.request_disconnect();
    }
}

#[derive(Clone)]
pub struct SessionHandle {
    commands: Sender<Command>,
    disconnect_requested: watch::Sender<bool>,
    command_admission_closed: Arc<Mutex<bool>>,
    worker_finished: watch::Receiver<bool>,
}

impl SessionHandle {
    pub fn is_disconnect_requested(&self) -> bool {
        *self.disconnect_requested.borrow()
    }

    fn request_disconnect(&self) {
        let mut admission_closed = self.command_admission_closed.lock().unwrap_or_else(|error| error.into_inner());
        *admission_closed = true;
        if !*self.worker_finished.borrow() {
            if !*self.disconnect_requested.borrow() {
                tracing::info!("disconnect requested");
            }
            self.disconnect_requested.send_replace(true);
        }
    }

    pub fn send(&self, message: impl OutboundMessage, delivery: Delivery) -> Result<(), Error> {
        let message = message.into();
        validate_outbound_message(&message)?;
        let admission_closed = self.command_admission_closed.lock().unwrap_or_else(|error| error.into_inner());
        if *admission_closed || *self.disconnect_requested.borrow() || *self.worker_finished.borrow() {
            return Err(Error::SessionClosed);
        }
        try_send_command(&self.commands, Command::SendMessage { message, delivery }, Error::SimulatorConnectionClosed)
    }

    pub async fn send_to(&self, simulator_id: SimulatorId, message: impl OutboundMessage, delivery: Delivery) -> Result<(), Error> {
        let message = message.into();
        validate_outbound_message(&message)?;
        let mut disconnect_requested = self.disconnect_requested.subscribe();
        let (completed, response) = oneshot::channel();
        {
            let admission_closed = self.command_admission_closed.lock().unwrap_or_else(|error| error.into_inner());
            if *admission_closed || *self.disconnect_requested.borrow() || *self.worker_finished.borrow() {
                return Err(Error::SessionClosed);
            }
            self.commands.try_send(Command::SendTo { simulator_id, message, delivery, completed }).map_err(|error| match error {
                TrySendError::Full(_) => Error::SimulatorCommandQueueFull,
                TrySendError::Closed(_) => Error::SessionClosed,
            })?;
        }
        tokio::select! {
            biased;
            _ = disconnect_requested.changed() => Err(Error::SessionClosed),
            result = response => result.map_err(|_| {
                if *self.disconnect_requested.borrow() || *self.worker_finished.borrow() || self.commands.is_closed() {
                    Error::SessionClosed
                } else {
                    Error::SimulatorConnectionClosed
                }
            })?,
        }
    }

    pub async fn reply_region_handshake(&self, simulator_id: SimulatorId, flags: u32) -> Result<(), Error> {
        let mut disconnect_requested = self.disconnect_requested.subscribe();
        let (completed, response) = oneshot::channel();
        {
            let admission_closed = self.command_admission_closed.lock().unwrap_or_else(|error| error.into_inner());
            if *admission_closed || *self.disconnect_requested.borrow() || *self.worker_finished.borrow() {
                return Err(Error::SessionClosed);
            }
        }
        let command = Command::ReplyRegionHandshake { simulator_id, flags, completed };
        tokio::select! {
            biased;
            _ = disconnect_requested.changed() => return Err(Error::SessionClosed),
            result = self.commands.send(command) => result.map_err(|_| Error::SessionClosed)?,
        }
        tokio::select! {
            biased;
            _ = disconnect_requested.changed() => Err(Error::SessionClosed),
            result = response => result.map_err(|_| {
                if *self.disconnect_requested.borrow() || *self.worker_finished.borrow() || self.commands.is_closed() {
                    Error::SessionClosed
                } else {
                    Error::SimulatorConnectionClosed
                }
            })?,
        }
    }

    pub fn try_reply_region_handshake(&self, simulator_id: SimulatorId, flags: u32) -> Result<oneshot::Receiver<Result<(), Error>>, Error> {
        let (completed, response) = oneshot::channel();
        {
            let admission_closed = self.command_admission_closed.lock().unwrap_or_else(|error| error.into_inner());
            if *admission_closed || *self.disconnect_requested.borrow() || *self.worker_finished.borrow() {
                return Err(Error::SessionClosed);
            }
            self.commands.try_send(Command::ReplyRegionHandshake { simulator_id, flags, completed }).map_err(|error| match error {
                TrySendError::Full(_) => Error::CommandQueueFull,
                TrySendError::Closed(_) => Error::SessionClosed,
            })?;
        }
        Ok(response)
    }

    pub async fn disconnect(&self) {
        self.request_disconnect();
        let mut worker_finished = self.worker_finished.clone();
        if !*worker_finished.borrow() {
            worker_finished.wait_for(|finished| *finished).await.ok();
        }
    }
}

pub(crate) enum Command {
    SubmitMfaCode(String),
    SendMessage { message: Message, delivery: Delivery },
    SendTo { simulator_id: SimulatorId, message: Message, delivery: Delivery, completed: oneshot::Sender<Result<(), Error>> },
    ReplyRegionHandshake { simulator_id: SimulatorId, flags: u32, completed: oneshot::Sender<Result<(), Error>> },
}

impl outbound_message::Sealed for TeleportCancel {}

impl outbound_message::Sealed for AgentUpdate {}

impl outbound_message::Sealed for SetAlwaysRun {}

impl outbound_message::Sealed for ChatFromViewer {}

impl outbound_message::Sealed for MapBlockRequest {}

impl outbound_message::Sealed for MoveInventoryFolder {}

impl outbound_message::Sealed for MoveInventoryItem {}

impl outbound_message::Sealed for RegionHandleRequest {}

impl outbound_message::Sealed for RequestMultipleObjects {}

fn validate_outbound_message(message: &Message) -> Result<(), Error> {
    match message {
        Message::ChatFromViewer(message) if message.chat_data.message.capacity() > MAX_OUTBOUND_CHAT_MESSAGE_BYTES => {
            Err(Error::OutboundMessageTooLarge)
        }
        Message::RequestMultipleObjects(message) if message.objects.capacity() > MAX_OUTBOUND_OBJECT_REQUESTS => {
            Err(Error::OutboundMessageTooLarge)
        }
        _ => Ok(()),
    }
}

fn try_send_command(commands: &Sender<Command>, command: Command, closed: Error) -> Result<(), Error> {
    commands.try_send(command).map_err(|error| match error {
        TrySendError::Full(_) => Error::CommandQueueFull,
        TrySendError::Closed(_) => closed,
    })
}

#[cfg(test)]
mod tests;
