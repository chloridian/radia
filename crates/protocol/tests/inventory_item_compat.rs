use protocol::inventory::{
    AisEditResponse, DecodeError, Item, Permissions, RequestedFolder, SaleInfo, decode_ais_edit_response, decode_ais_item,
    decode_fetch_inventory_descendents2, decode_fetch_inventory2,
};
use uuid::Uuid;

fn id(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

fn ais_item(item_id: Uuid, parent_id: Uuid, owner_id: Uuid, fields: &str) -> Vec<u8> {
    format!(
        "<llsd><map><key>item_id</key><uuid>{item_id}</uuid><key>agent_id</key><uuid>{owner_id}</uuid><key>parent_id</key><uuid>{parent_id}</uuid><key>name</key><string>note</string>{fields}</map></llsd>"
    )
    .into_bytes()
}

fn legacy_item(item_id: Uuid, parent_id: Uuid, owner_id: Uuid, fields: &str) -> String {
    format!(
        "<map><key>item_id</key><uuid>{item_id}</uuid><key>owner_id</key><uuid>{owner_id}</uuid><key>parent_id</key><uuid>{parent_id}</uuid><key>name</key><string>note</string>{fields}</map>"
    )
}

fn flat_fields() -> String {
    format!(
        "<key>creator_id</key><uuid>{}</uuid><key>owner_id</key><uuid>{}</uuid><key>group_id</key><uuid>{}</uuid><key>last_owner_id</key><uuid>{}</uuid><key>base_mask</key><integer>1</integer><key>owner_mask</key><integer>2</integer><key>group_mask</key><integer>3</integer><key>everyone_mask</key><integer>4</integer><key>next_owner_mask</key><integer>5</integer><key>sale_type</key><integer>6</integer><key>sale_price</key><integer>7</integer>",
        id(11),
        id(12),
        id(13),
        id(14),
    )
}

fn assert_flat_item(item: &Item) {
    assert_eq!(
        item.permissions,
        Permissions {
            creator_id: Some(id(11)),
            owner_id: Some(id(12)),
            group_id: Some(id(13)),
            last_owner_id: Some(id(14)),
            base_mask: Some(1),
            owner_mask: Some(2),
            group_mask: Some(3),
            everyone_mask: Some(4),
            next_owner_mask: Some(5),
        }
    );
    assert_eq!(item.sale_info, SaleInfo { sale_type: Some(6), sale_price: Some(7) });
}

#[test]
fn flat_item_fields_are_preserved() {
    let item_id = id(1);
    let parent_id = id(2);
    let owner_id = id(12);
    let decoded = decode_ais_item(&ais_item(item_id, parent_id, owner_id, &flat_fields()), item_id, owner_id)
        .expect("flat permission and sale fields are accepted");

    assert_eq!(decoded.id, item_id);
    assert_eq!(decoded.parent_id, Some(parent_id));
    assert_eq!(decoded.owner_id, Some(owner_id));
    assert_flat_item(&decoded);
}

#[test]
fn nested_fields_override_flat_fields() {
    let item_id = id(1);
    let parent_id = id(2);
    let owner_id = id(12);
    let flat = flat_fields()
        .replace(&format!("<key>owner_id</key><uuid>{}</uuid>", id(12)), "<key>owner_id</key><string>invalid flat UUID</string>")
        .replace("<key>base_mask</key><integer>1</integer>", "<key>base_mask</key><string>invalid flat mask</string>")
        .replace("<key>sale_type</key><integer>6</integer>", "<key>sale_type</key><string>invalid flat sale type</string>");
    let fields = format!(
        "{flat}<key>permissions</key><map><key>owner_id</key><uuid>{}</uuid><key>base_mask</key><integer>22</integer></map><key>sale_info</key><map><key>sale_type</key><integer>33</integer></map>",
        id(22),
    );
    let decoded = decode_ais_item(&ais_item(item_id, parent_id, owner_id, &fields), item_id, owner_id)
        .expect("nested values override only their matching flat fields");

    assert_eq!(
        decoded.permissions,
        Permissions {
            creator_id: Some(id(11)),
            owner_id: Some(id(22)),
            group_id: Some(id(13)),
            last_owner_id: Some(id(14)),
            base_mask: Some(22),
            owner_mask: Some(2),
            group_mask: Some(3),
            everyone_mask: Some(4),
            next_owner_mask: Some(5),
        }
    );
    assert_eq!(decoded.sale_info, SaleInfo { sale_type: Some(33), sale_price: Some(7) });
}

#[test]
fn malformed_flat_fields_are_rejected() {
    let item_id = id(1);
    let parent_id = id(2);
    let folder_id = id(3);
    let owner_id = id(12);
    for fields in [
        "<key>creator_id</key><uuid>not-a-uuid</uuid>",
        "<key>owner_id</key><string>not-a-uuid</string>",
        "<key>group_id</key><uuid>not-a-uuid</uuid>",
        "<key>last_owner_id</key><uuid>not-a-uuid</uuid>",
        "<key>base_mask</key><string>not-an-integer</string>",
        "<key>owner_mask</key><string>not-an-integer</string>",
        "<key>group_mask</key><string>not-an-integer</string>",
        "<key>everyone_mask</key><string>not-an-integer</string>",
        "<key>next_owner_mask</key><string>not-an-integer</string>",
        "<key>sale_type</key><string>not-an-integer</string>",
        "<key>sale_price</key><string>not-an-integer</string>",
    ] {
        let ais = ais_item(item_id, parent_id, owner_id, fields);
        assert!(
            matches!(decode_ais_item(&ais, item_id, owner_id), Err(DecodeError::MalformedLlsd | DecodeError::UnexpectedStructure(_))),
            "malformed flat field must be validated: {fields}"
        );
        assert!(matches!(decode_ais_edit_response(&ais, owner_id), Err(DecodeError::MalformedLlsd | DecodeError::UnexpectedStructure(_))));

        let legacy = legacy_item(item_id, folder_id, owner_id, fields);
        let fetched = format!("<llsd><map><key>items</key><array>{legacy}</array></map></llsd>");
        assert!(matches!(
            decode_fetch_inventory2(fetched.as_bytes(), owner_id, &[item_id]),
            Err(DecodeError::MalformedLlsd | DecodeError::UnexpectedStructure(_))
        ));
        let descendents = format!(
            "<llsd><map><key>folders</key><array><map><key>folder_id</key><uuid>{folder_id}</uuid><key>owner_id</key><uuid>{owner_id}</uuid><key>version</key><integer>1</integer><key>descendents</key><integer>1</integer><key>categories</key><array/><key>items</key><array>{legacy}</array></map></array></map></llsd>"
        );
        assert!(matches!(
            decode_fetch_inventory_descendents2(descendents.as_bytes(), &[RequestedFolder { id: folder_id, owner_id }]),
            Err(DecodeError::MalformedLlsd | DecodeError::UnexpectedStructure(_))
        ));
    }
}

#[test]
fn edit_item_preserves_flat_fields() {
    let item_id = id(1);
    let parent_id = id(2);
    let owner_id = id(12);
    let decoded = decode_ais_edit_response(&ais_item(item_id, parent_id, owner_id, &flat_fields()), owner_id)
        .expect("AIS edit item fields are decoded");

    assert_eq!(
        decoded,
        AisEditResponse {
            category: None,
            item: Some(Item {
                id: item_id,
                owner_id: Some(owner_id),
                parent_id: Some(parent_id),
                asset_id: None,
                asset_type: None,
                inventory_type: None,
                name: "note".into(),
                description: String::new(),
                flags: 0,
                created_at: 0,
                permissions: Permissions {
                    creator_id: Some(id(11)),
                    owner_id: Some(id(12)),
                    group_id: Some(id(13)),
                    last_owner_id: Some(id(14)),
                    base_mask: Some(1),
                    owner_mask: Some(2),
                    group_mask: Some(3),
                    everyone_mask: Some(4),
                    next_owner_mask: Some(5),
                },
                sale_info: SaleInfo { sale_type: Some(6), sale_price: Some(7) },
            }),
            created_categories: Vec::new(),
            created_items: Vec::new(),
            updated_categories: Vec::new(),
            updated_items: Vec::new(),
            removed_items: Vec::new(),
            categories_removed: Vec::new(),
            category_items_removed: Vec::new(),
            broken_links_removed: Vec::new(),
            updated_category_versions: Default::default(),
        }
    );
}

#[test]
fn legacy_decoders_keep_flat_fields() {
    let folder_id = id(3);
    let item_id = id(1);
    let owner_id = id(12);
    let parent_id = folder_id;
    let fields = flat_fields();
    let item = legacy_item(item_id, parent_id, owner_id, &fields);
    let descendents = format!(
        "<llsd><map><key>folders</key><array><map><key>folder_id</key><uuid>{folder_id}</uuid><key>owner_id</key><uuid>{owner_id}</uuid><key>version</key><integer>1</integer><key>descendents</key><integer>1</integer><key>categories</key><array/><key>items</key><array>{item}</array></map></array></map></llsd>"
    );
    let fetched = format!("<llsd><map><key>items</key><array>{item}</array></map></llsd>");

    let decoded = decode_fetch_inventory_descendents2(descendents.as_bytes(), &[RequestedFolder { id: folder_id, owner_id }])
        .expect("legacy folder records accept flat item fields");
    let direct_item = &decoded[0].direct_items[0];
    assert_flat_item(direct_item);

    let decoded = decode_fetch_inventory2(fetched.as_bytes(), owner_id, &[item_id]).expect("legacy fetched items accept flat fields");
    assert_flat_item(&decoded[0]);
}
