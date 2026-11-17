use std::{assert_matches, io::ErrorKind, net::SocketAddr, time::Duration};

use protocol::{
    lludp::{Message, Packet},
    login::ResponseData,
    messages::{
        CompletePingCheck, LogoutReply, LogoutRequest, PacketAck, PingId,
        agent::{logout_reply, logout_request},
        circuit::packet_ack::Packets,
    },
};
use tokio::{
    net::UdpSocket,
    spawn,
    sync::mpsc,
    task::yield_now,
    time::{self, Instant, timeout},
};
use uuid::Uuid;

use super::{
    AGENT_ID, LOGOUT_TIMEOUT, LogoutOutcome, PendingLogout, SESSION_ID, SimulatorConnectionLifecycle, ack, connect_runtime,
    connect_with_send_gate, login_session, new_send_gate, send_logout_reply, set_sequence, test_fixture,
};
use crate::{
    Error,
    circuit::{LOGOUT_REPLY_TIMEOUT, lludp::Circuit},
    event::{Event, EventEnvelope},
    fixtures::circuit,
    session::{SessionError, logout::LogoutUnavailable},
};

const CIRCUIT_CODE: u32 = 0x1122_3344;
pub(super) const SIMULATOR_TIMEOUT: Duration = Duration::from_secs(2);
pub(super) const RETRANSMISSION_TIMEOUT: Duration = Duration::from_secs(8);

pub(super) fn assert_no_datagram(socket: &UdpSocket) {
    let mut buffer = [0; 65_535];
    assert_matches!(socket.try_recv_from(&mut buffer), Err(error) if error.kind() == ErrorKind::WouldBlock);
}

pub(super) fn complete_ping(ping_id: u8) -> Message {
    Message::CompletePingCheck(CompletePingCheck { ping_id: PingId { ping_id } })
}

pub(super) fn packet_ack(packet_ids: &[u32]) -> Message {
    Message::PacketAck(PacketAck { packets: packet_ids.iter().map(|id| Packets { id: *id }).collect() })
}

pub(super) fn packet_ack_contains(message: &Message, packet_id: u32) -> bool {
    matches!(message, Message::PacketAck(ack) if ack.packets.iter().any(|packet| packet.id == packet_id))
}

pub(super) fn receive_packet(circuit: &mut Circuit, packet: &Packet) -> bool {
    circuit::receive_packet_at(circuit, packet, Instant::now()).expect("valid circuit packet").is_new_packet
}

pub(super) fn is_complete_ping(message: &Message, ping_id: u8) -> bool {
    matches!(message, Message::CompletePingCheck(ping) if ping.ping_id.ping_id == ping_id)
}

pub(super) fn logout_message() -> Message {
    Message::LogoutRequest(LogoutRequest {
        agent_data: logout_request::AgentData {
            agent_id: Uuid::parse_str(AGENT_ID).expect("valid test agent ID"),
            session_id: Uuid::parse_str(SESSION_ID).expect("valid test session ID"),
        },
    })
}

async fn receive(socket: &UdpSocket) -> (Vec<u8>, SocketAddr) {
    let mut buffer = [0; 65_535];
    let (size, peer) = socket.recv_from(&mut buffer).await.expect("read fake simulator datagram");
    (buffer[..size].to_vec(), peer)
}

pub(super) async fn receive_with_timeout(socket: &UdpSocket, wait: Duration) -> (Vec<u8>, SocketAddr) {
    let mut buffer = [0; 65_535];
    let (size, peer) = timeout(wait, socket.recv_from(&mut buffer))
        .await
        .expect("client should send a UDP datagram")
        .expect("read fake simulator datagram");
    (buffer[..size].to_vec(), peer)
}

pub(super) async fn receive_primary_connected(events: &mut mpsc::Receiver<EventEnvelope>) {
    timeout(SIMULATOR_TIMEOUT, async {
        loop {
            match events.recv().await.expect("client event channel should remain open").into_event() {
                Event::SimulatorConnected { region_handle: None, .. } => return,
                Event::CapabilityError { capability, .. } if capability == "EventQueueGet" => {}
                _ => panic!("unexpected event before primary connection"),
            }
        }
    })
    .await
    .expect("client should publish primary connection");
}

