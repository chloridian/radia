use std::{assert_matches, collections::HashMap, io, net::SocketAddr, sync::Arc};

use futures::{FutureExt, future};
use inventory::{
    AssetType, Category, CategoryKey, FolderType, Inventory, InventoryDelta, InventoryLimits, InventoryNodeId, InventoryOwner,
    InventorySnapshot, InventoryType, Item, ItemKey, PermissionMask, Permissions, SaleInfo, SaleType, ServerVersion,
};
use llsd::{Map, Value, xml};
use network::capabilities::Client;
use protocol::lludp::{Message, Packet};
use tokio::{
    net::UdpSocket,
    runtime::Handle,
    sync::watch,
    time::{self, timeout},
};
use utils::{AssetId, AvatarId, FolderId, GroupId, ItemId};
use uuid::Uuid;

use super::{
    controller::{EditSession, InventoryMove, MoveTransport},
    *,
};
use crate::{
    inventory::test_support::{LoopbackPeer, TEST_TIMEOUT},
    session,
};

struct UdpMovePeer {
    sender: UdpSocket,
    destination: SocketAddr,
}

impl MoveTransport for UdpMovePeer {
    fn send(&self, movement: InventoryMove) -> Result<(), network::Error> {
        let message = match movement {
            InventoryMove::Item(message) => Message::MoveInventoryItem(message),
            InventoryMove::Folder(message) => Message::MoveInventoryFolder(message),
        };
        let bytes = Packet::new(1, message).with_reliable().encode().map_err(|error| network::Error::SimulatorSend(error.to_string()))?;
        self.sender.try_send_to(&bytes, self.destination).map(|_| ()).map_err(|error| network::Error::SimulatorSend(error.to_string()))
    }
}

fn key(owner: InventoryOwner, id: u128) -> CategoryKey {
    CategoryKey::new(owner, FolderId::new(Uuid::from_u128(id)))
}

fn category(key: CategoryKey, parent: Option<CategoryKey>, folder_type: FolderType, name: &str) -> Category {
    Category {
        key,
        parent,
        preferred_folder_type: folder_type,
        name: Arc::from(name),
        server_version: ServerVersion::UNKNOWN,
        server_descendent_count: None,
    }
}

fn item(owner: InventoryOwner, parent: CategoryKey, id: u128, name: &str) -> Item {
    Item {
        key: ItemKey::new(owner, ItemId::new(Uuid::from_u128(id))),
        parent,
        asset_id: AssetId::new(Uuid::nil()),
        asset_type: AssetType::Notecard,
        inventory_type: InventoryType::Notecard,
        flags: 0,
        name: Arc::from(name),
        description: Arc::from(""),
        created_at: 0,
        permissions: Permissions {
            creator: AvatarId::new(Uuid::nil()),
            owner: AvatarId::new(owner.id()),
            group: GroupId::new(Uuid::nil()),
            last_owner: AvatarId::new(Uuid::nil()),
            base: PermissionMask::new(0),
            owner_mask: PermissionMask::new(0),
            group_mask: PermissionMask::new(0),
            everyone: PermissionMask::new(0),
            next_owner: PermissionMask::new(0),
        },
        sale: SaleInfo { sale_type: SaleType::NotForSale, price: 0 },
    }
}

