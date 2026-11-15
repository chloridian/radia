use llsd::{Map, Value, xml};
use protocol::inventory::{DecodeError, FolderDescendents, RequestedFolder, decode_fetch_inventory_descendents2};
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

fn decode_capture(body: &str) -> FolderDescendents {
    let mut folders =
        decode_fetch_inventory_descendents2(body.as_bytes(), &[RequestedFolder { id: Uuid::from_u128(1), owner_id: Uuid::from_u128(2) }])
            .expect("captured response decodes");
    assert_eq!(folders.len(), 1);
    folders.remove(0)
}

#[test]
fn link_capture_splits_by_parent() {
    let folder = decode_capture(include_str!("fixtures/legacy_one_link.xml"));

    assert_eq!(folder.id, Uuid::from_u128(1));
    assert_eq!(folder.owner_id, Some(Uuid::from_u128(2)));
    assert_eq!(folder.version, 2);
    assert_eq!(folder.descendent_count, 1);
    assert!(folder.direct_categories.is_empty());
    assert!(folder.auxiliary_categories.is_empty());
    assert_eq!(folder.direct_items.len(), 1);
    assert_eq!(folder.auxiliary_items.len(), 1);

    let link = &folder.direct_items[0];
    assert_eq!(link.id, Uuid::from_u128(6));
    assert_eq!(link.owner_id, Some(Uuid::from_u128(2)));
    assert_eq!(link.parent_id, Some(Uuid::from_u128(1)));
    assert_eq!(link.asset_id, Some(Uuid::from_u128(3)));
    assert_eq!(link.asset_type, Some(24));
    assert_eq!(link.inventory_type, Some(7));

    let target = &folder.auxiliary_items[0];
    assert_eq!(target.id, Uuid::from_u128(3));
    assert_eq!(target.owner_id, Some(Uuid::from_u128(2)));
    assert_eq!(target.parent_id, Some(Uuid::from_u128(4)));
    assert_eq!(target.asset_id, Some(Uuid::from_u128(5)));
    assert_eq!(target.asset_type, Some(7));
    assert_eq!(target.inventory_type, Some(7));
}

#[test]
fn mixed_capture_splits_by_parent() {
    let folder = decode_capture(include_str!("fixtures/legacy_link_and_item.xml"));

    assert_eq!(folder.id, Uuid::from_u128(1));
    assert_eq!(folder.owner_id, Some(Uuid::from_u128(2)));
    assert_eq!(folder.version, 7);
    assert_eq!(folder.descendent_count, 2);
    assert!(folder.direct_categories.is_empty());
    assert!(folder.auxiliary_categories.is_empty());
    assert_eq!(folder.direct_items.len(), 2);
    assert_eq!(folder.auxiliary_items.len(), 1);
    assert_eq!(folder.direct_items.iter().map(|item| item.id).collect::<Vec<_>>(), vec![Uuid::from_u128(6), Uuid::from_u128(7)]);
    assert!(folder.direct_items.iter().all(|item| item.owner_id == Some(Uuid::from_u128(2)) && item.parent_id == Some(Uuid::from_u128(1))));

    let target = &folder.auxiliary_items[0];
    assert_eq!(target.id, Uuid::from_u128(3));
    assert_eq!(target.owner_id, Some(Uuid::from_u128(2)));
    assert_eq!(target.parent_id, Some(Uuid::from_u128(4)));
    assert_eq!(target.asset_id, Some(Uuid::from_u128(5)));
    assert_eq!(target.asset_type, Some(7));
    assert_eq!(target.inventory_type, Some(7));
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
