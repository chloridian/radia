use std::{
    collections::VecDeque,
    io,
    net::SocketAddr,
    sync::Arc,
    time::{self, Duration},
};

use protocol::{
    LLUdpError,
    lludp::{Frequency, Message, MessageId, Packet, SchemaMessage},
    messages::{CompleteAgentMovement, CompletePingCheck, PingId, agent::complete_agent_movement},
    schema::ids::PACKET_ACK,
};
use tokio::{
    sync::{OwnedSemaphorePermit, mpsc, mpsc::error::TrySendError},
    time::Instant,
};
use utils::Summary;

use super::{
    ConnectionError, ConnectionRole, ConnectionTask, MAX_CONNECTION_PRECONNECT_EVENTS, MAX_QUEUED_SIMULATOR_EVENTS,
    NeighborEventQueueOutcome, SimulatorEvent, SimulatorRequest, active_queue_class, lifecycle::RunControl, outbound::MovementSend,
    preempt_deferred_send, preflight_loss_reason, queue_internal,
};
use crate::{
    circuit::{
        admission::PacketMemoryReservation,
        lludp::{Delivery, ReceiveAdmission},
        pacing::TrafficClass,
    },
    coverage::{CoverageLossReason, CoverageSource, record_coverage_loss, record_event_envelope_loss},
    event::{Event, EventEnvelope, PacketOrder},
};

const PACKET_REJECTION_SUMMARY_INTERVAL: Duration = Duration::from_secs(10);
const MAX_DECODE_FAILURE_SAMPLES: usize = 4;

enum EventQueueOutcome {
    Queued,
    ReceiverClosed,
    BacklogFull,
}

#[derive(Clone, Copy)]
enum RejectionKind {
    FrameInspection,
    AcknowledgementAdmission,
    DecodeAdmission,
    Preflight,
    RetainedMemoryAdmission,
    Decode,
    AllocationEstimateMismatch,
    EventAdmission,
}