fn inventory_fixture()
-> (Inventory, InventoryOwner, CategoryKey, CategoryKey, CategoryKey, CategoryKey, CategoryKey, ItemKey, ItemKey, CategoryKey) {
    let owner = InventoryOwner::new(Uuid::from_u128(1));
    let root = key(owner, 2);
    let first = key(owner, 3);
    let second = key(owner, 4);
    let trash = key(owner, 5);
    let protected = key(owner, 6);
    let trash_child = key(owner, 7);
    let item_key = ItemKey::new(owner, ItemId::new(Uuid::from_u128(8)));
    let trash_item_key = ItemKey::new(owner, ItemId::new(Uuid::from_u128(9)));
    let mut inventory = Inventory::new(category(root, None, FolderType::RootInventory, "root"), None, InventoryLimits::default())
        .expect("valid personal root");
    for category in [
        category(first, Some(root), FolderType::None, "first"),
        category(second, Some(root), FolderType::None, "second"),
        category(trash, Some(root), FolderType::Trash, "trash"),
        category(protected, Some(root), FolderType::Unknown(900), "protected"),
        category(trash_child, Some(trash), FolderType::None, "trash child"),
    ] {
        inventory.upsert_category(category).expect("add fixture category");
    }
    inventory.upsert_item(item(owner, first, 8, "item")).expect("add fixture item");
    inventory.upsert_item(item(owner, trash, 9, "trash item")).expect("add Trash item");
    (inventory, owner, root, first, second, trash, protected, item_key, trash_item_key, trash_child)
}

fn start_controller(
    peer: &LoopbackPeer,
    inventory: &Inventory,
    session: EditSession,
    transport: Arc<dyn MoveTransport>,
) -> (InventoryEditController, watch::Sender<Option<InventorySnapshot>>) {
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let (model_sender, model) = watch::channel(Some(inventory.snapshot()));
    let mut controller = InventoryEditController::new();
    controller.start_with_transport(&Handle::current(), client, session, transport, model, future::pending().boxed().shared());
    (controller, model_sender)
}

async fn apply_confirmation(
    controller: &mut InventoryEditController,
    inventory: &mut Inventory,
    model: &watch::Sender<Option<InventorySnapshot>>,
) {
    let Some(InventoryEditEvent::Apply { confirmation, applied }) = controller.next_event().await else {
        panic!("expected AIS confirmation event");
    };
    let current = inventory.snapshot();
    let result = inventory.apply_delta(confirmation.into_delta(&current)).is_ok();
    model.send_replace(Some(inventory.snapshot()));
    applied.send(result).expect("worker waits for confirmation result");
}

async fn confirm_udp_move(
    controller: &mut InventoryEditController,
    socket: &UdpSocket,
    inventory: &mut Inventory,
    model: &watch::Sender<Option<InventorySnapshot>>,
    owner: InventoryOwner,
    session_id: Uuid,
) {
    let Some(InventoryEditEvent::MoveStarted { expectation, confirmed, registered }) = controller.next_event().await else {
        panic!("expected UDP move registration event");
    };
    let mut pending = HashMap::from([(expectation.node, (expectation, confirmed))]);
    registered.send(()).expect("move worker waits for registration");
    let mut bytes = [0; 4096];
    let received = timeout(TEST_TIMEOUT, socket.recv_from(&mut bytes)).await.expect("outbound move packet timeout");
    let (size, _) = received.expect("read outbound move packet");
    let packet = Packet::decode(&bytes[..size]).expect("decode outbound move packet");
    assert!(packet.is_reliable());
    let message = packet.into_message();
    match (expectation.node.id(), &message) {
        (InventoryNodeId::Item(id), Message::MoveInventoryItem(message)) => {
            assert_eq!(message.agent_id, owner.id());
            assert_eq!(message.session_id, session_id);
            assert_eq!(message.inventory_data[0].item_id, id.uuid());
            assert_eq!(message.inventory_data[0].folder_id, expectation.to.id().uuid());
        }
        (InventoryNodeId::Category(id), Message::MoveInventoryFolder(message)) => {
            assert_eq!(message.agent_id, owner.id());
            assert_eq!(message.session_id, session_id);
            assert_eq!(message.inventory_data[0].folder_id, id.uuid());
            assert_eq!(message.inventory_data[0].parent_id, expectation.to.id().uuid());
        }
        (_, message) => panic!("unexpected outbound move: {message:?}"),
    }
    let before = inventory.snapshot();
    let update = session::apply_udp(inventory, owner.id(), &message)
        .expect("intake inbound UDP move update")
        .expect("move update belongs to the active owner");
    assert_eq!(update.rejected_blocks, 0);
    let selected = update
        .changes
        .changes
        .iter()
        .find(|change| matches!(change, inventory::InventoryChange::NodeMoved { node, .. } if *node == expectation.node));
    assert_matches!(
        selected,
        Some(inventory::InventoryChange::NodeMoved { node, from, to })
            if *node == expectation.node && *from == expectation.from && *to == expectation.to
    );
    session::confirm_pending_moves(&mut pending, &before, &inventory.snapshot());
    assert!(pending.is_empty());
    model.send_replace(Some(inventory.snapshot()));
}

