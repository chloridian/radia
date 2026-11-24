mod batch;
mod budget;
mod client;
mod retained_size;
mod retry;

use std::{
    assert_matches,
    future::pending,
    io::Write,
    net::SocketAddr,
    pin::pin,
    task::{Context, Waker},
    time::Duration,
};

use llsd::Value;
use protocol::{
    ChatType,
    lludp::{Message, Packet},
    messages::{ChatFromViewer, chat::chat_from_viewer},
};
use reqwest::StatusCode;
use test_support::lludp::vectors;
use tokio::{
    net::UdpSocket,
    sync::{mpsc, watch},
    task::{spawn, yield_now},
    time::timeout,
};
use uuid::Uuid;

use super::*;
use crate::{
    AgentSession,
    Error::CapabilitySessionClosed,
    Grid,
    capabilities::{
        CapabilityBudget, CapabilityUrl, Client, EventQueueExit,
        budget::{
            MAX_CAPABILITY_RESPONSE_BYTES, MAX_CONCURRENT_CAPABILITY_REQUESTS, MAX_CONCURRENT_EVENT_QUEUES, MAX_EVENT_QUEUE_RESPONSE_BYTES,
            MAX_LLSD_XML_ELEMENTS,
        },
        event_queue::{MAX_EVENT_QUEUE_ERRORS, MAX_QUEUED_CAPABILITY_EVENT_BYTES},
    },
    circuit::lludp::Delivery,
    coverage::CoverageLossLedger,
    event::{CLIENT_EVENT_CAPACITY, Event, EventEnvelope, PRIMARY_TERMINAL_EVENT_CAPACITY},
    fixtures::{
        event_queue::{EventReceiver, decode_request_body, map_field, receive_poll_error, set_sequence, test_fixture},
        http::{
            ConcurrentCapabilityTestServer, EventQueueBudgetTestServer, EventQueueTestServer, EventServerResponse, LocalServer,
            event_batch_response, hold_response, request_line, response, seed_grant, seed_response, write_response,
        },
        session::run_session,
        simulator::{acknowledgement, receive_message, receive_udp, test_simulator_id, test_source},
    },
    http,
    session::{COMMAND_CAPACITY, Command},
};

const CLIENT_TEST_TIMEOUT: Duration = Duration::from_secs(5);

const TWO_EVENT_BATCH: &str = "<llsd><map><key>id</key><integer>5</integer><key>events</key><array><map><key>message</key><string>First</string></map><map><key>message</key><string>Second</string></map></array></map></llsd>";
const MALFORMED_BATCH: &str = "<llsd><map>";

const TEST_TIMEOUT: Duration = Duration::from_secs(2);

fn available_event_queue_permits(budget: &CapabilityBudget) -> usize {
    budget.primary_event_queues.available_permits() + budget.neighbor_event_queues.available_permits()
}

async fn close_while_pending<F>(future: F, session_closed: &watch::Sender<bool>) -> RetryOutcome
where
    F: Future<Output = RetryOutcome>,
{
    tokio::pin!(future);
    tokio::select! {
        biased;
        outcome = &mut future => return outcome,
        _ = yield_now() => {}
    }
    session_closed.send_replace(true);
    timeout(TEST_TIMEOUT, &mut future).await.expect("session close cancels retry")
}

fn seed_context(events: mpsc::Sender<EventEnvelope>) -> EventQueueContext {
    EventQueueContext {
        simulator_id: test_simulator_id(1),
        source: SocketAddr::from(([127, 0, 0, 1], 0)),
        seed: Url::parse("http://127.0.0.1:9/seed").expect("valid seed URL"),
        http: http::Client::new(None, false, None).expect("build HTTP client"),
        events,
        budget: CapabilityBudget::new(),
        coverage_losses: CoverageLossLedger::new(),
        status: watch::channel(EventQueueStatus::default()).0,
    }
}

fn pending_event_queue_loss(
    events: &mpsc::Sender<EventEnvelope>,
    simulator_id: SimulatorId,
    socket: SocketAddr,
    stop: &Stop,
) -> PendingEventQueueLoss {
    let ledger = CoverageLossLedger::new();
    let coverage_source = CoverageSource::EventQueue { incarnation: 1 };
    let notifier = EventQueueLossNotifier::new(events.clone(), simulator_id, socket, stop.clone());
    PendingEventQueueLoss::new(ledger, simulator_id, coverage_source, notifier)
}

#[tokio::test]
async fn completion_survives_cancelled_polls() {
    for exit in [Some(EventQueueExit::StoppedWithCoverageLoss), None] {
        let (completed, completion) = oneshot::channel();
        let (role, _) = EventQueueRoleHandle::channel(true);
        let mut task = EventQueueTask::new(spawn(async {}), completion, role);
        tokio::select! {
            biased;
            exit = task.completion() => panic!("completion is not ready: {exit:?}"),
            _ = async {} => {},
        }

        if let Some(exit) = exit {
            completed.send(exit).expect("completion receiver survives polling cancellation");
        } else {
            drop(completed);
        }
        let expected = exit.unwrap_or(EventQueueExit::TaskFailed);
        assert_eq!(task.completion().await, expected);
        assert_eq!(task.completion().await, expected);
        assert_eq!(task.wait().await, expected);
    }
}

