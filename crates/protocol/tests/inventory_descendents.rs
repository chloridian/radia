use llsd::{Map, Value, xml};
use protocol::inventory::{DecodeError, RequestedFolder, decode_fetch_inventory_descendents2};
use uuid::Uuid;

fn item(id: Uuid, parent: Uuid, owner: Uuid, asset_id: Uuid, asset_type: i32, inventory_type: i32) -> Value {
    Value::Map(Map::from([
        ("item_id".into(), Value::Uuid(id)),
        ("owner_id".into(), Value::Uuid(owner)),
        ("parent_id".into(), Value::Uuid(parent)),
        ("asset_id".into(), Value::Uuid(asset_id)),
        ("type".into(), Value::Integer(asset_type)),
        ("inv_type".into(), Value::Integer(inventory_type)),
        ("name".into(), Value::String("inventory item".into())),
    ]))
}

fn folder(id: Uuid, owner: Uuid, items: Vec<Value>, descendent_count: i32) -> Value {
    Value::Map(Map::from([
        ("folder_id".into(), Value::Uuid(id)),
        ("owner_id".into(), Value::Uuid(owner)),
        ("version".into(), Value::Integer(8)),
        ("descendents".into(), Value::Integer(descendent_count)),
        ("categories".into(), Value::Array(Vec::new())),
        ("items".into(), Value::Array(items)),
    ]))
}

fn response(folders: Vec<Value>) -> Vec<u8> {
    let body = Value::Map(Map::from([("folders".into(), Value::Array(folders))]));
    xml::to_vec(&body).expect("legacy response fixture encodes")
}

#[test]
fn link_target_keeps_supplied_parent() {
    let owner = Uuid::from_u128(1);
    let folder_id = Uuid::from_u128(2);
    let link_id = Uuid::from_u128(3);
    let target_parent = Uuid::from_u128(4);
    let target_id = Uuid::from_u128(5);
    let link = item(link_id, folder_id, owner, target_id, 24, 24);
    let target = item(target_id, target_parent, owner, Uuid::from_u128(6), 7, 7);
    let bytes = response(vec![folder(folder_id, owner, vec![link, target], 2)]);

    let decoded = decode_fetch_inventory_descendents2(&bytes, &[RequestedFolder { id: folder_id, owner_id: owner }])
        .expect("link target in another folder is an auxiliary record");

    let folder = &decoded[0];
    assert_eq!(folder.direct_items.len(), 1);
    assert_eq!(folder.direct_items[0].id, link_id);
    assert_eq!(folder.direct_items[0].parent_id, Some(folder_id));
    assert_eq!(folder.auxiliary_items.len(), 1);
    assert_eq!(folder.auxiliary_items[0].id, target_id);
    assert_eq!(folder.auxiliary_items[0].owner_id, Some(owner));
    assert_eq!(folder.auxiliary_items[0].parent_id, Some(target_parent));
    assert_eq!(folder.descendent_count, 2);
}

#[test]
fn duplicate_identity_is_rejected() {
    let owner = Uuid::from_u128(1);
    let folder_id = Uuid::from_u128(2);
    let duplicate_id = Uuid::from_u128(3);
    let direct = item(duplicate_id, folder_id, owner, Uuid::from_u128(4), 7, 7);
    let auxiliary = item(duplicate_id, Uuid::from_u128(5), owner, Uuid::from_u128(6), 7, 7);
    let bytes = response(vec![folder(folder_id, owner, vec![direct, auxiliary], 2)]);

    assert_eq!(
        decode_fetch_inventory_descendents2(&bytes, &[RequestedFolder { id: folder_id, owner_id: owner }]),
        Err(DecodeError::InvalidResponse)
    );
}

#[test]
fn foreign_auxiliary_owner_is_rejected() {
    let owner = Uuid::from_u128(1);
    let folder_id = Uuid::from_u128(2);
    let target = item(Uuid::from_u128(3), Uuid::from_u128(4), Uuid::from_u128(5), Uuid::from_u128(6), 7, 7);
    let bytes = response(vec![folder(folder_id, owner, vec![target], 1)]);

    assert_eq!(
        decode_fetch_inventory_descendents2(&bytes, &[RequestedFolder { id: folder_id, owner_id: owner }]),
        Err(DecodeError::InvalidResponse)
    );
}

#[test]
fn reverse_batch_order_tracks_requests() {
    let owner = Uuid::from_u128(1);
    let requested = (100..111).map(|id| RequestedFolder { id: Uuid::from_u128(id), owner_id: owner }).collect::<Vec<_>>();
    let folders = requested.iter().rev().map(|requested_folder| folder(requested_folder.id, owner, Vec::new(), 0)).collect();

    let decoded = decode_fetch_inventory_descendents2(&response(folders), &requested).expect("reverse-ordered eleven-folder response");

    assert_eq!(decoded.iter().map(|folder| folder.id).collect::<Vec<_>>(), requested.iter().map(|folder| folder.id).collect::<Vec<_>>());
    assert!(decoded.iter().all(|folder| {
        folder.direct_categories.is_empty()
            && folder.direct_items.is_empty()
            && folder.auxiliary_categories.is_empty()
            && folder.auxiliary_items.is_empty()
    }));
}