fn metadata(name: &str, ids: &[Uuid]) -> Vec<u8> {
    let value = Value::Map(Map::from([(name.to_owned(), Value::Array(ids.iter().copied().map(Value::Uuid).collect()))]));
    xml::to_vec(&value).expect("encode AIS metadata response")
}

#[tokio::test]
async fn edits_apply_confirmed_responses() {
    let mut http = LoopbackPeer::start().await;
    let udp = UdpSocket::bind("127.0.0.1:0").await.expect("bind UDP move peer");
    let udp_address = udp.local_addr().expect("read UDP peer address");
    let sender = UdpSocket::bind("127.0.0.1:0").await.expect("bind UDP sender");
    let transport: Arc<dyn MoveTransport> = Arc::new(UdpMovePeer { sender, destination: udp_address });
    let (mut inventory, owner, root, first, second, trash, _, item, trash_item, trash_child) = inventory_fixture();
    let session_id = Uuid::from_u128(20);
    let (mut controller, model) = start_controller(&http, &inventory, EditSession { agent_id: owner.id(), session_id }, transport);
    let handle = controller.handle();

    let create_id = Uuid::from_u128(21);
    let create = tokio::spawn({
        let handle = handle.clone();
        async move { handle.create_folder(root, "created", FolderType::None).await }
    });
    http.grant(&[("InventoryAPIv3", "/ais")]).await;
    let request = http.next().await;
    assert_eq!(request.method, "POST");
    assert!(request.path.starts_with(&format!("/ais/category/{}?tid=", root.id().uuid())));
    let body = request.value();
    let category = body.as_map().and_then(|fields| fields.get("categories")).and_then(Value::as_array).and_then(|values| values.first());
    assert_eq!(category.and_then(Value::as_map).and_then(|fields| fields.get("name")), Some(&Value::String("created".into())));
    request.respond(200, metadata("_created_categories", &[create_id])).await;
    apply_confirmation(&mut controller, &mut inventory, &model).await;
    let created = CategoryKey::new(owner, FolderId::new(create_id));
    assert_eq!(create.await.expect("create result"), InventoryEditOutcome::Created(created));

    let rename_category = tokio::spawn({
        let handle = handle.clone();
        async move { handle.rename_category(created, "renamed folder").await }
    });
    let request = http.next().await;
    assert_eq!(request.method, "PATCH");
    assert_eq!(request.path, format!("/ais/category/{}", create_id));
    let later_rename = tokio::spawn({
        let handle = handle.clone();
        async move { handle.rename_category(created, "second rename").await }
    });
    let fields = request.value();
    assert_eq!(fields.as_map().and_then(|fields| fields.get("name")), Some(&Value::String("renamed folder".into())));
    request.respond(200, metadata("_updated_categories", &[create_id])).await;
    apply_confirmation(&mut controller, &mut inventory, &model).await;
    assert_eq!(rename_category.await.expect("category rename result"), InventoryEditOutcome::Applied);
    let request = http.next().await;
    assert_eq!(request.method, "PATCH");
    assert_eq!(request.path, format!("/ais/category/{}", create_id));
    let fields = request.value();
    assert_eq!(fields.as_map().and_then(|fields| fields.get("name")), Some(&Value::String("second rename".into())));
    request.respond(200, metadata("_updated_categories", &[create_id])).await;
    apply_confirmation(&mut controller, &mut inventory, &model).await;
    assert_eq!(later_rename.await.expect("queued category rename result"), InventoryEditOutcome::Applied);
    assert_eq!(inventory.snapshot().category(created).map(|category| category.name.as_ref()), Some("second rename"));

    let rename_item = tokio::spawn({
        let handle = handle.clone();
        async move { handle.rename_item(item, "renamed item").await }
    });
    let request = http.next().await;
    assert_eq!(request.method, "PATCH");
    assert_eq!(request.path, format!("/ais/item/{}", item.id().uuid()));
    request.respond(200, metadata("_updated_items", &[item.id().uuid()])).await;
    apply_confirmation(&mut controller, &mut inventory, &model).await;
    assert_eq!(rename_item.await.expect("item rename result"), InventoryEditOutcome::Applied);
    assert_eq!(inventory.snapshot().item(item).map(|item| item.name.as_ref()), Some("renamed item"));

    let empty_trash = tokio::spawn({
        let handle = handle.clone();
        async move { handle.empty_trash().await }
    });
    let request = http.next().await;
    assert_eq!(request.method, "DELETE");
    assert_eq!(request.path, format!("/ais/category/{}/children", trash.id().uuid()));
    let response = Value::Map(Map::from([
        ("_removed_items".into(), Value::Array(vec![Value::Uuid(trash_item.id().uuid())])),
        ("_categories_removed".into(), Value::Array(vec![Value::Uuid(trash_child.id().uuid())])),
    ]));
    request.respond(200, xml::to_vec(&response).expect("encode empty Trash response")).await;
    apply_confirmation(&mut controller, &mut inventory, &model).await;
    assert_eq!(empty_trash.await.expect("empty Trash result"), InventoryEditOutcome::Applied);
    assert!(inventory.snapshot().item(trash_item).is_none());
    assert!(inventory.snapshot().category(trash_child).is_none());

    let move_item = tokio::spawn({
        let handle = handle.clone();
        async move { handle.move_item(item, second).await }
    });
    confirm_udp_move(&mut controller, &udp, &mut inventory, &model, owner, session_id).await;
    assert_eq!(move_item.await.expect("move item result"), InventoryEditOutcome::Applied);

    let move_folder = tokio::spawn({
        let handle = handle.clone();
        async move { handle.move_folder(second, first).await }
    });
    confirm_udp_move(&mut controller, &udp, &mut inventory, &model, owner, session_id).await;
    assert_eq!(move_folder.await.expect("move folder result"), InventoryEditOutcome::Applied);

    let delete_item = tokio::spawn({
        let handle = handle.clone();
        async move { handle.delete_item(item).await }
    });
    confirm_udp_move(&mut controller, &udp, &mut inventory, &model, owner, session_id).await;
    assert_eq!(delete_item.await.expect("delete item result"), InventoryEditOutcome::Applied);

    let delete_folder = tokio::spawn(async move { handle.delete_folder(second).await });
    confirm_udp_move(&mut controller, &udp, &mut inventory, &model, owner, session_id).await;
    assert_eq!(delete_folder.await.expect("delete folder result"), InventoryEditOutcome::Applied);

    controller.stop().await;
}

