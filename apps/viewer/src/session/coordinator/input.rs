use std::future::Future;

use futures::{FutureExt, pin_mut};
use network::Event;
use tokio::time::Instant;

use crate::{
    inventory::InventoryFetchEvent,
    session::{environment::Completion, object_media::ObjectMediaResult, recovery::RecoveryCompletion, terrain::DecodedTerrain},
    world_reader::{ReadRequest, WorldReadHost},
};

pub(super) enum Input {
    Cancelled,
    Event(Option<Event>),
    Media(Option<ObjectMediaResult>),
    Terrain(DecodedTerrain),
    Environment(Option<Box<Completion>>),
    WorldRead(ReadRequest),
    CameraTick,
    Recovery(RecoveryCompletion),
    InventoryFetch(Option<InventoryFetchEvent>),
}

#[derive(Clone, Copy, Default)]
pub(super) enum OrdinaryInputTurn {
    #[default]
    Network,
    Inventory,
}

#[cfg(test)]
pub(super) async fn select_drain_input<E, M, T, N, C, Q, R, I>(
    reads: &mut WorldReadHost,
    event: E,
    media: M,
    terrain: T,
    environment: N,
    cancellation: C,
    camera_tick: Q,
    recovery: R,
    inventory_fetch: I,
) -> Input
where
    E: Future<Output = Option<Event>>,
    M: Future<Output = Option<ObjectMediaResult>>,
    T: Future<Output = DecodedTerrain>,
    N: Future<Output = Option<Box<Completion>>>,
    C: Future<Output = ()> + Clone,
    Q: Future<Output = Instant>,
    R: Future<Output = RecoveryCompletion>,
    I: Future<Output = Option<InventoryFetchEvent>>,
{
    let mut turn = OrdinaryInputTurn::default();
    select_drain_input_fair(reads, event, media, terrain, environment, cancellation, camera_tick, recovery, inventory_fetch, &mut turn)
        .await
}

pub(super) async fn select_drain_input_fair<E, M, T, N, C, Q, R, I>(
    reads: &mut WorldReadHost,
    event: E,
    media: M,
    terrain: T,
    environment: N,
    cancellation: C,
    camera_tick: Q,
    recovery: R,
    inventory_fetch: I,
    turn: &mut OrdinaryInputTurn,
) -> Input
where
    E: Future<Output = Option<Event>>,
    M: Future<Output = Option<ObjectMediaResult>>,
    T: Future<Output = DecodedTerrain>,
    N: Future<Output = Option<Box<Completion>>>,
    C: Future<Output = ()> + Clone,
    Q: Future<Output = Instant>,
    R: Future<Output = RecoveryCompletion>,
    I: Future<Output = Option<InventoryFetchEvent>>,
{
    let event = event.fuse();
    let media = media.fuse();
    let terrain = terrain.fuse();
    let environment = environment.fuse();
    let recovery = recovery.fuse();
    let inventory_fetch = inventory_fetch.fuse();
    if cancellation.clone().now_or_never().is_some() {
        return Input::Cancelled;
    }
    if let Some(request) = reads.take_reserved() {
        return Input::WorldRead(request);
    }
    let cancellation = cancellation.fuse();
    let camera_tick = camera_tick.fuse();
    let inventory_first = matches!(turn, OrdinaryInputTurn::Inventory);
    let input = {
        let read = reads.next_request().fuse();
        pin_mut!(event, media, terrain, environment, cancellation, camera_tick, read, recovery, inventory_fetch);
        let ordinary = async {
            if inventory_first {
                futures::select_biased! {
                    inventory_fetch = inventory_fetch => Input::InventoryFetch(inventory_fetch),
                    event = event => Input::Event(event),
                }
            } else {
                futures::select_biased! {
                    event = event => Input::Event(event),
                    inventory_fetch = inventory_fetch => Input::InventoryFetch(inventory_fetch),
                }
            }
        }
        .fuse();
        pin_mut!(ordinary);
        futures::select_biased! {
            _ = cancellation => Input::Cancelled,
            _ = camera_tick => Input::CameraTick,
            decoded = terrain => Input::Terrain(decoded),
            result = media => Input::Media(result),
            completion = environment => Input::Environment(completion),
            recovery = recovery => Input::Recovery(recovery),
            input = ordinary => input,
            request = read => Input::WorldRead(request),
        }
    };
    match &input {
        Input::Event(_) => *turn = OrdinaryInputTurn::Inventory,
        Input::InventoryFetch(_) => *turn = OrdinaryInputTurn::Network,
        _ => {}
    }
    if !matches!(&input, Input::WorldRead(_)) {
        reads.record_intake_turn();
    }
    input
}

#[cfg(test)]
mod inventory_intake_progress {
    use std::{
        future::{pending, ready},
        time,
    };

    use futures::{
        FutureExt,
        future::{BoxFuture, Shared},
    };
    use network::{Event, PacketOrder};
    use protocol::{
        lludp::Message,
        messages::region::region_handshake::{RegionHandshake, RegionInfo1},
    };
    use tokio::{
        runtime::Handle,
        sync::{mpsc, oneshot},
        time::Instant,
    };