pub(super) fn expected_fixture(name: &str, sequence: u32, reliable: bool) -> Vec<u8> {
    let mut bytes = test_fixture(name);
    bytes[0] = (bytes[0] & !0x40) | if reliable { 0x40 } else { 0 };
    set_sequence(&mut bytes, sequence);
    bytes
}

pub(super) fn expected_complete_movement(sequence: u32) -> Vec<u8> {
    let mut bytes = test_fixture("complete_agent_movement.hex");
    bytes[0] |= 0x40;
    set_sequence(&mut bytes, sequence);
    let end = bytes.len();
    bytes[end - 4..].copy_from_slice(&CIRCUIT_CODE.to_le_bytes());
    bytes
}

pub(super) fn expected_zerocoded_reply(sequence: u32) -> Vec<u8> {
    let mut bytes = test_fixture("region_handshake_reply.hex");
    bytes[0] |= 0x40;
    set_sequence(&mut bytes, sequence);
    bytes
}

pub(super) fn with_appended_ack(mut bytes: Vec<u8>, packet_id: u32) -> Vec<u8> {
    bytes[0] |= 0x10;
    bytes.extend_from_slice(&packet_id.to_be_bytes());
    bytes.push(1);
    bytes
}

pub(super) fn expected_chat(sequence: u32, message: &str, chat_type: u8, channel: i32) -> Vec<u8> {
    let mut bytes = vec![0x40];
    bytes.extend_from_slice(&sequence.to_be_bytes());
    bytes.extend_from_slice(&[0, 0xff, 0xff, 0x00, 0x50]);
    bytes.extend_from_slice(Uuid::parse_str(AGENT_ID).expect("valid agent UUID").as_bytes());
    bytes.extend_from_slice(Uuid::parse_str(SESSION_ID).expect("valid session UUID").as_bytes());
    bytes.extend_from_slice(&(message.len() as u16 + 1).to_le_bytes());
    bytes.extend_from_slice(message.as_bytes());
    bytes.push(0);
    bytes.push(chat_type);
    bytes.extend_from_slice(&channel.to_le_bytes());
    bytes
}

pub(super) fn start_ping_packet(sequence: u32, ping_id: u8, oldest_unacked: u32) -> Vec<u8> {
    let mut bytes = test_fixture("start_ping_check.hex");
    set_sequence(&mut bytes, sequence);
    bytes[7] = ping_id;
    bytes[8..12].copy_from_slice(&oldest_unacked.to_le_bytes());
    bytes
}

pub(super) fn server_ack_many(sequence: u32, acknowledged: &[u32]) -> Vec<u8> {
    let mut bytes = vec![0];
    bytes.extend_from_slice(&sequence.to_be_bytes());
    bytes.extend_from_slice(&[0, 0xff, 0xff, 0xff, 0xfb, acknowledged.len() as u8]);
    for id in acknowledged {
        bytes.extend_from_slice(&id.to_le_bytes());
    }
    bytes
}

pub(super) fn client_ack(sequence: u32, acknowledged: u32) -> Vec<u8> {
    server_ack_many(sequence, &[acknowledged])
}

