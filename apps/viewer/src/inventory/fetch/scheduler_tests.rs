use std::{
    assert_matches,
    collections::{HashMap, HashSet},
    fmt::Write as _,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use futures::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use inventory::{
    AssetType, Category, CategoryKey, FolderType, Inventory, InventoryChild, InventoryDelta, InventoryLimits, InventoryOwner,
    InventoryType, Item, ItemKey, LinkResolution, MAX_FOLDER_FETCH_CHILDREN, MAX_INVENTORY_ITEMS, PermissionMask, Permissions, SaleInfo,
    SaleType, ServerVersion,
};
use llsd::{Map, Value, xml};
use network::capabilities::{CapabilityUrl, Client};
use protocol::inventory::DecodeError;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{self, timeout},
};
use utils::{AssetId, AvatarId, FolderId, GroupId, ItemId};
use uuid::Uuid;

use super::{
    AdaptiveConcurrency, BackgroundQueueGauge, ForegroundRequest, InventoryFetchHandle, InventoryFetchOutcome, InventoryFetchRequests,
    InventoryLoadingStatus, MAX_AIS_DEPTH, MAX_BACKGROUND_CONCURRENCY, MAX_RETRY_AFTER, MAX_TOTAL_CONCURRENCY, RECOVERY_SUCCESS_THRESHOLD,
    REQUEST_CAPACITY, RESULT_CAPACITY, fetch_ais, retry_delay, retry_transient, run_scheduler,
};
use crate::{
    inventory::{
        InventoryEditController,
        fetch::{
            FetchFailureCause, FolderApplyOutcome, FolderFetch, FolderFetchProvenance, InventoryFetchError, InventoryFetchEvent,
            MAX_FETCH_RESPONSE_BYTES, MAX_FETCH_RETRIES, RequestFailure, ais, classify_request_error, legacy,
        },
        test_support::{LoopbackPeer, TEST_TIMEOUT},
    },
    session::{FolderFetchApply, apply_inventory_fetch},
};

type SchedulerTask = (
    InventoryFetchRequests,
    mpsc::Receiver<InventoryFetchEvent>,
    watch::Sender<Option<inventory::InventorySnapshot>>,
    JoinHandle<()>,
    oneshot::Sender<()>,
    oneshot::Sender<()>,
);

fn key(owner: InventoryOwner, id: u128) -> CategoryKey {
    CategoryKey::new(owner, FolderId::new(Uuid::from_u128(id)))
}

fn category(key: CategoryKey, parent: Option<CategoryKey>) -> Category {
    Category {
        key,
        parent,
        preferred_folder_type: FolderType::None,
        name: Arc::from(format!("folder-{}", key.id().uuid().as_u128())),
        server_version: ServerVersion::UNKNOWN,
        server_descendent_count: None,
    }
}

fn item(key: ItemKey, parent: CategoryKey, name: &str, description: &str, flags: u32) -> Item {
    Item {
        key,
        parent,
        asset_id: AssetId::new(Uuid::nil()),
        asset_type: AssetType::Notecard,
        inventory_type: InventoryType::Notecard,
        flags,
        name: Arc::from(name),
        description: Arc::from(description),
        created_at: 0,
        permissions: Permissions {
            creator: AvatarId::new(Uuid::nil()),
            owner: AvatarId::new(Uuid::nil()),
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

fn inventory_with_children() -> (Inventory, CategoryKey, CategoryKey, CategoryKey) {
    let owner = InventoryOwner::new(Uuid::from_u128(1));
    let root = key(owner, 1);
    let first = key(owner, 2);
    let second = key(owner, 3);
    let mut inventory = Inventory::new(category(root, None), None, InventoryLimits::default()).expect("valid inventory root");
    inventory.upsert_category(category(first, Some(root))).expect("add first child folder");
    inventory.upsert_category(category(second, Some(root))).expect("add second child folder");
    (inventory, root, first, second)
}

fn hypermedia_link(href: String) -> Value {
    Value::Map(Map::from([("href".into(), Value::String(href)), ("name".into(), Value::String("link".into()))]))
}

fn ais_category(category_id: Uuid, owner_id: Uuid, parent_id: Option<Uuid>, name: String, version: i32) -> Value {
    let base_uri = "https://inventory.example.invalid/";
    let parent_id = parent_id.unwrap_or_else(Uuid::nil);
    let category_uri = format!("{base_uri}category/{category_id}");
    let parent_uri = format!("{base_uri}category/{parent_id}");
    let links = Map::from([
        ("self".into(), hypermedia_link(category_uri.clone())),
        ("parent".into(), hypermedia_link(parent_uri)),
        ("children".into(), hypermedia_link(format!("{category_uri}/children"))),
        ("items".into(), hypermedia_link(format!("{category_uri}/items"))),
        ("links".into(), hypermedia_link(format!("{category_uri}/links"))),
    ]);
    Value::Map(Map::from([
        ("_base_uri".into(), Value::String(base_uri.into())),
        ("_links".into(), Value::Map(links)),
        ("agent_id".into(), Value::Uuid(owner_id)),
        ("category_id".into(), Value::Uuid(category_id)),
        ("name".into(), Value::String(name)),
        ("parent_id".into(), Value::Uuid(parent_id)),
        ("type_default".into(), Value::Integer(-1)),
        ("version".into(), Value::Integer(version)),
    ]))
}

fn ais_item(folder: CategoryKey, item_id: Uuid) -> Value {
    Value::Map(Map::from([
        ("item_id".into(), Value::Uuid(item_id)),
        ("parent_id".into(), Value::Uuid(folder.id().uuid())),
        ("name".into(), Value::String(format!("item-{}", item_id.as_u128()))),
        ("created_at".into(), Value::Integer(0)),
        ("type".into(), Value::Integer(7)),
        ("inv_type".into(), Value::Integer(7)),
    ]))
}

fn ais_root_response(root: CategoryKey, categories: Map) -> Vec<u8> {
    let mut response = ais_category(root.id().uuid(), root.owner().id(), None, format!("folder-{}", root.id().uuid().as_u128()), 2);
    let Value::Map(fields) = &mut response else { unreachable!() };
    fields.insert(
        "_embedded".into(),
        Value::Map(Map::from([
            ("categories".into(), Value::Map(categories)),
            ("items".into(), Value::Map(Map::new())),
            ("links".into(), Value::Map(Map::new())),
        ])),
    );
    xml::to_vec(&response).expect("encode root response")
}

fn ais_folder_response(folder: CategoryKey, categories: Map, items: Map) -> Vec<u8> {
    let mut response = ais_category(folder.id().uuid(), folder.owner().id(), None, format!("folder-{}", folder.id().uuid().as_u128()), 1);
    let Value::Map(fields) = &mut response else { unreachable!() };
    fields.insert(
        "_embedded".into(),
        Value::Map(Map::from([
            ("categories".into(), Value::Map(categories)),
            ("items".into(), Value::Map(items)),
            ("links".into(), Value::Map(Map::new())),
        ])),
    );
    xml::to_vec(&response).expect("encode AIS folder response")
}

fn ais_child_category(folder: CategoryKey, parent: CategoryKey, descendent_count: usize) -> Value {
    let mut value = ais_category(
        folder.id().uuid(),
        folder.owner().id(),
        Some(parent.id().uuid()),
        format!("folder-{}", folder.id().uuid().as_u128()),
        1,
    );
    let Value::Map(fields) = &mut value else { unreachable!() };
    fields.insert("descendents".into(), Value::Integer(descendent_count as i32));
    value
}

fn ais_item_details(folder: CategoryKey, item_id: Uuid, name: &str, description: &str, flags: u32) -> Value {
    let mut value = ais_item(folder, item_id);
    let Value::Map(fields) = &mut value else { unreachable!() };
    fields.insert("name".into(), Value::String(name.into()));
    fields.insert("desc".into(), Value::String(description.into()));
    fields.insert("flags".into(), Value::Integer(flags as i32));
    value
}

fn root_response(root: CategoryKey, children: &[CategoryKey]) -> Vec<u8> {
    let categories = children
        .iter()
        .map(|child| {
            (
                child.id().uuid().to_string(),
                ais_category(
                    child.id().uuid(),
                    child.owner().id(),
                    Some(root.id().uuid()),
                    format!("folder-{}", child.id().uuid().as_u128()),
                    1,
                ),
            )
        })
        .collect::<Map>();
    ais_root_response(root, categories)
}

fn root_response_with_mismatched_category_key(root: CategoryKey, key_id: Uuid, child_id: Uuid) -> Vec<u8> {
    let child = ais_category(child_id, root.owner().id(), Some(root.id().uuid()), format!("folder-{}", child_id.as_u128()), 1);
    ais_root_response(root, Map::from([(key_id.to_string(), child)]))
}

fn root_response_with_child_version(root: CategoryKey, child: CategoryKey, version: i32) -> Vec<u8> {
    let categories = Map::from([(
        child.id().uuid().to_string(),
        ais_category(
            child.id().uuid(),
            child.owner().id(),
            Some(root.id().uuid()),
            format!("folder-{}", child.id().uuid().as_u128()),
            version,
        ),
    )]);
    ais_root_response(root, categories)
}

fn empty_folder_response(folder: CategoryKey, version: i32) -> Vec<u8> {
    let mut response =
        ais_category(folder.id().uuid(), folder.owner().id(), None, format!("folder-{}", folder.id().uuid().as_u128()), version);
    let Value::Map(fields) = &mut response else { unreachable!() };
    fields.insert(
        "_embedded".into(),
        Value::Map(Map::from([
            ("categories".into(), Value::Map(Map::new())),
            ("items".into(), Value::Map(Map::new())),
            ("links".into(), Value::Map(Map::new())),
        ])),
    );
    xml::to_vec(&response).expect("encode empty folder response")
}

fn folder_response(folder: CategoryKey, item_count: usize, id_prefix: u128, version: i32) -> Vec<u8> {
    assert!(item_count <= MAX_FOLDER_FETCH_CHILDREN);
    let mut body = String::with_capacity(item_count.saturating_mul(240).saturating_add(256));
    write!(
        body,
        "<llsd><map><key>_base_uri</key><string>https://inventory.example.invalid/</string><key>_links</key><map><key>self</key><map><key>href</key><string>https://inventory.example.invalid/category/{}</string><key>name</key><string>link</string></map></map><key>agent_id</key><uuid>{}</uuid><key>category_id</key><uuid>{}</uuid><key>name</key><string>large</string><key>parent_id</key><uuid>00000000-0000-0000-0000-000000000000</uuid><key>type_default</key><integer>-1</integer><key>version</key><integer>{version}</integer><key>_embedded</key><map><key>categories</key><map/><key>links</key><map/><key>items</key><map>",
        folder.id().uuid(),
        folder.owner().id(), folder.id().uuid()
    )
    .expect("write large-folder header");
    let parent_id = folder.id().uuid();
    for index in 0..item_count {
        let item_id = Uuid::from_u128(id_prefix + index as u128);
        write!(body, "<key>{item_id}</key><map><key>item_id</key><uuid>{item_id}</uuid><key>parent_id</key><uuid>{parent_id}</uuid><key>name</key><string>i</string><key>created_at</key><integer>0</integer><key>type</key><integer>7</integer><key>inv_type</key><integer>7</integer></map>")
            .expect("write large-folder item");
    }
    body.push_str("</map></map></map></llsd>");
    body.into_bytes()
}

fn llsd_folder(folder: CategoryKey) -> Value {
    llsd_folder_with_children(folder, &[])
}

fn llsd_folder_with_children(folder: CategoryKey, children: &[CategoryKey]) -> Value {
    Value::Map(Map::from([
        ("folder_id".into(), Value::Uuid(folder.id().uuid())),
        ("owner_id".into(), Value::Uuid(folder.owner().id())),
        ("version".into(), Value::Integer(4)),
        ("descendents".into(), Value::Integer(children.len() as i32)),
        ("categories".into(), Value::Array(children.iter().map(|child| llsd_category(*child, folder)).collect())),
        ("items".into(), Value::Array(Vec::new())),
    ]))
}

fn llsd_item(id: Uuid, parent: Uuid, owner: Uuid, asset_id: Uuid, asset_type: i32, inventory_type: i32) -> Value {
    Value::Map(Map::from([
        ("item_id".into(), Value::Uuid(id)),
        ("owner_id".into(), Value::Uuid(owner)),
        ("parent_id".into(), Value::Uuid(parent)),
        ("asset_id".into(), Value::Uuid(asset_id)),
        ("type".into(), Value::Integer(asset_type)),
        ("inv_type".into(), Value::Integer(inventory_type)),
        ("name".into(), Value::String(format!("item-{}", id.as_u128()))),
    ]))
}

fn llsd_category(folder: CategoryKey, parent: CategoryKey) -> Value {
    Value::Map(Map::from([
        ("folder_id".into(), Value::Uuid(folder.id().uuid())),
        ("owner_id".into(), Value::Uuid(folder.owner().id())),
        ("parent_id".into(), Value::Uuid(parent.id().uuid())),
        ("name".into(), Value::String(format!("folder-{}", folder.id().uuid().as_u128()))),
        ("version".into(), Value::Integer(1)),
        ("descendents".into(), Value::Integer(0)),
        ("type_default".into(), Value::Integer(-1)),
    ]))
}

fn legacy_folder_response(folder: CategoryKey, descendent_count: i32, items: Vec<Value>) -> Vec<u8> {
    let response = Value::Map(Map::from([(
        "folders".into(),
        Value::Array(vec![Value::Map(Map::from([
            ("folder_id".into(), Value::Uuid(folder.id().uuid())),
            ("owner_id".into(), Value::Uuid(folder.owner().id())),
            ("version".into(), Value::Integer(4)),
            ("descendents".into(), Value::Integer(descendent_count)),
            ("categories".into(), Value::Array(Vec::new())),
            ("items".into(), Value::Array(items)),
        ]))]),
    )]));
    xml::to_vec(&response).expect("encode legacy folder fixture")
}

fn scheduler(client: Client, snapshot: inventory::InventorySnapshot) -> SchedulerTask {
    let (request_sender, requests_rx) = mpsc::channel(REQUEST_CAPACITY);
    let (events_tx, events_rx) = mpsc::channel(RESULT_CAPACITY);
    let (model_tx, model_rx) = watch::channel(Some(snapshot.clone()));
    let (refresh_incomplete, refresh_receiver) = watch::channel(0);
    let requests = InventoryFetchRequests::new(request_sender, refresh_incomplete);
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let cancellation: Shared<BoxFuture<'static, ()>> = async move {
        drop(cancel_rx.await);
    }
    .boxed()
    .shared();
    let (stop_tx, stop_rx) = oneshot::channel();
    let task = tokio::spawn(run_scheduler(
        client,
        snapshot,
        model_rx,
        requests_rx,
        events_tx,
        Arc::new(AtomicUsize::new(0)),
        Arc::clone(&requests.background_queue_depth),
        Arc::clone(&requests.background_queue_peak),
        refresh_receiver,
        cancellation,
        stop_rx,
    ));
    (requests, events_rx, model_tx, task, cancel_tx, stop_tx)
}

fn fetch_handle(requests: InventoryFetchRequests) -> InventoryFetchHandle {
    InventoryFetchHandle {
        requests,
        status: watch::channel(InventoryLoadingStatus::default()).1,
        edits: InventoryEditController::new().handle(),
    }
}

async fn next_event(events: &mut mpsc::Receiver<InventoryFetchEvent>) -> InventoryFetchEvent {
    timeout(TEST_TIMEOUT, events.recv()).await.expect("fetch event timeout").expect("fetch event channel closed")
}

fn take_folder_event(event: InventoryFetchEvent, expected: CategoryKey) -> (FolderFetch, oneshot::Sender<FolderApplyOutcome>) {
    match event {
        InventoryFetchEvent::Folder { fetch, applied } => {
            assert_eq!(fetch.folder, expected);
            (fetch, applied)
        }
        event => panic!("expected folder response, received {event:?}"),
    }
}

fn apply_and_acknowledge(
    inventory: &mut Inventory,
    model: &watch::Sender<Option<inventory::InventorySnapshot>>,
    fetch: FolderFetch,
    applied: oneshot::Sender<FolderApplyOutcome>,
) -> Result<FolderFetchApply, inventory::InventoryError> {
    let result = apply_inventory_fetch(inventory, fetch);
    let outcome = match &result {
        Ok(FolderFetchApply::Applied(_)) => {
            model.send_replace(Some(inventory.snapshot()));
            FolderApplyOutcome::Applied
        }
        Ok(FolderFetchApply::Stale) => FolderApplyOutcome::Stale,
        Err(_) => FolderApplyOutcome::Rejected,
    };
    applied.send(outcome).expect("scheduler waits for folder apply outcome");
    result
}

#[tokio::test]
async fn ais_decodes_recursive_listing() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let capability = CapabilityUrl::parse(peer.url("/ais").as_str()).expect("valid AIS URL");
    let owner = InventoryOwner::new(Uuid::from_u128(1));
    let root = key(owner, 10);
    let child = key(owner, 11);
    let item_id = Uuid::from_u128(12);
    let base_uri = "https://inventory.example.invalid/";
    let item_uri = format!("{base_uri}item/{item_id}");
    let category_uri = format!("{base_uri}category/{}", child.id().uuid());
    let item = Value::Map(Map::from([
        ("_base_uri".into(), Value::String(base_uri.into())),
        (
            "_links".into(),
            Value::Map(Map::from([("self".into(), hypermedia_link(item_uri)), ("parent".into(), hypermedia_link(category_uri.clone()))])),
        ),
        ("agent_id".into(), Value::Uuid(owner.id())),
        ("asset_id".into(), Value::Uuid(Uuid::from_u128(13))),
        ("created_at".into(), Value::Integer(1_700_000_000)),
        ("flags".into(), Value::Integer(0)),
        ("item_id".into(), Value::Uuid(item_id)),
        ("parent_id".into(), Value::Uuid(child.id().uuid())),
        ("name".into(), Value::String("card".into())),
        ("desc".into(), Value::String("sample".into())),
        ("type".into(), Value::Integer(7)),
        ("inv_type".into(), Value::Integer(7)),
        (
            "permissions".into(),
            Value::Map(Map::from([
                ("base_mask".into(), Value::Integer(581_632)),
                ("creator_id".into(), Value::Uuid(Uuid::from_u128(14))),
                ("everyone_mask".into(), Value::Integer(0)),
                ("group_id".into(), Value::Uuid(Uuid::nil())),
                ("group_mask".into(), Value::Integer(0)),
                ("last_owner_id".into(), Value::Uuid(Uuid::from_u128(15))),
                ("next_owner_mask".into(), Value::Integer(581_632)),
                ("owner_id".into(), Value::Uuid(Uuid::from_u128(16))),
                ("owner_mask".into(), Value::Integer(581_632)),
            ])),
        ),
        ("sale_info".into(), Value::Map(Map::from([("sale_price".into(), Value::Integer(0)), ("sale_type".into(), Value::Integer(0))]))),
    ]));
    let mut child_category = ais_category(child.id().uuid(), owner.id(), Some(root.id().uuid()), "child".into(), 4);
    let Value::Map(child_fields) = &mut child_category else { unreachable!() };
    child_fields.insert(
        "_embedded".into(),
        Value::Map(Map::from([
            ("categories".into(), Value::Map(Map::new())),
            ("items".into(), Value::Map(Map::from([(item_id.to_string(), item)]))),
            ("links".into(), Value::Map(Map::new())),
        ])),
    );
    let mut body = ais_category(root.id().uuid(), owner.id(), None, "root".into(), 3);
    let Value::Map(root_fields) = &mut body else { unreachable!() };
    root_fields.insert(
        "_embedded".into(),
        Value::Map(Map::from([
            ("categories".into(), Value::Map(Map::from([(child.id().uuid().to_string(), child_category)]))),
            ("items".into(), Value::Map(Map::new())),
            ("links".into(), Value::Map(Map::new())),
        ])),
    );
    let response = xml::to_vec(&body).expect("encode AIS fixture");
    let responder = tokio::spawn(async move {
        let request = peer.next().await;
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, format!("/ais/category/{}/children?depth=16", root.id().uuid()));
        assert!(request.headers.to_ascii_lowercase().contains("accept: application/llsd+xml"));
        request.respond(200, response).await;
    });

    let folders = ais::fetch_once(&client, &capability, root, MAX_AIS_DEPTH, false).await.expect("recursive AIS response");
    responder.await.expect("peer responder");
    assert_eq!(folders.len(), 2);
    assert_eq!(folders[0].folder, root);
    assert_eq!(folders[0].children.len(), 1);
    assert_eq!(folders[1].folder, child);
    assert_eq!(folders[1].children.len(), 1);
}

