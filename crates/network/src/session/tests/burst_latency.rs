use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use protocol::messages::ObjectUpdate;
use tokio::{
    net::UdpSocket,
    sync::{mpsc, watch},
    task::{JoinHandle, JoinSet},
    time::timeout,
};

use super::*;

const HANG_GUARD: Duration = Duration::from_secs(60);
const BURST_COUNT: usize = 12;
const PACKETS_PER_CIRCUIT: usize = 32;
const LARGE_OBJECT_COUNT: usize = 8;
const ACK_OBSERVATION_CAPACITY: usize = 1024;
const FIRST_SEQUENCE: u32 = 10_000;
const NEIGHBOR_REGION_BASE: u64 = 0x1000_0000_0000_1000;

struct SimulatorEndpoint {
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    id: SimulatorId,
}

struct SessionHarness {
    simulators: Vec<SimulatorEndpoint>,
    events: mpsc::Receiver<EventEnvelope>,
    commands: mpsc::Sender<Command>,
    disconnect: watch::Sender<bool>,
    client: JoinHandle<Result<(), crate::session::SessionError>>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct PacketKey {
    circuit: usize,
    sequence: u32,
}

struct AckObservation {
    key: PacketKey,
    received_at: Instant,
}

struct AckMonitor {
    shutdown: watch::Sender<bool>,
    observations: mpsc::Receiver<AckObservation>,
    tasks: JoinSet<()>,
}

#[derive(Default)]
struct Measurements {
    burst_elapsed: Vec<Duration>,
    acknowledgement_latency: Vec<Duration>,
    delivery_latency: Vec<Duration>,
    packet_count: usize,
    ordinary_bytes: usize,
    larger_bytes: usize,
}

impl AckMonitor {
    fn start(simulators: &[SimulatorEndpoint]) -> Self {
        let (observation_sender, observations) = mpsc::channel(ACK_OBSERVATION_CAPACITY);
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let mut tasks = JoinSet::new();

        for (circuit, simulator) in simulators.iter().enumerate() {
            let socket = Arc::clone(&simulator.socket);
            let mut shutdown_receiver = shutdown_receiver.clone();
            let observation_sender = observation_sender.clone();
            tasks.spawn(async move {
                let mut buffer = [0; 8 * 1024];
                loop {
                    tokio::select! {
                        changed = shutdown_receiver.changed() => {
                            if changed.is_err() || *shutdown_receiver.borrow() {
                                break;
                            }
                        }
                        received = socket.recv_from(&mut buffer) => {
                            let (size, _) = received.expect("read client LLUDP datagram");
                            let packet = Packet::decode(&buffer[..size]).expect("decode client LLUDP datagram");
                            for sequence in packet.appended_acks().iter().copied() {
                                if observation_sender.send(AckObservation {
                                    key: PacketKey { circuit, sequence },
                                    received_at: Instant::now(),
                                }).await.is_err() {
                                    return;
                                }
                            }
                            if let Message::PacketAck(ack) = packet.message() {
                                for entry in &ack.packets {
                                    if observation_sender.send(AckObservation {
                                        key: PacketKey { circuit, sequence: entry.id },
                                        received_at: Instant::now(),
                                    }).await.is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                    }
                }
            });
        }
        drop(observation_sender);

        Self { shutdown, observations, tasks }
    }

    async fn stop(mut self) {
        self.shutdown.send_replace(true);
        timeout(HANG_GUARD, async {
            while let Some(result) = self.tasks.join_next().await {
                result.expect("ACK monitor task should complete");
            }
        })
        .await
        .expect("ACK monitor tasks should stop");
    }
}

impl SessionHarness {
    async fn connect(neighbor_count: usize) -> Self {
        let primary_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind primary simulator"));
        let primary_endpoint = primary_socket.local_addr().expect("primary simulator address");
        let (commands, command_receiver) = mpsc::channel(COMMAND_CAPACITY);
        let (event_sender, mut events) = mpsc::channel(TEST_EVENT_CAPACITY);
        let (disconnect, disconnect_requested) = watch::channel(false);
        let client = spawn(run_session_with_options(
            login_session(primary_endpoint),
            commands.clone(),
            command_receiver,
            event_sender,
            disconnect_requested,
            test_options(),
        ));

        let (use_circuit_bytes, primary_peer) = receive(&primary_socket).await;
        let use_circuit = Packet::decode(&use_circuit_bytes).expect("decode primary UseCircuitCode");
        assert!(use_circuit.is_reliable());
        std::assert_matches!(use_circuit.message(), Message::UseCircuitCode(_));
        primary_socket.send_to(&ack(20, use_circuit.sequence()), primary_peer).await.expect("ack primary circuit");

        let movement = receive_message(&primary_socket, |message| matches!(message, Message::CompleteAgentMovement(_))).await;
        primary_socket.send_to(&ack(21, movement.sequence()), primary_peer).await.expect("ack initial movement");

        let mut handshake = test_fixture("region_handshake.hex");
        handshake[0] |= 0x40;
        set_sequence(&mut handshake, 22);
        primary_socket.send_to(&handshake, primary_peer).await.expect("send primary region handshake");
        let handshake_reply = receive_message(&primary_socket, |message| matches!(message, Message::RegionHandshakeReply(_))).await;

        let mut movement_complete = test_fixture("agent_movement_complete.hex");
        movement_complete[0] |= 0x40;
        set_sequence(&mut movement_complete, 23);
        primary_socket.send_to(&movement_complete, primary_peer).await.expect("complete primary movement");
        primary_socket.send_to(&ack(24, handshake_reply.sequence()), primary_peer).await.expect("ack primary region handshake reply");

        let mut primary_id = None;
        let mut ready_id = None;
        let mut movement_seen = false;
        while primary_id.is_none() || ready_id.is_none() || !movement_seen {
            match next_raw_event(&mut events).await {
                Event::SimulatorConnected { simulator_id, region_handle: None, .. } => primary_id = Some(simulator_id),
                Event::SessionReady { simulator_id, .. } => ready_id = Some(simulator_id),
                Event::SimulatorMessage { sequence: 23, .. } => movement_seen = true,
                Event::Failed(reason) => panic!("session failed during startup: {reason}"),
                _ => {}
            }
        }
        let primary_id = primary_id.expect("primary simulator ID");
        assert_eq!(ready_id, Some(primary_id));

        let mut simulators = vec![SimulatorEndpoint { socket: primary_socket, peer: primary_peer, id: primary_id }];
        for index in 0..neighbor_count {
            let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind neighbor simulator"));
            let endpoint = socket.local_addr().expect("neighbor simulator address");
            let region_handle = NEIGHBOR_REGION_BASE + index as u64 * 0x1000;
            let sequence = 30 + index as u32;
            let primary = simulators.first().expect("primary simulator exists");
            announce_neighbor(&primary.socket, primary.peer, endpoint, region_handle, sequence).await;

            let (use_circuit_bytes, peer) = receive(&socket).await;
            let use_circuit = Packet::decode(&use_circuit_bytes).expect("decode neighbor UseCircuitCode");
            assert!(use_circuit.is_reliable());
            std::assert_matches!(use_circuit.message(), Message::UseCircuitCode(_));
            socket.send_to(&ack(40, use_circuit.sequence()), peer).await.expect("ack neighbor circuit");

            let id = loop {
                match next_raw_event(&mut events).await {
                    Event::SimulatorConnected { simulator_id, region_handle: Some(handle), endpoint: connected_endpoint }
                        if handle == region_handle && connected_endpoint == endpoint =>
                    {
                        break simulator_id;
                    }
                    Event::Failed(reason) => panic!("session failed while connecting neighbor: {reason}"),
                    _ => {}
                }
            };
            simulators.push(SimulatorEndpoint { socket, peer, id });
        }

        Self { simulators, events, commands, disconnect, client }
    }

    fn start_ack_monitor(&self) -> AckMonitor {
        AckMonitor::start(&self.simulators)
    }

    async fn shutdown(self) {
        let primary = self.simulators.first().expect("primary simulator exists");
        self.disconnect.send_replace(true);
        let request = timeout(HANG_GUARD, receive_message(&primary.socket, |message| matches!(message, Message::LogoutRequest(_))))
            .await
            .expect("session should send LogoutRequest");
        let sequence = FIRST_SEQUENCE + BURST_COUNT as u32 * PACKETS_PER_CIRCUIT as u32;
        primary.socket.send_to(&ack(sequence, request.sequence()), primary.peer).await.expect("ack logout request");
        let mut reply = test_fixture("logout_reply.hex");
        set_sequence(&mut reply, sequence + 1);
        primary.socket.send_to(&reply, primary.peer).await.expect("complete session logout");
        let result = timeout(HANG_GUARD, self.client).await.expect("session task should stop").expect("session task should not panic");
        assert!(result.is_ok(), "session shutdown should complete cleanly");
        drop(self.commands);
    }
}

fn larger_object_update() -> (Vec<u8>, ObjectUpdate) {
    let fixture = test_fixture("object_update.hex");
    let packet = Packet::decode(&fixture).expect("decode object update fixture");
    let Message::ObjectUpdate(base) = packet.into_message() else {
        panic!("object update fixture should contain ObjectUpdate");
    };
    assert!(!base.objects.is_empty(), "object update fixture should contain object blocks");

    let mut objects = Vec::with_capacity(LARGE_OBJECT_COUNT);
    for index in 0..LARGE_OBJECT_COUNT {
        let mut object = base.objects[index % base.objects.len()].clone();
        object.local_id = 0x7000_0000 + index as u32;
        object.id = Uuid::from_u128(0x7000_0000_0000_0000_0000_0000_0000_0000 + index as u128);
        objects.push(object);
    }

    (fixture, ObjectUpdate { region_data: base.region_data, objects })
}

fn encode_measurement_packet(ordinary: &[u8], larger: &ObjectUpdate, sequence: u32, use_larger: bool) -> Vec<u8> {
    let wire = if use_larger {
        Packet::new(sequence, Message::ObjectUpdate(larger.clone())).with_reliable().encode().expect("encode larger reliable object update")
    } else {
        let mut wire = ordinary.to_vec();
        *wire.first_mut().expect("object update fixture contains packet flags") |= 0x40;
        set_sequence(&mut wire, sequence);
        wire
    };
    Packet::preflight(&wire).expect("measurement object packet should fit LLUDP bounds");
    wire
}

async fn measure_bursts(harness: &mut SessionHarness, monitor: &mut AckMonitor, ordinary: &[u8], larger: &ObjectUpdate) -> Measurements {
    let circuit_indices = harness.simulators.iter().enumerate().map(|(index, simulator)| (simulator.id, index)).collect::<HashMap<_, _>>();
    let mut measurements = Measurements { ordinary_bytes: ordinary.len(), ..Measurements::default() };

    for burst in 0..BURST_COUNT {
        let mut expected = HashSet::with_capacity(harness.simulators.len() * PACKETS_PER_CIRCUIT);
        let mut sent_at = HashMap::with_capacity(harness.simulators.len() * PACKETS_PER_CIRCUIT);
        let mut wires = Vec::with_capacity(harness.simulators.len() * PACKETS_PER_CIRCUIT);
        for (circuit, simulator) in harness.simulators.iter().enumerate() {
            for packet_index in 0..PACKETS_PER_CIRCUIT {
                let sequence = FIRST_SEQUENCE + burst as u32 * PACKETS_PER_CIRCUIT as u32 + packet_index as u32;
                let use_larger = packet_index % 2 == 1;
                let wire = encode_measurement_packet(ordinary, larger, sequence, use_larger);
                let key = PacketKey { circuit, sequence };
                expected.insert(key);
                wires.push((simulator, key, wire, use_larger));
            }
        }

        let burst_started = Instant::now();
        for (simulator, key, wire, use_larger) in &wires {
            sent_at.insert(*key, Instant::now());
            simulator.socket.send_to(wire, simulator.peer).await.expect("send reliable object update");
            measurements.packet_count += 1;
            if *use_larger {
                measurements.larger_bytes = wire.len();
            }
        }

        let mut acknowledged = HashSet::with_capacity(expected.len());
        let mut delivered = HashSet::with_capacity(expected.len());
        while acknowledged.len() < expected.len() || delivered.len() < expected.len() {
            tokio::select! {
                observation = monitor.observations.recv(), if acknowledged.len() < expected.len() => {
                    let observation = observation.expect("ACK monitor should stay connected");
                    if expected.contains(&observation.key) && acknowledged.insert(observation.key) {
                        let start = sent_at.get(&observation.key).expect("sent packet timestamp");
                        measurements.acknowledgement_latency.push(observation.received_at.duration_since(*start));
                    }
                }
                event = next_raw_event(&mut harness.events), if delivered.len() < expected.len() => {
                    match event {
                        Event::SimulatorMessage { simulator_id, sequence, message, .. } => {
                            std::assert_matches!(message, Message::ObjectUpdate(_));
                            let circuit = *circuit_indices.get(&simulator_id).expect("event belongs to an active circuit");
                            let key = PacketKey { circuit, sequence };
                            assert!(expected.contains(&key), "session delivered an unexpected burst packet");
                            assert!(delivered.insert(key), "session delivered each reliable object packet once");
                            let start = sent_at.get(&key).expect("sent event timestamp");
                            measurements.delivery_latency.push(Instant::now().duration_since(*start));
                        }
                        Event::Failed(reason) => panic!("session failed during UDP measurement: {reason}"),
                        _ => {}
                    }
                }
            }
        }

        assert_eq!(acknowledged, expected, "every reliable sequence should be acknowledged");
        assert_eq!(delivered, expected, "every reliable sequence should reach the application event channel");
        measurements.burst_elapsed.push(burst_started.elapsed());
    }

    measurements
}

fn latency_summary(samples: &[Duration]) -> (u128, u128, u128) {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let percentile = |percent: usize| sorted[(sorted.len() - 1) * percent / 100].as_micros();
    (percentile(50), percentile(95), sorted.last().expect("measurement has samples").as_micros())
}

fn report_measurement(circuits: usize, measurements: &Measurements) {
    let total_elapsed = measurements.burst_elapsed.iter().copied().sum::<Duration>();
    let acknowledged_burst_rate = measurements.packet_count as f64 / total_elapsed.as_secs_f64();
    let (burst_p50, burst_p95, burst_max) = latency_summary(&measurements.burst_elapsed);
    let (ack_p50, ack_p95, ack_max) = latency_summary(&measurements.acknowledgement_latency);
    let (delivery_p50, delivery_p95, delivery_max) = latency_summary(&measurements.delivery_latency);
    eprintln!(
        "manual UDP measurement: circuits={circuits}, packets={}, bursts={BURST_COUNT}, packets_per_circuit_burst={PACKETS_PER_CIRCUIT}, ordinary_bytes={}, larger_bytes={}, acknowledged_burst_packets_per_second={acknowledged_burst_rate:.0}, burst_us_p50/p95/max={burst_p50}/{burst_p95}/{burst_max}, ack_us_p50/p95/max={ack_p50}/{ack_p95}/{ack_max}, delivery_us_p50/p95/max={delivery_p50}/{delivery_p95}/{delivery_max}",
        measurements.packet_count, measurements.ordinary_bytes, measurements.larger_bytes,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual UDP measurement; run cargo test --release -p network udp_burst_measurement -- --ignored --nocapture"]
async fn udp_burst_measurement() {
    let (ordinary, larger) = larger_object_update();
    let ordinary_packet = encode_measurement_packet(&ordinary, &larger, FIRST_SEQUENCE, false);
    let larger_packet = encode_measurement_packet(&ordinary, &larger, FIRST_SEQUENCE, true);
    assert!(larger_packet.len() > ordinary_packet.len(), "larger object update should exceed the ordinary fixture size");
    let Message::ObjectUpdate(large_update) = Packet::decode(&larger_packet).expect("decode larger object update").into_message() else {
        panic!("larger packet should remain an object update");
    };
    assert_eq!(large_update.objects.len(), LARGE_OBJECT_COUNT);

    for neighbor_count in [0, 3] {
        let mut harness =
            timeout(HANG_GUARD, Box::pin(SessionHarness::connect(neighbor_count))).await.expect("session setup should complete");
        let circuits = harness.simulators.len();
        assert!(circuits * PACKETS_PER_CIRCUIT < crate::circuit::admission::SESSION_APPLICATION_EVENT_CREDIT_CAPACITY);
        let mut monitor = harness.start_ack_monitor();
        let measurements = timeout(HANG_GUARD, measure_bursts(&mut harness, &mut monitor, &ordinary, &larger))
            .await
            .expect("UDP measurement should complete");
        monitor.stop().await;
        report_measurement(circuits, &measurements);
        harness.shutdown().await;
    }
}