#[tokio::test]
async fn root_disconnect_awaits_logout_reply() {
    let simulator = UdpSocket::bind("127.0.0.1:0").await.expect("bind fake simulator");
    let mut connection = connect_runtime(login_session(simulator.local_addr().expect("simulator address")), ResponseData::default())
        .await
        .expect("connect root simulator circuit");

    let (use_circuit, peer) = receive(&simulator).await;
    assert_eq!(use_circuit, expected_fixture("use_circuit_code.hex", 1, true));
    simulator.send_to(&ack(20, 1), peer).await.expect("ack circuit establishment");
    let (movement_bytes, movement_peer) = receive(&simulator).await;
    assert_eq!(movement_peer, peer);
    let movement = Packet::decode(&movement_bytes).expect("decode initial movement");
    assert_matches!(movement.message(), Message::CompleteAgentMovement(_));
    simulator.send_to(&ack(21, movement.sequence()), peer).await.expect("ack initial movement");
    let mut disconnect = Box::pin(connection.state.disconnect_once());

    let (logout_bytes, logout_peer) = tokio::select! {
        biased;
        result = &mut disconnect => panic!("disconnect finished before the simulator received LogoutRequest: {result:?}"),
        packet = timeout(SIMULATOR_TIMEOUT, receive(&simulator)) => packet.expect("send LogoutRequest"),
    };
    assert_eq!(logout_peer, peer);
    let logout = Packet::decode(&logout_bytes).expect("decode LogoutRequest");
    assert_eq!(logout.sequence(), 3);
    assert_matches!(logout.message(), Message::LogoutRequest(_));

    let agent_id = Uuid::parse_str(AGENT_ID).expect("agent UUID");
    let session_id = Uuid::parse_str(SESSION_ID).expect("session UUID");
    for (sequence, reply_agent, reply_session) in [(22, Uuid::nil(), session_id), (23, agent_id, Uuid::nil())] {
        let reply = Packet::new(
            sequence,
            Message::LogoutReply(LogoutReply {
                agent_data: logout_reply::AgentData { agent_id: reply_agent, session_id: reply_session },
                inventory_data: Vec::new(),
            }),
        )
        .with_reliable()
        .encode()
        .expect("encode mismatched LogoutReply");
        simulator.send_to(&reply, peer).await.expect("send mismatched LogoutReply");
        loop {
            let bytes = tokio::select! {
                biased;
                result = &mut disconnect => panic!("mismatched LogoutReply ended disconnect: {result:?}"),
                packet = timeout(SIMULATOR_TIMEOUT, receive(&simulator)) => packet.expect("ack mismatched LogoutReply").0,
            };
            let packet = Packet::decode(&bytes).expect("decode reply acknowledgement");
            if packet_ack_contains(packet.message(), sequence) {
                break;
            }
        }
    }

    send_logout_reply(&simulator, peer, &logout, 24).await;
    assert_eq!(
        timeout(SIMULATOR_TIMEOUT, &mut disconnect).await.expect("matching reply completes disconnect").expect("disconnect result"),
        LogoutOutcome::Received
    );
    drop(disconnect);
    assert_eq!(connection.state.disconnect_once().await.expect("duplicate disconnect is idempotent"), LogoutOutcome::AlreadyReceived);
    assert_no_datagram(&simulator);
}

#[tokio::test(start_paused = true)]
async fn disconnect_reply_deadline_is_bounded() {
    let simulator = UdpSocket::bind("127.0.0.1:0").await.expect("bind fake simulator");
    let mut connection = connect_runtime(login_session(simulator.local_addr().expect("simulator address")), ResponseData::default())
        .await
        .expect("connect root simulator circuit");
    let (_, peer) = receive(&simulator).await;
    simulator.send_to(&ack(20, 1), peer).await.expect("ack circuit establishment");
    let (movement_bytes, _) = receive(&simulator).await;
    let movement = Packet::decode(&movement_bytes).expect("decode initial movement");
    assert_matches!(movement.message(), Message::CompleteAgentMovement(_));
    simulator.send_to(&ack(21, movement.sequence()), peer).await.expect("ack initial movement");
    let mut disconnect = Box::pin(connection.state.disconnect_once());
    let (logout_bytes, _) = tokio::select! {
        biased;
        result = &mut disconnect => panic!("disconnect finished before the simulator received LogoutRequest: {result:?}"),
        packet = timeout(SIMULATOR_TIMEOUT, receive(&simulator)) => packet.expect("send LogoutRequest"),
    };
    let logout = Packet::decode(&logout_bytes).expect("decode LogoutRequest");
    assert_matches!(logout.message(), Message::LogoutRequest(_));

    time::advance(LOGOUT_REPLY_TIMEOUT).await;
    yield_now().await;

    assert_eq!(
        timeout(SIMULATOR_TIMEOUT, &mut disconnect).await.expect("reply deadline completes disconnect").expect("disconnect result"),
        LogoutOutcome::Unavailable(LogoutUnavailable::ReplyDeadline)
    );
}