#[tokio::test]
async fn item_endpoint_decodes_requested_link() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let capability = CapabilityUrl::parse(peer.url("/ais").as_str()).expect("valid AIS URL");
    let owner = InventoryOwner::new(Uuid::from_u128(1));
    let parent = key(owner, 15);
    let item_id = Uuid::from_u128(16);
    let base_uri = "https://inventory.example.invalid/";
    let body = xml::to_vec(&Value::Map(Map::from([
        ("_base_uri".into(), Value::String(base_uri.into())),
        (
            "_links".into(),
            Value::Map(Map::from([
                ("self".into(), hypermedia_link(format!("{base_uri}item/{item_id}"))),
                ("parent".into(), hypermedia_link(format!("{base_uri}category/{}", parent.id().uuid()))),
            ])),
        ),
        ("agent_id".into(), Value::Uuid(owner.id())),
        ("created_at".into(), Value::Integer(1_700_000_000)),
        ("desc".into(), Value::String("linked landmark".into())),
        ("flags".into(), Value::Integer(0)),
        ("inv_type".into(), Value::Integer(3)),
        ("item_id".into(), Value::Uuid(item_id)),
        ("parent_id".into(), Value::Uuid(parent.id().uuid())),
        ("name".into(), Value::String("linked landmark".into())),
        ("asset_id".into(), Value::Uuid(Uuid::from_u128(17))),
        ("type".into(), Value::Integer(3)),
        (
            "permissions".into(),
            Value::Map(Map::from([
                ("base_mask".into(), Value::Integer(581_632)),
                ("creator_id".into(), Value::Uuid(Uuid::from_u128(18))),
                ("everyone_mask".into(), Value::Integer(0)),
                ("group_id".into(), Value::Uuid(Uuid::nil())),
                ("group_mask".into(), Value::Integer(0)),
                ("last_owner_id".into(), Value::Uuid(Uuid::from_u128(19))),
                ("next_owner_mask".into(), Value::Integer(581_632)),
                ("owner_id".into(), Value::Uuid(Uuid::from_u128(20))),
                ("owner_mask".into(), Value::Integer(581_632)),
            ])),
        ),
        ("sale_info".into(), Value::Map(Map::from([("sale_price".into(), Value::Integer(0)), ("sale_type".into(), Value::Integer(0))]))),
    ])))
    .expect("encode AIS item fixture");
    let responder = tokio::spawn(async move {
        let request = peer.next().await;
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, format!("/ais/item/{item_id}"));
        request.respond(200, body).await;
    });

    let item = ais::fetch_item_once(&client, &capability, owner, item_id).await.expect("AIS linked item");
    responder.await.expect("peer responder");
    assert_eq!(item.key.id().uuid(), item_id);
    assert_eq!(item.parent, parent);
    assert_eq!(item.name.as_ref(), "linked landmark");
}

#[tokio::test]
async fn fallback_uses_descendents_capabilities() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let personal_owner = InventoryOwner::new(Uuid::from_u128(1));
    let library_owner = InventoryOwner::new(Uuid::from_u128(2));
    let personal_root = key(personal_owner, 10);
    let library_root = key(library_owner, 20);
    let mut inventory = Inventory::new(category(personal_root, None), Some(category(library_root, None)), InventoryLimits::default())
        .expect("valid inventory roots");
    let (requests, mut events, model, task, _cancel, stop) = scheduler(client, inventory.snapshot());
    peer.grant(&[("FetchInventoryDescendents2", "/personal"), ("FetchLibDescendents2", "/library")]).await;

    let first = peer.next().await;
    let second = peer.next().await;
    let mut root_paths = [first.path.as_str(), second.path.as_str()];
    root_paths.sort_unstable();
    assert_eq!(root_paths, ["/library", "/personal"]);
    assert_eq!(first.method, "POST");
    assert_eq!(second.method, "POST");
    for request in [first, second] {
        let payload: Value = xml::from_slice_with_limits(&request.body, llsd::Limits::default()).expect("decode LLSD request");
        let folders = payload.as_map().and_then(|fields| fields.get("folders")).and_then(Value::as_array).expect("folders array");
        assert_eq!(folders.len(), 1);
        assert!(folders.len() <= 10);
        let fields = folders[0].as_map().expect("folder request map");
        assert!(fields.contains_key("folder_id"));
        assert!(fields.contains_key("owner_id"));
        let path = request.path.clone();
        let folder = if path == "/personal" { personal_root } else { library_root };
        let response = Value::Map(Map::from([("folders".into(), Value::Array(vec![llsd_folder(folder)]))]));
        request.respond(200, xml::to_string(&response).expect("encode fallback response")).await;
    }
    let mut applied = HashSet::new();
    loop {
        match next_event(&mut events).await {
            InventoryFetchEvent::Folder { fetch, applied: response } => {
                let folder = fetch.folder;
                apply_and_acknowledge(&mut inventory, &model, fetch, response).expect("apply fallback folder response");
                applied.insert(folder);
            }
            InventoryFetchEvent::BackgroundDone => break,
            InventoryFetchEvent::Failed { folder, error } => panic!("fallback failed for {folder:?}: {error}"),
        }
    }
    assert_eq!(applied, HashSet::from([personal_root, library_root]));
    drop(requests);
    stop.send(()).expect("stop fallback scheduler");
    task.await.expect("scheduler stopped");
}

