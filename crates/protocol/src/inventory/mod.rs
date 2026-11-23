mod wire;

use std::{collections::BTreeMap, error, fmt};

use uuid::Uuid;

pub use self::wire::{
    decode_ais_children, decode_ais_edit_response, decode_ais_item, decode_ais_library_children, decode_created_category_response,
    decode_fetch_inventory_descendents2, decode_fetch_inventory2, decode_fetch_lib_descendents2,
};
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_RESPONSE_DEPTH: usize = 192;
const MAX_RESPONSE_VALUES: usize = 4_000_000;
const MAX_CONTAINER_ITEMS: usize = 1_000_000;
const MAX_STRING_BYTES: usize = 4_096;
const MAX_MAP_KEY_BYTES: usize = 128;
const MAX_NAME_BYTES: usize = 1_024;
const MAX_DESCRIPTION_BYTES: usize = 4_096;
const MAX_HYPERMEDIA_URI_BYTES: usize = 2_048;
const MAX_AIS_DEPTH: usize = 50;
const MAX_CATEGORIES: usize = 131_074;
const MAX_ITEMS: usize = 1_000_000;
const MAX_FOLDER_CHILDREN: usize = 100_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    TooLarge,
    MalformedLlsd,
    UnexpectedStructure(&'static str),
    OwnerMismatch,
    ParentMismatch,
    DuplicateId,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => formatter.write_str("inventory response exceeds a decoding limit"),
            Self::MalformedLlsd => formatter.write_str("inventory response contains malformed LLSD"),
            Self::UnexpectedStructure(detail) => write!(formatter, "inventory response has unexpected structure: {detail}"),
            Self::OwnerMismatch => formatter.write_str("inventory response owner does not match requested owner"),
            Self::ParentMismatch => formatter.write_str("inventory response parent does not match expected folder"),
            Self::DuplicateId => formatter.write_str("inventory response contains a duplicate identifier"),
        }
    }
}