#[tokio::test]
async fn ais_failures_apply_nothing() {
    let mut http = LoopbackPeer::start().await;
    let (inventory, owner, _root, _folder, _second, trash, protected, item, _, _) = inventory_fixture();
    let (mut controller, _model) = start_controller(
        &http,
        &inventory,
        EditSession { agent_id: owner.id(), session_id: Uuid::from_u128(22) },
        Arc::new(NoopMoveTransport),
    );
    let handle = controller.handle();
    let rename = tokio::spawn({
        let handle = handle.clone();
        async move { handle.rename_item(item, "not confirmed").await }
    });
    http.grant(&[("InventoryAPIv3", "/ais")]).await;
    let request = http.next().await;
    assert_eq!(request.method, "PATCH");
    request.respond(400, b"{}").await;
    assert_eq!(
        timeout(TEST_TIMEOUT, rename).await.expect("4xx edit result timeout").expect("failed rename task"),
        InventoryEditOutcome::ServerError
    );
    assert_eq!(inventory.snapshot().item(item).map(|item| item.name.as_ref()), Some("item"));

    let empty = tokio::spawn({
        let handle = handle.clone();
        async move { handle.empty_trash().await }
    });
    let request = http.next().await;
    assert_eq!(request.method, "DELETE");
    request.respond(410, b"{}").await;
    let Some(InventoryEditEvent::RefreshFolders { folders, refreshed }) = controller.next_event().await else {
        panic!("expected refresh request after 410");
    };
    assert_eq!(folders, vec![trash]);
    refreshed.send(true).expect("edit worker waits for refresh scheduling");
    assert_eq!(empty.await.expect("gone result"), InventoryEditOutcome::Gone);
    assert!(inventory.snapshot().item(item).is_some());

    assert_eq!(
        handle.rename_category(protected, "rejected").await,
        InventoryEditOutcome::Rejected(InventoryEditRejection::ProtectedFolder)
    );
    let roots = inventory.snapshot().roots();
    assert_eq!(
        handle.rename_category(roots.personal(), "rejected").await,
        InventoryEditOutcome::Rejected(InventoryEditRejection::RootFolder)
    );
    assert!(http.requests.try_recv().is_err());

    controller.stop().await;
}