#[tokio::test]
async fn legacy_mapping_omits_auxiliary_targets() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let capability = CapabilityUrl::parse(peer.url("/legacy").as_str()).expect("valid legacy capability URL");
    let owner = InventoryOwner::new(Uuid::from_u128(2));
    let source = key(owner, 1);
    let target_parent = key(owner, 4);
    let link_id = Uuid::from_u128(6);
    let target_id = Uuid::from_u128(3);
    let responder = tokio::spawn(async move {
        let request = peer.next().await;
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/legacy");
        request.respond(200, include_bytes!("../../../../../crates/protocol/tests/fixtures/legacy_one_link.xml").to_vec()).await;
    });

    let mut fetched = legacy::fetch_batch(&client, &capability, &[source], false).await.expect("decode legacy link response");
    responder.await.expect("legacy response sent");

    assert_eq!(fetched.len(), 1);
    let fetch = fetched.pop().expect("one requested folder");
    assert_eq!(fetch.folder, source);
    assert_eq!(fetch.children.len(), 1);
    assert_eq!(fetch.descendent_count, Some(1));
    let InventoryChild::Item(link) = &fetch.children[0] else { panic!("expected the direct link item") };
    assert_eq!(link.key.id().uuid(), link_id);
    assert_eq!(link.parent, source);
    assert_eq!(link.asset_id.uuid(), target_id);

    let root = key(owner, 9);
    let mut inventory = Inventory::new(category(root, None), None, InventoryLimits::default()).expect("valid inventory root");
    inventory.upsert_category(category(source, Some(root))).expect("add source folder");
    inventory.upsert_category(category(target_parent, Some(root))).expect("add target parent");
    let target_key = ItemKey::new(owner, ItemId::new(target_id));
    let sibling_key = ItemKey::new(owner, ItemId::new(Uuid::from_u128(15)));
    inventory.upsert_item(item(target_key, target_parent, "existing target", "before fetch", 0)).expect("add existing link target");
    inventory.upsert_item(item(sibling_key, target_parent, "target sibling", "keep sibling", 0)).expect("add target sibling");
    let FolderFetchApply::Applied(_) = apply_inventory_fetch(&mut inventory, fetch).expect("apply direct folder children") else {
        panic!("current source fetch should apply");
    };
    let snapshot = inventory.snapshot();
    assert!(snapshot.is_complete(source));
    let link_key = ItemKey::new(owner, ItemId::new(link_id));
    assert_eq!(snapshot.item(link_key).map(|item| item.parent), Some(source));
    assert_eq!(
        snapshot.link_resolution(link_key),
        Some(LinkResolution::Resolved(inventory::InventoryNodeKey::new(owner, inventory::InventoryNodeId::Item(target_key.id()),)))
    );
    assert_eq!(snapshot.item(target_key).map(|item| item.parent), Some(target_parent));
    assert_eq!(snapshot.item(target_key).map(|item| item.name.as_ref()), Some("existing target"));
    assert_eq!(snapshot.item(sibling_key).map(|item| item.parent), Some(target_parent));
    assert_eq!(snapshot.child_count(target_parent), 2);
}

#[tokio::test]
async fn captured_listings_complete_foreground_fetches() {
    let cases: [(&str, &[u8], u32, &[u128]); 2] = [
        ("one-link", include_bytes!("../../../../../crates/protocol/tests/fixtures/legacy_one_link.xml"), 1, &[6]),
        ("link-and-item", include_bytes!("../../../../../crates/protocol/tests/fixtures/legacy_link_and_item.xml"), 2, &[6, 7]),
    ];

    for (fixture, response, expected_count, expected_items) in cases {
        let mut peer = LoopbackPeer::start().await;
        let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
        let owner = InventoryOwner::new(Uuid::from_u128(2));
        let root = key(owner, 9);
        let source = key(owner, 1);
        let target_parent = key(owner, 4);
        let mut root_category = category(root, None);
        root_category.server_version = ServerVersion::from_raw(1);
        root_category.server_descendent_count = Some(2);
        let mut source_category = category(source, Some(root));
        source_category.server_version = ServerVersion::from_raw(1);
        source_category.server_descendent_count = Some(0);
        let mut target_parent_category = category(target_parent, Some(root));
        target_parent_category.server_version = ServerVersion::from_raw(1);
        target_parent_category.server_descendent_count = Some(0);
        let mut inventory = Inventory::new(root_category, None, InventoryLimits::default()).expect("valid inventory root");
        inventory.upsert_category(source_category).expect("add source folder");
        inventory.upsert_category(target_parent_category).expect("add target parent");

        let (requests, mut events, model, task, _cancel, stop) = scheduler(client, inventory.snapshot());
        peer.grant(&[("FetchInventoryDescendents2", "/legacy")]).await;
        assert_matches!(next_event(&mut events).await, InventoryFetchEvent::BackgroundDone);

        for folder in [source, target_parent] {
            let mut updated = inventory.snapshot().category(folder).expect("folder exists").clone();
            updated.server_version = ServerVersion::UNKNOWN;
            updated.server_descendent_count = None;
            inventory.upsert_category(updated).expect("make folder fetchable");
        }
        model.send_replace(Some(inventory.snapshot()));
        let handle = fetch_handle(requests.clone());
        let source_completion = handle.request_folder(source).expect("request captured source folder");
        let source_request = peer.next().await;
        assert_eq!(source_request.method, "POST", "fixture: {fixture}");
        assert_eq!(source_request.path, "/legacy", "fixture: {fixture}");
        source_request.respond(200, response.to_vec()).await;

        let (fetch, applied) = take_folder_event(next_event(&mut events).await, source);
        assert_eq!(fetch.descendent_count, Some(expected_count), "fixture: {fixture}");
        let direct_ids = fetch
            .children
            .iter()
            .map(|child| match child {
                InventoryChild::Item(item) => item.key.id().uuid().as_u128(),
                InventoryChild::Category(_) => panic!("captured listing has no direct categories: {fixture}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(direct_ids, expected_items, "fixture: {fixture}");
        assert_eq!(fetch.children.len(), expected_count as usize, "fixture: {fixture}");
        apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("apply captured direct children");
        assert_eq!(
            source_completion.await.expect("foreground completion delivered"),
            InventoryFetchOutcome::Succeeded,
            "fixture: {fixture}"
        );
        assert!(inventory.snapshot().is_complete(source), "fixture: {fixture}");

        let link_key = ItemKey::new(owner, ItemId::new(Uuid::from_u128(6)));
        let target_key = ItemKey::new(owner, ItemId::new(Uuid::from_u128(3)));
        assert_eq!(inventory.snapshot().item(link_key).map(|item| item.parent), Some(source), "fixture: {fixture}");
        assert!(inventory.snapshot().item(target_key).is_none(), "fixture: {fixture}");
        assert_eq!(
            inventory.snapshot().link_resolution(link_key),
            Some(LinkResolution::NotLoaded(target_key.id().uuid())),
            "fixture: {fixture}"
        );

        let target_completion = handle.request_folder(target_parent).expect("request target parent");
        let target_request = peer.next().await;
        assert_eq!(target_request.method, "POST", "fixture: {fixture}");
        assert_eq!(target_request.path, "/legacy", "fixture: {fixture}");
        target_request
            .respond(
                200,
                legacy_folder_response(
                    target_parent,
                    1,
                    vec![llsd_item(target_key.id().uuid(), target_parent.id().uuid(), owner.id(), Uuid::from_u128(5), 7, 7)],
                ),
            )
            .await;
        let (fetch, applied) = take_folder_event(next_event(&mut events).await, target_parent);
        apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("apply target under its actual parent");
        assert_eq!(target_completion.await.expect("target completion delivered"), InventoryFetchOutcome::Succeeded, "fixture: {fixture}");

        let snapshot = inventory.snapshot();
        assert!(snapshot.is_complete(target_parent), "fixture: {fixture}");
        assert_eq!(snapshot.item(target_key).map(|item| item.parent), Some(target_parent), "fixture: {fixture}");
        assert_eq!(
            snapshot.link_resolution(link_key),
            Some(LinkResolution::Resolved(inventory::InventoryNodeKey::new(owner, inventory::InventoryNodeId::Item(target_key.id()),))),
            "fixture: {fixture}"
        );

        drop(handle);
        drop(requests);
        stop.send(()).expect("stop captured listing scheduler");
        task.await.expect("captured listing scheduler stopped");
    }
}

#[tokio::test]
async fn foreground_aux_target_resolves_later() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let owner = InventoryOwner::new(Uuid::from_u128(1));
    let root = key(owner, 20);
    let source = key(owner, 21);
    let target_parent = key(owner, 22);
    let link_id = Uuid::from_u128(23);
    let target_id = Uuid::from_u128(24);
    let mut root_category = category(root, None);
    root_category.server_version = ServerVersion::from_raw(1);
    root_category.server_descendent_count = Some(2);
    let mut source_category = category(source, Some(root));
    source_category.server_version = ServerVersion::from_raw(1);
    source_category.server_descendent_count = Some(0);
    let mut target_parent_category = category(target_parent, Some(root));
    target_parent_category.server_version = ServerVersion::from_raw(1);
    target_parent_category.server_descendent_count = Some(0);
    let mut inventory = Inventory::new(root_category, None, InventoryLimits::default()).expect("valid inventory root");
    inventory.upsert_category(source_category).expect("add source folder");
    inventory.upsert_category(target_parent_category).expect("add target parent");
    assert!(inventory.snapshot().is_complete(source));
    assert!(inventory.snapshot().is_complete(target_parent));

    let (requests, mut events, model, task, _cancel, stop) = scheduler(client, inventory.snapshot());
    peer.grant(&[("FetchInventoryDescendents2", "/legacy")]).await;
    assert_matches!(next_event(&mut events).await, InventoryFetchEvent::BackgroundDone);

    for folder in [source, target_parent] {
        let mut updated = inventory.snapshot().category(folder).expect("folder exists").clone();
        updated.server_version = ServerVersion::UNKNOWN;
        updated.server_descendent_count = None;
        inventory.upsert_category(updated).expect("make folder fetchable");
    }
    model.send_replace(Some(inventory.snapshot()));
    let handle = fetch_handle(requests.clone());
    let source_completion = handle.request_folder(source).expect("request source folder");
    let target_completion = handle.request_folder(target_parent).expect("request target parent");

    let source_request = peer.next().await;
    assert_eq!(source_request.method, "POST");
    assert_eq!(source_request.path, "/legacy");
    source_request
        .respond(
            200,
            legacy_folder_response(
                source,
                2,
                vec![
                    llsd_item(link_id, source.id().uuid(), owner.id(), target_id, 24, 24),
                    llsd_item(target_id, target_parent.id().uuid(), owner.id(), Uuid::from_u128(25), 7, 7),
                ],
            ),
        )
        .await;
    let (fetch, applied) = take_folder_event(next_event(&mut events).await, source);
    assert_eq!(fetch.descendent_count, Some(2));
    assert_eq!(fetch.children.len(), 1);
    apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("apply direct link without auxiliary target");

    let link_key = ItemKey::new(owner, ItemId::new(link_id));
    let target_key = ItemKey::new(owner, ItemId::new(target_id));
    assert_eq!(source_completion.await.expect("source fetch reaches a terminal outcome"), InventoryFetchOutcome::Failed);
    assert!(!inventory.snapshot().is_complete(source));
    assert!(inventory.snapshot().item(target_key).is_none());
    assert_eq!(inventory.snapshot().link_resolution(link_key), Some(LinkResolution::NotLoaded(target_id)));

    let target_request = peer.next().await;
    assert_eq!(target_request.method, "POST");
    assert_eq!(target_request.path, "/legacy");
    target_request
        .respond(
            200,
            legacy_folder_response(
                target_parent,
                1,
                vec![llsd_item(target_id, target_parent.id().uuid(), owner.id(), Uuid::from_u128(25), 7, 7)],
            ),
        )
        .await;
    let (fetch, applied) = take_folder_event(next_event(&mut events).await, target_parent);
    apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("apply target under its supplied parent");

    assert_eq!(target_completion.await.expect("target fetch reaches a terminal outcome"), InventoryFetchOutcome::Succeeded);
    assert!(inventory.snapshot().is_complete(target_parent));
    assert!(!inventory.snapshot().is_complete(source));
    assert_eq!(inventory.snapshot().item(target_key).map(|item| item.parent), Some(target_parent));
    assert_eq!(
        inventory.snapshot().link_resolution(link_key),
        Some(LinkResolution::Resolved(inventory::InventoryNodeKey::new(owner, inventory::InventoryNodeId::Item(target_key.id()),)))
    );

    let source_retry = handle.request_folder(source).expect("retry incomplete source folder");
    let source_retry_request = peer.next().await;
    assert_eq!(source_retry_request.method, "POST");
    assert_eq!(source_retry_request.path, "/legacy");
    source_retry_request
        .respond(200, legacy_folder_response(source, 1, vec![llsd_item(link_id, source.id().uuid(), owner.id(), target_id, 24, 24)]))
        .await;
    let (fetch, applied) = take_folder_event(next_event(&mut events).await, source);
    assert_eq!(fetch.descendent_count, Some(1));
    apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("apply retried source listing");
    assert_eq!(source_retry.await.expect("source retry completion delivered"), InventoryFetchOutcome::Succeeded);
    assert!(inventory.snapshot().is_complete(source));

    drop(handle);
    drop(requests);
    stop.send(()).expect("stop scheduler after target resolution");
    task.await.expect("scheduler stopped");
}

#[tokio::test]
async fn fallback_batches_ten_folders() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let owner = InventoryOwner::new(Uuid::from_u128(1));
    let root = key(owner, 70);
    let children = (71..82).map(|id| key(owner, id)).collect::<Vec<_>>();
    let mut root_category = category(root, None);
    root_category.server_version = ServerVersion::from_raw(1);
    root_category.server_descendent_count = Some(children.len() as u32);
    let mut inventory = Inventory::new(root_category, None, InventoryLimits::default()).expect("valid inventory root");
    for child in &children {
        inventory.upsert_category(category(*child, Some(root))).expect("add fallback child");
    }
    let (requests, mut events, model, task, _cancel, stop) = scheduler(client, inventory.snapshot());
    peer.grant(&[("FetchInventoryDescendents2", "/personal")]).await;

    let first_batch = peer.next().await;
    let second_batch = peer.next().await;
    let mut loaded = HashSet::new();
    for request in [first_batch, second_batch] {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/personal");
        let payload: Value = xml::from_slice_with_limits(&request.body, llsd::Limits::default()).expect("decode batch request");
        let requested = payload.as_map().and_then(|fields| fields.get("folders")).and_then(Value::as_array).expect("folder batch");
        assert!(requested.len() <= 10);
        assert!(!requested.is_empty());
        for value in requested {
            let fields = value.as_map().expect("folder request entry");
            let id = fields.get("folder_id").and_then(Value::as_uuid).expect("requested folder ID");
            loaded.insert(id);
        }
        let folders = requested
            .iter()
            .map(|value| {
                let fields = value.as_map().expect("folder request entry");
                let id = fields.get("folder_id").and_then(Value::as_uuid).expect("requested folder ID");
                llsd_folder(key(owner, id.as_u128()))
            })
            .collect();
        let response = Value::Map(Map::from([("folders".into(), Value::Array(folders))]));
        request.respond(200, xml::to_string(&response).expect("encode child fallback response")).await;
    }
    assert_eq!(loaded, children.iter().map(|child| child.id().uuid()).collect());

    let mut completed = HashSet::new();
    loop {
        match next_event(&mut events).await {
            InventoryFetchEvent::Folder { fetch, applied: response } => {
                let folder = fetch.folder;
                apply_and_acknowledge(&mut inventory, &model, fetch, response).expect("apply fallback batch folder response");
                completed.insert(folder);
            }
            InventoryFetchEvent::BackgroundDone => break,
            InventoryFetchEvent::Failed { folder, error } => panic!("fallback failed for {folder:?}: {error}"),
        }
    }
    assert_eq!(completed, children.into_iter().collect());
    drop(requests);
    stop.send(()).expect("stop batched fallback scheduler");
    task.await.expect("scheduler stopped");
}

#[tokio::test]
async fn recursive_403_reduces_depth() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let capability = CapabilityUrl::parse(peer.url("/ais").as_str()).expect("valid AIS URL");
    let owner = InventoryOwner::new(Uuid::from_u128(1));
    let root = key(owner, 30);
    let successful_body = empty_folder_response(root, 1);
    let responder = tokio::spawn(async move {
        for depth in [16, 8, 4, 2, 1, 0] {
            let request = peer.next().await;
            assert_eq!(request.path, format!("/ais/category/{}/children?depth={depth}", root.id().uuid()));
            if depth == 0 {
                request.respond(200, successful_body.clone()).await;
            } else {
                request.respond(403, "").await;
            }
        }
    });

    let fetched = fetch_ais(&client, &capability, root, MAX_AIS_DEPTH).await.expect("reduced-depth request");
    responder.await.expect("peer responder");
    assert_eq!(fetched.len(), 1);
    assert_eq!(fetched[0].folder, root);
}