    use super::{Input, OrdinaryInputTurn, select_drain_input_fair};
    use crate::{
        inventory::InventoryFetchEvent,
        session::{
            coordinator::{
                driver::{MAX_DEFERRED_INVENTORY_EVENTS, inventory_queue_is_full},
                events::WorldHandoff,
            },
            world::{SessionWorld, viewer_world_limits},
        },
        test_helpers::simulator_id,
        world_reader::{MAX_INTAKE_TURNS, WorldReadHost},
    };

    type Cancellation = Shared<BoxFuture<'static, ()>>;

    const ORDINARY_TURN_BOUND: usize = 2;

    fn idle_cancellation() -> Cancellation {
        pending::<()>().boxed().shared()
    }

    fn network_event() -> Event {
        Event::SimulatorConnected {
            simulator_id: simulator_id(),
            region_handle: None,
            endpoint: "127.0.0.1:13000".parse().expect("test endpoint is valid"),
        }
    }

    async fn select_inputs(
        reads: &mut WorldReadHost,
        turn: &mut OrdinaryInputTurn,
        event_ready: bool,
        inventory_ready: bool,
        cancellation: Cancellation,
    ) -> Input {
        let event: BoxFuture<'static, Option<Event>> = if event_ready { ready(Some(network_event())).boxed() } else { pending().boxed() };
        let inventory_fetch: BoxFuture<'static, Option<InventoryFetchEvent>> =
            if inventory_ready { ready(Some(InventoryFetchEvent::BackgroundDone)).boxed() } else { pending().boxed() };
        select_drain_input_fair(reads, event, pending(), pending(), pending(), cancellation, pending(), pending(), inventory_fetch, turn)
            .await
    }

    #[tokio::test]
    async fn ready_sources_alternate() {
        let world = SessionWorld::new(viewer_world_limits());
        let mut reads = WorldReadHost::new(world.world(), Handle::current());
        let mut turn = OrdinaryInputTurn::default();
        let mut selected = Vec::new();

        for _ in 0..(ORDINARY_TURN_BOUND * 4) {
            match select_inputs(&mut reads, &mut turn, true, true, idle_cancellation()).await {
                Input::Event(Some(_)) => selected.push(false),
                Input::InventoryFetch(Some(InventoryFetchEvent::BackgroundDone)) => selected.push(true),
                _ => panic!("a ready ordinary source was not selected"),
            }
        }

        for pair in selected.as_chunks::<ORDINARY_TURN_BOUND>().0 {
            assert_ne!(pair[0], pair[1], "both ready sources must be selected within two turns");
        }
    }

    #[tokio::test]
    async fn late_inventory_progresses() {
        let world = SessionWorld::new(viewer_world_limits());
        let mut reads = WorldReadHost::new(world.world(), Handle::current());
        let mut turn = OrdinaryInputTurn::default();

        for _ in 0..2 {
            let input = select_inputs(&mut reads, &mut turn, true, false, idle_cancellation()).await;
            let Input::Event(Some(_)) = input else { panic!("the only ready ordinary source must be selected") };
        }

        let mut inventory_turn = None;
        for attempt in 1..=ORDINARY_TURN_BOUND {
            if let Input::InventoryFetch(Some(InventoryFetchEvent::BackgroundDone)) =
                select_inputs(&mut reads, &mut turn, true, true, idle_cancellation()).await
            {
                inventory_turn = Some(attempt);
                break;
            }
        }

        assert!(inventory_turn.is_some(), "inventory must progress within two turns after becoming ready");
    }

    #[tokio::test]
    async fn world_progresses_with_full_inventory() {
        let mut world = SessionWorld::new(viewer_world_limits());
        let mut reads = WorldReadHost::new(world.world(), Handle::current());
        let mut handoff = WorldHandoff::default();
        let mut turn = OrdinaryInputTurn::default();
        let simulator = simulator_id();
        world.register_simulator(simulator, None).expect("register simulator region");
        let event = ready(Some(Event::SimulatorMessage {
            simulator_id: simulator,
            source: "127.0.0.1:13000".parse().expect("test endpoint"),
            sequence: 1,
            packet_order: PacketOrder::Ordered(1),
            received_at: time::Instant::now(),
            ping_delay: None,
            message: Message::RegionHandshake(RegionHandshake {
                region_info1: RegionInfo1 { sim_name: "inventory-pressure".into(), ..RegionInfo1::default() },
                region_info2: None,
                region_info3: None,
                region_info4: Vec::new(),
            }),
        }));
        let inventory_full = inventory_queue_is_full(MAX_DEFERRED_INVENTORY_EVENTS, 0);
        assert!(inventory_full, "the setup must hit the same event-count admission limit used by SessionDriver");
        let inventory_queue = async move {
            if inventory_full { pending::<Option<InventoryFetchEvent>>().await } else { Some(InventoryFetchEvent::BackgroundDone) }
        };

        let input = select_drain_input_fair(
            &mut reads,
            event,
            pending(),
            pending(),
            pending(),
            idle_cancellation(),
            pending(),
            pending(),
            inventory_queue,
            &mut turn,
        )
        .await;

        let Input::Event(Some(event @ Event::SimulatorMessage { .. })) = input else {
            panic!("a ready simulator world update must pass the full inventory gate");
        };
        let before = world.world().revision();
        handoff.apply_event(&mut world, &event, None);
        assert!(world.world().revision() > before, "the delivered world update must advance World state");
        reads.finish();
    }