struct NoopMoveTransport;

impl MoveTransport for NoopMoveTransport {
    fn send(&self, _movement: InventoryMove) -> Result<(), network::Error> {
        Err(network::Error::SessionClosed)
    }
}

#[tokio::test]
async fn missing_ais_uses_legacy_create() {
    let mut http = LoopbackPeer::start().await;
    let (mut inventory, owner, root, ..) = inventory_fixture();
    let (mut controller, model) = start_controller(
        &http,
        &inventory,
        EditSession { agent_id: owner.id(), session_id: Uuid::from_u128(23) },
        Arc::new(NoopMoveTransport),
    );
    let handle = controller.handle();
    let create = tokio::spawn(async move { handle.create_folder(root, "legacy", FolderType::None).await });
    http.grant(&[("CreateInventoryCategory", "/legacy")]).await;
    let request = http.next().await;
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/legacy");
    let body = request.value();
    let fields = body.as_map().expect("legacy request map");
    let id = fields.get("folder_id").and_then(Value::as_uuid).expect("requested folder ID");
    assert_eq!(fields.get("parent_id"), Some(&Value::Uuid(root.id().uuid())));
    assert_eq!(fields.get("type"), Some(&Value::Integer(FolderType::None.raw())));
    assert_eq!(fields.get("name"), Some(&Value::String("legacy".into())));
    let response = Value::Map(Map::from([
        ("folder_id".into(), Value::Uuid(id)),
        ("parent_id".into(), Value::Uuid(root.id().uuid())),
        ("type".into(), Value::Integer(FolderType::None.raw())),
        ("name".into(), Value::String("legacy".into())),
    ]));
    request.respond(200, xml::to_vec(&response).expect("encode legacy confirmation")).await;
    apply_confirmation(&mut controller, &mut inventory, &model).await;
    let key = CategoryKey::new(owner, FolderId::new(id));
    assert_eq!(create.await.expect("legacy create result"), InventoryEditOutcome::Created(key));
    assert!(inventory.snapshot().category(key).is_some());
    controller.stop().await;
}

