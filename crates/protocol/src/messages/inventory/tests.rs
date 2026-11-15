use uuid::Uuid;

use crate::{
    LLUdpError,
    lludp::{Message, codec::Writer},
    messages::{
        BulkUpdateInventory, InventoryFolderBlock, InventoryItemBlock, MoveInventoryFolder, MoveInventoryFolderBlock, MoveInventoryItem,
        MoveInventoryItemBlock, RemoveInventoryFolder, RemoveInventoryItem, RemoveInventoryObjects, SaveAssetIntoInventory,
        UpdateCreateInventoryItem,
    },
};

const TRUNCATION_CASES: [&str; 7] = [
    "UpdateCreateInventoryItem",
    "RemoveInventoryItem",
    "RemoveInventoryFolder",
    "RemoveInventoryObjects",
    "SaveAssetIntoInventory",
    "BulkUpdateInventory",
    "MoveInventoryItem",
];

fn uuid(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

fn item() -> InventoryItemBlock {
    InventoryItemBlock {
        item_id: uuid(3),
        folder_id: uuid(4),
        callback_id: 5,
        creator_id: uuid(6),
        owner_id: uuid(7),
        group_id: uuid(8),
        base_mask: 9,
        owner_mask: 10,
        group_mask: 11,
        everyone_mask: 12,
        next_owner_mask: 13,
        group_owned: true,
        asset_id: uuid(14),
        asset_type: 6,
        inventory_type: 6,
        flags: 15,
        sale_type: 1,
        sale_price: 16,
        name: "item".to_owned(),
        description: "description".to_owned(),
        creation_date: 17,
        crc: 18,
    }
}

fn messages() -> [Message; 8] {
    [
        Message::UpdateCreateInventoryItem(UpdateCreateInventoryItem {
            agent_id: uuid(1),
            sim_approved: true,
            transaction_id: uuid(2),
            inventory_data: vec![item()],
            invalid_blocks: 0,
        }),
        Message::RemoveInventoryItem(RemoveInventoryItem {
            agent_id: uuid(1),
            session_id: uuid(2),
            inventory_data: vec![uuid(3)],
            invalid_blocks: 0,
        }),
        Message::RemoveInventoryFolder(RemoveInventoryFolder {
            agent_id: uuid(1),
            session_id: uuid(2),
            folder_data: vec![uuid(3)],
            invalid_blocks: 0,
        }),
        Message::RemoveInventoryObjects(RemoveInventoryObjects {
            agent_id: uuid(1),
            session_id: uuid(2),
            folder_data: vec![uuid(3)],
            item_data: vec![uuid(4)],
            invalid_blocks: 0,
        }),
        Message::SaveAssetIntoInventory(SaveAssetIntoInventory { agent_id: uuid(1), item_id: uuid(2), new_asset_id: uuid(3) }),
        Message::BulkUpdateInventory(BulkUpdateInventory {
            agent_id: uuid(1),
            transaction_id: uuid(2),
            folder_data: vec![InventoryFolderBlock { folder_id: uuid(3), parent_id: uuid(4), folder_type: 6, name: "folder".to_owned() }],
            item_data: vec![item()],
            invalid_blocks: 0,
        }),
        Message::MoveInventoryItem(MoveInventoryItem {
            agent_id: uuid(1),
            session_id: uuid(2),
            stamp: true,
            inventory_data: vec![MoveInventoryItemBlock { item_id: uuid(3), folder_id: uuid(4), new_name: "renamed".to_owned() }],
            invalid_blocks: 0,
        }),
        Message::MoveInventoryFolder(MoveInventoryFolder {
            agent_id: uuid(1),
            session_id: uuid(2),
            stamp: false,
            inventory_data: vec![MoveInventoryFolderBlock { folder_id: uuid(3), parent_id: uuid(4) }],
            invalid_blocks: 0,
        }),
    ]
}

fn encode(message: &Message) -> Vec<u8> {
    let mut payload = Vec::new();
    message.encode_payload(&mut payload, usize::MAX).expect("encode inventory message");
    payload
}

#[test]
fn truncation_fails() {
    let messages = messages();
    for (case, message) in TRUNCATION_CASES.into_iter().zip(&messages) {
        let id = message.id();
        let mut payload = encode(message);
        payload.pop();
        assert_eq!(Message::decode(id, &payload), Err(LLUdpError::TruncatedPacket), "{case}");
    }
}

#[test]
fn moves_are_outbound() {
    let messages = messages();
    for (case, message) in ["MoveInventoryItem", "MoveInventoryFolder"].into_iter().zip(&messages[6..]) {
        assert!(!message.is_inbound_only(), "{case}");
    }
}

#[test]
fn nil_agent_is_rejected() {
    for message in messages() {
        let id = message.id();
        let mut payload = encode(&message);
        payload[..16].fill(0);
        assert_eq!(Message::decode(id, &payload), Err(LLUdpError::InvalidMessage));
    }
}

#[test]
fn invalid_block_isolated() {
    let message = Message::RemoveInventoryItem(RemoveInventoryItem {
        agent_id: uuid(1),
        session_id: uuid(2),
        inventory_data: vec![uuid(3), uuid(4)],
        invalid_blocks: 0,
    });
    let id = message.id();
    let mut payload = encode(&message);
    payload[33..49].fill(0);
    let (decoded, trailing) = Message::decode(id, &payload).expect("framing remains valid");
    assert!(trailing.is_empty());
    let Message::RemoveInventoryItem(decoded) = decoded else { panic!("remove message should remain typed") };
    assert_eq!(decoded.invalid_blocks, 1);
    assert_eq!(decoded.inventory_data, vec![uuid(4)]);
}

#[test]
fn malformed_count_fails() {
    let mut output = Vec::new();
    let mut writer = Writer::new(&mut output);
    writer.uuid(uuid(1));
    writer.uuid(uuid(2));
    writer.u8(1);
    assert_eq!(Message::decode(crate::schema::ids::REMOVE_INVENTORY_ITEM, &output), Err(LLUdpError::TruncatedPacket));
}