    #[tokio::test]
    async fn cancellation_precedes_reserved() {
        let world = SessionWorld::new(viewer_world_limits());
        let mut reads = WorldReadHost::new(world.world(), Handle::current());
        let mut turn = OrdinaryInputTurn::default();
        let reader = reads.reader();
        let mut read = Box::pin(reader.objects(None, 1));
        assert!(futures::poll!(read.as_mut()).is_pending());
        for _ in 0..MAX_INTAKE_TURNS {
            reads.record_intake_turn();
        }
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let cancellation = async move {
            let _ = cancel_rx.await;
        }
        .boxed()
        .shared();
        cancel_tx.send(()).expect("cancellation receiver is alive");

        let input = select_inputs(&mut reads, &mut turn, true, true, cancellation).await;

        let Input::Cancelled = input else { panic!("cancellation must precede ordinary input") };
        drop(read);
        reads.finish();
    }

    #[tokio::test]
    async fn reserved_reads_precede_ordinary() {
        let world = SessionWorld::new(viewer_world_limits());
        let mut reads = WorldReadHost::new(world.world(), Handle::current());
        let mut turn = OrdinaryInputTurn::default();
        let reader = reads.reader();
        let mut read = Box::pin(reader.objects(None, 1));
        assert!(futures::poll!(read.as_mut()).is_pending());
        for _ in 0..MAX_INTAKE_TURNS {
            reads.record_intake_turn();
        }

        let input = select_inputs(&mut reads, &mut turn, true, true, idle_cancellation()).await;
        let Input::WorldRead(request) = input else { panic!("a reserved read must precede ordinary input") };
        request.serve(world.world());
        let page = read.await.expect("the reserved read must receive its response");
        assert_eq!(page.objects.len(), 0);
        reads.finish();
    }

    #[tokio::test]
    async fn urgent_completion_precedes_ordinary() {
        let world = SessionWorld::new(viewer_world_limits());
        let mut reads = WorldReadHost::new(world.world(), Handle::current());
        let mut turn = OrdinaryInputTurn::default();

        let input = select_drain_input_fair(
            &mut reads,
            ready(Some(network_event())),
            pending(),
            pending(),
            pending(),
            idle_cancellation(),
            ready(Instant::now()),
            pending(),
            ready(Some(InventoryFetchEvent::BackgroundDone)),
            &mut turn,
        )
        .await;

        let Input::CameraTick = input else { panic!("a ready urgent completion must precede ordinary input") };
    }

    #[tokio::test]
    async fn sources_run_alone() {
        let world = SessionWorld::new(viewer_world_limits());
        let mut reads = WorldReadHost::new(world.world(), Handle::current());
        let mut turn = OrdinaryInputTurn::default();

        for _ in 0..4 {
            let input = select_inputs(&mut reads, &mut turn, true, false, idle_cancellation()).await;
            let Input::Event(Some(_)) = input else { panic!("network runs when it is the only ready source") };
        }
        for _ in 0..4 {
            let input = select_inputs(&mut reads, &mut turn, false, true, idle_cancellation()).await;
            let Input::InventoryFetch(Some(InventoryFetchEvent::BackgroundDone)) = input else {
                panic!("inventory runs when it is the only ready source")
            };
        }
    }

    #[tokio::test]
    async fn closed_stream_does_not_starve() {
        let world = SessionWorld::new(viewer_world_limits());
        let mut reads = WorldReadHost::new(world.world(), Handle::current());
        let mut turn = OrdinaryInputTurn::default();
        let (sender, mut inventory_events) = mpsc::channel::<InventoryFetchEvent>(1);
        drop(sender);

        let input = select_drain_input_fair(
            &mut reads,
            ready(Some(network_event())),
            pending(),
            pending(),
            pending(),
            idle_cancellation(),
            pending(),
            pending(),
            inventory_events.recv(),
            &mut turn,
        )
        .await;
        let Input::Event(Some(_)) = input else { panic!("the network source must run before its next preference") };

        let input = select_drain_input_fair(
            &mut reads,
            ready(Some(network_event())),
            pending(),
            pending(),
            pending(),
            idle_cancellation(),
            pending(),
            pending(),
            inventory_events.recv(),
            &mut turn,
        )
        .await;
        let Input::InventoryFetch(None) = input else { panic!("a closed inventory channel must use the wrapper's None outcome") };

        let input = select_drain_input_fair(
            &mut reads,
            ready(Some(network_event())),
            pending(),
            pending(),
            pending(),
            idle_cancellation(),
            pending(),
            pending(),
            inventory_events.recv(),
            &mut turn,
        )
        .await;
        let Input::Event(Some(_)) = input else { panic!("a closed inventory channel must not starve network input") };
    }
}