#[tokio::test(start_paused = true)]
async fn move_timeout_refetches_both_folders() {
    let http = LoopbackPeer::start().await;
    let udp = UdpSocket::bind("127.0.0.1:0").await.expect("bind UDP move peer");
    let address = udp.local_addr().expect("read UDP peer address");
    let sender = UdpSocket::bind("127.0.0.1:0").await.expect("bind UDP sender");
    let transport: Arc<dyn MoveTransport> = Arc::new(UdpMovePeer { sender, destination: address });
    let (mut inventory, owner, _root, first, second, _, _, item, _, _) = inventory_fixture();
    let (mut controller, model) =
        start_controller(&http, &inventory, EditSession { agent_id: owner.id(), session_id: Uuid::from_u128(24) }, transport);
    let handle = controller.handle();

    time::resume();
    let move_item = tokio::spawn(async move { handle.move_item(item, second).await });

    let Some(InventoryEditEvent::MoveStarted { expectation, confirmed: _confirmed, registered }) = controller.next_event().await else {
        panic!("expected UDP move registration event");
    };
    registered.send(()).expect("move worker waits for registration");
    let mut bytes = [0; 4096];
    let (size, _) = timeout(TEST_TIMEOUT, udp.recv_from(&mut bytes)).await.expect("outbound move packet timeout").expect("read packet");
    assert!(Packet::decode(&bytes[..size]).expect("decode outbound packet").is_reliable());
    time::pause();
    time::advance(controller::MOVE_CONFIRMATION_TIMEOUT).await;
    let Some(InventoryEditEvent::MoveTimedOut { expectation: timed_out, refreshed }) = controller.next_event().await else {
        panic!("expected timeout invalidation event");
    };
    assert_eq!(timed_out, expectation);
    let mut delta = InventoryDelta::default();
    delta.updated_category_versions.extend([(first, ServerVersion::UNKNOWN), (second, ServerVersion::UNKNOWN)]);
    let changes = inventory.apply_delta(delta).expect("mark move folders incomplete");
    assert!(changes.refetch_categories.contains(&first));
    assert!(changes.refetch_categories.contains(&second));
    model.send_replace(Some(inventory.snapshot()));
    refreshed.send(true).expect("move worker waits for refetch scheduling");
    assert_matches!(move_item.await.expect("timed out move result"), InventoryEditOutcome::TimedOut);
    controller.stop().await;
}

#[tokio::test(start_paused = true)]
async fn ais_timeout_refreshes_rename() {
    let mut http = LoopbackPeer::start().await;
    let (mut inventory, owner, _root, folder, _, _, _, item_key, _, _) = inventory_fixture();
    let (mut server_inventory, _, _, _, _, _, _, _, _, _) = inventory_fixture();
    let (mut controller, model) = start_controller(
        &http,
        &inventory,
        EditSession { agent_id: owner.id(), session_id: Uuid::from_u128(25) },
        Arc::new(NoopMoveTransport),
    );
    let handle = controller.handle();

    time::resume();
    let rename = tokio::spawn(async move { handle.rename_item(item_key, "late response").await });
    http.grant(&[("InventoryAPIv3", "/ais")]).await;
    let request = http.next().await;
    assert_eq!(request.method, "PATCH");
    time::pause();
    let item = server_inventory.snapshot().item(item_key).expect("server item").clone();
    let mut item = item;
    item.name = Arc::from("server committed rename");
    server_inventory
        .apply_delta(InventoryDelta { updated_items: vec![item], ..InventoryDelta::default() })
        .expect("apply server-side rename");
    let Some(InventoryEditEvent::RefreshFolders { folders, refreshed }) = controller.next_event().await else {
        panic!("expected refresh after uncertain AIS result");
    };
    assert_eq!(folders, vec![folder]);

    let invalidated = inventory
        .apply_delta(InventoryDelta { updated_category_versions: vec![(folder, ServerVersion::UNKNOWN)], ..InventoryDelta::default() })
        .expect("mark folder incomplete");
    assert!(invalidated.refetch_categories.contains(&folder));
    model.send_replace(Some(inventory.snapshot()));
    let fetched_item = server_inventory.snapshot().item(item_key).expect("refetched item").clone();
    inventory
        .apply_delta(InventoryDelta {
            updated_items: vec![fetched_item],
            updated_category_versions: vec![(folder, ServerVersion::known(2).expect("known server version"))],
            ..InventoryDelta::default()
        })
        .expect("apply refreshed folder contents");
    model.send_replace(Some(inventory.snapshot()));
    refreshed.send(true).expect("edit worker waits for refresh scheduling");

    assert_eq!(rename.await.expect("timed out rename result"), InventoryEditOutcome::TimedOut);
    if let Err(error) = request.try_respond(200, metadata("_updated_items", &[item_key.id().uuid()])).await {
        assert_matches!(error.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted);
    }
    assert_eq!(
        model.borrow().as_ref().and_then(|snapshot| snapshot.item(item_key)).map(|item| item.name.as_ref()),
        Some("server committed rename")
    );
    controller.stop().await;
}