impl error::Error for DecodeError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Category {
    pub id: Uuid,
    pub owner_id: Option<Uuid>,
    pub parent_id: Option<Uuid>,
    pub name: String,
    pub version: Option<i32>,
    pub descendent_count: Option<u32>,
    pub preferred_folder_type: Option<i32>,
    pub children: Option<Vec<Child>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Child {
    Category(Category),
    Item(Item),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Item {
    pub id: Uuid,
    pub owner_id: Option<Uuid>,
    pub parent_id: Option<Uuid>,
    pub asset_id: Option<Uuid>,
    pub asset_type: Option<i32>,
    pub inventory_type: Option<i32>,
    pub name: String,
    pub description: String,
    pub flags: u32,
    pub created_at: i64,
    pub permissions: Permissions,
    pub sale_info: SaleInfo,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Permissions {
    pub creator_id: Option<Uuid>,
    pub owner_id: Option<Uuid>,
    pub group_id: Option<Uuid>,
    pub last_owner_id: Option<Uuid>,
    pub base_mask: Option<u32>,
    pub owner_mask: Option<u32>,
    pub group_mask: Option<u32>,
    pub everyone_mask: Option<u32>,
    pub next_owner_mask: Option<u32>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SaleInfo {
    pub sale_type: Option<i32>,
    pub sale_price: Option<i32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FolderDescendents {
    pub id: Uuid,
    pub owner_id: Option<Uuid>,
    pub version: i32,
    pub descendent_count: u32,
    pub direct_categories: Vec<Category>,
    pub direct_items: Vec<Item>,
    pub auxiliary_categories: Vec<Category>,
    pub auxiliary_items: Vec<Item>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequestedFolder {
    pub id: Uuid,
    pub owner_id: Uuid,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AisEditResponse {
    pub category: Option<Category>,
    pub item: Option<Item>,
    pub created_categories: Vec<Uuid>,
    pub created_items: Vec<Uuid>,
    pub updated_categories: Vec<Uuid>,
    pub updated_items: Vec<Uuid>,
    pub removed_items: Vec<Uuid>,
    pub categories_removed: Vec<Uuid>,
    pub category_items_removed: Vec<Uuid>,
    pub broken_links_removed: Vec<Uuid>,
    pub updated_category_versions: BTreeMap<Uuid, i32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatedCategoryResponse {
    pub id: Uuid,
    pub parent_id: Uuid,
    pub preferred_folder_type: i32,
    pub name: String,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use llsd::{Map, Value, xml};

    use super::*;

    fn map_value(fields: impl IntoIterator<Item = (String, Value)>) -> Value {
        Value::Map(fields.into_iter().collect::<BTreeMap<_, _>>())
    }

    fn category(id: Uuid, parent: Option<Uuid>, descendent_count: usize) -> Value {
        let mut fields = Map::from([
            ("category_id".into(), Value::Uuid(id)),
            ("name".into(), Value::String("folder".into())),
            ("version".into(), Value::Integer(4)),
            ("descendents".into(), Value::Integer(descendent_count as i32)),
        ]);
        if let Some(parent) = parent {
            fields.insert("parent_id".into(), Value::Uuid(parent));
        }
        Value::Map(fields)
    }

    fn item(id: Uuid, parent: Uuid, owner: Uuid) -> Value {
        Value::Map(Map::from([
            ("item_id".into(), Value::Uuid(id)),
            ("agent_id".into(), Value::Uuid(owner)),
            ("created_at".into(), Value::Integer(1_700_000_000)),
            ("parent_id".into(), Value::Uuid(parent)),
            ("name".into(), Value::String("note".into())),
            ("desc".into(), Value::String("text".into())),
            ("type".into(), Value::Integer(7)),
            ("inv_type".into(), Value::Integer(7)),
        ]))
    }

    fn legacy_item(id: Uuid, parent: Uuid, owner: Uuid) -> Value {
        let Value::Map(mut fields) = item(id, parent, owner) else { unreachable!() };
        fields.remove("agent_id");
        fields.insert("owner_id".into(), Value::Uuid(owner));
        Value::Map(fields)
    }

    fn item_with_types(id: Uuid, parent: Uuid, owner: Uuid, asset_type: Value, inventory_type: Value) -> Value {
        Value::Map(Map::from([
            ("item_id".into(), Value::Uuid(id)),
            ("agent_id".into(), Value::Uuid(owner)),
            ("created_at".into(), Value::Integer(1_700_000_000)),
            ("parent_id".into(), Value::Uuid(parent)),
            ("name".into(), Value::String("item".into())),
            ("type".into(), asset_type),
            ("inv_type".into(), inventory_type),
        ]))
    }

    fn encode(value: &Value) -> Vec<u8> {
        xml::to_vec(value).expect("LLSD fixture encodes")
    }

    fn keyed(values: Vec<Value>, id_field: &str) -> Value {
        Value::Map(
            values
                .into_iter()
                .map(|value| {
                    let fields = value.as_map().expect("keyed child is a map");
                    let Some(Value::Uuid(id)) = fields.get(id_field) else { panic!("child ID is a UUID") };
                    (id.to_string(), value)
                })
                .collect(),
        )
    }

    #[test]
    fn ais_edit_decodes_metadata() {
        let ids = (1..=16).map(Uuid::from_u128).collect::<Vec<_>>();
        let metadata = Value::Map(Map::from([
            ("_created_categories".into(), Value::Array(vec![Value::Uuid(ids[0])])),
            ("_created_items".into(), Value::Array(vec![Value::Uuid(ids[1])])),
            ("_updated_items".into(), Value::Array(vec![Value::Uuid(ids[2])])),
            ("_updated_categories".into(), Value::Array(vec![Value::Uuid(ids[3])])),
            ("_removed_items".into(), Value::Array(vec![Value::Uuid(ids[4])])),
            ("_categories_removed".into(), Value::Array(vec![Value::Uuid(ids[5])])),
            ("_category_items_removed".into(), Value::Array(vec![Value::Uuid(ids[6])])),
            ("_broken_links_removed".into(), Value::Array(vec![Value::Uuid(ids[7])])),
            ("_updated_category_versions".into(), Value::Map(Map::from([(ids[8].to_string(), Value::Integer(12))]))),
            ("_moved_items".into(), Value::Array(vec![Value::Uuid(ids[9])])),
            ("_active_gestures_removed".into(), Value::Array(vec![Value::Uuid(ids[10])])),
            ("_attachments_removed".into(), Value::Map(Map::from([("0".into(), Value::Uuid(ids[11]))]))),
            ("_wearables_removed".into(), Value::Map(Map::from([("8".into(), Value::Uuid(ids[12]))]))),
            ("_total_items_removed".into(), Value::Integer(4)),
            ("_top_category_id".into(), Value::Uuid(ids[13])),
            ("_linked_ids".into(), Value::Map(Map::from([(ids[14].to_string(), Value::Uuid(ids[15]))]))),
            (
                "_update_result".into(),
                Value::Map(Map::from([
                    ("success".into(), Value::Boolean(true)),
                    ("code".into(), Value::Integer(0)),
                    ("message".into(), Value::String("ok".into())),
                ])),
            ),
        ]));

        let response = decode_ais_edit_response(&encode(&metadata), Uuid::from_u128(99)).expect("valid AIS metadata");

        assert_eq!(response.created_categories, vec![ids[0]]);
        assert_eq!(response.created_items, vec![ids[1]]);
        assert_eq!(response.updated_items, vec![ids[2]]);
        assert_eq!(response.updated_categories, vec![ids[3]]);
        assert_eq!(response.removed_items, vec![ids[4]]);
        assert_eq!(response.categories_removed, vec![ids[5]]);
        assert_eq!(response.category_items_removed, vec![ids[6]]);
        assert_eq!(response.broken_links_removed, vec![ids[7]]);
        assert_eq!(response.updated_category_versions, BTreeMap::from([(ids[8], 12)]));
    }

    #[test]
    fn ais_edit_rejects_mistyped_metadata() {
        let cases = [
            ("_moved_items", Value::Map(Map::new())),
            ("_updated_category_versions", Value::Array(Vec::new())),
            ("_attachments_removed", Value::Array(Vec::new())),
            ("_wearables_removed", Value::Array(Vec::new())),
            ("_linked_ids", Value::Array(Vec::new())),
            ("_total_items_removed", Value::Array(Vec::new())),
            ("_update_result", Value::Array(Vec::new())),
        ];
        for (name, value) in cases {
            let response = Value::Map(Map::from([(name.into(), value)]));
            assert!(
                matches!(decode_ais_edit_response(&encode(&response), Uuid::from_u128(99)), Err(DecodeError::UnexpectedStructure(_))),
                "{name} must use its AIS schema shape"
            );
        }
    }

    #[test]
    fn ais_recursive_maps_decode() {
        let owner = Uuid::from_u128(0x99);
        let root = Uuid::from_u128(1);
        let decoded = decode_ais_children(include_bytes!("../../tests/fixtures/ais_recursive_children.llsd.xml"), root, owner)
            .expect("official AIS response shape decodes");

        assert_eq!(decoded.owner_id, Some(owner));
        assert_eq!(decoded.descendent_count, Some(1));
        let root_children = decoded.children.expect("root children are present");
        assert_eq!(root_children.len(), 1);
        let Child::Category(folder) = &root_children[0] else { panic!("expected folder") };
        assert_eq!(folder.id, Uuid::from_u128(2));
        assert_eq!(folder.descendent_count, Some(3));
        let folder_children = folder.children.as_ref().expect("folder children are present");
        assert_eq!(folder_children.len(), 3);

        let Child::Item(note) = &folder_children[0] else { panic!("expected note item") };
        assert_eq!(note.id, Uuid::from_u128(5));
        assert_eq!(note.owner_id, Some(owner));
        assert_eq!(note.created_at, 1_700_000_000);
        assert_eq!(note.permissions.owner_id, Some(Uuid::from_u128(19)));
        assert_eq!(note.permissions.creator_id, Some(Uuid::from_u128(17)));
        assert_eq!(note.sale_info.sale_type, Some(0));
        assert_eq!(note.sale_info.sale_price, Some(0));

        let Child::Item(link) = &folder_children[1] else { panic!("expected link") };
        assert_eq!(link.id, Uuid::from_u128(6));
        assert_eq!(link.asset_id, Some(Uuid::from_u128(5)));
        assert_eq!(link.asset_type, Some(24));

        let Child::Category(nested) = &folder_children[2] else { panic!("expected nested folder") };
        assert_eq!(nested.id, Uuid::from_u128(3));
        assert_eq!(nested.descendent_count, Some(1));
        let nested_children = nested.children.as_ref().expect("nested folder children are present");
        assert_eq!(nested_children.len(), 1);
        let Child::Item(nested_item) = &nested_children[0] else { panic!("expected nested item") };
        assert_eq!(nested_item.id, Uuid::from_u128(7));
    }

    #[test]
    fn embedded_keys_match_child_ids() {
        let owner = Uuid::from_u128(1);
        let root = Uuid::from_u128(2);
        let child_id = Uuid::from_u128(3);
        let wrong_key = Uuid::from_u128(4);
        let cases = [
            (
                "categories",
                Value::Map(Map::from([
                    ("agent_id".into(), Value::Uuid(owner)),
                    ("category_id".into(), Value::Uuid(child_id)),
                    ("name".into(), Value::String("folder".into())),
                    ("parent_id".into(), Value::Uuid(root)),
                    ("type_default".into(), Value::Integer(-1)),
                    ("version".into(), Value::Integer(1)),
                ])),
            ),
            ("items", item(child_id, root, owner)),
            (
                "links",
                Value::Map(Map::from([
                    ("agent_id".into(), Value::Uuid(owner)),
                    ("item_id".into(), Value::Uuid(child_id)),
                    ("linked_id".into(), Value::Uuid(Uuid::from_u128(5))),
                    ("name".into(), Value::String("link".into())),
                    ("parent_id".into(), Value::Uuid(root)),
                    ("type".into(), Value::Integer(24)),
                ])),
            ),
        ];

        for (name, child) in cases {
            let mut root_fields = Map::from([
                ("agent_id".into(), Value::Uuid(owner)),
                ("category_id".into(), Value::Uuid(root)),
                ("name".into(), Value::String("root".into())),
                ("parent_id".into(), Value::Uuid(Uuid::from_u128(6))),
                ("type_default".into(), Value::Integer(-1)),
                ("version".into(), Value::Integer(1)),
            ]);
            root_fields.insert("_embedded".into(), map_value([(name.into(), Value::Map(Map::from([(wrong_key.to_string(), child)])))]));
            assert!(
                matches!(decode_ais_children(&encode(&Value::Map(root_fields)), root, owner), Err(DecodeError::UnexpectedStructure(_))),
                "{name} map key must match the child's ID"
            );
        }
    }

    #[test]
    fn embedded_arrays_are_rejected() {
        let owner = Uuid::from_u128(1);
        let root = Uuid::from_u128(2);
        let mut root_fields = match category(root, None, 0) {
            Value::Map(fields) => fields,
            _ => unreachable!(),
        };
        root_fields.insert("_embedded".into(), map_value([("items".into(), Value::Array(Vec::new()))]));
        assert_eq!(
            decode_ais_children(&encode(&Value::Map(root_fields)), root, owner),
            Err(DecodeError::UnexpectedStructure("expected map"))
        );
    }

    #[test]
    fn partial_children_omit_count() {
        let owner = Uuid::from_u128(1);
        let root = Uuid::from_u128(2);
        let mut root_fields = match category(root, None, 0) {
            Value::Map(fields) => fields,
            _ => unreachable!(),
        };
        root_fields.insert("agent_id".into(), Value::Uuid(owner));
        root_fields
            .insert("_embedded".into(), map_value([("items".into(), keyed(vec![item(Uuid::from_u128(3), root, owner)], "item_id"))]));

        let decoded =
            decode_ais_children(&encode(&Value::Map(root_fields)), root, owner).expect("partial children are valid but incomplete");

        assert_eq!(decoded.descendent_count, None);
    }

    #[test]
    fn unknown_key_limit_is_accepted() {
        let owner = Uuid::from_u128(1);
        let folder = Uuid::from_u128(2);
        let mut fields = match category(folder, None, 0) {
            Value::Map(fields) => fields,
            _ => unreachable!(),
        };
        fields.insert("agent_id".into(), Value::Uuid(owner));
        fields.insert("x".repeat(128), Value::String("ignored".into()));

        let decoded = decode_ais_children(&encode(&Value::Map(fields)), folder, owner).expect("128-byte unknown key is accepted");

        assert_eq!(decoded.id, folder);
        assert_eq!(decoded.owner_id, Some(owner));
        assert_eq!(decoded.parent_id, None);
        assert_eq!(decoded.name, "folder");
        assert_eq!(decoded.version, Some(4));
        assert_eq!(decoded.descendent_count, None);
        assert_eq!(decoded.preferred_folder_type, None);
        assert_eq!(decoded.children, None);
    }

    #[test]
    fn unknown_key_over_limit_rejected() {
        let owner = Uuid::from_u128(1);
        let folder = Uuid::from_u128(2);
        let mut fields = match category(folder, None, 0) {
            Value::Map(fields) => fields,
            _ => unreachable!(),
        };
        fields.insert("agent_id".into(), Value::Uuid(owner));
        fields.insert("x".repeat(129), Value::String("ignored".into()));

        assert_eq!(decode_ais_children(&encode(&Value::Map(fields)), folder, owner), Err(DecodeError::TooLarge));
    }

    #[test]
    fn duplicate_key_uses_last_value() {
        let folder = Uuid::from_u128(2);
        let earlier = Uuid::from_u128(1);
        let bytes = format!(
            "<llsd><map><key>category_id</key><uuid>{earlier}</uuid><key>category_id</key><uuid>{folder}</uuid><key>name</key><integer>1</integer><key>name</key><string>last</string></map></llsd>"
        );

        let decoded = decode_ais_children(bytes.as_bytes(), folder, Uuid::from_u128(99)).expect("later map values replace earlier ones");

        assert_eq!(decoded.id, folder);
        assert_eq!(decoded.name, "last");
    }

    #[test]
    fn legacy_alias_conflict_is_invalid() {
        let owner = Uuid::from_u128(1);
        let folder = Uuid::from_u128(2);
        let child = Uuid::from_u128(3);
        let other = Uuid::from_u128(4);
        let bytes = format!(
            "<llsd><map><key>folders</key><array><map><key>folder_id</key><uuid>{folder}</uuid><key>owner_id</key><uuid>{owner}</uuid><key>version</key><integer>1</integer><key>descendents</key><integer>1</integer><key>categories</key><array><map><key>folder_id</key><uuid>{child}</uuid><key>category_id</key><uuid>{other}</uuid><key>name</key><string>child</string></map></array><key>items</key><array/></map></array></map></llsd>"
        );

        assert_eq!(
            decode_fetch_inventory_descendents2(bytes.as_bytes(), &[RequestedFolder { id: folder, owner_id: owner }]),
            Err(DecodeError::UnexpectedStructure("conflicting uuid aliases"))
        );
    }

    #[test]
    fn nested_unknown_depth_is_bounded() {
        let folder = Uuid::from_u128(2);
        let mut bytes = format!("<llsd><map><key>category_id</key><uuid>{folder}</uuid><key>ignored</key>");
        for _ in 0..MAX_RESPONSE_DEPTH {
            bytes.push_str("<map><key>x</key>");
        }
        bytes.push_str("<undef/>");
        for _ in 0..MAX_RESPONSE_DEPTH {
            bytes.push_str("</map>");
        }
        bytes.push_str("</map></llsd>");

        assert_eq!(decode_ais_children(bytes.as_bytes(), folder, Uuid::from_u128(99)), Err(DecodeError::TooLarge));
    }

    #[test]
    fn malformed_unknown_uuid_is_invalid() {
        let folder = Uuid::from_u128(2);
        let bytes = format!(
            "<llsd><map><key>category_id</key><uuid>{folder}</uuid><key>ignored</key><map><key>bad</key><uuid>not-a-uuid</uuid></map></map></llsd>"
        );

        assert_eq!(decode_ais_children(bytes.as_bytes(), folder, Uuid::from_u128(99)), Err(DecodeError::MalformedLlsd));
    }

    #[test]
    fn created_at_requires_integer() {
        let owner = Uuid::from_u128(1);
        let root = Uuid::from_u128(2);
        let mut root_fields = match category(root, None, 1) {
            Value::Map(fields) => fields,
            _ => unreachable!(),
        };
        root_fields.insert("agent_id".into(), Value::Uuid(owner));
        let Value::Map(mut item_fields) = item(Uuid::from_u128(3), root, owner) else { unreachable!() };
        item_fields.insert("created_at".into(), Value::Date(llsd::Date::UNIX_EPOCH));
        root_fields.insert(
            "_embedded".into(),
            map_value([
                ("categories".into(), Value::Map(Map::new())),
                ("items".into(), keyed(vec![Value::Map(item_fields)], "item_id")),
                ("links".into(), Value::Map(Map::new())),
            ]),
        );
        assert_eq!(
            decode_ais_children(&encode(&Value::Map(root_fields)), root, owner),
            Err(DecodeError::UnexpectedStructure("expected integer"))
        );
    }

    #[test]
    fn official_type_names_decode() {
        let owner = Uuid::from_u128(1);
        let folder = Uuid::from_u128(2);
        let asset_types = [
            ("texture", 0),
            ("sound", 1),
            ("callcard", 2),
            ("landmark", 3),
            ("script", 4),
            ("clothing", 5),
            ("object", 6),
            ("notecard", 7),
            ("category", 8),
            ("lsltext", 10),
            ("lslbyte", 11),
            ("txtr_tga", 12),
            ("bodypart", 13),
            ("snd_wav", 17),
            ("img_tga", 18),
            ("jpeg", 19),
            ("animatn", 20),
            ("gesture", 21),
            ("simstate", 22),
            ("link", 24),
            ("link_f", 25),
            ("widget", 40),
            ("person", 45),
            ("mesh", 49),
            ("settings", 56),
            ("material", 57),
            ("gltf", 58),
            ("glbin", 59),
            ("invalid", 255),
            ("llassettype_bad_lookup", 255),
            ("-1", -1),
        ];
        let inventory_types = [
            ("texture", 0),
            ("sound", 1),
            ("callcard", 2),
            ("landmark", 3),
            ("object", 6),
            ("notecard", 7),
            ("category", 8),
            ("root", 9),
            ("script", 10),
            ("snapshot", 15),
            ("attach", 17),
            ("wearable", 18),
            ("animation", 19),
            ("gesture", 20),
            ("mesh", 22),
            ("widget", 23),
            ("person", 24),
            ("settings", 25),
            ("material", 26),
            ("gltf", 27),
            ("glbin", 28),
            ("", 255),
        ];
        let mut expected = Vec::new();
        let mut items = Vec::new();
        for (index, (name, raw)) in asset_types.into_iter().enumerate() {
            expected.push((raw, 10));
            items.push(item_with_types(
                Uuid::from_u128(100 + index as u128),
                folder,
                owner,
                Value::String(name.into()),
                Value::String("script".into()),
            ));
        }
        for (index, (name, raw)) in inventory_types.into_iter().enumerate() {
            expected.push((0, raw));
            items.push(item_with_types(
                Uuid::from_u128(200 + index as u128),
                folder,
                owner,
                Value::String("texture".into()),
                Value::String(name.into()),
            ));
        }
        items.push(item_with_types(
            Uuid::from_u128(300),
            folder,
            owner,
            Value::String("future_asset".into()),
            Value::String("future_inventory_type".into()),
        ));
        expected.push((255, 255));
        items.push(item_with_types(Uuid::from_u128(301), folder, owner, Value::Integer(7), Value::Boolean(true)));
        expected.push((7, 255));

        let mut fields = match category(folder, None, 0) {
            Value::Map(fields) => fields,
            _ => unreachable!(),
        };
        fields.insert("descendents".into(), Value::Integer(items.len() as i32));
        fields.insert(
            "_embedded".into(),
            map_value([
                ("categories".into(), Value::Map(Map::new())),
                ("items".into(), keyed(items, "item_id")),
                ("links".into(), Value::Map(Map::new())),
            ]),
        );
        let decoded = decode_ais_children(&encode(&Value::Map(fields)), folder, owner).expect("one unknown field does not reject a folder");
        let children = decoded.children.expect("folder items are present");
        assert_eq!(children.len(), expected.len());
        for (child, (asset_type, inventory_type)) in children.iter().zip(expected) {
            let Child::Item(item) = child else { panic!("expected item") };
            assert_eq!(item.asset_type, Some(asset_type));
            assert_eq!(item.inventory_type, Some(inventory_type));
        }
    }

    #[test]
    fn legacy_folder_batch_is_typed() {
        let owner = Uuid::from_u128(1);
        let folder = Uuid::from_u128(2);
        let child = Uuid::from_u128(3);
        let response = Value::Map(Map::from([(
            "folders".into(),
            Value::Array(vec![Value::Map(Map::from([
                ("folder_id".into(), Value::Uuid(folder)),
                ("owner_id".into(), Value::Uuid(owner)),
                ("version".into(), Value::Integer(8)),
                ("descendents".into(), Value::Integer(1)),
                ("categories".into(), Value::Array(vec![category(child, Some(folder), 0)])),
                ("items".into(), Value::Array(Vec::new())),
            ]))]),
        )]));
        let decoded = decode_fetch_inventory_descendents2(&encode(&response), &[RequestedFolder { id: folder, owner_id: owner }])
            .expect("valid legacy response");
        assert_eq!(decoded[0].version, 8);
        assert_eq!(decoded[0].direct_categories[0].id, child);
    }

    #[test]
    fn folder_limit_is_typed() {
        let owner = Uuid::from_u128(1);
        let folder = Uuid::from_u128(2);
        let mut value = match category(folder, None, 0) {
            Value::Map(fields) => fields,
            _ => unreachable!(),
        };
        value.insert("agent_id".into(), Value::Uuid(owner));
        let items = (0..=MAX_FOLDER_CHILDREN)
            .map(|index| (Uuid::from_u128(index as u128 + 100).to_string(), Value::Map(Map::new())))
            .collect::<Map>();
        value.insert(
            "_embedded".into(),
            map_value([
                ("categories".into(), Value::Map(Map::new())),
                ("items".into(), Value::Map(items)),
                ("links".into(), Value::Map(Map::new())),
            ]),
        );
        assert_eq!(decode_ais_children(&encode(&Value::Map(value)), folder, owner), Err(DecodeError::TooLarge));
    }

    #[test]
    fn wrong_owner_is_rejected() {
        let owner = Uuid::from_u128(1);
        let root = Uuid::from_u128(2);
        let mut value = match category(root, None, 0) {
            Value::Map(fields) => fields,
            _ => unreachable!(),
        };
        value.insert("agent_id".into(), Value::Uuid(Uuid::from_u128(3)));
        assert_eq!(decode_ais_children(&encode(&Value::Map(value)), root, owner), Err(DecodeError::OwnerMismatch));
    }

    #[test]
    fn library_accepts_nil_linden_owners() {
        let library_owner = Uuid::from_u128(1);
        let linden_owner = Uuid::from_u128(999);
        let root = Uuid::from_u128(2);
        let child_folder = Uuid::from_u128(3);
        let nil_item = Uuid::from_u128(4);
        let linden_item = Uuid::from_u128(5);

        let mut root_fields = match category(root, None, 3) {
            Value::Map(fields) => fields,
            _ => unreachable!(),
        };
        root_fields.insert("agent_id".into(), Value::Uuid(Uuid::nil()));
        let mut sub_fields = match category(child_folder, Some(root), 0) {
            Value::Map(fields) => fields,
            _ => unreachable!(),
        };
        sub_fields.insert("agent_id".into(), Value::Uuid(linden_owner));
        root_fields.insert(
            "_embedded".into(),
            map_value([
                ("categories".into(), keyed(vec![Value::Map(sub_fields)], "category_id")),
                ("items".into(), keyed(vec![item(nil_item, root, Uuid::nil()), item(linden_item, root, linden_owner)], "item_id")),
                ("links".into(), Value::Map(Map::new())),
            ]),
        );

        let decoded = decode_ais_library_children(&encode(&Value::Map(root_fields)), root).expect("library AIS children decode");
        assert_eq!(decoded.id, root);
        assert_eq!(decoded.children.as_ref().map(Vec::len), Some(3));

        let legacy_response = Value::Map(Map::from([(
            "folders".into(),
            Value::Array(vec![Value::Map(Map::from([
                ("folder_id".into(), Value::Uuid(root)),
                ("owner_id".into(), Value::Uuid(Uuid::nil())),
                ("version".into(), Value::Integer(1)),
                ("descendents".into(), Value::Integer(2)),
                ("categories".into(), Value::Array(vec![category(child_folder, Some(root), 0)])),
                ("items".into(), Value::Array(vec![legacy_item(linden_item, root, linden_owner)])),
            ]))]),
        )]));
        let decoded_legacy =
            decode_fetch_lib_descendents2(&encode(&legacy_response), &[RequestedFolder { id: root, owner_id: library_owner }])
                .expect("library legacy descendents decode");
        assert_eq!(decoded_legacy[0].direct_categories.len(), 1);
        assert_eq!(decoded_legacy[0].direct_items.len(), 1);
    }

    #[test]
    fn legacy_items_reject_unrequested_id() {
        let owner = Uuid::from_u128(1);
        let parent = Uuid::from_u128(2);
        let item_id = Uuid::from_u128(3);
        let response = Value::Map(Map::from([("items".into(), Value::Array(vec![legacy_item(item_id, parent, owner)]))]));
        assert_eq!(
            decode_fetch_inventory2(&encode(&response), owner, &[Uuid::from_u128(4)]),
            Err(DecodeError::UnexpectedStructure("unrequested item id"))
        );
    }

    #[test]
    fn item_batches_follow_requested_ids() {
        let owner = Uuid::from_u128(1);
        let parent = Uuid::from_u128(2);
        let requested = (100..111).map(Uuid::from_u128).collect::<Vec<_>>();
        let mut items = requested.iter().map(|id| legacy_item(*id, parent, owner)).collect::<Vec<_>>();
        let response = map_value([("items".into(), Value::Array(items.clone()))]);
        let decoded = decode_fetch_inventory2(&encode(&response), owner, &requested).expect("item batches are not folder-scheduler policy");
        assert_eq!(decoded.iter().map(|item| item.id).collect::<Vec<_>>(), requested);
        assert!(decoded.iter().all(|item| item.parent_id == Some(parent) && item.owner_id == Some(owner)));
        items[10] = items[0].clone();
        let response = map_value([("items".into(), Value::Array(items))]);
        assert_eq!(decode_fetch_inventory2(&encode(&response), owner, &requested), Err(DecodeError::DuplicateId));
    }

    #[test]
    fn folder_batches_follow_requested_ids() {
        let owner = Uuid::from_u128(1);
        let requested = (100..111).map(|id| RequestedFolder { id: Uuid::from_u128(id), owner_id: owner }).collect::<Vec<_>>();
        let mut folders = requested
            .iter()
            .rev()
            .map(|folder| {
                map_value([
                    ("folder_id".into(), Value::Uuid(folder.id)),
                    ("owner_id".into(), Value::Uuid(owner)),
                    ("version".into(), Value::Integer(8)),
                    ("descendents".into(), Value::Integer(0)),
                    ("categories".into(), Value::Array(Vec::new())),
                    ("items".into(), Value::Array(Vec::new())),
                ])
            })
            .collect::<Vec<_>>();
        let response = map_value([("folders".into(), Value::Array(folders.clone()))]);
        let decoded =
            decode_fetch_inventory_descendents2(&encode(&response), &requested).expect("folder batch sizing belongs to the caller");
        assert_eq!(
            decoded.iter().map(|folder| folder.id).collect::<Vec<_>>(),
            requested.iter().map(|folder| folder.id).collect::<Vec<_>>()
        );
        assert!(decoded.iter().all(|folder| {
            folder.version == 8
                && folder.direct_categories.is_empty()
                && folder.direct_items.is_empty()
                && folder.auxiliary_categories.is_empty()
                && folder.auxiliary_items.is_empty()
        }));
        folders[10] = folders[0].clone();
        let response = map_value([("folders".into(), Value::Array(folders))]);
        assert_eq!(decode_fetch_inventory_descendents2(&encode(&response), &requested), Err(DecodeError::DuplicateId));
    }
}