#[tokio::test]
async fn disconnect_send_failure_is_unexpected() {
    let simulator = UdpSocket::bind("127.0.0.1:0").await.expect("bind fake simulator");
    let (send_gate, pause_sends, send_entered) = new_send_gate();
    let mut connection =
        connect_with_send_gate(login_session(simulator.local_addr().expect("simulator address")), ResponseData::default(), send_gate)
            .await
            .expect("connect root simulator circuit");
    let (_, peer) = receive(&simulator).await;
    simulator.send_to(&ack(20, 1), peer).await.expect("ack circuit establishment");
    let (movement_bytes, _) = receive(&simulator).await;
    let movement = Packet::decode(&movement_bytes).expect("decode initial movement");
    assert_matches!(movement.message(), Message::CompleteAgentMovement(_));
    simulator.send_to(&ack(21, movement.sequence()), peer).await.expect("ack initial movement");
    pause_sends.send_replace(true);
    let mut disconnect = Box::pin(connection.state.disconnect_once());
    tokio::select! {
        biased;
        result = &mut disconnect => panic!("disconnect ended before entering the local send: {result:?}"),
        _ = timeout(SIMULATOR_TIMEOUT, send_entered.notified()) => {}
    }
    drop(pause_sends);

    assert_matches!(
        timeout(SIMULATOR_TIMEOUT, &mut disconnect).await.expect("send failure completes disconnect"),
        Err(SessionError::Network(Error::SimulatorSend(_)))
    );
}

#[tokio::test]
async fn disconnect_reports_closed_primary() {
    let simulator = UdpSocket::bind("127.0.0.1:0").await.expect("bind fake simulator");
    let mut connection = connect_runtime(login_session(simulator.local_addr().expect("simulator address")), ResponseData::default())
        .await
        .expect("connect root simulator circuit");
    receive(&simulator).await;
    let (logout, receiver) = mpsc::channel(1);
    drop(receiver);
    connection.state.primary_mut().logout = logout;

    assert_eq!(
        connection.state.disconnect_once().await.expect("closed route is an unavailable logout"),
        LogoutOutcome::Unavailable(LogoutUnavailable::ConnectionClosed)
    );
    assert_no_datagram(&simulator);
}

#[tokio::test(start_paused = true)]
async fn disconnect_logout_respects_deadline() {
    let simulator = UdpSocket::bind("127.0.0.1:0").await.expect("bind fake simulator");
    let mut connection = connect_runtime(login_session(simulator.local_addr().expect("simulator address")), ResponseData::default())
        .await
        .expect("connect root simulator circuit");
    receive(&simulator).await;
    let (logout, _logout_receiver) = mpsc::channel(1);
    let (queued_logout, _queued_response) = PendingLogout::new();
    logout.try_send(queued_logout).expect("fill the destination logout queue");
    connection.state.primary_mut().logout = logout;
    let disconnect = spawn(async move { connection.state.disconnect_once().await });

    yield_now().await;
    time::advance(LOGOUT_TIMEOUT).await;

    assert_eq!(
        disconnect.await.expect("disconnect task should not panic").expect("deadline is an unavailable logout"),
        LogoutOutcome::Unavailable(LogoutUnavailable::SendDeadline)
    );
}

#[tokio::test]
async fn failed_disconnect_reports_no_send() {
    let simulator = UdpSocket::bind("127.0.0.1:0").await.expect("bind fake simulator");
    let mut connection = connect_runtime(login_session(simulator.local_addr().expect("simulator address")), ResponseData::default())
        .await
        .expect("connect root simulator circuit");
    receive(&simulator).await;
    connection.state.primary_mut().connection = SimulatorConnectionLifecycle::Closed;

    assert_eq!(
        connection.state.disconnect_once().await.expect("failed root connection is an unavailable logout"),
        LogoutOutcome::Unavailable(LogoutUnavailable::ConnectionClosed)
    );
    assert_no_datagram(&simulator);
}