#[tokio::test]
async fn depth_zero_403_too_large() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let capability = CapabilityUrl::parse(peer.url("/ais").as_str()).expect("valid AIS URL");
    let owner = InventoryOwner::new(Uuid::from_u128(1));
    let root = key(owner, 31);
    let responder = tokio::spawn(async move {
        let request = peer.next().await;
        assert_eq!(request.path, format!("/ais/category/{}/children?depth=0", root.id().uuid()));
        request.respond(403, "").await;
    });

    assert_matches!(fetch_ais(&client, &capability, root, 0).await, Err(InventoryFetchError::FolderTooLarge));
    responder.await.expect("peer responder");
}

#[tokio::test]
async fn transient_failure_retries() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let capability = CapabilityUrl::parse(peer.url("/ais").as_str()).expect("valid AIS URL");
    let owner = InventoryOwner::new(Uuid::from_u128(1));
    let root = key(owner, 40);
    let successful_body = empty_folder_response(root, 1);
    let responder = tokio::spawn(async move {
        let first = peer.next().await;
        assert_eq!(first.path, format!("/ais/category/{}/children?depth=0", root.id().uuid()));
        first.respond_with_headers(429, "", &[("Retry-After", "0")]).await;
        let second = peer.next().await;
        assert_eq!(second.path, format!("/ais/category/{}/children?depth=0", root.id().uuid()));
        second.respond_with_headers(503, "", &[("Retry-After", "0")]).await;
        let third = peer.next().await;
        assert_eq!(third.path, format!("/ais/category/{}/children?depth=0", root.id().uuid()));
        third.respond(200, successful_body).await;
    });

    let fetched = fetch_ais(&client, &capability, root, 0).await.expect("retry should recover");
    responder.await.expect("peer responder");
    assert_eq!(fetched.len(), 1);
}

#[tokio::test]
async fn ais_batch_missing_folder_splits() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let owner = InventoryOwner::new(Uuid::from_u128(1));
    let root = key(owner, 41);
    let omitted = key(owner, 42);
    let present = key(owner, 43);
    let mut inventory = Inventory::new(category(root, None), None, InventoryLimits::default()).expect("valid inventory root");
    let (_requests, mut events, model, task, _cancel, stop) = scheduler(client, inventory.snapshot());
    peer.grant(&[("InventoryAPIv3", "/ais")]).await;

    let root_request = peer.next().await;
    assert_eq!(root_request.path, format!("/ais/category/{}/children?depth=16", root.id().uuid()));
    root_request.respond(200, root_response(root, &[omitted, present])).await;
    let (fetch, applied) = take_folder_event(next_event(&mut events).await, root);
    apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("apply root response");

    let batch_request = peer.next().await;
    assert_eq!(
        batch_request.path,
        format!("/ais/category/{}/children?depth=16&children={},{}", root.id().uuid(), omitted.id().uuid(), present.id().uuid())
    );
    batch_request.respond(200, empty_folder_response(present, 2)).await;

    let first_request = peer.next().await;
    let second_request = peer.next().await;
    let omitted_path = format!("/ais/category/{}/children?depth=0", omitted.id().uuid());
    let present_path = format!("/ais/category/{}/children?depth=0", present.id().uuid());
    let (omitted_request, present_request) =
        if first_request.path == omitted_path { (first_request, second_request) } else { (second_request, first_request) };
    assert_eq!(omitted_request.path, omitted_path);
    assert_eq!(present_request.path, present_path);
    omitted_request.respond(404, "").await;
    present_request.respond(200, empty_folder_response(present, 2)).await;

    let mut omitted_reported = false;
    let mut present_applied = false;
    loop {
        match next_event(&mut events).await {
            InventoryFetchEvent::Failed { folder, error } => {
                assert_eq!(folder, omitted);
                assert_matches!(error, InventoryFetchError::HttpStatus(404));
                omitted_reported = true;
            }
            InventoryFetchEvent::Folder { fetch, applied } => {
                assert_eq!(fetch.folder, present);
                apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("apply present folder response");
                present_applied = true;
            }
            InventoryFetchEvent::BackgroundDone => break,
        }
    }
    assert!(omitted_reported);
    assert!(present_applied);
    assert!(inventory.snapshot().is_complete(present));
    assert!(!inventory.snapshot().is_complete(omitted));

    drop(stop);
    task.await.expect("scheduler task completes");
}

#[tokio::test]
async fn missing_folder_keeps_walk_retryable() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let owner = InventoryOwner::new(Uuid::from_u128(1));
    let root = key(owner, 41);
    let missing = key(owner, 42);
    let valid = key(owner, 43);
    let inventory = Inventory::new(category(root, None), None, InventoryLimits::default()).expect("valid inventory root");
    let (requests, mut events, model, task, _cancel, stop) = scheduler(client, inventory.snapshot());
    peer.grant(&[("InventoryAPIv3", "/ais")]).await;

    let root_request = peer.next().await;
    assert_eq!(root_request.path, format!("/ais/category/{}/children?depth=16", root.id().uuid()));
    root_request.respond(200, root_response(root, &[missing, valid])).await;
    let (fetch, applied) = take_folder_event(next_event(&mut events).await, root);
    let mut inventory = inventory;
    apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("apply root response");

    let batch_request = peer.next().await;
    assert_eq!(
        batch_request.path,
        format!("/ais/category/{}/children?depth=16&children={},{}", root.id().uuid(), missing.id().uuid(), valid.id().uuid())
    );
    batch_request.respond(404, "").await;

    let first_request = peer.next().await;
    let second_request = peer.next().await;
    let missing_path = format!("/ais/category/{}/children?depth=0", missing.id().uuid());
    let valid_path = format!("/ais/category/{}/children?depth=0", valid.id().uuid());
    let (missing_request, valid_request) =
        if first_request.path == missing_path { (first_request, second_request) } else { (second_request, first_request) };
    assert_eq!(missing_request.path, missing_path);
    assert_eq!(valid_request.path, valid_path);
    missing_request.respond(404, "").await;
    valid_request.respond(200, empty_folder_response(valid, 2)).await;

    let mut missing_reported = false;
    let mut valid_applied = false;
    loop {
        match next_event(&mut events).await {
            InventoryFetchEvent::Failed { folder, error } => {
                assert_eq!(folder, missing);
                assert_matches!(error, InventoryFetchError::HttpStatus(404));
                missing_reported = true;
            }
            InventoryFetchEvent::Folder { fetch, applied } => {
                assert_eq!(fetch.folder, valid);
                apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("apply surviving folder response");
                valid_applied = true;
            }
            InventoryFetchEvent::BackgroundDone => break,
        }
    }
    assert!(missing_reported);
    assert!(valid_applied);
    assert!(inventory.snapshot().is_complete(valid));
    assert!(!inventory.snapshot().is_complete(missing));

    let (_status_sender, status) = watch::channel(InventoryLoadingStatus::default());
    let handle = InventoryFetchHandle { requests: requests.clone(), status, edits: InventoryEditController::new().handle() };
    for _ in 0..2 {
        let completion = handle.request_folder(missing).expect("queue folder retry");
        let retry = peer.next().await;
        assert_eq!(retry.path, missing_path);
        retry.respond(404, "").await;
        match next_event(&mut events).await {
            InventoryFetchEvent::Failed { folder, error } => {
                assert_eq!(folder, missing);
                assert_matches!(error, InventoryFetchError::HttpStatus(404));
            }
            event => panic!("expected failed folder retry, received {event:?}"),
        }
        assert_eq!(completion.await.expect("fetch outcome delivered"), InventoryFetchOutcome::Failed);
    }

    let completion = handle.request_folder(missing).expect("queue successful folder retry");
    let retry = peer.next().await;
    assert_eq!(retry.path, missing_path);
    retry.respond(200, empty_folder_response(missing, 2)).await;
    let (fetch, applied) = take_folder_event(next_event(&mut events).await, missing);
    apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("apply retried folder response");
    assert_eq!(completion.await.expect("fetch outcome delivered"), InventoryFetchOutcome::Succeeded);
    assert!(inventory.snapshot().is_complete(missing));

    drop(requests);
    stop.send(()).expect("stop scheduler after folder retry");
    task.await.expect("scheduler stopped");
}

