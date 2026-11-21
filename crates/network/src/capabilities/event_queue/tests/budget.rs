use std::{assert_matches, future::poll_fn, net::TcpStream, task::Poll};

use super::*;
use crate::{capabilities::budget::EVENT_QUEUE_REQUEST_TIMEOUT, fixtures::event_queue::EventQueueBudgetHarness};

async fn held_body(server: &EventQueueBudgetTestServer, client: &http::Client, body_bytes: usize) -> (http::IncomingResponse, TcpStream) {
    let request = {
        let client = client.clone();
        let url = server.url("/event-queue");
        spawn(async move { client.post_headers(&url, "application/llsd+xml", None, Vec::new(), 1024).await })
    };
    let mut stream = server.next_poll();
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {body_bytes}\r\nRetry-After: 7\r\nConnection: close\r\n\r\n")
        .expect("send response headers");
    let incoming = request.await.expect("request task").expect("parsed response headers");
    (incoming, stream)
}

#[tokio::test(flavor = "multi_thread")]
async fn body_lanes_preserve_primary() {
    let server = EventQueueBudgetTestServer::start();
    let client = http::Client::new(Some(EVENT_QUEUE_REQUEST_TIMEOUT), false, None).expect("HTTP client");
    let budget = CapabilityBudget::new();
    let body = b"<llsd><map><key>id</key><integer>5</integer><key>events</key><array/></map></llsd>";
    let (first, mut first_stream) = held_body(&server, &client, body.len()).await;
    let (second, second_stream) = held_body(&server, &client, body.len()).await;
    let (waiting, mut waiting_stream) = held_body(&server, &client, body.len()).await;
    let (primary, mut primary_stream) = held_body(&server, &client, body.len()).await;

    let mut first = Box::pin(read_admitted_body(first, Arc::clone(&budget.neighbor_event_bodies)));
    let mut second = Box::pin(read_admitted_body(second, Arc::clone(&budget.neighbor_event_bodies)));
    let mut waiting = Box::pin(read_admitted_body(waiting, Arc::clone(&budget.neighbor_event_bodies)));
    let mut primary = Box::pin(read_admitted_body(primary, Arc::clone(&budget.primary_event_bodies)));
    assert_matches!(
        poll_fn(|cx| Poll::Ready((
            first.as_mut().poll(cx),
            second.as_mut().poll(cx),
            waiting.as_mut().poll(cx),
            primary.as_mut().poll(cx)
        )))
        .await,
        (Poll::Pending, Poll::Pending, Poll::Pending, Poll::Pending)
    );
    assert_eq!(budget.neighbor_event_bodies.available_permits(), 0);
    assert_eq!(budget.primary_event_bodies.available_permits(), 0, "neighbors leave the primary lane available");

    first_stream.write_all(body).expect("release first neighbor body");
    let (response, body_permit) = first.await.expect("first neighbor body completes");
    assert_matches!(poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx))).await, Poll::Pending);
    let role_permit = Arc::clone(&budget.neighbor_event_queues).acquire_owned().await.expect("neighbor poll admission");
    let (id, events, role_permit) =
        decode_admitted_event_batch(response.body, role_permit, body_permit).await.expect("decode admitted body");
    assert_eq!(id, 5);
    assert!(events.is_empty());
    drop(role_permit);
    assert_matches!(poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx))).await, Poll::Pending);
    assert_eq!(budget.neighbor_event_bodies.available_permits(), 0, "the waiting body takes released decoder admission");
    waiting_stream.write_all(body).expect("release waiting body");
    let (response, permit) = waiting.await.expect("waiting body completes");
    assert_eq!(response.body, body);
    drop(permit);

    primary_stream.write_all(body).expect("release primary body");
    let (response, permit) = primary.await.expect("primary completes independently of neighbors");
    assert_eq!(response.body, body);
    drop(permit);
    drop(second);
    let permit = Arc::clone(&budget.neighbor_event_bodies).acquire_many_owned(2).await.expect("cancellation releases both neighbor lanes");
    drop(permit);
    drop((first_stream, second_stream, waiting_stream, primary_stream));
    server.stop();
    assert_eq!(server.finish(), 0);
}

