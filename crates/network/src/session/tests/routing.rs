use super::*;
use crate::fixtures::test_simulator_id;

async fn run_queued_target_does_not_follow_neighbor_replacement() {
    let mut harness = Box::pin(connected_neighbor()).await;
    let neighbor_id = harness.neighbor_id;
    let replacement = UdpSocket::bind("127.0.0.1:0").await.expect("bind replacement simulator");
    let endpoint = replacement.local_addr().expect("replacement endpoint");
    let region_handle = 0x1000_0000_0000_1000;

    announce_neighbor(&harness.primary, harness.primary_peer, endpoint, region_handle, 50).await;
    let (bytes, replacement_peer) = receive(&replacement).await;
    let circuit = Packet::decode(&bytes).expect("decode replacement circuit packet");
    std::assert_matches!(circuit.message(), Message::UseCircuitCode(_));
    replacement.send_to(&ack(60, circuit.sequence()), replacement_peer).await.expect("ack replacement circuit");

    let replacement_id = timeout(WAIT, async {
        loop {
            if let Event::SimulatorConnected { simulator_id, region_handle: Some(handle), endpoint: connected_endpoint } =
                next_event(&mut harness.events).await
                && handle == region_handle
                && connected_endpoint == endpoint
            {
                break simulator_id;
            }
        }
    })
    .await
    .expect("replacement connection should be announced");
    assert_ne!(replacement_id, neighbor_id);

    let (completed, result) = oneshot::channel();
    harness
        .commands
        .send(Command::SendTo { simulator_id: neighbor_id, message: object_request(), delivery: Delivery::Reliable, completed })
        .await
        .expect("queue an exact send while the replacement route is installed");
    std::assert_matches!(
        timeout(WAIT, result).await.expect("stale target send should complete").expect("send reply"),
        Err(Error::SimulatorUnavailable)
    );

    let mut buffer = [0; 65_535];
    let observer = clone_udp_socket(&replacement).expect("inspect replacement endpoint");
    loop {
        match observer.recv_from(&mut buffer) {
            Ok((size, _)) => {
                let packet = Packet::decode(&buffer[..size]).expect("decode replacement traffic");
                assert!(
                    !matches!(packet.message(), Message::RequestMultipleObjects(_)),
                    "a queued send for the retired simulator ID must not reach its replacement"
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("read replacement traffic: {error}"),
        }
    }

    harness.shutdown().await;
}

async fn run_neighbor_targets_and_replacements() {
    let mut harness = Box::pin(connected_neighbor()).await;
    receive_ack(&harness.primary, 23).await;
    send_targeted(&harness.commands, harness.neighbor_id, object_request())
        .await
        .expect("route an object request to the selected simulator");
    let (_, sent_peer) =
        receive_message_with_peer(&harness.neighbor, |message| matches!(message, Message::RequestMultipleObjects(_))).await;
    assert_eq!(sent_peer, harness.neighbor_peer);
    let mut buffer = [0; 65_535];
    std::assert_matches!(clone_udp_socket(&harness.primary).expect("inspect primary endpoint").recv_from(&mut buffer), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock);

    let mut disable = test_fixture("disable_simulator.hex");
    set_sequence(&mut disable, 42);
    harness.neighbor.send_to(&disable, harness.neighbor_peer).await.expect("disable neighbor circuit");
    loop {
        if matches!(next_event(&mut harness.events).await, Event::SimulatorDisconnected { simulator_id, .. } if simulator_id == harness.neighbor_id)
        {
            break;
        }
    }
    std::assert_matches!(send_targeted(&harness.commands, harness.neighbor_id, object_request()).await, Err(Error::SimulatorUnavailable));

    let neighbor_endpoint = harness.neighbor.local_addr().expect("neighbor address");
    announce_neighbor(&harness.primary, harness.primary_peer, neighbor_endpoint, 0x1000_0000_0000_1000, 50).await;
    let (bytes, replacement_peer) = receive(&harness.neighbor).await;
    std::assert_matches!(Packet::decode(&bytes).expect("decode replacement circuit packet").message(), Message::UseCircuitCode(_));
    harness.neighbor.send_to(&ack(60, 1), replacement_peer).await.expect("ack replacement circuit");
    let replacement_id = loop {
        match next_event(&mut harness.events).await {
            Event::SimulatorConnected { simulator_id, region_handle: Some(0x1000_0000_0000_1000), endpoint }
                if endpoint == neighbor_endpoint =>
            {
                break simulator_id;
            }
            _ => {}
        }
    };
    assert_ne!(replacement_id, harness.neighbor_id);
    std::assert_matches!(send_targeted(&harness.commands, harness.neighbor_id, object_request()).await, Err(Error::SimulatorUnavailable));
    send_targeted(&harness.commands, replacement_id, object_request()).await.expect("send to replacement");
    let (_, sent_peer) =
        receive_message_with_peer(&harness.neighbor, |message| matches!(message, Message::RequestMultipleObjects(_))).await;
    assert_eq!(sent_peer, replacement_peer);
    harness.shutdown().await;
}

async fn run_same_endpoint_teleport_retry_promotes_replacement() {
    let mut harness = Box::pin(connected_neighbor()).await;
    let endpoint = harness.neighbor.local_addr().expect("neighbor endpoint");
    let region_handle = 0x1000_0000_0000_1000;
    let session = login_session(harness.primary.local_addr().expect("primary endpoint"));
    let first_finish = Packet::new(50, teleport_finish_to(session.agent_id, endpoint, region_handle, session.seed_capability.as_str()))
        .with_reliable()
        .encode()
        .expect("encode first handoff attempt");
    harness.primary.send_to(&first_finish, harness.primary_peer).await.expect("announce first handoff attempt");
    receive_ack(&harness.primary, 50).await;

    timeout(WAIT, async {
        loop {
            if matches!(
                next_raw_event(&mut harness.events).await,
                Event::SimulatorMessage { simulator_id, message: Message::TeleportFinish(_), .. }
                    if simulator_id == harness.primary_id
            ) {
                break;
            }
        }
    })
    .await
    .expect("session admits the first handoff attempt");

    let (completed, cancellation_result) = oneshot::channel();
    harness
        .commands
        .send(Command::SendTo {
            simulator_id: harness.primary_id,
            message: Message::TeleportCancel(protocol::messages::TeleportCancel {
                agent_id: session.agent_id,
                session_id: session.session_id,
            }),
            delivery: Delivery::Reliable,
            completed,
        })
        .await
        .expect("send handoff cancellation");
    let cancellation = timeout(WAIT, receive_message(&harness.primary, |message| matches!(message, Message::TeleportCancel(_))))
        .await
        .expect("cancellation reaches the primary simulator");
    harness.primary.send_to(&ack(51, cancellation.sequence()), harness.primary_peer).await.expect("ack cancellation");
    timeout(WAIT, cancellation_result)
        .await
        .expect("cancellation send completes")
        .expect("cancellation completion reply")
        .expect("cancellation reaches the local socket");

    let retry = Packet::new(52, teleport_finish_to(session.agent_id, endpoint, region_handle, session.seed_capability.as_str()))
        .with_reliable()
        .encode()
        .expect("encode replacement handoff attempt");
    harness.primary.send_to(&retry, harness.primary_peer).await.expect("retry the same region and endpoint");
    receive_ack(&harness.primary, 52).await;
    let (circuit, replacement_peer) =
        timeout(WAIT, receive_message_with_peer(&harness.neighbor, |message| matches!(message, Message::UseCircuitCode(_))))
            .await
            .expect("replacement attempt opens a new simulator circuit");
    harness.neighbor.send_to(&ack(60, circuit.sequence()), replacement_peer).await.expect("ack replacement circuit");

    let mut old_disconnected = false;
    let replacement_id = timeout(WAIT, async {
        loop {
            match next_raw_event(&mut harness.events).await {
                Event::SimulatorDisconnected { simulator_id, region_handle: Some(handle), .. }
                    if simulator_id == harness.neighbor_id && handle == region_handle =>
                {
                    old_disconnected = true;
                }
                Event::SimulatorConnected { simulator_id, region_handle: Some(handle), endpoint: connected_endpoint }
                    if handle == region_handle && connected_endpoint == endpoint && simulator_id != harness.neighbor_id =>
                {
                    break simulator_id;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("replacement connection becomes available");
    assert_ne!(replacement_id, harness.neighbor_id);

    let mut handshake = test_fixture("region_handshake.hex");
    handshake[0] |= 0x40;
    set_sequence(&mut handshake, 70);
    harness.neighbor.send_to(&handshake, replacement_peer).await.expect("handshake replacement circuit");
    let reply = timeout(WAIT, receive_message(&harness.neighbor, |message| matches!(message, Message::RegionHandshakeReply(_))))
        .await
        .expect("replacement handshake reply");
    let movement = timeout(WAIT, receive_message(&harness.neighbor, |message| matches!(message, Message::CompleteAgentMovement(_))))
        .await
        .expect("replacement movement request");
    harness.neighbor.send_to(&ack(71, reply.sequence()), replacement_peer).await.expect("ack replacement handshake reply");
    harness.neighbor.send_to(&ack(72, movement.sequence()), replacement_peer).await.expect("ack replacement movement request");
    harness
        .neighbor
        .send_to(&movement_complete_packet(73, session.agent_id, session.session_id, region_handle), replacement_peer)
        .await
        .expect("complete movement on replacement circuit");

    let mut promoted = false;
    timeout(WAIT, async {
        while !promoted || !old_disconnected {
            match next_raw_event(&mut harness.events).await {
                Event::SimulatorPromoted { simulator_id, .. } if simulator_id == replacement_id => promoted = true,
                Event::SimulatorDisconnected { simulator_id, region_handle: Some(handle), .. }
                    if simulator_id == harness.neighbor_id && handle == region_handle =>
                {
                    old_disconnected = true;
                }
                Event::Failed(reason) => panic!("same-endpoint retry failed: {reason}"),
                _ => {}
            }
        }
    })
    .await
    .expect("the replacement promotes and the old completion is absorbed");

    harness
        .commands
        .send(Command::SendMessage { message: object_request(), delivery: Delivery::Unreliable })
        .await
        .expect("send through the replacement primary");
    let (_, sent_peer) =
        timeout(WAIT, receive_message_with_peer(&harness.neighbor, |message| matches!(message, Message::RequestMultipleObjects(_))))
            .await
            .expect("replacement route remains active after the old completion");
    assert_eq!(sent_peer, replacement_peer);

    harness.disconnect.send_replace(true);
    let logout = timeout(WAIT, receive_message(&harness.neighbor, |message| matches!(message, Message::LogoutRequest(_))))
        .await
        .expect("replacement primary receives logout");
    send_logout_reply(&harness.neighbor, replacement_peer, &logout, 74).await;
    timeout(WAIT, harness.client).await.expect("same-endpoint session shutdown").expect("session task").expect("clean shutdown");
}

#[tokio::test]
async fn primary_intro_precedes_udp_data() {
    let simulator = UdpSocket::bind("127.0.0.1:0").await.expect("bind primary simulator");
    let (commands, command_receiver) = mpsc::channel(COMMAND_CAPACITY);
    let (event_sender, mut events) = mpsc::channel(TEST_EVENT_CAPACITY);
    let (disconnect, disconnect_requested) = watch::channel(false);
    let client = spawn(run_session(
        login_session(simulator.local_addr().expect("primary simulator address")),
        commands.clone(),
        command_receiver,
        event_sender,
        disconnect_requested,
    ));

    let (_, peer) = receive(&simulator).await;
    let mut chat = test_fixture("chat_from_simulator.hex");
    chat[0] |= 0x40;
    set_sequence(&mut chat, 19);
    simulator.send_to(&chat, peer).await.expect("send data before circuit acknowledgement");
    receive_message(&simulator, |message| matches!(message, Message::PacketAck(acks) if acks.packets.iter().any(|packet| packet.id == 19)))
        .await;
    assert!(events.try_recv().is_err(), "primary application data must wait for the simulator introduction");

    simulator.send_to(&ack(20, 1), peer).await.expect("ack primary circuit");
    receive_message(&simulator, |message| matches!(message, Message::CompleteAgentMovement(_))).await;
    let first = next_raw_event(&mut events).await;
    let simulator_id = match first {
        Event::SimulatorConnected { simulator_id, region_handle: None, .. } => simulator_id,
        event => panic!("expected primary introduction before other events, got {event:?}"),
    };
    std::assert_matches!(
        next_raw_event(&mut events).await,
        Event::SimulatorMessage { simulator_id: id, sequence: 19, message: Message::ChatFromSimulator(_), .. }
            if id == simulator_id
    );

    disconnect.send_replace(true);
    let logout = receive_message(&simulator, |message| matches!(message, Message::LogoutRequest(_))).await;
    send_logout_reply(&simulator, peer, &logout, 21).await;
    assert!(timeout(WAIT, client).await.expect("disconnect should finish").expect("client task").is_ok());
    drop(commands);
}

#[tokio::test]
async fn replacing_neighbor_releases_queued_state() {
    let primary = UdpSocket::bind("127.0.0.1:0").await.expect("bind primary simulator");
    let old_neighbor = UdpSocket::bind("127.0.0.1:0").await.expect("bind old neighbor");
    let new_neighbor = UdpSocket::bind("127.0.0.1:0").await.expect("bind replacement neighbor");
    let old_endpoint = old_neighbor.local_addr().expect("read old neighbor address");
    let new_endpoint = new_neighbor.local_addr().expect("read replacement address");
    let region_handle = 0x1000_0000_0000_1000;
    let mut connection = connect_runtime(login_session(primary.local_addr().expect("primary simulator address")), ResponseData::default())
        .await
        .expect("connect primary circuit");
    let _ = receive(&primary).await;
    let (application_events, _application_event_receiver) = mpsc::channel(8);
    let (neighbor_events, _neighbor_event_receiver) = mpsc::channel::<SimulatorEvent>(8);
    let (_shutdown_signal, shutdown) = watch::channel(false);
    let (neighbor_requests, _neighbor_request_receiver) = mpsc::channel(8);
    let mut run_state = RunState::new();
    let (capability_events, _capability_event_receiver) = mpsc::channel(8);
    let mut setup =
        PromotionSetup { state: &mut run_state, neighbor_requests, application_events, neighbor_events, shutdown, capability_events };
    let root_simulator_id = connection.state.simulator_id;

    connection
        .state
        .start_neighbor(root_simulator_id, region_handle, old_endpoint, None, &mut setup)
        .await
        .expect("start initial neighbor");
    let _ = receive(&old_neighbor).await;
    let old_id = connection
        .state
        .simulators
        .iter()
        .find_map(|(id, record)| (record.region_handle == Some(region_handle) && record.endpoint == old_endpoint).then_some(*id))
        .expect("old neighbor is tracked");
    let packet_memory = connection.state.simulators.get(&old_id).expect("old simulator record").packet_memory.clone();
    let packet = test_fixture("chat_from_simulator.hex");
    connection.state.simulators.get_mut(&old_id).expect("old simulator record").preconnect_events.push_back(retained_packet_event(
        &packet_memory,
        old_id,
        old_endpoint,
        &packet,
    ));
    setup.state.preconnect_events_len += 1;
    let seed_capability = Url::parse("http://127.0.0.1:1/seed").expect("seed URL");
    connection.state.simulators.get_mut(&old_id).expect("old simulator record").capabilities = Some(SimulatorCapabilitySession {
        seed_capability: seed_capability.clone(),
        client: Client::new_with_budget(seed_capability, CapabilityBudget::new()).expect("create old capability client"),
        event_queue: EventQueueLifecycle::NotStarted,
    });
    assert!(packet_memory.global_available_bytes() < MAX_QUEUED_SIMULATOR_EVENT_BYTES);

    connection
        .state
        .start_neighbor(root_simulator_id, region_handle, new_endpoint, None, &mut setup)
        .await
        .expect("replace unannounced neighbor");
    let _ = receive(&new_neighbor).await;

    let replacement_id = connection
        .state
        .simulators
        .iter()
        .find_map(|(id, record)| {
            (record.is_current() && record.region_handle == Some(region_handle) && record.endpoint == new_endpoint).then_some(*id)
        })
        .expect("replacement remains tracked");
    assert_ne!(replacement_id, old_id);
    let old_record = connection.state.simulators.get(&old_id).expect("retired record remains until task completion");
    assert!(old_record.is_retiring());
    assert!(old_record.preconnect_events.is_empty(), "replacement drops old pre-connect events immediately");
    assert_eq!(packet_memory.global_available_bytes(), MAX_QUEUED_SIMULATOR_EVENT_BYTES);
    assert_eq!(packet_memory.neighbor_available_bytes(), MAX_QUEUED_NEIGHBOR_EVENT_BYTES);
    assert_eq!(packet_memory.connection_available_bytes(), MAX_NEIGHBOR_CONNECTION_EVENT_BYTES);
    assert!(old_record.capabilities.is_none(), "replacement stops the old capability session immediately");

    drop(setup);
    run_state.connections.shutdown().await;
}

#[tokio::test]
async fn preconnect_overflow_reports_coverage() {
    let primary = UdpSocket::bind("127.0.0.1:0").await.expect("bind primary simulator");
    let primary_endpoint = primary.local_addr().expect("read primary address");
    let mut connection = connect_runtime(login_session(primary_endpoint), ResponseData::default()).await.expect("connect primary circuit");
    let simulator_id = test_simulator_id(53);
    let neighbor_endpoint = "127.0.0.1:13003".parse().expect("parse neighbor endpoint");
    let region_handle = 0x1000_0000_0000_1003;
    let neighbor_task = spawn(pending::<()>());
    let (neighbor_commands, _neighbor_command_receiver) = mpsc::channel(1);
    let mut run_state = RunState::new();
    connection.state.simulators.insert(
        simulator_id,
        test_simulator_record(neighbor_endpoint, Some(region_handle), neighbor_task.abort_handle(), neighbor_commands),
    );
    let (events, mut received) = mpsc::channel(1);
    for _ in 0..MAX_PRECONNECT_EVENTS {
        connection
            .state
            .publish_simulator_event(simulator_id, Event::Failed("buffered".into()).into(), &events, &mut run_state)
            .await
            .expect("buffer pre-connect event");
    }
    let source = CoverageSource::EventQueue { incarnation: 54 };
    let watermark = connection.state.coverage_losses.watermark(simulator_id, source);

    connection
        .state
        .publish_simulator_event(
            simulator_id,
            event_with_coverage_watermark(
                Event::CapabilityEvent {
                    simulator_id,
                    source: neighbor_endpoint,
                    message: "CrossedRegion".into(),
                    body: llsd::Value::Undefined,
                },
                watermark,
            ),
            &events,
            &mut run_state,
        )
        .await
        .expect("a full pre-connect buffer should report coverage loss without ending the session");

    let record = connection.state.simulators.get(&simulator_id).expect("queue pressure must keep the neighbor connected");
    assert_eq!(record.preconnect_events.len(), MAX_PRECONNECT_EVENTS);
    assert_eq!(run_state.preconnect_events_len, MAX_PRECONNECT_EVENTS);
    assert_eq!(record.connection, SimulatorConnectionLifecycle::Connecting);
    assert!(received.try_recv().is_err(), "the pre-connect event remains buffered until connection ordering is established");
    std::assert_matches!(
        connection.state.coverage_losses.snapshot().losses.as_slice(),
        [CoverageLoss::Source(record)]
            if record.watermark.simulator_id == simulator_id
                && record.watermark.source == source
                && record.reason == CoverageLossReason::EventAdmission
    );
    neighbor_task.abort();
    assert!(neighbor_task.await.expect_err("test neighbor worker should be cancelled").is_cancelled());
}

#[tokio::test]
async fn neighbor_circuits_are_independent() {
    let mut harness = Box::pin(connected_neighbor()).await;
    let neighbor_peer = harness.neighbor_peer;
    let neighbor_id = harness.neighbor_id;
    assert_ne!(harness.primary_id, neighbor_id);

    let mut neighbor_handshake = test_fixture("region_handshake.hex");
    neighbor_handshake[0] |= 0x40;
    set_sequence(&mut neighbor_handshake, 41);
    harness.neighbor.send_to(&neighbor_handshake, neighbor_peer).await.expect("send neighbor handshake");
    let reply = receive_message(&harness.neighbor, |message| matches!(message, Message::RegionHandshakeReply(_))).await;
    assert!(reply.appended_acks().contains(&41));
    std::assert_matches!(
        next_event(&mut harness.events).await,
        Event::SimulatorMessage { simulator_id, source, sequence: 41, message: Message::RegionHandshake(_), .. }
            if simulator_id == neighbor_id && source == harness.neighbor.local_addr().expect("neighbor address")
    );

    let mut disable = test_fixture("disable_simulator.hex");
    set_sequence(&mut disable, 42);
    harness.neighbor.send_to(&disable, neighbor_peer).await.expect("disable neighbor circuit");
    loop {
        if matches!(next_event(&mut harness.events).await, Event::SimulatorDisconnected { simulator_id, region_handle: Some(0x1000_0000_0000_1000), .. } if simulator_id == neighbor_id)
        {
            break;
        }
    }

    let mut chat = test_fixture("chat_from_simulator.hex");
    chat[0] |= 0x40;
    set_sequence(&mut chat, 43);
    harness.neighbor.send_to(&chat, neighbor_peer).await.expect("send after neighbor disabled");
    let mut buffer = [0; 65_535];
    match clone_udp_socket(&harness.neighbor).expect("inspect disabled neighbor endpoint").recv_from(&mut buffer) {
        Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::ConnectionReset) => {}
        Err(error) => panic!("read disabled neighbor traffic: {error}"),
        Ok((size, _)) => {
            panic!("disabled circuit sent another packet: {:?}", Packet::decode(&buffer[..size]).expect("decode packet").message())
        }
    }

    harness.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn neighbor_delivers_data_before_terminal() {
    let mut harness = Box::pin(connected_neighbor()).await;

    let mut chat = test_fixture("chat_from_simulator.hex");
    chat[0] |= 0x40;
    set_sequence(&mut chat, 41);
    harness.neighbor.send_to(&chat, harness.neighbor_peer).await.expect("send final neighbor message");

    let mut disable = test_fixture("disable_simulator.hex");
    set_sequence(&mut disable, 42);
    harness.neighbor.send_to(&disable, harness.neighbor_peer).await.expect("close neighbor circuit");

    std::assert_matches!(
        next_event(&mut harness.events).await,
        Event::SimulatorMessage { simulator_id, sequence: 41, message: Message::ChatFromSimulator(_), .. }
            if simulator_id == harness.neighbor_id
    );
    std::assert_matches!(
        next_event(&mut harness.events).await,
        Event::SimulatorDisconnected { simulator_id, region_handle: Some(0x1000_0000_0000_1000), .. }
            if simulator_id == harness.neighbor_id
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn local_ids_are_simulator_scoped() {
    let mut harness = Box::pin(connected_neighbor()).await;

    let mut primary_update = test_fixture("object_update.hex");
    set_sequence(&mut primary_update, 44);
    harness.primary.send_to(&primary_update, harness.primary_peer).await.expect("send primary object update");

    let mut neighbor_update = test_fixture("object_update.hex");
    set_sequence(&mut neighbor_update, 45);
    harness.neighbor.send_to(&neighbor_update, harness.neighbor_peer).await.expect("send neighbor object update");

    let mut received_simulators = Vec::with_capacity(2);
    timeout(WAIT, async {
        while received_simulators.len() < 2 {
            if let Event::SimulatorMessage { simulator_id, message: Message::ObjectUpdate(update), .. } =
                next_event(&mut harness.events).await
            {
                assert_eq!(update.objects[0].local_id, 0x1020_3040);
                received_simulators.push(simulator_id);
            }
        }
    })
    .await
    .expect("receive the object update from both circuits");

    assert!(received_simulators.contains(&harness.primary_id));
    assert!(received_simulators.contains(&harness.neighbor_id));
    assert_ne!(harness.primary_id, harness.neighbor_id);
    harness.shutdown().await;
}

#[test]
fn neighbor_targets_and_replacements() {
    run_large_stack_test(run_neighbor_targets_and_replacements);
}

#[test]
fn queued_target_ignores_replacement() {
    run_large_stack_test(run_queued_target_does_not_follow_neighbor_replacement);
}

#[test]
fn same_endpoint_retry_promotes_replacement() {
    run_large_stack_test(run_same_endpoint_teleport_retry_promotes_replacement);
}

#[tokio::test]
async fn old_completion_keeps_replacement_route() {
    let mut harness = Box::pin(connected_neighbor()).await;
    let replacement = UdpSocket::bind("127.0.0.1:0").await.expect("bind replacement neighbor");
    let replacement_endpoint = replacement.local_addr().expect("replacement address");
    let region_handle = 0x1000_0000_0000_1000;

    announce_neighbor(&harness.primary, harness.primary_peer, replacement_endpoint, region_handle, 50).await;
    let (bytes, replacement_peer) = receive(&replacement).await;
    std::assert_matches!(Packet::decode(&bytes).expect("decode replacement circuit packet").message(), Message::UseCircuitCode(_));
    replacement.send_to(&ack(60, 1), replacement_peer).await.expect("ack replacement circuit");

    let mut replacement_id = None;
    let mut old_disconnected = false;
    timeout(WAIT, async {
        while replacement_id.is_none() || !old_disconnected {
            match next_event(&mut harness.events).await {
                Event::SimulatorConnected { simulator_id, region_handle: Some(handle), endpoint }
                    if handle == region_handle && endpoint == replacement_endpoint =>
                {
                    replacement_id = Some(simulator_id);
                }
                Event::SimulatorDisconnected { simulator_id, region_handle: Some(handle), .. }
                    if simulator_id == harness.neighbor_id && handle == region_handle =>
                {
                    old_disconnected = true;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("observe old completion and replacement introduction");

    let replacement_id = replacement_id.expect("replacement connection ID");
    assert_ne!(replacement_id, harness.neighbor_id);
    send_targeted(&harness.commands, replacement_id, object_request()).await.expect("old completion must preserve the replacement route");
    let (_, sent_peer) = receive_message_with_peer(&replacement, |message| matches!(message, Message::RequestMultipleObjects(_))).await;
    assert_eq!(sent_peer, replacement_peer);

    harness.shutdown().await;
}

#[tokio::test]
async fn neighbor_endpoint_validation() {
    let mut harness = Box::pin(connected_neighbor()).await;
    let primary_endpoint = harness.primary.local_addr().expect("primary simulator address");
    let neighbor_endpoint = harness.neighbor.local_addr().expect("neighbor simulator address");
    let candidates = [
        (0x2000_0000_0000_0000, primary_endpoint),
        (0x3000_0000_0000_0000, neighbor_endpoint),
        (0x4000_0000_0000_0000, SocketAddr::from(([127, 0, 0, 1], 0))),
        (0x5000_0000_0000_0000, SocketAddr::from(([0, 0, 0, 0], 13000))),
        (0x6000_0000_0000_0000, SocketAddr::from(([224, 0, 0, 1], 13000))),
        (0x7000_0000_0000_0000, SocketAddr::from(([255, 255, 255, 255], 13000))),
    ];

    for (index, (region_handle, endpoint)) in candidates.into_iter().enumerate() {
        announce_neighbor(&harness.primary, harness.primary_peer, endpoint, region_handle, 50 + index as u32).await;
        let failed_endpoint = timeout(WAIT, async {
            loop {
                if let Event::SimulatorConnectFailed { region_handle: failed_region, endpoint, .. } = next_event(&mut harness.events).await
                    && failed_region == region_handle
                {
                    break endpoint;
                }
            }
        })
        .await
        .expect("invalid or duplicate endpoint should fail before starting a connection");
        assert_eq!(failed_endpoint, endpoint);
    }

    harness.shutdown().await;
}

#[tokio::test]
async fn duplicate_replacement_keeps_old_neighbor() {
    let mut harness = Box::pin(connected_neighbor()).await;
    let other = UdpSocket::bind("127.0.0.1:0").await.expect("bind other neighbor");
    let other_endpoint = other.local_addr().expect("read other neighbor address");
    let other_region = 0x2000_0000_0000_2000;
    announce_neighbor(&harness.primary, harness.primary_peer, other_endpoint, other_region, 50).await;
    let (bytes, other_peer) = receive(&other).await;
    std::assert_matches!(Packet::decode(&bytes).expect("decode other neighbor bootstrap").message(), Message::UseCircuitCode(_));
    other.send_to(&ack(60, 1), other_peer).await.expect("ack other neighbor bootstrap");
    loop {
        if matches!(
            next_event(&mut harness.events).await,
            Event::SimulatorConnected { region_handle: Some(handle), endpoint, .. }
                if handle == other_region && endpoint == other_endpoint
        ) {
            break;
        }
    }

    let old_id = harness.neighbor_id;
    let old_region = 0x1000_0000_0000_1000;
    announce_neighbor(&harness.primary, harness.primary_peer, other_endpoint, old_region, 51).await;
    timeout(WAIT, async {
        loop {
            if matches!(
                next_event(&mut harness.events).await,
                Event::SimulatorConnectFailed { region_handle, endpoint, .. }
                    if region_handle == old_region && endpoint == other_endpoint
            ) {
                break;
            }
        }
    })
    .await
    .expect("duplicate endpoint replacement should be rejected");

    send_targeted(&harness.commands, old_id, object_request()).await.expect("the existing route should remain active");
    let (_, sent_peer) =
        receive_message_with_peer(&harness.neighbor, |message| matches!(message, Message::RequestMultipleObjects(_))).await;
    assert_eq!(sent_peer, harness.neighbor_peer);
    harness.shutdown().await;
}