#[tokio::test]
async fn stale_response_requeues_folder() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let owner = InventoryOwner::new(Uuid::from_u128(1));
    let root = key(owner, 44);
    let mut root_category = category(root, None);
    root_category.server_version = ServerVersion::from_raw(7);
    root_category.server_descendent_count = Some(1);
    let mut inventory = Inventory::new(root_category.clone(), None, InventoryLimits::default()).expect("valid inventory root");
    for revision in 1..=7 {
        let mut update = root_category.clone();
        update.name = Arc::from(format!("folder-{revision}"));
        inventory.upsert_category(update).expect("advance folder generation");
    }
    let start_generation = inventory.snapshot().folder_generation(root).expect("folder generation is tracked");
    assert_eq!(start_generation.get(), 7);

    let (requests, mut events, model, task, _cancel, stop) = scheduler(client, inventory.snapshot());
    peer.grant(&[("InventoryAPIv3", "/ais")]).await;
    let initial_request = peer.next().await;
    assert_eq!(initial_request.path, format!("/ais/category/{}/children?depth=16", root.id().uuid()));
    initial_request.respond(404, "").await;
    let InventoryFetchEvent::Failed { folder, error } = next_event(&mut events).await else {
        panic!("expected initial background failure");
    };
    assert_eq!(folder, root);
    assert_matches!(error, InventoryFetchError::HttpStatus(404));
    assert_matches!(next_event(&mut events).await, InventoryFetchEvent::BackgroundDone);

    let (_status_sender, status) = watch::channel(InventoryLoadingStatus::default());
    let handle = InventoryFetchHandle { requests: requests.clone(), status, edits: InventoryEditController::new().handle() };
    let completion = handle.request_folder(root).expect("queue foreground retry");
    let request = peer.next().await;
    assert_eq!(request.path, format!("/ais/category/{}/children?depth=0", root.id().uuid()));

    let mut live_root = inventory.snapshot().category(root).expect("root remains present").clone();
    live_root.server_version = ServerVersion::from_raw(8);
    live_root.server_descendent_count = Some(1);
    inventory.upsert_category(live_root).expect("apply live folder update");
    let live_snapshot = inventory.snapshot();
    assert_eq!(live_snapshot.folder_generation(root).map(|generation| generation.get()), Some(8));
    assert_eq!(live_snapshot.category(root).map(|category| category.server_version), Some(ServerVersion::from_raw(8)));
    model.send_replace(Some(live_snapshot));

    request.respond(200, folder_response(root, 1, 44_000, -1)).await;
    let (fetch, applied) = take_folder_event(next_event(&mut events).await, root);
    assert_eq!(fetch.provenance, FolderFetchProvenance::StartedAt { folder: Some(start_generation), owner: 0 });
    let FolderFetchApply::Stale = apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("discard stale response") else {
        panic!("live update should make the response stale");
    };
    assert_eq!(completion.await.expect("foreground outcome delivered"), InventoryFetchOutcome::Failed);
    assert!(inventory.snapshot().item(ItemKey::new(owner, ItemId::new(Uuid::from_u128(44_000)))).is_none());
    assert_eq!(inventory.snapshot().category(root).map(|category| category.server_version), Some(ServerVersion::from_raw(8)));
    assert!(!inventory.snapshot().is_complete(root));

    let retry = peer.next().await;
    assert_eq!(retry.path, format!("/ais/category/{}/children?depth=0", root.id().uuid()));
    retry.respond(200, empty_folder_response(root, 9)).await;
    let (fetch, applied) = take_folder_event(next_event(&mut events).await, root);
    let FolderFetchApply::Applied(_) = apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("apply retry") else {
        panic!("retry response should apply");
    };
    assert_matches!(next_event(&mut events).await, InventoryFetchEvent::BackgroundDone);
    assert!(inventory.snapshot().is_complete(root));

    drop(handle);
    drop(requests);
    stop.send(()).expect("stop scheduler");
    task.await.expect("scheduler stopped");
}

#[tokio::test]
async fn unrelated_change_keeps_fetch_fresh() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let owner = InventoryOwner::new(Uuid::from_u128(51));
    let library_owner = InventoryOwner::new(Uuid::from_u128(52));
    let root = key(owner, 53);
    let library = key(library_owner, 54);
    let mut personal_root = category(root, None);
    personal_root.server_version = ServerVersion::from_raw(7);
    personal_root.server_descendent_count = Some(1);
    let mut library_root = category(library, None);
    library_root.server_version = ServerVersion::from_raw(3);
    library_root.server_descendent_count = Some(0);
    let mut inventory = Inventory::new(personal_root, Some(library_root), InventoryLimits::default()).expect("valid inventory roots");
    let target_generation = inventory.snapshot().folder_generation(root).expect("personal folder generation is tracked");
    let library_generation = inventory.snapshot().folder_generation(library).expect("library folder generation is tracked");

    let (requests, mut events, model, task, _cancel, stop) = scheduler(client, inventory.snapshot());
    peer.grant(&[("InventoryAPIv3", "/ais")]).await;
    let request = peer.next().await;
    assert_eq!(request.path, format!("/ais/category/{}/children?depth=16", root.id().uuid()));

    inventory
        .apply_delta(InventoryDelta { updated_category_versions: vec![(library, ServerVersion::from_raw(4))], ..InventoryDelta::default() })
        .expect("update unrelated library folder");
    let changed_snapshot = inventory.snapshot();
    assert_eq!(changed_snapshot.folder_generation(root), Some(target_generation));
    assert_ne!(changed_snapshot.folder_generation(library), Some(library_generation));
    model.send_replace(Some(changed_snapshot));

    request.respond(200, folder_response(root, 1, 53_000, 8)).await;
    let (fetch, applied) = take_folder_event(next_event(&mut events).await, root);
    assert_eq!(fetch.provenance, FolderFetchProvenance::StartedAt { folder: Some(target_generation), owner: 0 });
    let FolderFetchApply::Applied(_) =
        apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("apply unchanged folder response")
    else {
        panic!("unrelated folder change should not stale this response");
    };
    assert!(inventory.snapshot().is_complete(root));
    assert_eq!(inventory.snapshot().child_count(root), 1);
    assert_matches!(next_event(&mut events).await, InventoryFetchEvent::BackgroundDone);

    drop(requests);
    stop.send(()).expect("stop scheduler");
    task.await.expect("scheduler stopped");
}

#[tokio::test]
async fn parent_update_stales_child() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let owner = InventoryOwner::new(Uuid::from_u128(61));
    let root = key(owner, 62);
    let child = key(owner, 63);
    let item_id = Uuid::from_u128(64);
    let mut child_category = category(child, Some(root));
    child_category.server_version = ServerVersion::from_raw(10);
    child_category.server_descendent_count = Some(0);
    let inventory = Inventory::from_categories(category(root, None), None, vec![child_category], InventoryLimits::default())
        .expect("valid inventory folders");
    let child_generation = inventory.snapshot().folder_generation(child).expect("child generation is tracked");

    let (requests, mut events, model, task, _cancel, stop) = scheduler(client, inventory.snapshot());
    peer.grant(&[("InventoryAPIv3", "/ais")]).await;
    let request = peer.next().await;
    assert_eq!(request.path, format!("/ais/category/{}/children?depth=16", root.id().uuid()));
    let mut recursive_child =
        ais_category(child.id().uuid(), owner.id(), Some(root.id().uuid()), format!("folder-{}", child.id().uuid().as_u128()), 11);
    let Value::Map(fields) = &mut recursive_child else { unreachable!() };
    fields.insert(
        "_embedded".into(),
        Value::Map(Map::from([
            ("categories".into(), Value::Map(Map::new())),
            ("items".into(), Value::Map(Map::from([(item_id.to_string(), ais_item(child, item_id))]))),
            ("links".into(), Value::Map(Map::new())),
        ])),
    );
    request.respond(200, ais_root_response(root, Map::from([(child.id().uuid().to_string(), recursive_child)]))).await;

    let (root_fetch, root_applied) = take_folder_event(next_event(&mut events).await, root);
    let mut inventory = inventory;
    let FolderFetchApply::Applied(_) =
        apply_and_acknowledge(&mut inventory, &model, root_fetch, root_applied).expect("apply root response")
    else {
        panic!("root response should apply");
    };
    let current_child_generation = inventory.snapshot().folder_generation(child).expect("child remains present");
    assert_ne!(current_child_generation, child_generation);
    assert!(!inventory.snapshot().is_complete(child));

    let (child_fetch, child_applied) = take_folder_event(next_event(&mut events).await, child);
    assert_eq!(child_fetch.provenance, FolderFetchProvenance::StartedAt { folder: Some(child_generation), owner: 0 });
    let FolderFetchApply::Stale =
        apply_and_acknowledge(&mut inventory, &model, child_fetch, child_applied).expect("discard child response")
    else {
        panic!("parent update should stale the recursive child response");
    };
    assert!(inventory.snapshot().item(ItemKey::new(owner, ItemId::new(item_id))).is_none());

    let retry = peer.next().await;
    assert_eq!(retry.path, format!("/ais/category/{}/children?depth=16", child.id().uuid()));
    retry.respond(200, folder_response(child, 1, item_id.as_u128(), 12)).await;
    let (child_fetch, child_applied) = take_folder_event(next_event(&mut events).await, child);
    let FolderFetchApply::Applied(_) =
        apply_and_acknowledge(&mut inventory, &model, child_fetch, child_applied).expect("apply child refetch")
    else {
        panic!("child refetch should apply");
    };
    assert!(inventory.snapshot().is_complete(child));
    assert!(inventory.snapshot().item(ItemKey::new(owner, ItemId::new(item_id))).is_some());
    assert_matches!(next_event(&mut events).await, InventoryFetchEvent::BackgroundDone);

    drop(requests);
    stop.send(()).expect("stop scheduler");
    task.await.expect("scheduler stopped");
}

#[tokio::test]
async fn rejected_folder_retries() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let owner = InventoryOwner::new(Uuid::from_u128(1));
    let root = key(owner, 45);
    let child = key(owner, 46);
    let mut inventory = Inventory::new(category(root, None), None, InventoryLimits::default()).expect("valid inventory root");
    let (requests, mut events, model, task, _cancel, stop) = scheduler(client, inventory.snapshot());
    peer.grant(&[("InventoryAPIv3", "/ais")]).await;

    let first = peer.next().await;
    first.respond(200, root_response_with_mismatched_category_key(root, child.id().uuid(), Uuid::from_u128(47))).await;
    let InventoryFetchEvent::Failed { folder, error } = next_event(&mut events).await else {
        panic!("expected first folder response to fail validation");
    };
    assert_eq!(folder, root);
    assert_matches!(error, InventoryFetchError::InvalidResponse(DecodeError::UnexpectedStructure(_)));
    assert!(!inventory.snapshot().is_complete(root));

    requests.sender.send(ForegroundRequest::background(root)).await.expect("queue retry after rejected folder response");
    let retry = peer.next().await;
    assert_eq!(retry.path, format!("/ais/category/{}/children?depth=0", root.id().uuid()));
    retry.respond(200, empty_folder_response(root, 2)).await;
    let fetch = loop {
        match next_event(&mut events).await {
            InventoryFetchEvent::Folder { fetch, applied } => break (fetch, applied),
            InventoryFetchEvent::BackgroundDone => {}
            InventoryFetchEvent::Failed { folder, error } => panic!("retry failed for {folder:?}: {error}"),
        }
    };
    let (fetch, applied) = fetch;
    assert_eq!(fetch.folder, root);
    apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("retry applies clean response");
    assert!(inventory.snapshot().is_complete(root));

    drop(requests);
    stop.send(()).expect("stop scheduler after retry");
    task.await.expect("scheduler stopped");
}