#[tokio::test]
async fn cancelled_wait_aborts_worker() {
    let (worker_lifetime, worker_dropped) = oneshot::channel::<()>();
    let worker = spawn(async move {
        let _lifetime = worker_lifetime;
        pending::<()>().await;
    });
    let (completed, completion) = oneshot::channel();
    completed.send(EventQueueExit::Stopped).expect("report completion before the worker exits");
    let (role, _) = EventQueueRoleHandle::channel(true);
    let task = EventQueueTask::new(worker, completion, role);
    {
        let mut wait = pin!(task.wait());
        assert!(wait.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
    }

    assert!(timeout(TEST_TIMEOUT, worker_dropped).await.expect("cancelled wait must abort its worker").is_err());
}

#[test]
fn retry_after_hint_is_bounded() {
    assert_eq!(event_queue_retry_delay_with_hint(2, None), Duration::from_secs(7));
    assert_eq!(event_queue_retry_delay_with_hint(2, Some(Duration::from_secs(5))), Duration::from_secs(5));
    assert_eq!(event_queue_retry_delay_with_hint(2, Some(Duration::from_secs(90))), Duration::from_secs(30));
}

#[tokio::test]
async fn poll_backoff_stops_on_close() {
    let (events, _received) = mpsc::channel(1);
    let (_shutdown, shutdown) = watch::channel(false);
    let (session_closed, session_closed_receiver) = watch::channel(false);
    let mut error_count = 0;
    let simulator_id = test_simulator_id(1);
    let source = SocketAddr::from(([127, 0, 0, 1], 0));
    let stop = Stop::new(shutdown, session_closed_receiver);
    let mut pending_loss = pending_event_queue_loss(&events, simulator_id, source, &stop);

    assert_eq!(
        close_while_pending(
            retry_event_queue_with_hint(
                simulator_id,
                &mut error_count,
                source,
                &events,
                "poll failed".into(),
                None,
                &stop,
                &mut pending_loss,
            ),
            &session_closed,
        )
        .await,
        RetryOutcome::Stopped
    );
}

#[tokio::test]
async fn seed_backoff_stops_on_close() {
    let (events, _received) = mpsc::channel(1);
    let (_shutdown, shutdown_receiver) = watch::channel(false);
    let (session_closed, session_closed_receiver) = watch::channel(false);
    let mut retries = 0;
    let context = seed_context(events);
    let stop = Stop::new(shutdown_receiver, session_closed_receiver);

    assert_eq!(
        close_while_pending(
            retry_seed_capability_lookup(&context, &mut retries, "seed lookup failed".into(), Some(Duration::from_secs(30)), &stop),
            &session_closed,
        )
        .await,
        RetryOutcome::Stopped
    );
}

#[tokio::test]
async fn poll_limit_stops_on_close() {
    let (events, mut received) = mpsc::channel(1);
    events.try_send(EventEnvelope::new(Event::Failed("occupy event channel".into()))).expect("fill event channel");
    let (_shutdown, shutdown) = watch::channel(false);
    let (session_closed, session_closed_receiver) = watch::channel(false);
    let mut error_count = MAX_EVENT_QUEUE_ERRORS;
    let simulator_id = test_simulator_id(1);
    let source = SocketAddr::from(([127, 0, 0, 1], 0));
    let stop = Stop::new(shutdown, session_closed_receiver);
    let mut pending_loss = pending_event_queue_loss(&events, simulator_id, source, &stop);

    assert_eq!(
        close_while_pending(
            retry_event_queue_with_hint(
                simulator_id,
                &mut error_count,
                source,
                &events,
                "poll failed".into(),
                None,
                &stop,
                &mut pending_loss,
            ),
            &session_closed,
        )
        .await,
        RetryOutcome::Stopped
    );
    assert_matches!(received.try_recv().map(EventEnvelope::into_event), Ok(Event::Failed(_)));
}

#[tokio::test]
async fn seed_limit_stops_on_close() {
    let (events, mut received) = mpsc::channel(1);
    events.try_send(EventEnvelope::new(Event::Failed("occupy event channel".into()))).expect("fill event channel");
    let (_shutdown, shutdown_receiver) = watch::channel(false);
    let (session_closed, session_closed_receiver) = watch::channel(false);
    let mut retries = SEED_LOOKUP_RETRY_LIMIT;
    let context = seed_context(events);
    let stop = Stop::new(shutdown_receiver, session_closed_receiver);

    assert_eq!(
        close_while_pending(
            retry_seed_capability_lookup(&context, &mut retries, "seed lookup failed".into(), Some(Duration::from_secs(30)), &stop),
            &session_closed,
        )
        .await,
        RetryOutcome::Stopped
    );
    assert_matches!(received.try_recv().map(EventEnvelope::into_event), Ok(Event::Failed(_)));
}