#[tokio::test]
async fn ais_503_refreshes_committed_rename() {
    let mut http = LoopbackPeer::start().await;
    let (mut inventory, owner, _root, folder, _, _, _, item_key, _, _) = inventory_fixture();
    let (mut server_inventory, _, _, _, _, _, _, _, _, _) = inventory_fixture();
    let (mut controller, model) = start_controller(
        &http,
        &inventory,
        EditSession { agent_id: owner.id(), session_id: Uuid::from_u128(26) },
        Arc::new(NoopMoveTransport),
    );
    let handle = controller.handle();
    let rename = tokio::spawn(async move { handle.rename_item(item_key, "requested rename").await });

    http.grant(&[("InventoryAPIv3", "/ais")]).await;
    let request = http.next().await;
    assert_eq!(request.method, "PATCH");
    assert_eq!(request.path, format!("/ais/item/{}", item_key.id().uuid()));
    let fields = request.value();
    assert_eq!(fields.as_map().and_then(|fields| fields.get("name")), Some(&Value::String("requested rename".into())));

    let mut committed_item = server_inventory.snapshot().item(item_key).expect("server item").clone();
    committed_item.name = Arc::from("server committed rename");
    server_inventory
        .apply_delta(InventoryDelta { updated_items: vec![committed_item], ..InventoryDelta::default() })
        .expect("commit server-side rename");
    request.respond(503, b"{}").await;

    let Some(InventoryEditEvent::RefreshFolders { folders, refreshed }) = controller.next_event().await else {
        panic!("expected refresh after 503");
    };
    assert_eq!(folders, vec![folder]);
    assert_eq!(inventory.snapshot().item(item_key).map(|item| item.name.as_ref()), Some("item"));
    assert_eq!(model.borrow().as_ref().and_then(|snapshot| snapshot.item(item_key)).map(|item| item.name.as_ref()), Some("item"));

    let invalidated = inventory
        .apply_delta(InventoryDelta { updated_category_versions: vec![(folder, ServerVersion::UNKNOWN)], ..InventoryDelta::default() })
        .expect("mark folder incomplete");
    assert!(invalidated.refetch_categories.contains(&folder));
    model.send_replace(Some(inventory.snapshot()));
    assert_eq!(model.borrow().as_ref().and_then(|snapshot| snapshot.item(item_key)).map(|item| item.name.as_ref()), Some("item"));

    let fetched_item = server_inventory.snapshot().item(item_key).expect("refetched item").clone();
    inventory
        .apply_delta(InventoryDelta {
            updated_items: vec![fetched_item],
            updated_category_versions: vec![(folder, ServerVersion::known(2).expect("known server version"))],
            ..InventoryDelta::default()
        })
        .expect("apply refreshed folder contents");
    model.send_replace(Some(inventory.snapshot()));
    refreshed.send(true).expect("edit worker waits for refresh scheduling");

    assert_eq!(rename.await.expect("server error rename result"), InventoryEditOutcome::ServerError);
    assert_eq!(
        model.borrow().as_ref().and_then(|snapshot| snapshot.item(item_key)).map(|item| item.name.as_ref()),
        Some("server committed rename")
    );
    controller.stop().await;
}