#[tokio::test]
async fn body_admission_keeps_deadline() {
    let server = LocalServer::start(vec![b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nRetry-After: 7\r\nConnection: close\r\n\r\n".to_vec()]);
    let client = http::Client::new(Some(EVENT_QUEUE_REQUEST_TIMEOUT), false, None).expect("HTTP client");
    let incoming = client
        .post_headers(&server.url("/event-queue"), "application/llsd+xml", None, Vec::new(), 1024)
        .await
        .expect("parsed response headers");
    let budget = Arc::new(Semaphore::new(1));
    let held = Arc::clone(&budget).acquire_owned().await.expect("occupy body admission");
    let mut response = Box::pin(read_admitted_body(incoming, Arc::clone(&budget)));
    assert_matches!(poll_fn(|cx| Poll::Ready(response.as_mut().poll(cx))).await, Poll::Pending);
    time::pause();
    time::advance(EVENT_QUEUE_REQUEST_TIMEOUT).await;
    let error = response.await.expect_err("admission remains within the original request deadline");
    assert_eq!(error.kind, http::ErrorKind::Timeout);
    assert_eq!(error.status, Some(StatusCode::OK));
    assert_eq!(error.retry_after, Some(Duration::from_secs(7)));
    drop(held);
    let permit = budget.acquire().await.expect("timed-out admission leaves no lease behind");
    drop(permit);
    assert_eq!(server.finish().len(), 1);
}

#[tokio::test]
async fn broken_body_releases_admission() {
    let server = LocalServer::start(vec![
        b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nshort".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_vec(),
    ]);
    let client = http::Client::new(Some(EVENT_QUEUE_REQUEST_TIMEOUT), false, None).expect("HTTP client");
    let budget = Arc::new(Semaphore::new(1));
    let error = post_event_queue(&client, &server.url("/event-queue"), &Value::Undefined, 1024, Arc::clone(&budget))
        .await
        .expect_err("truncated response body fails");
    assert_eq!(error.kind, http::ErrorKind::Transport);
    assert_eq!(error.status, Some(StatusCode::OK));
    let (response, permit) = post_event_queue(&client, &server.url("/event-queue"), &Value::Undefined, 1024, budget)
        .await
        .expect("failed read releases admission for the next body");
    assert_eq!(response.body, b"ok");
    drop(permit);
    assert_eq!(server.finish().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_budget_bounds_event_polls() {
    let task_count = MAX_CONCURRENT_EVENT_QUEUES + 1;
    let active_server = EventQueueBudgetTestServer::start();
    let waiting_server = EventQueueBudgetTestServer::start();
    let harness = EventQueueBudgetHarness::new(1);
    let clients = (0..2).map(|_| harness.client(active_server.url("/seed"))).collect::<Vec<_>>();
    let events = harness.events.clone();
    let waiting_client = harness.client(waiting_server.url("/seed"));
    events.try_send(EventEnvelope::new(Event::SendFailed("hold event forwarding".into()))).expect("fill the application event channel");
    let shutdown = harness.shutdown.clone();
    let mut tasks = Vec::with_capacity(task_count);

    for index in 0..MAX_CONCURRENT_EVENT_QUEUES {
        let client = clients[index % clients.len()].clone();
        let task = if index == 0 {
            harness.start_with_role(&client, test_simulator_id(index as u64 + 1), test_source(), CoverageLossLedger::new(), true)
        } else {
            harness.start(&client, test_simulator_id(index as u64 + 1), test_source(), CoverageLossLedger::new())
        };
        tasks.push(task);
    }
    let mut received_events = EventReceiver::new(harness.received_events);

    let mut polls = (0..MAX_CONCURRENT_EVENT_QUEUES).map(|_| active_server.next_poll()).collect::<Vec<_>>();
    for _ in 0..MAX_CONCURRENT_EVENT_QUEUES {
        active_server.next_seed();
    }
    let waiting_task = waiting_client
        .start_event_queue(
            test_simulator_id(task_count as u64),
            test_source(),
            harness.events.clone(),
            harness.shutdown_requested.clone(),
            CoverageLossLedger::new(),
        )
        .expect("start waiting event queue");
    waiting_server.next_seed();
    assert_eq!(available_event_queue_permits(&harness.budget), 0, "all event queue permits are held by active polls");

    waiting_client.close_session();
    let waiting_exit = timeout(CLIENT_TEST_TIMEOUT, waiting_task.wait()).await;

    let mut primary_waiter = Box::pin(Arc::clone(&harness.budget.primary_event_queues).acquire_owned());
    let mut neighbor_waiter = Box::pin(Arc::clone(&harness.budget.neighbor_event_queues).acquire_owned());
    assert_matches!(
        poll_fn(|context| Poll::Ready((primary_waiter.as_mut().poll(context), neighbor_waiter.as_mut().poll(context)))).await,
        (Poll::Pending, Poll::Pending)
    );

    assert_eq!(harness.budget.queued_event_bytes.available_permits(), 0, "active polls reserved the complete batch budget");
    write_response(polls.pop().expect("one event poll to release"), event_batch_response(5, &["First", "Second"]));
    let decoded_batch = timeout(CLIENT_TEST_TIMEOUT, harness.budget.queued_event_bytes.acquire())
        .await
        .expect("decoded batch releases unused byte admission")
        .expect("batch budget remains open");
    assert_matches!(
        poll_fn(|context| Poll::Ready((primary_waiter.as_mut().poll(context), neighbor_waiter.as_mut().poll(context)))).await,
        (Poll::Pending, Poll::Pending),
        "a decoded batch holds its poll slot while forwarding is blocked"
    );
    drop(decoded_batch);
    assert_matches!(
        timeout(CLIENT_TEST_TIMEOUT, received_events.recv()).await.expect("held event channel item").expect("event channel remains open"),
        Event::SendFailed(message) if message == "hold event forwarding"
    );
    let batch_event = timeout(CLIENT_TEST_TIMEOUT, received_events.recv())
        .await
        .expect("complete decoded batch should be forwarded")
        .expect("event channel remains open");
    let forwarded_batch = matches!(batch_event, Event::CapabilityEventBatch { events, .. }
        if events.iter().map(|event| event.message.as_str()).collect::<Vec<_>>() == ["First", "Second"]);
    let released = timeout(CLIENT_TEST_TIMEOUT, async {
        tokio::select! {
            permit = &mut primary_waiter => permit,
            permit = &mut neighbor_waiter => permit,
        }
    })
    .await
    .expect("forwarding releases the poll slot")
    .expect("poll budget remains open");
    drop((primary_waiter, neighbor_waiter, released));
    polls.push(active_server.next_poll());

    shutdown.send_replace(true);
    drop(polls);

    timeout(CLIENT_TEST_TIMEOUT, async {
        for task in tasks {
            assert_eq!(task.wait().await, EventQueueExit::Stopped);
        }
    })
    .await
    .expect("all event queue tasks should stop");
    active_server.stop();
    waiting_server.stop();
    assert_eq!(active_server.finish(), 0, "all active polls were observed");
    assert_eq!(waiting_server.finish(), 0, "a queue waiting for admission never polled before it closed");

    assert_eq!(waiting_exit.expect("session close should cancel permit acquisition"), EventQueueExit::Stopped);
    assert!(forwarded_batch, "the complete decoded batch is forwarded before the active poll starts again");
}

#[tokio::test(flavor = "multi_thread")]
async fn ordinary_requests_run_during_polls() {
    let event_server = EventQueueTestServer::start(|base| {
        let mut event_url = base.clone();
        event_url.set_path("/event-queue");
        vec![
            EventServerResponse::Response(seed_grant(&event_url)),
            EventServerResponse::Response(event_batch_response(5, &["First", "Second"])),
            hold_response(),
        ]
    });
    let ordinary_body = "ordinary response";
    let ordinary_server = LocalServer::start(vec![response(200, "OK", "application/llsd+xml", None, ordinary_body)]);
    let budget = CapabilityBudget::new();
    let ordinary_request_capacity = budget.ordinary_requests.available_permits();
    let event_queue_capacity = available_event_queue_permits(&budget);
    let queued_event_capacity = budget.queued_event_bytes.available_permits();
    let client = Client::new_with_budget(event_server.url("/seed"), budget.clone()).expect("active Tokio runtime");
    let (event_sender, mut received_events) = mpsc::channel(CLIENT_EVENT_CAPACITY);
    let (shutdown, shutdown_requested) = watch::channel(false);
    let task = client
        .start_event_queue(test_simulator_id(1), test_source(), event_sender, shutdown_requested, CoverageLossLedger::new())
        .expect("start EventQueue");

    let seed_request = event_server.next_request();
    let first_poll = event_server.next_request();
    assert!(request_line(&seed_request).starts_with("POST /seed HTTP/1.1"));
    assert!(request_line(&first_poll).starts_with("POST /event-queue HTTP/1.1"));
    assert_eq!(map_field(&decode_request_body(&first_poll), "ack"), Some(&Value::Undefined));

    let batch = timeout(CLIENT_TEST_TIMEOUT, received_events.recv())
        .await
        .expect("EventQueue response should be decoded")
        .expect("event channel remains open");
    assert_matches!(&batch.event, Event::CapabilityEventBatch { events, .. } if events.len() == 2);
    assert!(budget.queued_event_bytes.available_permits() < queued_event_capacity, "decoded batch must retain its queue reservation");
    let second_poll = event_server.next_request();
    assert_eq!(map_field(&decode_request_body(&second_poll), "ack"), Some(&Value::Integer(5)));
    assert_eq!(available_event_queue_permits(&budget), event_queue_capacity - 1, "long poll holds its concurrency slot");

    let ordinary_url = CapabilityUrl::parse(ordinary_server.url("/ordinary").as_str()).expect("valid ordinary capability URL");
    let ordinary_client = client.clone();
    let ordinary_task =
        spawn(async move { ordinary_client.get_bytes_with_response_limit(&ordinary_url, MAX_CAPABILITY_RESPONSE_BYTES).await });
    assert_eq!(
        timeout(CLIENT_TEST_TIMEOUT, ordinary_task)
            .await
            .expect("ordinary decode should finish")
            .expect("ordinary request task")
            .expect("ordinary response should complete"),
        ordinary_body.as_bytes()
    );
    assert_eq!(available_event_queue_permits(&budget), event_queue_capacity - 1);
    assert_eq!(ordinary_server.finish().len(), 1);

    shutdown.send_replace(true);
    assert_eq!(timeout(CLIENT_TEST_TIMEOUT, task.wait()).await.expect("EventQueue should stop"), EventQueueExit::Stopped);
    assert!(
        budget.queued_event_bytes.available_permits() < queued_event_capacity,
        "the queued batch keeps its retained-event reservation until the envelope is dropped"
    );
    drop(batch);
    assert_eq!(event_server.finish().len(), 0, "the seed and both polls were already observed");
    assert_eq!(budget.ordinary_requests.available_permits(), ordinary_request_capacity);
    assert_eq!(available_event_queue_permits(&budget), event_queue_capacity);
    assert_eq!(budget.queued_event_bytes.available_permits(), queued_event_capacity);

    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn waiting_queue_gets_freed_slot() {
    let active_server = EventQueueBudgetTestServer::start();
    let waiting_server = EventQueueBudgetTestServer::start();
    let primary_server = EventQueueBudgetTestServer::start();
    let budget = CapabilityBudget::new();
    let active_client = Client::new_with_budget(active_server.url("/seed"), budget.clone()).expect("active Tokio runtime");
    let waiting_client = Client::new_with_budget(waiting_server.url("/seed"), budget.clone()).expect("active Tokio runtime");
    let primary_client = Client::new_with_budget(primary_server.url("/seed"), budget.clone()).expect("active Tokio runtime");
    let (events, _received_events) = mpsc::channel(CLIENT_EVENT_CAPACITY);
    let (shutdown, shutdown_requested) = watch::channel(false);
    let mut active_tasks = Vec::with_capacity(MAX_CONCURRENT_EVENT_QUEUES - 1);

    for index in 0..MAX_CONCURRENT_EVENT_QUEUES - 1 {
        active_tasks.push(
            active_client
                .start_event_queue(
                    test_simulator_id(index as u64 + 1),
                    test_source(),
                    events.clone(),
                    shutdown_requested.clone(),
                    CoverageLossLedger::new(),
                )
                .expect("start admitted EventQueue"),
        );
    }
    for _ in 0..MAX_CONCURRENT_EVENT_QUEUES - 1 {
        active_server.next_seed();
    }
    let mut active_polls = (0..MAX_CONCURRENT_EVENT_QUEUES - 1).map(|_| active_server.next_poll()).collect::<Vec<_>>();

    let mut next_neighbor_slot = Box::pin(Arc::clone(&budget.neighbor_event_queues).acquire_owned());
    assert!(poll_fn(|context| Poll::Ready(next_neighbor_slot.as_mut().poll(context).is_pending())).await);

    let waiting_task = waiting_client
        .start_event_queue(
            test_simulator_id(MAX_CONCURRENT_EVENT_QUEUES as u64),
            test_source(),
            events.clone(),
            shutdown_requested.clone(),
            CoverageLossLedger::new(),
        )
        .expect("start waiting EventQueue");
    waiting_server.next_seed();

    let primary_task = primary_client
        .start_primary_event_queue(
            test_simulator_id(MAX_CONCURRENT_EVENT_QUEUES as u64 + 1),
            test_source(),
            events,
            shutdown_requested.clone(),
            CoverageLossLedger::new(),
        )
        .expect("start primary EventQueue");
    primary_server.next_seed();
    let primary_poll = primary_server.next_poll();
    let mut next_primary_slot = Box::pin(Arc::clone(&budget.primary_event_queues).acquire_owned());
    assert_matches!(
        poll_fn(|context| Poll::Ready((next_primary_slot.as_mut().poll(context), next_neighbor_slot.as_mut().poll(context)))).await,
        (Poll::Pending, Poll::Pending),
        "active polls retain both role budgets"
    );

    active_polls.remove(0).write_all(&response(404, "Not Found", "text/plain", None, "stopped")).expect("stop one active EventQueue poll");
    let released = timeout(CLIENT_TEST_TIMEOUT, next_neighbor_slot)
        .await
        .expect("stopped poll releases its neighbor slot")
        .expect("neighbor budget remains open");
    drop((released, next_primary_slot));
    let waiting_poll = waiting_server.next_poll();

    shutdown.send_replace(true);
    drop((active_polls, waiting_poll, primary_poll));
    for task in active_tasks {
        assert_ne!(task.wait().await, EventQueueExit::TaskFailed, "active queue should stop without panicking");
    }
    assert_eq!(waiting_task.wait().await, EventQueueExit::Stopped);
    assert_eq!(primary_task.wait().await, EventQueueExit::Stopped);
    active_server.stop();
    waiting_server.stop();
    primary_server.stop();
    assert_eq!(active_server.finish(), 0, "all active poll requests were observed");
    assert_eq!(waiting_server.finish(), 0, "the waiting poll was observed");
    assert_eq!(primary_server.finish(), 0, "the primary poll was observed");
}

#[tokio::test(flavor = "multi_thread")]
async fn primary_slot_follows_role() {
    let neighbor_server = EventQueueBudgetTestServer::start();
    let old_primary_server = EventQueueBudgetTestServer::start();
    let new_primary_server = EventQueueBudgetTestServer::start();
    let budget = CapabilityBudget::new();
    let neighbor_client = Client::new_with_budget(neighbor_server.url("/seed"), budget.clone()).expect("active Tokio runtime");
    let old_primary_client = Client::new_with_budget(old_primary_server.url("/seed"), budget.clone()).expect("active Tokio runtime");
    let new_primary_client = Client::new_with_budget(new_primary_server.url("/seed"), budget).expect("active Tokio runtime");
    let (events, mut received_events) = mpsc::channel(CLIENT_EVENT_CAPACITY);
    let (shutdown, shutdown_requested) = watch::channel(false);
    let old_coverage_losses = CoverageLossLedger::new();
    let mut neighbors = Vec::with_capacity(MAX_CONCURRENT_EVENT_QUEUES - 1);

    for index in 0..MAX_CONCURRENT_EVENT_QUEUES - 1 {
        neighbors.push(
            neighbor_client
                .start_event_queue(
                    test_simulator_id(index as u64 + 1),
                    test_source(),
                    events.clone(),
                    shutdown_requested.clone(),
                    CoverageLossLedger::new(),
                )
                .expect("start neighbor EventQueue"),
        );
    }
    for _ in 0..MAX_CONCURRENT_EVENT_QUEUES - 1 {
        neighbor_server.next_seed();
    }
    let neighbor_polls = (0..MAX_CONCURRENT_EVENT_QUEUES - 1).map(|_| neighbor_server.next_poll()).collect::<Vec<_>>();

    let old_primary = old_primary_client
        .start_primary_event_queue(
            test_simulator_id(MAX_CONCURRENT_EVENT_QUEUES as u64),
            test_source(),
            events.clone(),
            shutdown_requested.clone(),
            old_coverage_losses.clone(),
        )
        .expect("start old primary EventQueue");
    old_primary_server.next_seed();
    let mut old_poll = old_primary_server.next_poll();

    let partial_response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/llsd+xml\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
        TWO_EVENT_BATCH.len() + 32,
        TWO_EVENT_BATCH
    );
    old_poll.write_all(partial_response.as_bytes()).expect("send a partial event batch from the old simulator");
    old_primary.role_handle().set_primary(false);
    let new_primary = new_primary_client
        .start_event_queue(
            test_simulator_id(MAX_CONCURRENT_EVENT_QUEUES as u64 + 1),
            test_source(),
            events,
            shutdown_requested.clone(),
            CoverageLossLedger::new(),
        )
        .expect("start promoted EventQueue");
    new_primary.role_handle().set_primary(true);
    new_primary_server.next_seed();
    let new_poll =
        new_primary_server.polls.recv_timeout(Duration::from_secs(1)).expect("the promoted EventQueue acquires the released primary slot");

    new_primary.role_handle().set_primary(false);
    old_primary.role_handle().set_primary(true);
    let returned_poll =
        old_primary_server.polls.recv_timeout(Duration::from_secs(1)).expect("a re-promoted EventQueue reacquires the primary slot");
    assert!(old_coverage_losses.snapshot().losses.is_empty(), "role transitions must not record false EventQueue coverage loss");
    assert!(received_events.try_recv().is_err(), "role transitions must not emit EventQueueCoverageLost");

    shutdown.send_replace(true);
    drop((neighbor_polls, old_poll, new_poll, returned_poll));
    for task in neighbors {
        assert_ne!(task.wait().await, EventQueueExit::TaskFailed, "neighbor queue should stop without panicking");
    }
    assert_eq!(old_primary.wait().await, EventQueueExit::Stopped);
    assert_eq!(new_primary.wait().await, EventQueueExit::Stopped);
    neighbor_server.stop();
    old_primary_server.stop();
    new_primary_server.stop();
    assert_eq!(neighbor_server.finish(), 0, "all neighbor polls were observed");
    assert_eq!(old_primary_server.finish(), 0, "all former primary polls were observed");
    assert_eq!(new_primary_server.finish(), 0, "all new primary polls were observed");
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_frees_slot() {
    let mut server = ConcurrentCapabilityTestServer::start(MAX_CONCURRENT_CAPABILITY_REQUESTS + 1);
    let client = Client::new(server.url("/seed")).expect("active Tokio runtime");
    let capability = CapabilityUrl::parse(server.url("/capability").as_str()).expect("valid capability URL");
    let mut requests = (0..MAX_CONCURRENT_CAPABILITY_REQUESTS)
        .map(|_| {
            let client = client.clone();
            let capability = capability.clone();
            spawn(async move { client.post(&capability, "probe").await })
        })
        .collect::<Vec<_>>();

    for _ in 0..MAX_CONCURRENT_CAPABILITY_REQUESTS {
        server.next_request().await;
    }

    let client = client.clone();
    requests.push(spawn(async move { client.post(&capability, "probe").await }));
    let cancelled = requests.remove(0);
    cancelled.abort();
    assert!(cancelled.await.expect_err("aborted request task").is_cancelled());

    server.next_request().await;

    for request in requests {
        request.abort();
        assert!(request.await.expect_err("aborted queued request task").is_cancelled());
    }
    server.finish();
}