struct DecodedPacket {
    packet: Packet,
    admission: ReceiveAdmission,
    received_at: Instant,
    decode_permit: Option<OwnedSemaphorePermit>,
    ack_decode_permit: Option<OwnedSemaphorePermit>,
    memory_reservation: Option<PacketMemoryReservation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DecodeFailureSample {
    message: String,
    error: String,
    count: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct PacketRejectionWindow {
    counts: [u64; 8],
    decode_failures: Vec<DecodeFailureSample>,
    additional_decode_failures: u64,
}

impl PacketRejectionWindow {
    fn record(&mut self, kind: RejectionKind) {
        let index = rejection_index(kind);
        self.counts[index] = self.counts[index].saturating_add(1);
    }

    fn record_decode_failure(&mut self, message_id: MessageId, error: &LLUdpError) {
        self.record(RejectionKind::Decode);
        let message = format_message_id(message_id);
        let error = error.to_string();
        if let Some(sample) = self.decode_failures.iter_mut().find(|sample| sample.message == message && sample.error == error) {
            sample.count = sample.count.saturating_add(1);
        } else if self.decode_failures.len() < MAX_DECODE_FAILURE_SAMPLES {
            self.decode_failures.push(DecodeFailureSample { message, error, count: 1 });
        } else {
            self.additional_decode_failures = self.additional_decode_failures.saturating_add(1);
        }
    }

    fn format_decode_failures(&self) -> Option<String> {
        if self.decode_failures.is_empty() {
            return None;
        }
        let mut entries: Vec<String> =
            self.decode_failures.iter().map(|sample| format!("{}: {} (x{})", sample.message, sample.error, sample.count)).collect();
        if self.additional_decode_failures > 0 {
            entries.push(format!("+{} more", self.additional_decode_failures));
        }
        Some(entries.join(", "))
    }
}

pub(super) struct PacketRejections(Summary<PacketRejectionWindow>);

impl Default for PacketRejections {
    fn default() -> Self {
        Self(Summary::new(PACKET_REJECTION_SUMMARY_INTERVAL))
    }
}

impl PacketRejections {
    pub(super) fn flush(&mut self) {
        if let Some(window) = self.flush_window() {
            report_packet_rejections(window);
        }
    }

    fn flush_window(&mut self) -> Option<PacketRejectionWindow> {
        self.0.flush()
    }

    fn record(&mut self, kind: RejectionKind) {
        if let Some(window) = self.record_at(Instant::now().into_std(), kind) {
            report_packet_rejections(window);
        }
    }

    fn record_at(&mut self, now: time::Instant, kind: RejectionKind) -> Option<PacketRejectionWindow> {
        self.0.record(now, |window| window.record(kind))
    }

    fn record_decode_failure(&mut self, message_id: MessageId, error: &LLUdpError) {
        if let Some(window) = self.record_decode_failure_at(Instant::now().into_std(), message_id, error) {
            report_packet_rejections(window);
        }
    }

    fn record_decode_failure_at(&mut self, now: time::Instant, message_id: MessageId, error: &LLUdpError) -> Option<PacketRejectionWindow> {
        self.0.record(now, |window| window.record_decode_failure(message_id, error))
    }
}

fn rejection_index(kind: RejectionKind) -> usize {
    match kind {
        RejectionKind::FrameInspection => 0,
        RejectionKind::AcknowledgementAdmission => 1,
        RejectionKind::DecodeAdmission => 2,
        RejectionKind::Preflight => 3,
        RejectionKind::RetainedMemoryAdmission => 4,
        RejectionKind::Decode => 5,
        RejectionKind::AllocationEstimateMismatch => 6,
        RejectionKind::EventAdmission => 7,
    }
}

fn format_message_id(message_id: MessageId) -> String {
    if let Some(schema) = SchemaMessage::schema_by_id(message_id) {
        schema.name.to_owned()
    } else {
        match message_id.frequency {
            Frequency::High => format!("High({})", message_id.number),
            Frequency::Medium => format!("Medium({})", message_id.number),
            Frequency::Low => format!("Low({})", message_id.number),
            Frequency::Fixed => format!("Fixed({:#010x})", message_id.number),
        }
    }
}

fn report_packet_rejections(window: PacketRejectionWindow) {
    let [
        frame_inspection,
        acknowledgement_admission,
        decode_admission,
        preflight,
        retained_memory_admission,
        decode,
        allocation_estimate_mismatch,
        event_admission,
    ] = window.counts;
    let decode_failures = window.format_decode_failures();
    if allocation_estimate_mismatch > 0 {
        tracing::error!(
            frame_inspection,
            acknowledgement_admission,
            decode_admission,
            preflight,
            retained_memory_admission,
            decode,
            allocation_estimate_mismatch,
            event_admission,
            decode_failures = decode_failures.as_deref(),
            "simulator packets rejected; allocation estimate mismatch"
        );
    } else {
        tracing::warn!(
            frame_inspection,
            acknowledgement_admission,
            decode_admission,
            preflight,
            retained_memory_admission,
            decode,
            allocation_estimate_mismatch,
            event_admission,
            decode_failures = decode_failures.as_deref(),
            "simulator packets rejected"
        );
    }
}

impl ConnectionTask {
    pub(super) fn receive_datagram(&mut self, received: io::Result<usize>, buffer: &[u8]) -> Result<RunControl, ConnectionError> {
        self.event_delivery_turn = !self.pending_notifications.is_empty() || !self.pending_events.is_empty();
        let size = received.map_err(|error| ConnectionError::Receive(Arc::new(error)))?;
        if *self.shutdown.borrow() || self.events.is_closed() {
            return Ok(RunControl::Finish(None));
        }
        self.receive_packet(&buffer[..size])
    }

    fn receive_packet(&mut self, bytes: &[u8]) -> Result<RunControl, ConnectionError> {
        let frame = match Packet::inspect(bytes) {
            Ok(frame) => frame,
            Err(_) => {
                self.reject(RejectionKind::FrameInspection, CoverageLossReason::PreflightRejected);
                return Ok(RunControl::Continue);
            }
        };
        let received_at = Instant::now();
        self.record_motion_packet(received_at);
        let admission =
            match self.connection.circuit.prepare_receive(frame.sequence(), frame.is_reliable(), frame.appended_acks(), received_at) {
                Ok(admission) => admission,
                Err(_) => {
                    self.reject(RejectionKind::AcknowledgementAdmission, CoverageLossReason::AcknowledgementAdmission);
                    return Ok(RunControl::Continue);
                }
            };
        if matches!(admission, ReceiveAdmission::Duplicate(_)) {
            preempt_deferred_send(
                &mut self.pending_send,
                &mut self.outbound,
                &self.connection.circuit,
                self.ack_timer_due || self.phase.is_draining(),
            )?;
            self.last_udp_received = received_at;
            return Ok(RunControl::Continue);
        }
        let message_id = match frame.message_id() {
            Ok(id) => id,
            Err(_) => {
                self.reject(RejectionKind::FrameInspection, CoverageLossReason::PreflightRejected);
                return Ok(RunControl::Continue);
            }
        };
        let is_packet_ack = message_id == PACKET_ACK;
        let decode_permit = if is_packet_ack {
            None
        } else {
            let Some(permit) = self.packet_memory.try_acquire_decode_slot() else {
                self.reject(RejectionKind::DecodeAdmission, CoverageLossReason::DecodeAdmission);
                return Ok(RunControl::Continue);
            };
            Some(permit)
        };
        let preflighted = match frame.preflighted() {
            Ok(preflighted) => preflighted,
            Err(error) => {
                self.reject(RejectionKind::Preflight, preflight_loss_reason(error, message_id));
                drop(decode_permit);
                return Ok(RunControl::Continue);
            }
        };
        let preflight = preflighted.preflight();
        let _ack_decode_permit = if is_packet_ack {
            let Some(permit) = self.packet_memory.try_acquire_ack_decode_slot(preflight) else {
                self.reject(RejectionKind::DecodeAdmission, CoverageLossReason::DecodeAdmission);
                drop(decode_permit);
                return Ok(RunControl::Continue);
            };
            Some(permit)
        } else {
            None
        };
        let memory_reservation = if is_packet_ack {
            None
        } else {
            match self.packet_memory.reserve_packet(preflight) {
                Ok(reservation) => Some(reservation),
                Err(_) => {
                    self.note_application_admission_failure("packet_memory");
                    self.reject(RejectionKind::RetainedMemoryAdmission, CoverageLossReason::RetainedMemoryAdmission);
                    drop(decode_permit);
                    return Ok(RunControl::Continue);
                }
            }
        };
        let packet = match preflighted.decode() {
            Ok(packet) => packet,
            Err(error) => {
                if matches!(error, LLUdpError::DecodedPacketTooLarge) {
                    self.reject(RejectionKind::AllocationEstimateMismatch, CoverageLossReason::AllocationEstimateMismatch);
                } else {
                    self.reject_decode_failure(message_id, &error);
                }
                drop(memory_reservation);
                drop(decode_permit);
                return Ok(RunControl::Continue);
            }
        };
        self.process_decoded_packet(DecodedPacket {
            packet,
            admission,
            received_at,
            decode_permit,
            ack_decode_permit: _ack_decode_permit,
            memory_reservation,
        })
    }

    fn process_decoded_packet(&mut self, decoded: DecodedPacket) -> Result<RunControl, ConnectionError> {
        let DecodedPacket { packet, admission, received_at, decode_permit, ack_decode_permit: _ack_decode_permit, mut memory_reservation } =
            decoded;
        if let Some(reservation) = &mut memory_reservation
            && self.packet_memory.trim_packet_reservation(reservation, &packet).is_err()
        {
            self.reject(RejectionKind::AllocationEstimateMismatch, CoverageLossReason::AllocationEstimateMismatch);
            drop(memory_reservation);
            drop(decode_permit);
            return Ok(RunControl::Continue);
        }
        let message = packet.message();
        let primary_state = self.connection.current_primary_state();
        self.connection.invalidate_stale_handoff(primary_state.role_generation(), &mut self.outbound, &mut self.pending_send);
        let connection_role = primary_state.connection_role();
        let dispatch_message = super::connection_message_has_application_event(
            message,
            connection_role,
            self.connection.handoff.target(),
            self.connection.handoff.movement_sent(),
        );
        let application_credit = if dispatch_message {
            let Some(permit) = self.packet_memory.application_events().try_acquire() else {
                self.note_application_admission_failure("event_credit");
                self.reject(RejectionKind::EventAdmission, CoverageLossReason::EventAdmission);
                self.last_udp_received = received_at;
                drop(memory_reservation);
                drop(decode_permit);
                return Ok(RunControl::Continue);
            };
            Some(permit)
        } else {
            None
        };
        let event_queue_full = if self.phase.is_established() {
            self.pending_events.len() >= MAX_QUEUED_SIMULATOR_EVENTS
        } else {
            self.preconnect_messages.len() >= MAX_CONNECTION_PRECONNECT_EVENTS
        };
        if dispatch_message && (self.phase.is_closing() || self.events.is_closed()) {
            self.reject(RejectionKind::EventAdmission, CoverageLossReason::EventAdmission);
            self.last_udp_received = received_at;
            drop(memory_reservation);
            drop(decode_permit);
            return Ok(RunControl::Continue);
        }
        if dispatch_message && event_queue_full {
            self.note_application_admission_failure("connection_event_queue");
            self.reject(RejectionKind::EventAdmission, CoverageLossReason::EventAdmission);
            self.last_udp_received = received_at;
            drop(application_credit);
            drop(memory_reservation);
            drop(decode_permit);
            return Ok(RunControl::Continue);
        }
        self.last_udp_received = received_at;
        let receive_outcome = self.connection.circuit.commit_received_packet(admission, &packet);
        preempt_deferred_send(
            &mut self.pending_send,
            &mut self.outbound,
            &self.connection.circuit,
            self.ack_timer_due || self.phase.is_draining(),
        )?;
        drop(decode_permit);
        let is_new_packet = receive_outcome.is_new_packet;
        let dispatch_allowed = !self.phase.is_closing();
        if let Some(control) = self.establish_connection_if_ready()? {
            return Ok(control);
        }
        if !is_new_packet {
            return Ok(RunControl::Continue);
        }
        let dispatch_message = self.handle_simulator_message(message, connection_role, dispatch_allowed)?;
        self.record_motion_packet(received_at);
        preempt_deferred_send(
            &mut self.pending_send,
            &mut self.outbound,
            &self.connection.circuit,
            self.ack_timer_due || self.phase.is_draining(),
        )?;
        self.dispatch_new_packet(
            packet,
            receive_outcome.packet_order,
            received_at,
            dispatch_message,
            dispatch_allowed,
            application_credit,
            memory_reservation,
        )
    }

    fn establish_connection_if_ready(&mut self) -> Result<Option<RunControl>, ConnectionError> {
        if self.phase.is_established() || self.connection.circuit.has_pending(self.connection.use_circuit_sequence) {
            return Ok(None);
        }
        self.phase.mark_established();
        let measured_ping_delay = self.connection.circuit.measured_ping_delay_average();
        self.motion_status_sender.send_modify(|status| {
            status.measured_ping_delay = measured_ping_delay;
            status.connected = true;
        });
        tracing::info!(role = ?self.connection.current_primary_state().connection_role(), "simulator connected");
        match self.queue_event(Event::SimulatorConnected {
            simulator_id: self.connection.simulator_id,
            region_handle: self.connection.region_handle,
            endpoint: self.connection.session.simulator_address,
        }) {
            NeighborEventQueueOutcome::ReceiverClosed => return Ok(Some(RunControl::Finish(None))),
            NeighborEventQueueOutcome::Queued | NeighborEventQueueOutcome::BacklogFull => {}
        }
        match self.queue_event(Event::SimulatorMotionStatus {
            simulator_id: self.connection.simulator_id,
            motion_status: self.motion_status_receiver.clone(),
        }) {
            NeighborEventQueueOutcome::ReceiverClosed => return Ok(Some(RunControl::Finish(None))),
            NeighborEventQueueOutcome::Queued | NeighborEventQueueOutcome::BacklogFull => {}
        }
        while let Some(event) = self.preconnect_messages.pop_front() {
            let admission_rejected = self.events.is_closed() || self.pending_events.len() >= MAX_QUEUED_SIMULATOR_EVENTS;
            let watermark = event.coverage_watermark();
            match self.queue_event(event) {
                NeighborEventQueueOutcome::ReceiverClosed => {
                    if let Some(watermark) = watermark {
                        record_coverage_loss(
                            &self.connection.coverage_losses,
                            watermark.simulator_id,
                            watermark.source,
                            CoverageLossReason::EventAdmission,
                        );
                    }
                    for queued in self.pending_events.drain(..) {
                        record_event_envelope_loss(&self.connection.coverage_losses, queued, CoverageLossReason::EventAdmission);
                    }
                    return Ok(Some(RunControl::Finish(None)));
                }
                NeighborEventQueueOutcome::Queued => {
                    self.packet_memory.application_events().note_success();
                }
                NeighborEventQueueOutcome::BacklogFull => {
                    if admission_rejected && let Some(watermark) = watermark {
                        record_coverage_loss(
                            &self.connection.coverage_losses,
                            watermark.simulator_id,
                            watermark.source,
                            CoverageLossReason::EventAdmission,
                        );
                    }
                }
            }
            if self.phase.close_reason().is_some() {
                for queued in self.preconnect_messages.drain(..) {
                    if let Some(watermark) = queued.coverage_watermark() {
                        record_coverage_loss(
                            &self.connection.coverage_losses,
                            watermark.simulator_id,
                            watermark.source,
                            CoverageLossReason::EventAdmission,
                        );
                    }
                }
                break;
            }
        }
        if self.connection.initial_role == ConnectionRole::Primary {
            queue_internal(
                &mut self.outbound,
                Message::CompleteAgentMovement(CompleteAgentMovement {
                    agent_data: complete_agent_movement::AgentData {
                        agent_id: self.connection.session.agent_id,
                        session_id: self.connection.session.session_id,
                        circuit_code: self.connection.session.circuit_code,
                    },
                }),
                Delivery::Reliable,
                TrafficClass::Deadline,
                MovementSend::Initial,
                active_queue_class(self.pending_send.as_ref()),
            )?;
        }
        if self.phase.has_region_handshake() && self.connection.region_handshake_reply_sent && self.connection.handoff.target().is_some() {
            self.connection.queue_handoff_movement(&mut self.outbound, self.pending_send.as_ref())?;
        }
        Ok(None)
    }

    fn dispatch_new_packet(
        &mut self,
        packet: Packet,
        packet_order: PacketOrder,
        received_at: Instant,
        dispatch_message: bool,
        dispatch_allowed: bool,
        application_credit: Option<OwnedSemaphorePermit>,
        mut memory_reservation: Option<PacketMemoryReservation>,
    ) -> Result<RunControl, ConnectionError> {
        if dispatch_message && dispatch_allowed {
            let mut event = EventEnvelope::with_retention_and_coverage(
                Event::SimulatorMessage {
                    simulator_id: self.connection.simulator_id,
                    source: self.connection.session.simulator_address,
                    sequence: packet.sequence(),
                    packet_order,
                    received_at: received_at.into_std(),
                    ping_delay: self.connection.circuit.measured_ping_delay_average(),
                    message: packet.into_message(),
                },
                memory_reservation.take().expect("application packet was admitted").into_retention(),
                self.connection.coverage_losses.watermark(self.connection.simulator_id, CoverageSource::Lludp),
            );
            event.attach_application_credit(application_credit.expect("application packet was admitted"));
            return Ok(self.queue_simulator_message_event(event));
        } else if dispatch_message {
            self.note_application_admission_failure("connection_closing");
            self.reject(RejectionKind::EventAdmission, CoverageLossReason::EventAdmission);
        }
        Ok(RunControl::Continue)
    }

    fn record_motion_packet(&mut self, received_at: Instant) {
        let measured_ping_delay = self.connection.circuit.measured_ping_delay_average();
        let connected = self.phase.is_established();
        let stats = self.sample_circuit_stats(received_at);
        self.motion_status_sender.send_modify(|status| {
            status.last_packet_received_at = received_at.into_std();
            status.measured_ping_delay = measured_ping_delay;
            status.connected = connected;
            status.stats = stats;
        });
    }

    pub(super) fn sample_circuit_stats(&mut self, now: Instant) -> super::CircuitStats {
        let (packets_in, packets_out, unacked, resent) = self.connection.circuit.packet_counts();
        let elapsed = now.saturating_duration_since(self.stats_sample_at);
        if elapsed >= Duration::from_secs(1) {
            let secs = elapsed.as_secs_f64();
            self.packets_in_per_sec = ((packets_in.saturating_sub(self.stats_sample_in)) as f64 / secs).round() as u32;
            self.packets_out_per_sec = ((packets_out.saturating_sub(self.stats_sample_out)) as f64 / secs).round() as u32;
            self.stats_sample_at = now;
            self.stats_sample_in = packets_in;
            self.stats_sample_out = packets_out;
        }
        super::CircuitStats {
            packets_in,
            packets_out,
            packets_in_per_sec: self.packets_in_per_sec,
            packets_out_per_sec: self.packets_out_per_sec,
            unacked,
            resent,
            throttle_sent: self.connection.agent_throttle_sent,
        }
    }

    fn queue_simulator_message_event(&mut self, event: EventEnvelope) -> RunControl {
        if self.phase.is_established() {
            let admission_rejected = self.events.is_closed() || self.pending_events.len() >= MAX_QUEUED_SIMULATOR_EVENTS;
            let watermark = event.coverage_watermark();
            match self.queue_event(event) {
                NeighborEventQueueOutcome::ReceiverClosed => {
                    if let Some(watermark) = watermark {
                        record_coverage_loss(
                            &self.connection.coverage_losses,
                            watermark.simulator_id,
                            watermark.source,
                            CoverageLossReason::EventAdmission,
                        );
                    }
                    for queued in self.pending_events.drain(..) {
                        record_event_envelope_loss(&self.connection.coverage_losses, queued, CoverageLossReason::EventAdmission);
                    }
                    return RunControl::Finish(None);
                }
                NeighborEventQueueOutcome::Queued | NeighborEventQueueOutcome::BacklogFull => {
                    if admission_rejected && let Some(watermark) = watermark {
                        record_coverage_loss(
                            &self.connection.coverage_losses,
                            watermark.simulator_id,
                            watermark.source,
                            CoverageLossReason::EventAdmission,
                        );
                    }
                }
            }
        } else if self.preconnect_messages.len() < MAX_CONNECTION_PRECONNECT_EVENTS {
            self.preconnect_messages.push_back(event);
            self.packet_memory.application_events().note_success();
        } else {
            self.packet_rejections.record(RejectionKind::EventAdmission);
            if let Some(watermark) = event.coverage_watermark() {
                record_coverage_loss(
                    &self.connection.coverage_losses,
                    watermark.simulator_id,
                    watermark.source,
                    CoverageLossReason::EventAdmission,
                );
            }
            for queued in self.preconnect_messages.drain(..) {
                if let Some(watermark) = queued.coverage_watermark() {
                    record_coverage_loss(
                        &self.connection.coverage_losses,
                        watermark.simulator_id,
                        watermark.source,
                        CoverageLossReason::EventAdmission,
                    );
                }
            }
        }
        RunControl::Continue
    }

    fn handle_simulator_message(
        &mut self,
        message: &Message,
        connection_role: ConnectionRole,
        dispatch_allowed: bool,
    ) -> Result<bool, ConnectionError> {
        let mut dispatch_message = true;
        match message {
            Message::EnableSimulator(enabled) => {
                let info = &enabled.simulator_info;
                let endpoint = SocketAddr::from((info.ip, info.port));
                if let Err(error) = self.neighbor_requests.try_send(SimulatorRequest::Enable {
                    simulator_id: self.connection.simulator_id,
                    region_handle: info.handle,
                    endpoint,
                }) {
                    let reason = match error {
                        TrySendError::Full(_) => ConnectionError::NeighborRequestQueueFull,
                        TrySendError::Closed(_) => ConnectionError::SimulatorManagerClosed,
                    };
                    return Err(reason);
                }
            }
            Message::DisableSimulator(_) => {
                self.phase.close(None, false);
                dispatch_message = false;
            }
            Message::RegionHandshake(_) => {
                self.phase.mark_region_handshake();
                self.connection.region_handshake_reply_sent = false;
            }
            Message::AgentMovementComplete(movement) => {
                let matches_current_movement = match connection_role {
                    ConnectionRole::Primary => {
                        movement.agent_data.agent_id == self.connection.session.agent_id
                            && movement.agent_data.session_id == self.connection.session.session_id
                    }
                    ConnectionRole::Neighbor => self.connection.handoff.target().is_some_and(|handoff| {
                        self.connection.handoff.movement_sent()
                            && movement.agent_data.agent_id == handoff.agent_id
                            && movement.agent_data.session_id == handoff.session_id
                            && movement.data.region_handle == handoff.region_handle
                    }),
                };
                if !matches_current_movement {
                    dispatch_message = false;
                } else {
                    if connection_role == ConnectionRole::Primary {
                        self.connection.region_handle = Some(movement.data.region_handle);
                    }
                    if dispatch_allowed && !self.connection.agent_throttle_sent {
                        queue_internal(
                            &mut self.outbound,
                            super::agent_throttle_message(&self.connection.session),
                            Delivery::Reliable,
                            TrafficClass::Control,
                            MovementSend::None,
                            active_queue_class(self.pending_send.as_ref()),
                        )?;
                        self.connection.agent_throttle_sent = true;
                    }
                }
            }
            Message::StartPingCheck(ping) => {
                dispatch_message = false;
                self.connection.circuit.clear_duplicate_list(ping.oldest_unacked, Instant::now());
                queue_internal(
                    &mut self.outbound,
                    Message::CompletePingCheck(CompletePingCheck { ping_id: PingId { ping_id: ping.ping_id.ping_id } }),
                    Delivery::Unreliable,
                    TrafficClass::Control,
                    MovementSend::None,
                    active_queue_class(self.pending_send.as_ref()),
                )?;
            }
            Message::CompletePingCheck(ping) => {
                dispatch_message = false;
                self.connection.circuit.record_ping_response(ping.ping_id.ping_id, Instant::now());
            }
            Message::LogoutReply(reply)
                if self.phase.is_closing()
                    && reply.agent_data.agent_id == self.connection.session.agent_id
                    && reply.agent_data.session_id == self.connection.session.session_id =>
            {
                dispatch_message = false;
                self.phase.complete_logout_receipt(super::LogoutReceipt::Received);
                self.phase.begin_draining();
            }
            Message::LogoutReply(_) | Message::PacketAck(_) => dispatch_message = false,
            _ => {}
        }
        Ok(dispatch_message)
    }

    fn queue_event(&mut self, event: impl Into<EventEnvelope>) -> NeighborEventQueueOutcome {
        queue_neighbor_event(&self.events, &mut self.pending_events, event)
    }

    fn reject(&mut self, kind: RejectionKind, reason: CoverageLossReason) {
        self.packet_rejections.record(kind);
        record_coverage_loss(&self.connection.coverage_losses, self.connection.simulator_id, CoverageSource::Lludp, reason);
    }

    fn reject_decode_failure(&mut self, message_id: MessageId, error: &LLUdpError) {
        self.packet_rejections.record_decode_failure(message_id, error);
        record_coverage_loss(
            &self.connection.coverage_losses,
            self.connection.simulator_id,
            CoverageSource::Lludp,
            CoverageLossReason::DecodeRejected,
        );
    }

    fn note_application_admission_failure(&self, reason: &'static str) {
        if self.packet_memory.application_events().note_failure() {
            tracing::warn!(simulator = self.connection.simulator_id.get(), reason, "application event intake is under pressure");
        }
    }
}

pub(crate) fn queue_neighbor_event(
    events: &mpsc::Sender<SimulatorEvent>,
    pending_events: &mut VecDeque<EventEnvelope>,
    event: impl Into<EventEnvelope>,
) -> NeighborEventQueueOutcome {
    match queue_neighbor_channel_event(events, pending_events, event.into()) {
        EventQueueOutcome::Queued => NeighborEventQueueOutcome::Queued,
        EventQueueOutcome::ReceiverClosed => NeighborEventQueueOutcome::ReceiverClosed,
        EventQueueOutcome::BacklogFull => NeighborEventQueueOutcome::BacklogFull,
    }
}

fn queue_neighbor_channel_event(
    events: &mpsc::Sender<SimulatorEvent>,
    pending_events: &mut VecDeque<EventEnvelope>,
    event: EventEnvelope,
) -> EventQueueOutcome {
    if events.is_closed() {
        return EventQueueOutcome::ReceiverClosed;
    }

    if !pending_events.is_empty() {
        return if pending_events.len() < MAX_QUEUED_SIMULATOR_EVENTS {
            pending_events.push_back(event);
            EventQueueOutcome::Queued
        } else {
            EventQueueOutcome::BacklogFull
        };
    }

    match events.try_send(SimulatorEvent::Simulator(event)) {
        Ok(()) => EventQueueOutcome::Queued,
        Err(TrySendError::Closed(_)) => EventQueueOutcome::ReceiverClosed,
        Err(TrySendError::Full(SimulatorEvent::Simulator(event))) => {
            pending_events.push_back(event);
            EventQueueOutcome::Queued
        }
        Err(TrySendError::Full(SimulatorEvent::SendCompleted { .. } | SimulatorEvent::Finished { .. })) => {
            unreachable!("only simulator events enter the neighbor event backlog")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use protocol::schema::ids::{AGENT_MOVEMENT_COMPLETE, COMPLETE_PING_CHECK, OBJECT_UPDATE, START_PING_CHECK};

    use super::*;

    #[test]
    fn known_schema_records_failure() {
        let mut rejections = PacketRejections::default();
        let now = time::Instant::now();
        let window = rejections
            .record_decode_failure_at(now, OBJECT_UPDATE, &LLUdpError::TruncatedPacket)
            .expect("first rejection reports immediately");

        assert_eq!(window.counts[rejection_index(RejectionKind::Decode)], 1);
        assert_eq!(window.decode_failures.len(), 1);
        assert_eq!(window.decode_failures[0].message, "ObjectUpdate");
        assert_eq!(window.decode_failures[0].error, LLUdpError::TruncatedPacket.to_string());
        assert_eq!(window.decode_failures[0].count, 1);
        assert_eq!(window.additional_decode_failures, 0);
        let formatted = window.format_decode_failures().expect("decode failure summary");
        assert!(formatted.contains("ObjectUpdate"));
        assert!(formatted.contains(&LLUdpError::TruncatedPacket.to_string()));
        assert!(formatted.contains("(x1)"));
    }

    #[test]
    fn unknown_id_formats_failure() {
        let mut rejections = PacketRejections::default();
        let now = time::Instant::now();
        let unknown_id = MessageId::low(0xfff0);
        assert!(SchemaMessage::schema_by_id(unknown_id).is_none());

        let window = rejections
            .record_decode_failure_at(now, unknown_id, &LLUdpError::InvalidMessageId)
            .expect("first rejection reports immediately");

        assert_eq!(window.decode_failures.len(), 1);
        assert_eq!(window.decode_failures[0].message, "Low(65520)");
        assert_eq!(window.decode_failures[0].error, LLUdpError::InvalidMessageId.to_string());
    }

    #[test]
    fn failure_samples_bounded() {
        let mut rejections = PacketRejections::default();
        let start = time::Instant::now();
        assert_matches!(rejections.record_decode_failure_at(start, OBJECT_UPDATE, &LLUdpError::TruncatedPacket), Some(_));

        let within_window = start + Duration::from_secs(1);
        assert!(rejections.record_decode_failure_at(within_window, OBJECT_UPDATE, &LLUdpError::TruncatedPacket).is_none());
        assert!(rejections.record_decode_failure_at(within_window, OBJECT_UPDATE, &LLUdpError::TruncatedPacket).is_none());
        assert!(rejections.record_decode_failure_at(within_window, START_PING_CHECK, &LLUdpError::InvalidMessage).is_none());
        assert!(rejections.record_decode_failure_at(within_window, COMPLETE_PING_CHECK, &LLUdpError::InvalidZeroCode).is_none());
        assert!(rejections.record_decode_failure_at(within_window, AGENT_MOVEMENT_COMPLETE, &LLUdpError::InvalidSchemaMessage).is_none());
        assert!(rejections.record_decode_failure_at(within_window, MessageId::low(0xfff1), &LLUdpError::InvalidMessageId).is_none());
        assert!(rejections.record_decode_failure_at(within_window, MessageId::low(0xfff2), &LLUdpError::InvalidMessageId).is_none());

        let window = rejections
            .record_decode_failure_at(start + PACKET_REJECTION_SUMMARY_INTERVAL, OBJECT_UPDATE, &LLUdpError::TruncatedPacket)
            .expect("summary reports when interval elapses");

        assert_eq!(window.counts[rejection_index(RejectionKind::Decode)], 8);
        assert_eq!(window.decode_failures.len(), MAX_DECODE_FAILURE_SAMPLES);
        assert_eq!(window.decode_failures[0].message, "ObjectUpdate");
        assert_eq!(window.decode_failures[0].count, 3);
        assert_eq!(window.additional_decode_failures, 2);
        let formatted = window.format_decode_failures().expect("decode failure summary");
        assert!(formatted.contains("+2 more"));
    }

    #[test]
    fn other_rejections_unchanged() {
        let mut rejections = PacketRejections::default();
        let start = time::Instant::now();
        let first = rejections.record_at(start, RejectionKind::FrameInspection).expect("first reports immediately");
        assert_eq!(first.counts, [1, 0, 0, 0, 0, 0, 0, 0]);
        assert!(first.format_decode_failures().is_none());

        let within = start + Duration::from_secs(1);
        assert!(rejections.record_at(within, RejectionKind::Preflight).is_none());
        assert!(rejections.record_at(within, RejectionKind::AllocationEstimateMismatch).is_none());
        let flushed = rejections.flush_window().expect("pending rejections flush");
        assert_eq!(flushed.counts, [0, 0, 0, 1, 0, 0, 1, 0]);
        assert!(flushed.format_decode_failures().is_none());
    }
}
