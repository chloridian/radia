use std::assert_matches;

use tokio::time;

use super::*;
use crate::{
    circuit::admission::SESSION_APPLICATION_EVENT_CREDIT_CAPACITY,
    coverage::{CoverageLoss, CoverageLossReason, CoverageSource},
    fixtures::event_with_coverage_watermark,
};

#[tokio::test]
async fn primary_queue_exit_keeps_session() {
    let simulator = UdpSocket::bind("127.0.0.1:0").await.expect("bind primary simulator");
    let endpoint = simulator.local_addr().expect("read simulator address");
    let connection = connect_runtime(login_session(endpoint), ResponseData::default()).await.expect("connect simulator circuit");
    let _ = receive(&simulator).await;

    let mut buffer = [0; 65_535];
    for exit in [EventQueueExit::Stopped, EventQueueExit::RetryLimitReached, EventQueueExit::StoppedWithCoverageLoss] {
        connection.state.note_primary_event_queue_exit(connection.state.primary_id, exit);
        assert_eq!(
            simulator.try_recv_from(&mut buffer).expect_err("EventQueue exit must not log out the simulator").kind(),
            ErrorKind::WouldBlock,
            "{exit:?}"
        );
    }
}

#[test]
fn former_primary_failure_keeps_identity() {
    let simulator_id = SimulatorId::allocate().expect("allocate former primary ID");
    let endpoint = "127.0.0.1:9000".parse().expect("parse endpoint");
    let event = neighbor_completion_event(
        simulator_id,
        None,
        endpoint,
        &SimulatorTaskOutcome::Finished(Err(ConnectionError::ShutdownReason("retry limit reached"))),
    );
    assert_matches!(
        event,
        Event::SimulatorFailed { simulator_id: failed_id, region_handle: None, endpoint: failed_endpoint, reason }
            if failed_id == simulator_id && failed_endpoint == endpoint && reason == "retry limit reached"
    );
}

#[tokio::test]
async fn credit_exhaustion_reports_coverage() {
    let simulator = UdpSocket::bind("127.0.0.1:0").await.expect("bind primary simulator");
    let endpoint = simulator.local_addr().expect("read simulator address");
    let coverage_losses = CoverageLossLedger::new();
    let mut connection = connect_with_coverage(login_session(endpoint), ResponseData::default(), coverage_losses.clone())
        .await
        .expect("connect simulator circuit");
    receive(&simulator).await;

    let (events, mut received_events) = mpsc::channel(2);
    let mut run_state = RunState::new();
    for index in 0..SESSION_APPLICATION_EVENT_CREDIT_CAPACITY {
        let permit =
            connection.state.packet_memory.application_events().try_acquire().expect("test backlog fits the session application capacity");
        let mut event = EventEnvelope::new(Event::CapabilityEvent {
            simulator_id: connection.state.primary_id,
            source: endpoint,
            message: "pending event".into(),
            body: Value::Integer(i32::try_from(index).expect("test event index fits")),
        });
        event.attach_application_credit(permit);
        run_state.pending_events.push_back(event);
    }
    let simulator_id = connection.state.primary_id;
    let source = CoverageSource::EventQueue { incarnation: 42 };
    let watermark = coverage_losses.watermark(simulator_id, source);

    connection
        .state
        .publish_event(
            &events,
            &mut run_state,
            event_with_coverage_watermark(
                Event::CapabilityEvent { simulator_id, source: endpoint, message: "CrossedRegion".into(), body: Value::Undefined },
                watermark,
            ),
        )
        .await
        .expect("a full event queue should report coverage loss without ending the session");

    assert!(received_events.try_recv().is_err(), "the full queue must not admit an extra event");
    assert!(connection.state.logout.deadline().is_none(), "queue pressure must not start simulator logout");
    assert_matches!(
        coverage_losses.snapshot().losses.as_slice(),
        [CoverageLoss::Source(record)]
            if record.watermark.simulator_id == simulator_id
                && record.watermark.source == source
                && record.reason == CoverageLossReason::EventAdmission
    );

    run_state.pending_events.clear();
    connection
        .state
        .publish_event(&events, &mut run_state, Event::SimulatorDisconnected { simulator_id, region_handle: None, endpoint })
        .await
        .expect("events should flow again after the consumer drains the backlog");
    assert_matches!(received_events.try_recv().map(|event| event.event), Ok(Event::SimulatorDisconnected { .. }));
    assert!(connection.state.logout.deadline().is_none());
}

#[tokio::test]
async fn terminal_logout_skips_neighbor_events() {
    let simulator = UdpSocket::bind("127.0.0.1:0").await.expect("bind primary simulator");
    let endpoint = simulator.local_addr().expect("read simulator address");
    let mut connection = connect_runtime(login_session(endpoint), ResponseData::default()).await.expect("connect primary simulator");
    receive(&simulator).await;

    let mut run_state = RunState::new();
    let (neighbor_requests, _neighbor_request_rx) = mpsc::channel(1);
    let (application_events, _application_event_rx) = mpsc::channel(1);
    let (neighbor_events, _neighbor_event_rx) = mpsc::channel(1);
    let (_shutdown, shutdown) = watch::channel(false);
    connection.state.logout = SessionLogout::Received { deadline: Some(time::Instant::now() + LOGOUT_TIMEOUT) };
    let (capability_events, _capability_event_rx) = mpsc::channel(1);
    let mut setup =
        PromotionSetup { state: &mut run_state, neighbor_requests, application_events, neighbor_events, shutdown, capability_events };
    let body = Value::Map(Map::from([(
        "SimulatorInfo".to_owned(),
        Value::Array(vec![Value::Map(Map::from([
            ("Handle".to_owned(), Value::Binary(0x1000_u64.to_be_bytes().to_vec().into())),
            ("IP".to_owned(), Value::Binary(vec![127, 0, 0, 1].into())),
            ("Port".to_owned(), Value::Integer(13_000)),
        ]))]),
    )]));
    let mut handoff_state = HandoffState::default();
    let simulator_id = connection.state.primary_id;

    connection
        .state
        .process_capability_event(simulator_id, "EnableSimulator", &body, None, &mut setup, &mut handoff_state)
        .await
        .expect("terminal capability events are discarded without starting work");

    drop(setup);
    assert!(run_state.connections.is_empty());
    assert_eq!(connection.state.simulators.len(), 1);
    drop(connection);
}