#[tokio::test]
async fn refetch_signal_fetches_complete_folder() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let owner = InventoryOwner::new(Uuid::from_u128(1));
    let root = key(owner, 47);
    let mut root_category = category(root, None);
    root_category.server_version = ServerVersion::from_raw(1);
    root_category.server_descendent_count = Some(0);
    let mut inventory = Inventory::new(root_category, None, InventoryLimits::default()).expect("valid complete inventory root");
    assert!(inventory.snapshot().is_complete(root));

    let (requests, mut events, model, task, _cancel, stop) = scheduler(client, inventory.snapshot());
    peer.grant(&[("InventoryAPIv3", "/ais")]).await;
    assert_matches!(next_event(&mut events).await, InventoryFetchEvent::BackgroundDone);

    let changes = inventory
        .apply_delta(InventoryDelta { updated_category_versions: vec![(root, ServerVersion::from_raw(0))], ..InventoryDelta::default() })
        .expect("older live version requests a refetch");
    assert_eq!(changes.refetch_categories, [root]);
    model.send_replace(Some(inventory.snapshot()));
    let (_status, status_receiver) = watch::channel(InventoryLoadingStatus::default());
    let handle =
        InventoryFetchHandle { requests: requests.clone(), status: status_receiver, edits: InventoryEditController::new().handle() };
    handle.refetch_folder(root).expect("queue live-update refetch");
    let request = peer.next().await;
    assert_eq!(request.path, format!("/ais/category/{}/children?depth=0", root.id().uuid()));
    request.respond(200, empty_folder_response(root, 2)).await;

    let InventoryFetchEvent::Folder { fetch, applied } = next_event(&mut events).await else {
        panic!("expected folder fetch after refetch signal");
    };
    assert_eq!(fetch.folder, root);
    apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("apply live-update refetch response");

    drop(requests);
    stop.send(()).expect("stop scheduler");
    task.await.expect("scheduler stopped");
}

#[tokio::test]
async fn parent_metadata_requeues_child() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let owner = InventoryOwner::new(Uuid::from_u128(1));
    let root = key(owner, 48);
    let child = key(owner, 49);
    let mut inventory = Inventory::new(category(root, None), None, InventoryLimits::default()).expect("valid inventory root");
    inventory.upsert_category(category(child, Some(root))).expect("valid child folder");
    let (requests, mut events, model, task, _cancel, stop) = scheduler(client, inventory.snapshot());
    peer.grant(&[("InventoryAPIv3", "/ais")]).await;

    let first = peer.next().await;
    let second = peer.next().await;
    let (root_request, child_request) =
        if first.path.ends_with(&format!("{}/children?depth=16", root.id().uuid())) { (first, second) } else { (second, first) };
    assert_eq!(root_request.path, format!("/ais/category/{}/children?depth=16", root.id().uuid()));
    assert_eq!(child_request.path, format!("/ais/category/{}/children?depth=16", child.id().uuid()));

    child_request.respond(200, empty_folder_response(child, 10)).await;
    let (child_fetch, child_applied) = take_folder_event(next_event(&mut events).await, child);
    apply_and_acknowledge(&mut inventory, &model, child_fetch, child_applied).expect("apply first child response");
    assert!(inventory.snapshot().is_complete(child));

    root_request.respond(200, root_response_with_child_version(root, child, 11)).await;
    let (fetch, applied) = take_folder_event(next_event(&mut events).await, root);
    let FolderFetchApply::Stale = apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("discard stale parent response")
    else {
        panic!("child update should stale the parent response");
    };
    assert!(inventory.snapshot().is_complete(child));

    let parent_retry = peer.next().await;
    assert_eq!(parent_retry.path, format!("/ais/category/{}/children?depth=0", root.id().uuid()));
    parent_retry.respond(200, root_response_with_child_version(root, child, 11)).await;
    let (fetch, applied) = take_folder_event(next_event(&mut events).await, root);
    let FolderFetchApply::Applied(_) = apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("apply parent retry") else {
        panic!("parent retry should apply");
    };
    assert!(!inventory.snapshot().is_complete(child));

    let child_retry = peer.next().await;
    assert_eq!(child_retry.path, format!("/ais/category/{}/children?depth=0", child.id().uuid()));
    child_retry.respond(200, empty_folder_response(child, 12)).await;
    let (fetch, applied) = take_folder_event(next_event(&mut events).await, child);
    apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("apply child retry");
    assert!(inventory.snapshot().is_complete(child));
    assert_matches!(next_event(&mut events).await, InventoryFetchEvent::BackgroundDone);

    drop(requests);
    stop.send(()).expect("stop scheduler after child retry");
    task.await.expect("scheduler stopped");
}

#[tokio::test]
async fn foreground_runs_beside_background() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let owner = InventoryOwner::new(Uuid::from_u128(1));
    let root = key(owner, 1);
    let first = key(owner, 2);
    let second = key(owner, 3);
    let grandchild = key(owner, 4);
    let mut root_cat = category(root, None);
    root_cat.server_version = ServerVersion::from_raw(1);
    root_cat.server_descendent_count = Some(2);
    let mut first_cat = category(first, Some(root));
    first_cat.server_version = ServerVersion::from_raw(1);
    first_cat.server_descendent_count = Some(1);
    let mut inventory = Inventory::new(root_cat, None, InventoryLimits::default()).expect("valid inventory root");
    inventory.upsert_category(first_cat).expect("add first child");
    inventory.upsert_category(category(second, Some(root))).expect("add second child");
    inventory.upsert_category(category(grandchild, Some(first))).expect("add grandchild");

    let (requests, mut events, model, task, _cancel, stop) = scheduler(client, inventory.snapshot());
    peer.grant(&[("InventoryAPIv3", "/ais")]).await;

    let first_bg = peer.next().await;
    let second_bg = peer.next().await;
    let second_path = format!("/ais/category/{}/children?depth=16", second.id().uuid());
    let grandchild_path = format!("/ais/category/{}/children?depth=16", grandchild.id().uuid());
    let (bg_second, bg_grandchild) = if first_bg.path == second_path { (first_bg, second_bg) } else { (second_bg, first_bg) };
    assert_eq!(bg_second.path, second_path);
    assert_eq!(bg_grandchild.path, grandchild_path);

    let extra = key(owner, 5);
    inventory.upsert_category(category(extra, Some(first))).expect("add extra foreground folder");
    model.send_replace(Some(inventory.snapshot()));
    let handle = fetch_handle(requests.clone());
    let fg_completion = handle.request_folder(extra).expect("enqueue foreground folder");

    let foreground = peer.next().await;
    assert_eq!(foreground.path, format!("/ais/category/{}/children?depth=0", extra.id().uuid()));
    foreground.respond(200, empty_folder_response(extra, 2)).await;
    let (fetch, applied) = take_folder_event(next_event(&mut events).await, extra);
    apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("apply foreground response");
    assert_eq!(fg_completion.await.expect("foreground outcome delivered"), InventoryFetchOutcome::Succeeded);

    bg_second.respond(200, empty_folder_response(second, 2)).await;
    bg_grandchild.respond(200, empty_folder_response(grandchild, 2)).await;
    let mut completed = HashSet::new();
    while completed.len() < 2 {
        match next_event(&mut events).await {
            InventoryFetchEvent::Folder { fetch, applied } => {
                let folder = fetch.folder;
                apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("apply background response");
                completed.insert(folder);
            }
            event => panic!("expected background folder response, received {event:?}"),
        }
    }
    assert_eq!(completed, HashSet::from([second, grandchild]));
    assert_matches!(next_event(&mut events).await, InventoryFetchEvent::BackgroundDone);

    drop(handle);
    drop(requests);
    stop.send(()).expect("stop foreground scheduler");
    task.await.expect("scheduler stopped");
}

#[tokio::test]
async fn cancellation_drops_foreground_completion() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let owner = InventoryOwner::new(Uuid::from_u128(1));
    let root = key(owner, 50);
    let inventory = Inventory::new(category(root, None), None, InventoryLimits::default()).expect("valid inventory root");
    let (requests, _events, _model, task, cancel, _stop) = scheduler(client, inventory.snapshot());
    peer.grant(&[("InventoryAPIv3", "/ais")]).await;
    let background = peer.next().await;
    assert_eq!(background.path, format!("/ais/category/{}/children?depth=16", root.id().uuid()));
    let handle = fetch_handle(requests.clone());
    let completion = handle.request_folder(root).expect("queue foreground folder");
    cancel.send(()).expect("cancel session");
    background.wait_for_close().await;
    timeout(TEST_TIMEOUT, task).await.expect("cancelled scheduler timeout").expect("scheduler task");
    assert!(completion.await.is_err());
    assert!(!inventory.snapshot().is_complete(root));
}

#[tokio::test]
async fn huge_inventory_stays_bounded() {
    const ITEMS_PER_FOLDER: usize = 50_000;
    const _: () = assert!(MAX_INVENTORY_ITEMS >= ITEMS_PER_FOLDER * 2);
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let (mut inventory, root, first, second) = inventory_with_children();
    for folder in [first, second] {
        let mut child = inventory.snapshot().category(folder).expect("child folder exists").clone();
        child.server_version = ServerVersion::from_raw(0);
        child.server_descendent_count = Some(0);
        inventory.upsert_category(child).expect("mark child folder complete");
    }
    let started = Instant::now();
    let (requests, mut events, model, task, _cancel, stop) = scheduler(client, inventory.snapshot());
    peer.grant(&[("InventoryAPIv3", "/ais")]).await;

    let root_request = peer.next().await;
    root_request.respond(200, root_response(root, &[first, second])).await;
    let (root_fetch, root_applied) = take_folder_event(next_event(&mut events).await, root);
    apply_and_acknowledge(&mut inventory, &model, root_fetch, root_applied).expect("apply root response");

    let batch_request = peer.next().await;
    assert_eq!(
        batch_request.path,
        format!("/ais/category/{}/children?depth=16&children={},{}", root.id().uuid(), first.id().uuid(), second.id().uuid())
    );
    batch_request.respond(403, "").await;
    for depth in [8, 4, 2, 1, 0] {
        let req = peer.next().await;
        assert_eq!(
            req.path,
            format!("/ais/category/{}/children?depth={depth}&children={},{}", root.id().uuid(), first.id().uuid(), second.id().uuid())
        );
        req.respond(403, "").await;
    }

    let first_request = peer.next().await;
    let second_request = peer.next().await;
    let inflight = [first_request.path.as_str(), second_request.path.as_str()].into_iter().collect::<HashSet<_>>();
    let first_path = format!("/ais/category/{}/children?depth=0", first.id().uuid());
    let second_path = format!("/ais/category/{}/children?depth=0", second.id().uuid());
    assert_eq!(inflight, HashSet::from([first_path.as_str(), second_path.as_str()]));
    let peak_in_flight = 2;
    assert!(peak_in_flight <= MAX_BACKGROUND_CONCURRENCY);

    let first_body = folder_response(first, ITEMS_PER_FOLDER, 10_000, 1);
    let second_body = folder_response(second, ITEMS_PER_FOLDER, 100_000, 1);
    let largest_response = first_body.len().max(second_body.len());
    assert!(largest_response <= MAX_FETCH_RESPONSE_BYTES);
    first_request.respond(200, first_body).await;
    second_request.respond(200, second_body).await;

    let mut applied_folders = HashSet::new();
    while applied_folders.len() < 2 {
        let (fetch, applied) = match next_event(&mut events).await {
            InventoryFetchEvent::Folder { fetch, applied } => (fetch, applied),
            event => panic!("expected large folder response, received {event:?}"),
        };
        assert!(fetch.folder == first || fetch.folder == second);
        let folder = fetch.folder;
        let FolderFetchApply::Applied(_) =
            apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("apply large folder response")
        else {
            panic!("large folder response should apply: {folder:?}");
        };
        applied_folders.insert(folder);
    }
    assert_matches!(next_event(&mut events).await, InventoryFetchEvent::BackgroundDone);
    let snapshot = inventory.snapshot();
    assert_eq!(snapshot.items().count(), ITEMS_PER_FOLDER * 2);
    assert_eq!(snapshot.categories().count(), 3);
    assert!(snapshot.is_complete(root));
    assert!(snapshot.is_complete(first));
    assert!(snapshot.is_complete(second));
    drop(requests);
    stop.send(()).expect("stop large inventory scheduler");
    task.await.expect("scheduler stopped");
    eprintln!(
        "loaded {} items in {:?}; peak in-flight requests: {}; largest response: {} bytes",
        ITEMS_PER_FOLDER * 2,
        started.elapsed(),
        peak_in_flight,
        largest_response
    );
}

#[tokio::test]
async fn owner_refresh_reconciles_deep_folders() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let personal = InventoryOwner::new(Uuid::from_u128(701));
    let library = InventoryOwner::new(Uuid::from_u128(702));
    let personal_root = key(personal, 1_000);
    let library_root = key(library, 2_000);
    let mut personal_root_category = category(personal_root, None);
    personal_root_category.server_version = ServerVersion::from_raw(1);
    personal_root_category.server_descendent_count = Some(1);
    let mut library_root_category = category(library_root, None);
    library_root_category.server_version = ServerVersion::from_raw(1);
    library_root_category.server_descendent_count = Some(1);
    let mut inventory = Inventory::new(personal_root_category, Some(library_root_category), InventoryLimits::default())
        .expect("valid personal and Library roots");
    let mut children_by_folder = HashMap::<CategoryKey, Vec<CategoryKey>>::new();
    let depth = usize::from(MAX_AIS_DEPTH) + 1;
    let mut add_owner_tree = |owner: InventoryOwner, root: CategoryKey, first_id: u128, leaf_ids: (u128, u128)| {
        let mut path = vec![root];
        let mut parent = root;
        for level in 1..=depth {
            let folder = key(owner, first_id + level as u128);
            let child_count = if level == depth { 2 } else { 1 };
            let mut value = category(folder, Some(parent));
            value.server_version = ServerVersion::from_raw(1);
            value.server_descendent_count = Some(child_count);
            inventory.upsert_category(value).expect("add nested folder");
            children_by_folder.entry(parent).or_default().push(folder);
            path.push(folder);
            parent = folder;
        }
        let source = key(owner, leaf_ids.0);
        let target = key(owner, leaf_ids.1);
        for (folder, child_count) in [(source, if owner == personal { 4 } else { 0 }), (target, 0)] {
            let mut value = category(folder, Some(parent));
            value.server_version = ServerVersion::from_raw(1);
            value.server_descendent_count = Some(child_count);
            inventory.upsert_category(value).expect("add deep leaf folder");
            children_by_folder.entry(parent).or_default().push(folder);
        }
        (path, source, target)
    };
    let (personal_path, personal_source, personal_target) = add_owner_tree(personal, personal_root, 1_000, (1_100, 1_101));
    let (library_path, library_source, library_target) = add_owner_tree(library, library_root, 2_000, (2_100, 2_101));
    drop(add_owner_tree);

    let rename_id = Uuid::from_u128(90_001);
    let update_id = Uuid::from_u128(90_002);
    let delete_id = Uuid::from_u128(90_003);
    let move_id = Uuid::from_u128(90_004);
    let rename_key = ItemKey::new(personal, ItemId::new(rename_id));
    let update_key = ItemKey::new(personal, ItemId::new(update_id));
    let delete_key = ItemKey::new(personal, ItemId::new(delete_id));
    let move_key = ItemKey::new(personal, ItemId::new(move_id));
    inventory.upsert_item(item(rename_key, personal_source, "old name", "fixture", 0)).expect("insert item before lost rename");
    inventory.upsert_item(item(update_key, personal_source, "same name", "old description", 0)).expect("insert item before lost update");
    inventory.upsert_item(item(delete_key, personal_source, "delete me", "fixture", 0)).expect("insert item before lost delete");
    inventory.upsert_item(item(move_key, personal_source, "move me", "fixture", 0)).expect("insert item before lost move");
    let initial = inventory.snapshot();
    assert!(initial.owner_is_complete(personal));
    assert!(initial.owner_is_complete(library));
    let expected_root_children = initial.children(personal_root).collect::<HashSet<_>>();
    let expected_root_count = initial.child_count(personal_root);
    let mut expected = inventory.clone();
    let renamed = item(rename_key, personal_source, "new name", "fixture", 0);
    let updated = item(update_key, personal_source, "same name", "new description", 8);
    let moved = item(move_key, personal_target, "move me", "fixture", 0);
    let FolderFetchApply::Applied(_) = apply_inventory_fetch(
        &mut expected,
        FolderFetch {
            folder: personal_source,
            version: ServerVersion::from_raw(1),
            descendent_count: Some(2),
            children: vec![InventoryChild::Item(renamed.clone()), InventoryChild::Item(updated.clone())],
            provenance: FolderFetchProvenance::Untracked,
        },
    )
    .expect("sequential oracle applies source response") else {
        panic!("source response applies against the sequential oracle");
    };
    let FolderFetchApply::Applied(_) = apply_inventory_fetch(
        &mut expected,
        FolderFetch {
            folder: personal_target,
            version: ServerVersion::from_raw(1),
            descendent_count: Some(1),
            children: vec![InventoryChild::Item(moved.clone())],
            provenance: FolderFetchProvenance::Untracked,
        },
    )
    .expect("sequential oracle applies target response") else {
        panic!("target response applies against the sequential oracle");
    };

    let all_folders = personal_path
        .iter()
        .copied()
        .chain([personal_source, personal_target])
        .chain(library_path.iter().copied())
        .chain([library_source, library_target])
        .collect::<HashSet<_>>();
    let mut direct_counts = HashMap::new();
    for folder in &all_folders {
        let item_count = if *folder == personal_source {
            2
        } else if *folder == personal_target {
            1
        } else {
            0
        };
        direct_counts.insert(*folder, children_by_folder.get(folder).map_or(0, Vec::len) + item_count);
    }
    let mut responses = HashMap::new();
    for folder in &all_folders {
        let categories = children_by_folder
            .get(folder)
            .into_iter()
            .flatten()
            .map(|child| {
                (child.id().uuid().to_string(), ais_child_category(*child, *folder, direct_counts.get(child).copied().unwrap_or_default()))
            })
            .collect::<Map>();
        let items = if *folder == personal_source {
            Map::from([
                (rename_id.to_string(), ais_item_details(*folder, rename_id, "new name", "fixture", 0)),
                (update_id.to_string(), ais_item_details(*folder, update_id, "same name", "new description", 8)),
            ])
        } else if *folder == personal_target {
            Map::from([(move_id.to_string(), ais_item_details(*folder, move_id, "move me", "fixture", 0))])
        } else {
            Map::new()
        };
        responses.insert(*folder, ais_folder_response(*folder, categories, items));
    }

    let (requests, mut events, model, task, _cancel, stop) = scheduler(client, initial);
    peer.grant(&[("InventoryAPIv3", "/ais"), ("LibraryAPIv3", "/ais")]).await;
    assert_matches!(next_event(&mut events).await, InventoryFetchEvent::BackgroundDone);
    inventory.invalidate_all_owners().expect("invalidate personal and Library contents");
    model.send_replace(Some(inventory.snapshot()));
    let handle = InventoryFetchHandle {
        requests: requests.clone(),
        status: watch::channel(InventoryLoadingStatus::default()).1,
        edits: InventoryEditController::new().handle(),
    };
    handle.refetch_incomplete().expect("request bounded owner refresh");

    let mut requested = HashSet::new();
    let mut applied_folders = HashSet::new();
    loop {
        tokio::select! {
            request = peer.next() => {
                let folder = all_folders.iter().copied().find(|folder| {
                    request.path == format!("/ais/category/{}/children?depth=0", folder.id().uuid())
                }).expect("owner refresh requests each folder directly at depth zero");
                requested.insert(folder);
                request.respond(200, responses.get(&folder).expect("response exists for each folder").clone()).await;
            }
            event = events.recv() => {
                match event.expect("owner refresh keeps its scheduler event channel open") {
                    InventoryFetchEvent::Folder { fetch, applied } => {
                        let folder = fetch.folder;
                        let outcome = apply_and_acknowledge(&mut inventory, &model, fetch, applied);
                        match outcome.expect("refreshed response agrees with the sequential oracle") {
                            FolderFetchApply::Applied(_) => { applied_folders.insert(folder); }
                            FolderFetchApply::Stale => {}
                        }
                    }
                    InventoryFetchEvent::Failed { folder, error } => panic!("refresh of {folder:?} failed: {error}"),
                    InventoryFetchEvent::BackgroundDone => break,
                }
            }
        }
    }

    assert_eq!(requested, all_folders);
    assert_eq!(applied_folders, all_folders);
    let actual = inventory.snapshot();
    let expected = expected.snapshot();
    for folder in &all_folders {
        assert_eq!(actual.category(*folder), expected.category(*folder));
        assert_eq!(actual.children(*folder).collect::<HashSet<_>>(), expected.children(*folder).collect::<HashSet<_>>());
        assert_eq!(actual.child_count(*folder), expected.child_count(*folder));
        assert!(actual.is_complete(*folder));
    }
    assert_eq!(actual.item(rename_key), expected.item(rename_key));
    assert_eq!(actual.item(update_key), expected.item(update_key));
    assert!(actual.item(delete_key).is_none());
    assert_eq!(actual.item(move_key).map(|item| item.parent), Some(personal_target));
    assert_eq!(actual.item(move_key), expected.item(move_key));
    assert_eq!(actual.children(personal_root).collect::<HashSet<_>>(), expected_root_children);
    assert_eq!(actual.child_count(personal_root), expected_root_count);
    assert!(actual.owner_is_complete(personal));
    assert!(actual.owner_is_complete(library));

    drop(handle);
    drop(requests);
    stop.send(()).expect("stop owner refresh scheduler");
    task.await.expect("scheduler stopped");
}

#[tokio::test]
async fn library_accepts_nil_linden_owners() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let personal = InventoryOwner::new(Uuid::from_u128(100));
    let library = InventoryOwner::new(Uuid::from_u128(101));
    let linden = Uuid::from_u128(102);
    let personal_root = key(personal, 103);
    let library_root = key(library, 104);
    let library_item_id = Uuid::from_u128(105);
    let mut inventory = Inventory::new(category(personal_root, None), Some(category(library_root, None)), InventoryLimits::default())
        .expect("valid inventory roots");
    let (requests, mut events, model, task, _cancel, stop) = scheduler(client, inventory.snapshot());
    peer.grant(&[("InventoryAPIv3", "/ais"), ("FetchLibDescendents2", "/lib")]).await;

    let first = peer.next().await;
    let second = peer.next().await;
    let personal_path = format!("/ais/category/{}/children?depth=16", personal_root.id().uuid());
    let (personal_req, library_req) = if first.path == personal_path { (first, second) } else { (second, first) };
    assert_eq!(personal_req.path, personal_path);
    assert_eq!(library_req.path, "/lib");
    personal_req.respond(200, empty_folder_response(personal_root, 1)).await;

    let library_body = xml::to_vec(&Value::Map(Map::from([(
        "folders".into(),
        Value::Array(vec![Value::Map(Map::from([
            ("folder_id".into(), Value::Uuid(library_root.id().uuid())),
            ("owner_id".into(), Value::Uuid(Uuid::nil())),
            ("version".into(), Value::Integer(1)),
            ("descendents".into(), Value::Integer(1)),
            ("categories".into(), Value::Array(Vec::new())),
            (
                "items".into(),
                Value::Array(vec![Value::Map(Map::from([
                    ("item_id".into(), Value::Uuid(library_item_id)),
                    ("owner_id".into(), Value::Uuid(linden)),
                    ("parent_id".into(), Value::Uuid(library_root.id().uuid())),
                    ("name".into(), Value::String("Library Note".into())),
                    ("desc".into(), Value::String(String::new())),
                    ("type".into(), Value::Integer(7)),
                    ("inv_type".into(), Value::Integer(7)),
                ]))]),
            ),
        ]))]),
    )])))
    .expect("encode library descendents");
    library_req.respond(200, library_body).await;

    let mut done = false;
    while !done {
        match next_event(&mut events).await {
            InventoryFetchEvent::Folder { fetch, applied } => {
                apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("apply folder");
            }
            InventoryFetchEvent::Failed { folder, error } => panic!("unexpected failure for {folder:?}: {error}"),
            InventoryFetchEvent::BackgroundDone => done = true,
        }
    }
    assert!(inventory.snapshot().is_complete(personal_root));
    assert!(inventory.snapshot().is_complete(library_root));
    assert!(inventory.snapshot().item(ItemKey::new(library, ItemId::new(library_item_id))).is_some());

    drop(requests);
    stop.send(()).expect("stop scheduler");
    task.await.expect("scheduler stopped");
}

#[tokio::test]
async fn malformed_response_keeps_cause() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let capability = CapabilityUrl::parse(peer.url("/ais").as_str()).expect("valid AIS URL");
    let owner = InventoryOwner::new(Uuid::from_u128(110));
    let root = key(owner, 111);
    let responder = tokio::spawn(async move {
        let request = peer.next().await;
        request.respond(200, "<llsd><not-valid-xml").await;
    });

    let result = fetch_ais(&client, &capability, root, 0).await;
    responder.await.expect("peer responder");
    assert_matches!(result, Err(InventoryFetchError::InvalidResponse(DecodeError::MalformedLlsd)));
}

#[tokio::test(start_paused = true)]
async fn retry_after_paces_attempts() {
    let mut attempts = Vec::new();
    let result = retry_transient(None, || {
        attempts.push(time::Instant::now());
        let attempt = attempts.len();
        async move {
            if attempt == 1 {
                Err(RequestFailure::Transient { cause: FetchFailureCause::HttpStatus(429), retry_after: Some(Duration::from_secs(5)) })
            } else {
                Ok(())
            }
        }
    })
    .await;

    assert_eq!(result, Ok(()));
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[1].duration_since(attempts[0]), Duration::from_secs(5));
}

#[tokio::test(start_paused = true)]
async fn exhausted_retries_keep_last_failure() {
    let mut attempts = 0_u32;
    let result: Result<(), RequestFailure> = retry_transient(None, || {
        let attempt = attempts;
        attempts += 1;
        async move {
            let status = if attempt < MAX_FETCH_RETRIES { 429 } else { 503 };
            Err(RequestFailure::Transient { cause: FetchFailureCause::HttpStatus(status), retry_after: Some(Duration::ZERO) })
        }
    })
    .await;

    assert_eq!(attempts, MAX_FETCH_RETRIES + 1);
    assert_matches!(result, Err(RequestFailure::Transient { cause: FetchFailureCause::HttpStatus(503), .. }));
}

#[tokio::test]
async fn retry_exhaustion_reports_status() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let capability = CapabilityUrl::parse(peer.url("/ais").as_str()).expect("valid AIS URL");
    let owner = InventoryOwner::new(Uuid::from_u128(120));
    let root = key(owner, 121);

    let exhaust_task = tokio::spawn({
        let client = client.clone();
        let capability = capability.clone();
        async move { fetch_ais(&client, &capability, root, 0).await }
    });
    for attempt in 0..=MAX_FETCH_RETRIES {
        let req = peer.next().await;
        let status = if attempt < MAX_FETCH_RETRIES { 429 } else { 503 };
        req.respond_with_headers(status, "", &[("Retry-After", "0")]).await;
    }
    let error = exhaust_task.await.expect("exhaust task").expect_err("exhausted retries should fail");
    assert_eq!(error, InventoryFetchError::RetryExhausted(FetchFailureCause::HttpStatus(503)));
    assert!(error.to_string().contains("HTTP 503"));
}

#[test]
fn timeout_cancel_keep_cause() {
    assert_eq!(classify_request_error(network::Error::CapabilityRequestCanceled), RequestFailure::Cancelled);
    assert_eq!(classify_request_error(network::Error::CapabilitySessionClosed), RequestFailure::Cancelled);
    assert_eq!(retry_delay(0, Some(Duration::from_secs(86_400))), MAX_RETRY_AFTER);
    let error = InventoryFetchError::RetryExhausted(FetchFailureCause::Timeout);
    assert!(error.to_string().contains("timed out"));
}

#[tokio::test]
async fn logout_cancellation_emits_zero_failures() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let owner = InventoryOwner::new(Uuid::from_u128(130));
    let root = key(owner, 131);
    let inventory = Inventory::new(category(root, None), None, InventoryLimits::default()).expect("valid inventory root");

    let (requests, mut events, _model, task, cancel, _stop) = scheduler(client.clone(), inventory.snapshot());
    let _seed_req = peer.next().await;
    cancel.send(()).expect("cancel during discover");
    task.await.expect("scheduler exits cleanly during discover");
    drop(requests);
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, InventoryFetchEvent::Failed { .. }), "unexpected failure event during discover cancel: {event:?}");
    }

    let mut peer2 = LoopbackPeer::start().await;
    let client2 = Client::new(peer2.url("/seed")).expect("active Tokio runtime");
    let (requests2, mut events2, _model2, task2, cancel2, _stop2) = scheduler(client2, inventory.snapshot());
    peer2.grant(&[("InventoryAPIv3", "/ais")]).await;
    let in_flight = peer2.next().await;
    in_flight.respond_with_headers(429, "", &[("Retry-After", "10")]).await;
    cancel2.send(()).expect("cancel during retry backoff");
    task2.await.expect("scheduler exits cleanly during retry backoff");
    drop(requests2);
    while let Ok(event) = events2.try_recv() {
        assert!(!matches!(event, InventoryFetchEvent::Failed { .. }), "unexpected failure event during backoff cancel: {event:?}");
    }
}

#[test]
fn background_queue_gauge_tracks_peak() {
    let depth = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    {
        let gauge = BackgroundQueueGauge { depth: Arc::clone(&depth), peak: Arc::clone(&peak) };
        gauge.set(5);
        assert_eq!((depth.load(Ordering::Relaxed), peak.load(Ordering::Relaxed)), (5, 5));
        gauge.set(2);
        assert_eq!((depth.load(Ordering::Relaxed), peak.load(Ordering::Relaxed)), (2, 5));
    }
    assert_eq!((depth.load(Ordering::Relaxed), peak.load(Ordering::Relaxed)), (0, 5));
}

fn inventory_with_independent_leaves(count: usize) -> (Inventory, CategoryKey, Vec<CategoryKey>) {
    let owner = InventoryOwner::new(Uuid::from_u128(900));
    let root = key(owner, 901);
    let mut root_cat = category(root, None);
    root_cat.server_version = ServerVersion::from_raw(1);
    root_cat.server_descendent_count = Some(count as u32);
    let mut inventory = Inventory::new(root_cat, None, InventoryLimits::default()).expect("valid root");
    let mut leaves = Vec::with_capacity(count);
    for idx in 0..count {
        let parent = key(owner, 1_000 + idx as u128 * 2);
        let mut parent_cat = category(parent, Some(root));
        parent_cat.server_version = ServerVersion::from_raw(1);
        parent_cat.server_descendent_count = Some(1);
        inventory.upsert_category(parent_cat).expect("add complete parent");
        let leaf = key(owner, 1_001 + idx as u128 * 2);
        inventory.upsert_category(category(leaf, Some(parent))).expect("add incomplete leaf");
        leaves.push(leaf);
    }
    (inventory, root, leaves)
}

fn folder_from_path(path: &str, owner: InventoryOwner) -> CategoryKey {
    let id = path
        .strip_prefix("/ais/category/")
        .and_then(|rest| rest.split('/').next())
        .and_then(|uuid| Uuid::parse_str(uuid).ok())
        .expect("AIS category UUID in path");
    CategoryKey::new(owner, FolderId::new(id))
}

#[tokio::test]
async fn background_concurrency_ceiling() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let (mut inventory, root, leaves) = inventory_with_independent_leaves(MAX_BACKGROUND_CONCURRENCY + 4);
    let owner = root.owner();
    let (requests, mut events, model, task, _cancel, stop) = scheduler(client, inventory.snapshot());
    peer.grant(&[("InventoryAPIv3", "/ais")]).await;

    let mut held = Vec::with_capacity(MAX_BACKGROUND_CONCURRENCY);
    for _ in 0..MAX_BACKGROUND_CONCURRENCY {
        held.push(peer.next().await);
    }
    assert!(
        timeout(Duration::from_millis(50), peer.next()).await.is_err(),
        "background concurrency must not exceed MAX_BACKGROUND_CONCURRENCY ({MAX_BACKGROUND_CONCURRENCY})"
    );

    let first = held.remove(0);
    let folder = folder_from_path(&first.path, owner);
    first.respond(200, empty_folder_response(folder, 2)).await;
    let (fetch, applied) = take_folder_event(next_event(&mut events).await, folder);
    apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("apply completed leaf");

    let replacement = peer.next().await;
    held.push(replacement);
    assert_eq!(held.len(), MAX_BACKGROUND_CONCURRENCY);

    for req in held {
        let f = folder_from_path(&req.path, owner);
        req.respond(200, empty_folder_response(f, 2)).await;
    }
    let mut done = false;
    while !done {
        tokio::select! {
            req = peer.next() => {
                let f = folder_from_path(&req.path, owner);
                req.respond(200, empty_folder_response(f, 2)).await;
            }
            event = events.recv() => match event.expect("event channel open") {
                InventoryFetchEvent::Folder { fetch, applied } => {
                    apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("apply leaf");
                }
                InventoryFetchEvent::BackgroundDone => done = true,
                InventoryFetchEvent::Failed { folder, error } => panic!("unexpected failure for {folder:?}: {error}"),
            }
        }
    }
    assert_eq!(leaves.iter().filter(|f| inventory.snapshot().is_complete(**f)).count(), leaves.len());

    drop(requests);
    stop.send(()).expect("stop ceiling scheduler");
    task.await.expect("scheduler stopped");
}

#[tokio::test]
async fn foreground_uses_reserved_slot() {
    let mut peer = LoopbackPeer::start().await;
    let client = Client::new(peer.url("/seed")).expect("active Tokio runtime");
    let (mut inventory, root, _leaves) = inventory_with_independent_leaves(MAX_BACKGROUND_CONCURRENCY + 2);
    let owner = root.owner();
    let fg_folder = key(owner, 9_999);
    let (requests, mut events, model, task, _cancel, stop) = scheduler(client, inventory.snapshot());
    peer.grant(&[("InventoryAPIv3", "/ais")]).await;

    let mut held = Vec::with_capacity(MAX_BACKGROUND_CONCURRENCY);
    for _ in 0..MAX_BACKGROUND_CONCURRENCY {
        held.push(peer.next().await);
    }

    inventory.upsert_category(category(fg_folder, Some(root))).expect("add foreground folder");
    model.send_replace(Some(inventory.snapshot()));
    let handle = fetch_handle(requests.clone());
    let fg_completion = handle.request_folder(fg_folder).expect("queue foreground folder");

    let fg_req = timeout(TEST_TIMEOUT, peer.next()).await.expect("foreground uses the reserved 20th slot");
    assert_eq!(fg_req.path, format!("/ais/category/{}/children?depth=0", fg_folder.id().uuid()));
    assert_eq!(held.len() + 1, MAX_TOTAL_CONCURRENCY);
    fg_req.respond(200, empty_folder_response(fg_folder, 2)).await;
    let (fetch, applied) = take_folder_event(next_event(&mut events).await, fg_folder);
    apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("apply foreground folder");
    assert_eq!(fg_completion.await.expect("foreground completes"), InventoryFetchOutcome::Succeeded);

    for req in held {
        let f = folder_from_path(&req.path, owner);
        req.respond(200, empty_folder_response(f, 2)).await;
    }
    let mut done = false;
    while !done {
        tokio::select! {
            req = peer.next() => {
                let f = folder_from_path(&req.path, owner);
                req.respond(200, empty_folder_response(f, 2)).await;
            }
            event = events.recv() => match event.expect("event channel open") {
                InventoryFetchEvent::Folder { fetch, applied } => {
                    apply_and_acknowledge(&mut inventory, &model, fetch, applied).expect("apply remaining leaf");
                }
                InventoryFetchEvent::BackgroundDone => done = true,
                InventoryFetchEvent::Failed { folder, error } => panic!("unexpected failure for {folder:?}: {error}"),
            }
        }
    }

    drop(handle);
    drop(requests);
    stop.send(()).expect("stop reserved-slot scheduler");
    task.await.expect("scheduler stopped");
}

#[test]
fn rate_limit_halves_and_waits() {
    let mut c = AdaptiveConcurrency::new();
    let now = time::Instant::now();
    c.on_backoff(now, 0, Some(Duration::from_secs(5)));
    c.on_backoff(now, 0, Some(Duration::from_secs(5)));

    assert_eq!(c.limit, 4, "two rate limits halve concurrency from 19 -> 9 -> 4");
    assert_eq!(c.backoff_deadline(now), Some(now + Duration::from_secs(5)));
    assert!(!c.can_launch_background(0, 0, now + Duration::from_secs(4)), "no background requests during Retry-After");
    assert!(c.can_launch_background(3, 3, now + Duration::from_secs(5)));
    assert!(!c.can_launch_background(4, 4, now + Duration::from_secs(5)));
    assert_eq!(c.backoff_deadline(now + Duration::from_secs(5)), None);
}

#[test]
fn concurrency_recovers_after_backoff() {
    let mut c = AdaptiveConcurrency::new();
    let now = time::Instant::now();
    for _ in 0..3 {
        c.on_backoff(now, 0, Some(Duration::from_secs(2)));
    }
    assert_eq!(c.limit, 2, "three rate limits reduce concurrency from 19 -> 9 -> 4 -> 2");

    c.on_success(now + Duration::from_secs(1));
    c.on_success(now + Duration::from_secs(1));
    assert_eq!(c.limit, 2, "successes during backoff do not raise the limit");

    let after = now + Duration::from_secs(2);
    for _ in 0..RECOVERY_SUCCESS_THRESHOLD {
        c.on_success(after);
    }
    assert_eq!(c.limit, 3);

    for _ in 0..MAX_BACKGROUND_CONCURRENCY * RECOVERY_SUCCESS_THRESHOLD {
        c.on_success(after);
    }
    assert_eq!(c.limit, MAX_BACKGROUND_CONCURRENCY);
    assert!(!c.can_launch_background(0, MAX_TOTAL_CONCURRENCY, after), "the reserved slot stays free");
}
