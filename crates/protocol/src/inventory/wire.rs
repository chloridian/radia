use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    marker::PhantomData,
};

use llsd::{Binary, Date, Limits, Uri, xml};
use serde::{
    Deserialize, Deserializer,
    de::{self, DeserializeOwned, DeserializeSeed, EnumAccess, MapAccess, SeqAccess, VariantAccess, Visitor},
};
use uuid::Uuid;

use super::{
    AisEditResponse, Category, Child, CreatedCategoryResponse, DecodeError, FolderDescendents, Item, MAX_AIS_DEPTH, MAX_CATEGORIES,
    MAX_CONTAINER_ITEMS, MAX_DESCRIPTION_BYTES, MAX_FOLDER_CHILDREN, MAX_HYPERMEDIA_URI_BYTES, MAX_ITEMS, MAX_MAP_KEY_BYTES,
    MAX_NAME_BYTES, MAX_RESPONSE_BYTES, MAX_RESPONSE_DEPTH, MAX_RESPONSE_VALUES, MAX_STRING_BYTES, Permissions, RequestedFolder, SaleInfo,
};

#[derive(Clone, Copy)]
enum InventoryWireFormat {
    Ais,
    Legacy,
}

#[derive(Default)]
struct ParseCounts {
    categories: usize,
    items: usize,
}

#[derive(Clone, Debug)]
enum Scalar {
    Undefined,
    Boolean,
    Integer(i32),
    Real,
    Uuid(Uuid),
    String(String),
    Date(Date),
    Uri(Uri),
    Other,
}

#[derive(Deserialize)]
#[serde(field_identifier)]
enum LlsdTag {
    Undefined,
    Boolean,
    Integer,
    Real,
    #[serde(rename = "UUID")]
    Uuid,
    String,
    Date,
    #[serde(rename = "URI")]
    Uri,
    Binary,
    Array,
    Map,
}

impl<'de> Deserialize<'de> for Scalar {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_enum("Value", &[], ScalarVisitor)
    }
}

struct ScalarVisitor;

impl<'de> Visitor<'de> for ScalarVisitor {
    type Value = Scalar;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an LLSD scalar or container")
    }

    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<Self::Value, A::Error> {
        let (tag, variant) = data.variant::<LlsdTag>()?;
        match tag {
            LlsdTag::Undefined => {
                variant.unit_variant()?;
                Ok(Scalar::Undefined)
            }
            LlsdTag::Boolean => variant.newtype_variant::<bool>().map(|_| Scalar::Boolean),
            LlsdTag::Integer => variant.newtype_variant().map(Scalar::Integer),
            LlsdTag::Real => variant.newtype_variant::<f64>().map(|_| Scalar::Real),
            LlsdTag::Uuid => {
                let value = variant.newtype_variant::<String>()?;
                Uuid::parse_str(&value).map(Scalar::Uuid).map_err(de::Error::custom)
            }
            LlsdTag::String => variant.newtype_variant().map(Scalar::String),
            LlsdTag::Date => variant.newtype_variant().map(Scalar::Date),
            LlsdTag::Uri => variant.newtype_variant().map(Scalar::Uri),
            LlsdTag::Binary | LlsdTag::Array | LlsdTag::Map => {
                skip_value_variant(variant, tag)?;
                Ok(Scalar::Other)
            }
        }
    }
}

struct IgnoredSeed;

impl<'de> DeserializeSeed<'de> for IgnoredSeed {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_enum("Value", &[], IgnoredVisitor)
    }
}

struct IgnoredVisitor;

impl<'de> Visitor<'de> for IgnoredVisitor {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an LLSD value")
    }

    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<Self::Value, A::Error> {
        let (tag, variant) = data.variant::<LlsdTag>()?;
        skip_value_variant(variant, tag)
    }
}

fn skip_value_variant<'de, V: VariantAccess<'de>>(variant: V, tag: LlsdTag) -> Result<(), V::Error> {
    match tag {
        LlsdTag::Undefined => variant.unit_variant()?,
        LlsdTag::Boolean => {
            variant.newtype_variant::<bool>()?;
        }
        LlsdTag::Integer => {
            variant.newtype_variant::<i32>()?;
        }
        LlsdTag::Real => {
            variant.newtype_variant::<f64>()?;
        }
        LlsdTag::Uuid => {
            let value = variant.newtype_variant::<String>()?;
            Uuid::parse_str(&value).map_err(de::Error::custom)?;
        }
        LlsdTag::String => {
            variant.newtype_variant::<String>()?;
        }
        LlsdTag::Date => {
            variant.newtype_variant::<Date>()?;
        }
        LlsdTag::Uri => {
            variant.newtype_variant::<Uri>()?;
        }
        LlsdTag::Binary => {
            variant.newtype_variant::<Binary>()?;
        }
        LlsdTag::Array => {
            variant.newtype_variant_seed(IgnoredArraySeed)?;
        }
        LlsdTag::Map => {
            variant.newtype_variant_seed(IgnoredMapSeed)?;
        }
    }
    Ok(())
}

struct IgnoredArraySeed;

impl<'de> DeserializeSeed<'de> for IgnoredArraySeed {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(IgnoredArrayVisitor)
    }
}

struct IgnoredArrayVisitor;

impl<'de> Visitor<'de> for IgnoredArrayVisitor {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an LLSD array")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        while access.next_element_seed(IgnoredSeed)?.is_some() {}
        Ok(())
    }
}

struct IgnoredMapSeed;

impl<'de> DeserializeSeed<'de> for IgnoredMapSeed {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(IgnoredMapVisitor)
    }
}

struct IgnoredMapVisitor;

impl<'de> Visitor<'de> for IgnoredMapVisitor {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an LLSD map")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        while access.next_key::<String>()?.is_some() {
            access.next_value_seed(IgnoredSeed)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct MapValue<T>(Result<T, DecodeError>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for MapValue<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_enum("Value", &[], MapValueVisitor(PhantomData))
    }
}

struct MapValueVisitor<T>(PhantomData<T>);

impl<'de, T: Deserialize<'de>> Visitor<'de> for MapValueVisitor<T> {
    type Value = MapValue<T>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an LLSD map")
    }

    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<Self::Value, A::Error> {
        let (tag, variant) = data.variant::<LlsdTag>()?;
        if matches!(tag, LlsdTag::Map) {
            variant.newtype_variant().map(|value| MapValue(Ok(value)))
        } else {
            skip_value_variant(variant, tag)?;
            Ok(MapValue(Err(DecodeError::InvalidResponse)))
        }
    }
}

#[derive(Clone, Debug)]
struct ArrayValue<T>(Result<Vec<T>, DecodeError>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for ArrayValue<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_enum("Value", &[], ArrayValueVisitor(PhantomData))
    }
}

struct ArrayValueVisitor<T>(PhantomData<T>);

impl<'de, T: Deserialize<'de>> Visitor<'de> for ArrayValueVisitor<T> {
    type Value = ArrayValue<T>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an LLSD array")
    }

    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<Self::Value, A::Error> {
        let (tag, variant) = data.variant::<LlsdTag>()?;
        if matches!(tag, LlsdTag::Array) {
            variant.newtype_variant().map(|value| ArrayValue(Ok(value)))
        } else {
            skip_value_variant(variant, tag)?;
            Ok(ArrayValue(Err(DecodeError::InvalidResponse)))
        }
    }
}

struct ItemArrayValue(Result<Vec<Item>, DecodeError>);

impl<'de> Deserialize<'de> for ItemArrayValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_enum("Value", &[], ItemArrayValueVisitor)
    }
}

struct ItemArrayValueVisitor;

impl<'de> Visitor<'de> for ItemArrayValueVisitor {
    type Value = ItemArrayValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an array of legacy inventory items")
    }

    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<Self::Value, A::Error> {
        let (tag, variant) = data.variant::<LlsdTag>()?;
        if matches!(tag, LlsdTag::Array) {
            variant.newtype_variant_seed(ItemArraySeed)
        } else {
            skip_value_variant(variant, tag)?;
            Ok(ItemArrayValue(Err(DecodeError::InvalidResponse)))
        }
    }
}

struct ItemArraySeed;

impl<'de> DeserializeSeed<'de> for ItemArraySeed {
    type Value = ItemArrayValue;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(ItemArrayVisitor)
    }
}

struct ItemArrayVisitor;

impl<'de> Visitor<'de> for ItemArrayVisitor {
    type Value = ItemArrayValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an array of legacy inventory items")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut items = Vec::new();
        let mut error = None;
        while let Some(LegacyItemValue(item)) = access.next_element()? {
            match item {
                Ok(item) if error.is_none() => items.push(item),
                Err(value) if error.is_none() => {
                    error = Some(value);
                    items = Vec::new();
                }
                _ => {}
            }
        }
        if error.is_none() {
            items.shrink_to_fit();
        }
        Ok(ItemArrayValue(error.map_or(Ok(items), Err)))
    }
}

struct CategoryArrayValue(Result<Vec<Category>, DecodeError>);

impl<'de> Deserialize<'de> for CategoryArrayValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_enum("Value", &[], CategoryArrayValueVisitor)
    }
}

struct CategoryArrayValueVisitor;

impl<'de> Visitor<'de> for CategoryArrayValueVisitor {
    type Value = CategoryArrayValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an array of legacy inventory categories")
    }

    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<Self::Value, A::Error> {
        let (tag, variant) = data.variant::<LlsdTag>()?;
        if matches!(tag, LlsdTag::Array) {
            variant.newtype_variant_seed(CategoryArraySeed)
        } else {
            skip_value_variant(variant, tag)?;
            Ok(CategoryArrayValue(Err(DecodeError::InvalidResponse)))
        }
    }
}

struct CategoryArraySeed;

impl<'de> DeserializeSeed<'de> for CategoryArraySeed {
    type Value = CategoryArrayValue;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(CategoryArrayVisitor)
    }
}

struct CategoryArrayVisitor;

impl<'de> Visitor<'de> for CategoryArrayVisitor {
    type Value = CategoryArrayValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an array of legacy inventory categories")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut categories = Vec::new();
        let mut error = None;
        while let Some(LegacyCategoryValue(category)) = access.next_element()? {
            match category {
                Ok(category) if error.is_none() => categories.push(category),
                Err(value) if error.is_none() => {
                    error = Some(value);
                    categories = Vec::new();
                }
                _ => {}
            }
        }
        if error.is_none() {
            categories.shrink_to_fit();
        }
        Ok(CategoryArrayValue(error.map_or(Ok(categories), Err)))
    }
}

fn decode_llsd<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, DecodeError> {
    xml::from_slice_with_limits(
        bytes,
        Limits {
            max_bytes: MAX_RESPONSE_BYTES,
            max_depth: MAX_RESPONSE_DEPTH,
            max_values: MAX_RESPONSE_VALUES,
            max_container_items: MAX_CONTAINER_ITEMS,
            max_string_bytes: MAX_STRING_BYTES,
            max_map_key_bytes: MAX_MAP_KEY_BYTES,
            max_binary_bytes: 0,
        },
    )
    .map_err(|error| match error {
        llsd::Error::LimitExceeded(_) => DecodeError::TooLarge,
        _ => DecodeError::InvalidResponse,
    })
}

fn scalar_uuid(value: &Scalar) -> Result<Uuid, DecodeError> {
    match value {
        Scalar::Uuid(id) => Ok(*id),
        Scalar::String(value) => Uuid::parse_str(value).map_err(|_| DecodeError::InvalidResponse),
        _ => Err(DecodeError::InvalidResponse),
    }
}

fn uuid_fields(values: &[Option<&Scalar>]) -> Result<Option<Uuid>, DecodeError> {
    let mut found = None;
    for value in values.iter().flatten() {
        let id = scalar_uuid(value)?;
        if found.is_some_and(|previous| previous != id) {
            return Err(DecodeError::InvalidResponse);
        }
        found = Some(id);
    }
    Ok(found)
}

fn integer(value: Option<&Scalar>) -> Result<Option<i32>, DecodeError> {
    match value {
        None => Ok(None),
        Some(Scalar::Integer(value)) => Ok(Some(*value)),
        Some(_) => Err(DecodeError::InvalidResponse),
    }
}

fn unsigned(value: Option<&Scalar>) -> Result<Option<u32>, DecodeError> {
    let Some(Scalar::Integer(value)) = value else {
        return if value.is_none() { Ok(None) } else { Err(DecodeError::InvalidResponse) };
    };
    if *value < 0 {
        return Err(DecodeError::InvalidResponse);
    }
    Ok(Some(u32::try_from(*value).map_err(|_| DecodeError::InvalidResponse)?))
}

fn count(value: Option<&Scalar>) -> Result<Option<u32>, DecodeError> {
    let value = unsigned(value)?;
    if value.is_some_and(|value| value as usize > MAX_FOLDER_CHILDREN) {
        return Err(DecodeError::TooLarge);
    }
    Ok(value)
}

fn string(value: Option<Scalar>, default: &str) -> Result<String, DecodeError> {
    match value {
        Some(Scalar::String(value)) => Ok(value),
        None => Ok(default.to_owned()),
        Some(_) => Err(DecodeError::InvalidResponse),
    }
}

fn timestamp(value: Option<&Scalar>) -> Result<Option<i64>, DecodeError> {
    match value {
        None => Ok(None),
        Some(Scalar::Integer(value)) => Ok(Some(i64::from(*value))),
        Some(Scalar::Date(value)) if value.unix_seconds().is_finite() => {
            let seconds = value.unix_seconds();
            (seconds >= i64::MIN as f64 && seconds <= i64::MAX as f64)
                .then_some(seconds.trunc() as i64)
                .map(Some)
                .ok_or(DecodeError::InvalidResponse)
        }
        Some(_) => Err(DecodeError::InvalidResponse),
    }
}

fn string_or_uri(value: &Scalar) -> Option<&str> {
    match value {
        Scalar::String(value) => Some(value),
        Scalar::Uri(value) => Some(value.as_str()),
        _ => None,
    }
}

#[derive(Clone, Default)]
struct PermissionFields {
    creator_id: Option<Scalar>,
    owner_id: Option<Scalar>,
    group_id: Option<Scalar>,
    last_owner_id: Option<Scalar>,
    base_mask: Option<Scalar>,
    owner_mask: Option<Scalar>,
    group_mask: Option<Scalar>,
    everyone_mask: Option<Scalar>,
    next_owner_mask: Option<Scalar>,
}

impl PermissionFields {
    fn get(&self, name: &str) -> Option<&Scalar> {
        match name {
            "creator_id" => self.creator_id.as_ref(),
            "owner_id" => self.owner_id.as_ref(),
            "group_id" => self.group_id.as_ref(),
            "last_owner_id" => self.last_owner_id.as_ref(),
            "base_mask" => self.base_mask.as_ref(),
            "owner_mask" => self.owner_mask.as_ref(),
            "group_mask" => self.group_mask.as_ref(),
            "everyone_mask" => self.everyone_mask.as_ref(),
            "next_owner_mask" => self.next_owner_mask.as_ref(),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for PermissionFields {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(PermissionFieldsVisitor)
    }
}

struct PermissionFieldsVisitor;

impl<'de> Visitor<'de> for PermissionFieldsVisitor {
    type Value = PermissionFields;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an inventory permissions map")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut fields = PermissionFields::default();
        while let Some(key) = access.next_key::<String>()? {
            match key.as_str() {
                "creator_id" => fields.creator_id = Some(access.next_value()?),
                "owner_id" => fields.owner_id = Some(access.next_value()?),
                "group_id" => fields.group_id = Some(access.next_value()?),
                "last_owner_id" => fields.last_owner_id = Some(access.next_value()?),
                "base_mask" => fields.base_mask = Some(access.next_value()?),
                "owner_mask" => fields.owner_mask = Some(access.next_value()?),
                "group_mask" => fields.group_mask = Some(access.next_value()?),
                "everyone_mask" => fields.everyone_mask = Some(access.next_value()?),
                "next_owner_mask" => fields.next_owner_mask = Some(access.next_value()?),
                _ => access.next_value_seed(IgnoredSeed)?,
            }
        }
        Ok(fields)
    }
}

#[derive(Clone, Default)]
struct SaleFields {
    sale_type: Option<Scalar>,
    sale_price: Option<Scalar>,
}

impl SaleFields {
    fn get(&self, name: &str) -> Option<&Scalar> {
        match name {
            "sale_type" => self.sale_type.as_ref(),
            "sale_price" => self.sale_price.as_ref(),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for SaleFields {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(SaleFieldsVisitor)
    }
}

struct SaleFieldsVisitor;

impl<'de> Visitor<'de> for SaleFieldsVisitor {
    type Value = SaleFields;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an inventory sale map")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut fields = SaleFields::default();
        while let Some(key) = access.next_key::<String>()? {
            match key.as_str() {
                "sale_type" => fields.sale_type = Some(access.next_value()?),
                "sale_price" => fields.sale_price = Some(access.next_value()?),
                _ => access.next_value_seed(IgnoredSeed)?,
            }
        }
        Ok(fields)
    }
}

#[derive(Clone, Default)]
struct LinkFields {
    href: Option<Scalar>,
}

impl<'de> Deserialize<'de> for LinkFields {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(LinkFieldsVisitor)
    }
}

struct LinkFieldsVisitor;

impl<'de> Visitor<'de> for LinkFieldsVisitor {
    type Value = LinkFields;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an inventory link map")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut fields = LinkFields::default();
        while let Some(key) = access.next_key::<String>()? {
            if key == "href" {
                fields.href = Some(access.next_value()?);
            } else {
                access.next_value_seed(IgnoredSeed)?;
            }
        }
        Ok(fields)
    }
}

#[derive(Clone)]
struct LinkValue(Result<LinkFields, DecodeError>);

impl<'de> Deserialize<'de> for LinkValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        MapValue::<LinkFields>::deserialize(deserializer).map(|value| Self(value.0))
    }
}

type LinksFields = BTreeMap<String, LinkValue>;

#[derive(Clone, Default)]
struct CategoryFields {
    category_id: Option<Scalar>,
    folder_id: Option<Scalar>,
    agent_id: Option<Scalar>,
    owner_id: Option<Scalar>,
    parent_id: Option<Scalar>,
    name: Option<Scalar>,
    version: Option<Scalar>,
    descendents: Option<Scalar>,
    descendent_count: Option<Scalar>,
    type_default: Option<Scalar>,
    type_: Option<Scalar>,
    base_uri: Option<Scalar>,
    links: Option<MapValue<LinksFields>>,
    embedded: Option<MapValue<EmbeddedFields>>,
}

impl CategoryFields {
    fn has_scalar(name: &str) -> bool {
        matches!(
            name,
            "category_id"
                | "folder_id"
                | "agent_id"
                | "owner_id"
                | "parent_id"
                | "name"
                | "version"
                | "descendents"
                | "descendent_count"
                | "type_default"
                | "type"
                | "_base_uri"
        )
    }

    fn set_scalar(&mut self, name: &str, value: Scalar) -> bool {
        let target = match name {
            "category_id" => &mut self.category_id,
            "folder_id" => &mut self.folder_id,
            "agent_id" => &mut self.agent_id,
            "owner_id" => &mut self.owner_id,
            "parent_id" => &mut self.parent_id,
            "name" => &mut self.name,
            "version" => &mut self.version,
            "descendents" => &mut self.descendents,
            "descendent_count" => &mut self.descendent_count,
            "type_default" => &mut self.type_default,
            "type" => &mut self.type_,
            "_base_uri" => &mut self.base_uri,
            _ => return false,
        };
        *target = Some(value);
        true
    }

    fn read_field<'de, A: MapAccess<'de>>(&mut self, name: &str, access: &mut A) -> Result<bool, A::Error> {
        if Self::has_scalar(name) {
            self.set_scalar(name, access.next_value()?);
            return Ok(true);
        }
        match name {
            "_links" => {
                self.links = Some(access.next_value()?);
                Ok(true)
            }
            "_embedded" => {
                self.embedded = Some(access.next_value()?);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn into_category(self, format: InventoryWireFormat) -> Result<Category, DecodeError> {
        validate_hypermedia(self.base_uri.as_ref(), self.links.as_ref())?;
        let (id, owner_id, parent_id, name, version, descendent_count, preferred_folder_type) = match format {
            InventoryWireFormat::Ais => (
                uuid_fields(&[self.category_id.as_ref()])?,
                uuid_fields(&[self.agent_id.as_ref()])?,
                uuid_fields(&[self.parent_id.as_ref()])?,
                string(self.name, "")?,
                integer(self.version.as_ref())?,
                None,
                integer(self.type_default.as_ref())?,
            ),
            InventoryWireFormat::Legacy => (
                uuid_fields(&[self.folder_id.as_ref(), self.category_id.as_ref()])?,
                uuid_fields(&[self.owner_id.as_ref(), self.agent_id.as_ref()])?,
                uuid_fields(&[self.parent_id.as_ref()])?,
                string(self.name, "")?,
                integer(self.version.as_ref())?,
                count(self.descendents.as_ref().or(self.descendent_count.as_ref()))?,
                integer(self.type_default.as_ref().or(self.type_.as_ref()))?,
            ),
        };
        let id = id.filter(|id| !id.is_nil()).ok_or(DecodeError::InvalidResponse)?;
        if name.len() > MAX_NAME_BYTES || descendent_count.is_some_and(|count| count as usize > MAX_FOLDER_CHILDREN) {
            return Err(DecodeError::InvalidResponse);
        }
        let mut category = Category { id, owner_id, parent_id, name, version, descendent_count, preferred_folder_type, children: None };
        let Some(embedded) = self.embedded else { return Ok(category) };
        if matches!(format, InventoryWireFormat::Legacy) {
            return Err(DecodeError::InvalidResponse);
        }
        let embedded_complete =
            embedded.0.as_ref().is_ok_and(|embedded| embedded.categories.is_some() && embedded.items.is_some() && embedded.links.is_some());
        let embedded = embedded.0?;
        let categories = into_map(embedded.categories)?;
        let items = into_map(embedded.items)?;
        let links = into_map(embedded.links)?;
        let direct_count = categories
            .as_ref()
            .map_or(0, BTreeMap::len)
            .checked_add(items.as_ref().map_or(0, BTreeMap::len))
            .and_then(|count| count.checked_add(links.as_ref().map_or(0, BTreeMap::len)))
            .ok_or(DecodeError::TooLarge)?;
        if direct_count > MAX_FOLDER_CHILDREN {
            return Err(DecodeError::TooLarge);
        }
        let mut children = Vec::with_capacity(direct_count);
        for (key, value) in items.into_iter().flatten() {
            let expected_id = Uuid::parse_str(&key).map_err(|_| DecodeError::InvalidResponse)?;
            let item = value.0?;
            if item.id != expected_id {
                return Err(DecodeError::InvalidResponse);
            }
            children.push(Child::Item(item));
        }
        for (key, value) in links.into_iter().flatten() {
            let expected_id = Uuid::parse_str(&key).map_err(|_| DecodeError::InvalidResponse)?;
            let item = value.0?;
            if item.id != expected_id {
                return Err(DecodeError::InvalidResponse);
            }
            children.push(Child::Item(item));
        }
        for (key, value) in categories.into_iter().flatten() {
            let expected_id = Uuid::parse_str(&key).map_err(|_| DecodeError::InvalidResponse)?;
            let child = value.0?;
            if child.id != expected_id {
                return Err(DecodeError::InvalidResponse);
            }
            children.push(Child::Category(child));
        }
        if embedded_complete {
            category.descendent_count = Some(direct_count as u32);
        }
        category.children = Some(children);
        Ok(category)
    }
}

impl<'de> Deserialize<'de> for CategoryFields {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(CategoryFieldsVisitor)
    }
}

struct CategoryFieldsVisitor;

impl<'de> Visitor<'de> for CategoryFieldsVisitor {
    type Value = CategoryFields;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an inventory category map")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut fields = CategoryFields::default();
        while let Some(key) = access.next_key::<String>()? {
            if !fields.read_field(&key, &mut access)? {
                access.next_value_seed(IgnoredSeed)?;
            }
        }
        Ok(fields)
    }
}

#[derive(Clone, Default)]
struct ItemFields {
    item_id: Option<Scalar>,
    agent_id: Option<Scalar>,
    owner_id: Option<Scalar>,
    parent_id: Option<Scalar>,
    linked_id: Option<Scalar>,
    asset_id: Option<Scalar>,
    type_: Option<Scalar>,
    asset_type: Option<Scalar>,
    inv_type: Option<Scalar>,
    inventory_type: Option<Scalar>,
    desc: Option<Scalar>,
    description: Option<Scalar>,
    name: Option<Scalar>,
    flags: Option<Scalar>,
    created_at: Option<Scalar>,
    creation_date: Option<Scalar>,
    creator_id: Option<Scalar>,
    group_id: Option<Scalar>,
    last_owner_id: Option<Scalar>,
    base_mask: Option<Scalar>,
    owner_mask: Option<Scalar>,
    group_mask: Option<Scalar>,
    everyone_mask: Option<Scalar>,
    next_owner_mask: Option<Scalar>,
    sale_type: Option<Scalar>,
    sale_price: Option<Scalar>,
    base_uri: Option<Scalar>,
    permissions: Option<MapValue<PermissionFields>>,
    sale_info: Option<MapValue<SaleFields>>,
    links: Option<MapValue<LinksFields>>,
}

impl ItemFields {
    fn has_scalar(name: &str) -> bool {
        matches!(
            name,
            "item_id"
                | "agent_id"
                | "owner_id"
                | "parent_id"
                | "linked_id"
                | "asset_id"
                | "type"
                | "asset_type"
                | "inv_type"
                | "inventory_type"
                | "desc"
                | "description"
                | "name"
                | "flags"
                | "created_at"
                | "creation_date"
                | "creator_id"
                | "group_id"
                | "last_owner_id"
                | "base_mask"
                | "owner_mask"
                | "group_mask"
                | "everyone_mask"
                | "next_owner_mask"
                | "sale_type"
                | "sale_price"
                | "_base_uri"
        )
    }

    fn set_scalar(&mut self, name: &str, value: Scalar) -> bool {
        let target = match name {
            "item_id" => &mut self.item_id,
            "agent_id" => &mut self.agent_id,
            "owner_id" => &mut self.owner_id,
            "parent_id" => &mut self.parent_id,
            "linked_id" => &mut self.linked_id,
            "asset_id" => &mut self.asset_id,
            "type" => &mut self.type_,
            "asset_type" => &mut self.asset_type,
            "inv_type" => &mut self.inv_type,
            "inventory_type" => &mut self.inventory_type,
            "desc" => &mut self.desc,
            "description" => &mut self.description,
            "name" => &mut self.name,
            "flags" => &mut self.flags,
            "created_at" => &mut self.created_at,
            "creation_date" => &mut self.creation_date,
            "creator_id" => &mut self.creator_id,
            "group_id" => &mut self.group_id,
            "last_owner_id" => &mut self.last_owner_id,
            "base_mask" => &mut self.base_mask,
            "owner_mask" => &mut self.owner_mask,
            "group_mask" => &mut self.group_mask,
            "everyone_mask" => &mut self.everyone_mask,
            "next_owner_mask" => &mut self.next_owner_mask,
            "sale_type" => &mut self.sale_type,
            "sale_price" => &mut self.sale_price,
            "_base_uri" => &mut self.base_uri,
            _ => return false,
        };
        *target = Some(value);
        true
    }

    fn read_field<'de, A: MapAccess<'de>>(&mut self, name: &str, access: &mut A) -> Result<bool, A::Error> {
        if Self::has_scalar(name) {
            self.set_scalar(name, access.next_value()?);
            return Ok(true);
        }
        match name {
            "permissions" => {
                self.permissions = Some(access.next_value()?);
                Ok(true)
            }
            "sale_info" => {
                self.sale_info = Some(access.next_value()?);
                Ok(true)
            }
            "_links" => {
                self.links = Some(access.next_value()?);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn into_item(self, format: InventoryWireFormat, link: bool) -> Result<Item, DecodeError> {
        validate_hypermedia(self.base_uri.as_ref(), self.links.as_ref())?;
        let id = uuid_fields(&[self.item_id.as_ref()])?.filter(|id| !id.is_nil()).ok_or(DecodeError::InvalidResponse)?;
        let owner_names = match format {
            InventoryWireFormat::Ais => [self.agent_id.as_ref(), None],
            InventoryWireFormat::Legacy => [self.owner_id.as_ref(), self.agent_id.as_ref()],
        };
        let owner_id = uuid_fields(&owner_names)?;
        let parent_id = uuid_fields(&[self.parent_id.as_ref()])?;
        let asset_id = match (format, link) {
            (InventoryWireFormat::Ais, true) => uuid_fields(&[self.linked_id.as_ref()])?,
            (InventoryWireFormat::Ais, false) => uuid_fields(&[self.asset_id.as_ref()])?,
            (InventoryWireFormat::Legacy, _) => uuid_fields(&[self.asset_id.as_ref(), self.linked_id.as_ref()])?,
        };
        if matches!((format, link), (InventoryWireFormat::Ais, true)) && asset_id.is_none() {
            return Err(DecodeError::InvalidResponse);
        }
        let asset_type = inventory_type(
            match format {
                InventoryWireFormat::Ais => self.type_.as_ref(),
                InventoryWireFormat::Legacy => self.type_.as_ref().or(self.asset_type.as_ref()),
            },
            named_asset_type,
        );
        let inventory_type = inventory_type(
            match format {
                InventoryWireFormat::Ais => self.inv_type.as_ref(),
                InventoryWireFormat::Legacy => self.inv_type.as_ref().or(self.inventory_type.as_ref()),
            },
            named_inventory_type,
        );
        let name = string(self.name, "")?;
        let description = string(
            match format {
                InventoryWireFormat::Ais => self.desc,
                InventoryWireFormat::Legacy => self.desc.or(self.description),
            },
            "",
        )?;
        if name.len() > MAX_NAME_BYTES || description.len() > MAX_DESCRIPTION_BYTES {
            return Err(DecodeError::InvalidResponse);
        }
        let flags = unsigned(self.flags.as_ref())?.unwrap_or_default();
        let created_at = match format {
            InventoryWireFormat::Ais => integer(self.created_at.as_ref())?.map(i64::from).unwrap_or_default(),
            InventoryWireFormat::Legacy => timestamp(self.created_at.as_ref().or(self.creation_date.as_ref()))?.unwrap_or_default(),
        };
        let permissions = self.permissions.as_ref().map(|value| value.0.as_ref().map_err(|_| DecodeError::InvalidResponse)).transpose()?;
        let sale_info = self.sale_info.as_ref().map(|value| value.0.as_ref().map_err(|_| DecodeError::InvalidResponse)).transpose()?;
        Ok(Item {
            id,
            owner_id,
            parent_id,
            asset_id,
            asset_type,
            inventory_type,
            name,
            description,
            flags,
            created_at,
            permissions: Permissions {
                creator_id: nested_uuid(permissions, self.creator_id.as_ref(), "creator_id")?,
                owner_id: nested_uuid(permissions, self.owner_id.as_ref(), "owner_id")?,
                group_id: nested_uuid(permissions, self.group_id.as_ref(), "group_id")?,
                last_owner_id: nested_uuid(permissions, self.last_owner_id.as_ref(), "last_owner_id")?,
                base_mask: nested_unsigned(permissions, self.base_mask.as_ref(), "base_mask")?,
                owner_mask: nested_unsigned(permissions, self.owner_mask.as_ref(), "owner_mask")?,
                group_mask: nested_unsigned(permissions, self.group_mask.as_ref(), "group_mask")?,
                everyone_mask: nested_unsigned(permissions, self.everyone_mask.as_ref(), "everyone_mask")?,
                next_owner_mask: nested_unsigned(permissions, self.next_owner_mask.as_ref(), "next_owner_mask")?,
            },
            sale_info: SaleInfo {
                sale_type: nested_integer(sale_info, self.sale_type.as_ref(), "sale_type")?,
                sale_price: nested_integer(sale_info, self.sale_price.as_ref(), "sale_price")?,
            },
        })
    }
}

impl<'de> Deserialize<'de> for ItemFields {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(ItemFieldsVisitor)
    }
}

struct ItemFieldsVisitor;

impl<'de> Visitor<'de> for ItemFieldsVisitor {
    type Value = ItemFields;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an inventory item map")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut fields = ItemFields::default();
        while let Some(key) = access.next_key::<String>()? {
            if !fields.read_field(&key, &mut access)? {
                access.next_value_seed(IgnoredSeed)?;
            }
        }
        Ok(fields)
    }
}

#[derive(Clone, Default)]
struct EmbeddedFields {
    categories: Option<MapValue<BTreeMap<String, AisCategoryValue>>>,
    items: Option<MapValue<BTreeMap<String, AisItemValue>>>,
    links: Option<MapValue<BTreeMap<String, AisLinkValue>>>,
}

impl<'de> Deserialize<'de> for EmbeddedFields {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(EmbeddedFieldsVisitor)
    }
}

struct EmbeddedFieldsVisitor;

impl<'de> Visitor<'de> for EmbeddedFieldsVisitor {
    type Value = EmbeddedFields;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an embedded inventory map")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut fields = EmbeddedFields::default();
        while let Some(key) = access.next_key::<String>()? {
            match key.as_str() {
                "categories" => fields.categories = Some(access.next_value()?),
                "items" => fields.items = Some(access.next_value()?),
                "links" => fields.links = Some(access.next_value()?),
                _ => access.next_value_seed(IgnoredSeed)?,
            }
        }
        Ok(fields)
    }
}

#[derive(Clone)]
struct AisCategoryValue(Result<Category, DecodeError>);

impl<'de> Deserialize<'de> for AisCategoryValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        MapValue::<CategoryFields>::deserialize(deserializer)
            .map(|value| Self(value.0.and_then(|fields| fields.into_category(InventoryWireFormat::Ais))))
    }
}

#[derive(Clone)]
struct LegacyCategoryValue(Result<Category, DecodeError>);

impl<'de> Deserialize<'de> for LegacyCategoryValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        MapValue::<CategoryFields>::deserialize(deserializer)
            .map(|value| Self(value.0.and_then(|fields| fields.into_category(InventoryWireFormat::Legacy))))
    }
}

#[derive(Clone)]
struct AisItemValue(Result<Item, DecodeError>);

impl<'de> Deserialize<'de> for AisItemValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        MapValue::<ItemFields>::deserialize(deserializer)
            .map(|value| Self(value.0.and_then(|fields| fields.into_item(InventoryWireFormat::Ais, false))))
    }
}

#[derive(Clone)]
struct AisLinkValue(Result<Item, DecodeError>);

impl<'de> Deserialize<'de> for AisLinkValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        MapValue::<ItemFields>::deserialize(deserializer)
            .map(|value| Self(value.0.and_then(|fields| fields.into_item(InventoryWireFormat::Ais, true))))
    }
}

#[derive(Clone)]
struct LegacyItemValue(Result<Item, DecodeError>);

impl<'de> Deserialize<'de> for LegacyItemValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        MapValue::<ItemFields>::deserialize(deserializer)
            .map(|value| Self(value.0.and_then(|fields| fields.into_item(InventoryWireFormat::Legacy, false))))
    }
}

fn nested_value<'a>(nested: Option<&'a PermissionFields>, flat: Option<&'a Scalar>, name: &str) -> Option<&'a Scalar> {
    nested.and_then(|fields| fields.get(name)).or(flat)
}

fn nested_uuid(nested: Option<&PermissionFields>, flat: Option<&Scalar>, name: &str) -> Result<Option<Uuid>, DecodeError> {
    nested_value(nested, flat, name).map(scalar_uuid).transpose()
}

fn nested_unsigned(nested: Option<&PermissionFields>, flat: Option<&Scalar>, name: &str) -> Result<Option<u32>, DecodeError> {
    unsigned(nested_value(nested, flat, name))
}

fn nested_integer(nested: Option<&SaleFields>, flat: Option<&Scalar>, name: &str) -> Result<Option<i32>, DecodeError> {
    integer(nested.and_then(|fields| fields.get(name)).or(flat))
}

fn inventory_type(value: Option<&Scalar>, named: fn(&str) -> i32) -> Option<i32> {
    value.map(|value| match value {
        Scalar::Integer(value) => *value,
        Scalar::String(value) => named(value),
        _ => 255,
    })
}

fn validate_hypermedia(base_uri: Option<&Scalar>, links: Option<&MapValue<LinksFields>>) -> Result<(), DecodeError> {
    if base_uri.is_some_and(|value| string_or_uri(value).is_none_or(|uri| uri.len() > MAX_HYPERMEDIA_URI_BYTES)) {
        return Err(DecodeError::InvalidResponse);
    }
    if let Some(links) = links {
        let links = links.0.as_ref().map_err(|_| DecodeError::InvalidResponse)?;
        for link in links.values() {
            let link = link.0.as_ref().map_err(|_| DecodeError::InvalidResponse)?;
            let href = link.href.as_ref().and_then(string_or_uri).ok_or(DecodeError::InvalidResponse)?;
            if href.len() > MAX_HYPERMEDIA_URI_BYTES {
                return Err(DecodeError::InvalidResponse);
            }
        }
    }
    Ok(())
}

fn into_map<T>(value: Option<MapValue<T>>) -> Result<Option<T>, DecodeError> {
    value.map(|value| value.0).transpose()
}

#[derive(Default)]
struct FolderFields {
    folder_id: Option<Scalar>,
    owner_id: Option<Scalar>,
    version: Option<Scalar>,
    descendents: Option<Scalar>,
    categories: Option<CategoryArrayValue>,
    items: Option<ItemArrayValue>,
}

impl<'de> Deserialize<'de> for FolderFields {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(FolderFieldsVisitor)
    }
}

struct FolderFieldsVisitor;

impl<'de> Visitor<'de> for FolderFieldsVisitor {
    type Value = FolderFields;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a legacy inventory folder map")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut fields = FolderFields::default();
        while let Some(key) = access.next_key::<String>()? {
            match key.as_str() {
                "folder_id" => fields.folder_id = Some(access.next_value()?),
                "owner_id" => fields.owner_id = Some(access.next_value()?),
                "version" => fields.version = Some(access.next_value()?),
                "descendents" => fields.descendents = Some(access.next_value()?),
                "categories" => fields.categories = Some(access.next_value()?),
                "items" => fields.items = Some(access.next_value()?),
                _ => access.next_value_seed(IgnoredSeed)?,
            }
        }
        Ok(fields)
    }
}

struct FolderValue(Result<FolderFields, DecodeError>);

impl<'de> Deserialize<'de> for FolderValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        MapValue::<FolderFields>::deserialize(deserializer).map(|value| Self(value.0))
    }
}

#[derive(Default)]
struct FetchDescendentsFields {
    folders: Option<ArrayValue<FolderValue>>,
}

impl<'de> Deserialize<'de> for FetchDescendentsFields {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(FetchDescendentsFieldsVisitor)
    }
}

struct FetchDescendentsFieldsVisitor;

impl<'de> Visitor<'de> for FetchDescendentsFieldsVisitor {
    type Value = FetchDescendentsFields;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a legacy descendents response map")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut fields = FetchDescendentsFields::default();
        while let Some(key) = access.next_key::<String>()? {
            if key == "folders" {
                fields.folders = Some(access.next_value()?);
            } else {
                access.next_value_seed(IgnoredSeed)?;
            }
        }
        Ok(fields)
    }
}

#[derive(Default)]
struct FetchItemsFields {
    items: Option<ItemArrayValue>,
}

impl<'de> Deserialize<'de> for FetchItemsFields {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(FetchItemsFieldsVisitor)
    }
}

struct FetchItemsFieldsVisitor;

impl<'de> Visitor<'de> for FetchItemsFieldsVisitor {
    type Value = FetchItemsFields;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a legacy inventory items response map")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut fields = FetchItemsFields::default();
        while let Some(key) = access.next_key::<String>()? {
            if key == "items" {
                fields.items = Some(access.next_value()?);
            } else {
                access.next_value_seed(IgnoredSeed)?;
            }
        }
        Ok(fields)
    }
}

fn optional_category_array(value: Option<CategoryArrayValue>, required_count: u32) -> Result<Vec<Category>, DecodeError> {
    match value {
        Some(value) => value.0,
        None if required_count == 0 => Ok(Vec::new()),
        None => Err(DecodeError::InvalidResponse),
    }
}

fn optional_item_array(value: Option<ItemArrayValue>, required_count: u32) -> Result<Vec<Item>, DecodeError> {
    match value {
        Some(value) => value.0,
        None if required_count == 0 => Ok(Vec::new()),
        None => Err(DecodeError::InvalidResponse),
    }
}

fn parse_folder_descendents(fields: FolderFields, requested: RequestedFolder) -> Result<FolderDescendents, DecodeError> {
    let id = uuid_fields(&[fields.folder_id.as_ref()])?.ok_or(DecodeError::InvalidResponse)?;
    let owner_id = uuid_fields(&[fields.owner_id.as_ref()])?;
    if id != requested.id || owner_id.is_some_and(|owner| owner != requested.owner_id) {
        return Err(DecodeError::InvalidResponse);
    }
    let version = integer(fields.version.as_ref())?.unwrap_or(-1);
    let descendent_count = count(fields.descendents.as_ref())?.ok_or(DecodeError::InvalidResponse)?;
    let raw_categories = optional_category_array(fields.categories, descendent_count)?;
    let raw_items = optional_item_array(fields.items, descendent_count)?;
    let record_count = raw_categories.len().checked_add(raw_items.len()).ok_or(DecodeError::TooLarge)?;
    if record_count > MAX_FOLDER_CHILDREN {
        return Err(DecodeError::TooLarge);
    }
    let mut identities = HashSet::with_capacity(record_count);
    for category in &raw_categories {
        validate_legacy_category(category, requested.owner_id)?;
        if !identities.insert(category.id) || category.parent_id.is_some_and(|parent| parent.is_nil()) {
            return Err(DecodeError::InvalidResponse);
        }
    }
    for item in &raw_items {
        validate_item(item, requested.owner_id, None)?;
        if !identities.insert(item.id) || item.parent_id.is_some_and(|parent| parent.is_nil()) {
            return Err(DecodeError::InvalidResponse);
        }
    }
    let all_categories_direct = raw_categories.iter().all(|category| category.parent_id.is_none_or(|parent| parent == id));
    let (direct_categories, auxiliary_categories) = if all_categories_direct {
        (raw_categories, Vec::new())
    } else {
        let mut direct = Vec::with_capacity(raw_categories.len());
        let mut auxiliary = Vec::new();
        for category in raw_categories {
            if category.parent_id.is_none_or(|parent| parent == id) {
                direct.push(category);
            } else {
                auxiliary.push(category);
            }
        }
        (direct, auxiliary)
    };
    let all_items_direct = raw_items.iter().all(|item| item.parent_id.is_none_or(|parent| parent == id));
    let (direct_items, auxiliary_items) = if all_items_direct {
        (raw_items, Vec::new())
    } else {
        let mut direct = Vec::with_capacity(raw_items.len());
        let mut auxiliary = Vec::new();
        for item in raw_items {
            if item.parent_id.is_none_or(|parent| parent == id) {
                direct.push(item);
            } else {
                auxiliary.push(item);
            }
        }
        (direct, auxiliary)
    };
    Ok(FolderDescendents {
        id,
        owner_id,
        version,
        descendent_count,
        direct_categories,
        direct_items,
        auxiliary_categories,
        auxiliary_items,
    })
}

fn validate_legacy_category(category: &Category, expected_owner: Uuid) -> Result<(), DecodeError> {
    if category.owner_id.is_some_and(|owner| owner != expected_owner) {
        return Err(DecodeError::InvalidResponse);
    }
    Ok(())
}

fn validate_item(item: &Item, expected_owner: Uuid, expected_parent: Option<Uuid>) -> Result<(), DecodeError> {
    if item.owner_id.is_some_and(|owner| owner != expected_owner)
        || expected_parent.is_some_and(|parent| item.parent_id.is_some_and(|actual| actual != parent))
    {
        return Err(DecodeError::InvalidResponse);
    }
    Ok(())
}

fn validate_category_tree(
    category: Category,
    expected_owner: Uuid,
    expected_parent: Option<Uuid>,
    depth: usize,
    counts: &mut ParseCounts,
) -> Result<Category, DecodeError> {
    if depth > MAX_AIS_DEPTH {
        return Err(DecodeError::TooLarge);
    }
    counts.categories = counts.categories.checked_add(1).ok_or(DecodeError::TooLarge)?;
    if counts.categories > MAX_CATEGORIES {
        return Err(DecodeError::TooLarge);
    }
    if category.owner_id.is_some_and(|owner| owner != expected_owner)
        || expected_parent.is_some_and(|parent| category.parent_id.is_some_and(|actual| actual != parent))
    {
        return Err(DecodeError::InvalidResponse);
    }
    let Category { id, owner_id, parent_id, name, version, descendent_count, preferred_folder_type, children } = category;
    let Some(children) = children else {
        return Ok(Category { id, owner_id, parent_id, name, version, descendent_count, preferred_folder_type, children: None });
    };
    let mut validated = Vec::with_capacity(children.len());
    for child in children {
        match child {
            Child::Category(child) => {
                validated.push(Child::Category(validate_category_tree(child, expected_owner, Some(id), depth.saturating_add(1), counts)?))
            }
            Child::Item(item) => {
                counts.items = counts.items.checked_add(1).ok_or(DecodeError::TooLarge)?;
                if counts.items > MAX_ITEMS {
                    return Err(DecodeError::TooLarge);
                }
                validate_item(&item, expected_owner, Some(id))?;
                validated.push(Child::Item(item));
            }
        }
    }
    Ok(Category { id, owner_id, parent_id, name, version, descendent_count, preferred_folder_type, children: Some(validated) })
}

#[derive(Default)]
struct CreatedCategoryFields {
    folder_id: Option<Scalar>,
    parent_id: Option<Scalar>,
    type_: Option<Scalar>,
    name: Option<Scalar>,
}

impl<'de> Deserialize<'de> for CreatedCategoryFields {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(CreatedCategoryFieldsVisitor)
    }
}

struct CreatedCategoryFieldsVisitor;

impl<'de> Visitor<'de> for CreatedCategoryFieldsVisitor {
    type Value = CreatedCategoryFields;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an AIS created category response map")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut fields = CreatedCategoryFields::default();
        while let Some(key) = access.next_key::<String>()? {
            match key.as_str() {
                "folder_id" => fields.folder_id = Some(access.next_value()?),
                "parent_id" => fields.parent_id = Some(access.next_value()?),
                "type" => fields.type_ = Some(access.next_value()?),
                "name" => fields.name = Some(access.next_value()?),
                _ => access.next_value_seed(IgnoredSeed)?,
            }
        }
        Ok(fields)
    }
}

#[derive(Default)]
struct UpdateResultFields {
    code: Option<Scalar>,
    success: Option<Scalar>,
    message: Option<Scalar>,
    fault_code: Option<Scalar>,
    server_response: Option<Scalar>,
}

impl<'de> Deserialize<'de> for UpdateResultFields {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(UpdateResultFieldsVisitor)
    }
}

struct UpdateResultFieldsVisitor;

impl<'de> Visitor<'de> for UpdateResultFieldsVisitor {
    type Value = UpdateResultFields;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an AIS update result map")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut fields = UpdateResultFields::default();
        while let Some(key) = access.next_key::<String>()? {
            match key.as_str() {
                "code" => fields.code = Some(access.next_value()?),
                "success" => fields.success = Some(access.next_value()?),
                "message" => fields.message = Some(access.next_value()?),
                "fault_code" => fields.fault_code = Some(access.next_value()?),
                "server_response" => fields.server_response = Some(access.next_value()?),
                _ => access.next_value_seed(IgnoredSeed)?,
            }
        }
        Ok(fields)
    }
}

#[derive(Default)]
struct AisEditSharedFields {
    agent_id: Option<Scalar>,
    owner_id: Option<Scalar>,
    parent_id: Option<Scalar>,
    name: Option<Scalar>,
    type_: Option<Scalar>,
    base_uri: Option<Scalar>,
    links: Option<MapValue<LinksFields>>,
}

impl AisEditSharedFields {
    fn set_scalar(&mut self, name: &str, value: Scalar) {
        let target = match name {
            "agent_id" => &mut self.agent_id,
            "owner_id" => &mut self.owner_id,
            "parent_id" => &mut self.parent_id,
            "name" => &mut self.name,
            "type" => &mut self.type_,
            "_base_uri" => &mut self.base_uri,
            _ => return,
        };
        *target = Some(value);
    }

    fn apply_to_category(&mut self, target: &mut CategoryFields) {
        target.agent_id = self.agent_id.take();
        target.owner_id = self.owner_id.take();
        target.parent_id = self.parent_id.take();
        target.name = self.name.take();
        target.type_ = self.type_.take();
        target.base_uri = self.base_uri.take();
        target.links = self.links.take();
    }

    fn apply_to_item(&mut self, target: &mut ItemFields) {
        target.agent_id = self.agent_id.take();
        target.owner_id = self.owner_id.take();
        target.parent_id = self.parent_id.take();
        target.name = self.name.take();
        target.type_ = self.type_.take();
        target.base_uri = self.base_uri.take();
        target.links = self.links.take();
    }
}

#[derive(Default)]
struct AisEditFields {
    shared: AisEditSharedFields,
    category: CategoryFields,
    item: ItemFields,
    created_categories: Option<ArrayValue<Scalar>>,
    created_items: Option<ArrayValue<Scalar>>,
    updated_categories: Option<ArrayValue<Scalar>>,
    updated_items: Option<ArrayValue<Scalar>>,
    removed_items: Option<ArrayValue<Scalar>>,
    categories_removed: Option<ArrayValue<Scalar>>,
    category_items_removed: Option<ArrayValue<Scalar>>,
    broken_links_removed: Option<ArrayValue<Scalar>>,
    active_gestures_removed: Option<ArrayValue<Scalar>>,
    moved_items: Option<ArrayValue<Scalar>>,
    updated_category_versions: Option<MapValue<BTreeMap<String, Scalar>>>,
    attachments_removed: Option<MapValue<BTreeMap<String, Scalar>>>,
    wearables_removed: Option<MapValue<BTreeMap<String, Scalar>>>,
    linked_ids: Option<MapValue<BTreeMap<String, Scalar>>>,
    update_result: Option<MapValue<UpdateResultFields>>,
    top_category_id: Option<Scalar>,
    total_items_removed: Option<Scalar>,
}

impl<'de> Deserialize<'de> for AisEditFields {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(AisEditFieldsVisitor)
    }
}

struct AisEditFieldsVisitor;

impl<'de> Visitor<'de> for AisEditFieldsVisitor {
    type Value = AisEditFields;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an AIS edit response map")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut fields = AisEditFields::default();
        while let Some(key) = access.next_key::<String>()? {
            if is_shared_scalar(&key) {
                fields.shared.set_scalar(&key, access.next_value()?);
                continue;
            }
            if key == "_links" {
                fields.shared.links = Some(access.next_value()?);
                continue;
            }
            if fields.category.read_field(&key, &mut access)? || fields.item.read_field(&key, &mut access)? {
                continue;
            }
            match key.as_str() {
                "_created_categories" => fields.created_categories = Some(access.next_value()?),
                "_created_items" => fields.created_items = Some(access.next_value()?),
                "_updated_categories" => fields.updated_categories = Some(access.next_value()?),
                "_updated_items" => fields.updated_items = Some(access.next_value()?),
                "_removed_items" => fields.removed_items = Some(access.next_value()?),
                "_categories_removed" => fields.categories_removed = Some(access.next_value()?),
                "_category_items_removed" => fields.category_items_removed = Some(access.next_value()?),
                "_broken_links_removed" => fields.broken_links_removed = Some(access.next_value()?),
                "_active_gestures_removed" => fields.active_gestures_removed = Some(access.next_value()?),
                "_moved_items" => fields.moved_items = Some(access.next_value()?),
                "_updated_category_versions" => fields.updated_category_versions = Some(access.next_value()?),
                "_attachments_removed" => fields.attachments_removed = Some(access.next_value()?),
                "_wearables_removed" => fields.wearables_removed = Some(access.next_value()?),
                "_linked_ids" => fields.linked_ids = Some(access.next_value()?),
                "_update_result" => fields.update_result = Some(access.next_value()?),
                "_top_category_id" => fields.top_category_id = Some(access.next_value()?),
                "_total_items_removed" => fields.total_items_removed = Some(access.next_value()?),
                _ => access.next_value_seed(IgnoredSeed)?,
            }
        }
        Ok(fields)
    }
}

fn is_shared_scalar(name: &str) -> bool {
    matches!(name, "agent_id" | "owner_id" | "parent_id" | "name" | "type" | "_base_uri")
}

impl AisEditFields {
    fn into_response(mut self, expected_owner: Uuid) -> Result<AisEditResponse, DecodeError> {
        let has_category = self.category.category_id.is_some();
        let has_item = self.item.item_id.is_some();
        if has_category && has_item {
            return Err(DecodeError::InvalidResponse);
        }
        if has_category {
            self.shared.apply_to_category(&mut self.category);
        } else if has_item {
            self.shared.apply_to_item(&mut self.item);
        }
        let mut category = has_category.then(|| self.category.into_category(InventoryWireFormat::Ais)).transpose()?;
        let item = has_item.then(|| self.item.into_item(InventoryWireFormat::Ais, false)).transpose()?;
        if let Some(value) = category.take() {
            category = Some(validate_category_tree(value, expected_owner, None, 0, &mut ParseCounts::default())?);
        }
        if let Some(item) = &item {
            validate_item(item, expected_owner, None)?;
        }
        validate_map_ids(&self.attachments_removed, MAX_ITEMS, MapIdKind::IntegerKey)?;
        validate_map_ids(&self.wearables_removed, MAX_ITEMS, MapIdKind::IntegerKey)?;
        validate_map_ids(&self.linked_ids, MAX_ITEMS, MapIdKind::UuidKey)?;
        validate_metadata_ids(&self.active_gestures_removed, MAX_ITEMS)?;
        validate_metadata_ids(&self.moved_items, MAX_ITEMS)?;
        if let Some(result) = &self.update_result {
            validate_update_result(result)?;
        }
        if self.top_category_id.as_ref().map(scalar_uuid).transpose()?.is_some_and(|id| id.is_nil()) {
            return Err(DecodeError::InvalidResponse);
        }
        if integer(self.total_items_removed.as_ref())?.is_some_and(|total| total < 0) {
            return Err(DecodeError::InvalidResponse);
        }
        Ok(AisEditResponse {
            category,
            item,
            created_categories: metadata_ids(self.created_categories, MAX_CATEGORIES)?,
            created_items: metadata_ids(self.created_items, MAX_ITEMS)?,
            updated_categories: metadata_ids(self.updated_categories, MAX_CATEGORIES)?,
            updated_items: metadata_ids(self.updated_items, MAX_ITEMS)?,
            removed_items: metadata_ids(self.removed_items, MAX_ITEMS)?,
            categories_removed: metadata_ids(self.categories_removed, MAX_CATEGORIES)?,
            category_items_removed: metadata_ids(self.category_items_removed, MAX_ITEMS)?,
            broken_links_removed: metadata_ids(self.broken_links_removed, MAX_ITEMS)?,
            updated_category_versions: metadata_versions(self.updated_category_versions)?,
        })
    }
}

enum MapIdKind {
    IntegerKey,
    UuidKey,
}

fn map_values<T>(value: &Option<MapValue<T>>) -> Result<Option<&T>, DecodeError> {
    value.as_ref().map(|value| value.0.as_ref().map_err(|_| DecodeError::InvalidResponse)).transpose()
}

fn metadata_ids(value: Option<ArrayValue<Scalar>>, limit: usize) -> Result<Vec<Uuid>, DecodeError> {
    let Some(value) = value else { return Ok(Vec::new()) };
    let values = value.0?;
    if values.len() > limit {
        return Err(DecodeError::TooLarge);
    }
    let mut ids = Vec::with_capacity(values.len());
    let mut unique = HashSet::with_capacity(values.len());
    for value in values {
        let id = scalar_uuid(&value)?;
        if id.is_nil() || !unique.insert(id) {
            return Err(DecodeError::InvalidResponse);
        }
        ids.push(id);
    }
    Ok(ids)
}

fn validate_metadata_ids(value: &Option<ArrayValue<Scalar>>, limit: usize) -> Result<(), DecodeError> {
    let Some(value) = value else { return Ok(()) };
    let values = value.0.as_ref().map_err(|_| DecodeError::InvalidResponse)?;
    if values.len() > limit {
        return Err(DecodeError::TooLarge);
    }
    let mut unique = HashSet::with_capacity(values.len());
    for value in values {
        let id = scalar_uuid(value)?;
        if id.is_nil() || !unique.insert(id) {
            return Err(DecodeError::InvalidResponse);
        }
    }
    Ok(())
}

fn validate_map_ids(value: &Option<MapValue<BTreeMap<String, Scalar>>>, limit: usize, kind: MapIdKind) -> Result<(), DecodeError> {
    let Some(values) = map_values(value)? else { return Ok(()) };
    if values.len() > limit {
        return Err(DecodeError::TooLarge);
    }
    let mut indices = HashSet::with_capacity(values.len());
    for (key, value) in values {
        match kind {
            MapIdKind::IntegerKey => {
                let index = key.parse::<u32>().map_err(|_| DecodeError::InvalidResponse)?;
                if !indices.insert(index) {
                    return Err(DecodeError::InvalidResponse);
                }
                scalar_uuid(value)?;
            }
            MapIdKind::UuidKey => {
                Uuid::parse_str(key).map_err(|_| DecodeError::InvalidResponse)?;
                scalar_uuid(value)?;
            }
        }
    }
    Ok(())
}

fn metadata_versions(value: Option<MapValue<BTreeMap<String, Scalar>>>) -> Result<BTreeMap<Uuid, i32>, DecodeError> {
    let Some(values) = value else { return Ok(BTreeMap::new()) };
    let values = values.0?;
    if values.len() > MAX_CATEGORIES {
        return Err(DecodeError::TooLarge);
    }
    values
        .into_iter()
        .map(|(id, version)| {
            let id = Uuid::parse_str(&id).map_err(|_| DecodeError::InvalidResponse)?;
            if id.is_nil() {
                return Err(DecodeError::InvalidResponse);
            }
            let version = integer(Some(&version))?.ok_or(DecodeError::InvalidResponse)?;
            Ok((id, version))
        })
        .collect()
}

fn validate_update_result(value: &MapValue<UpdateResultFields>) -> Result<(), DecodeError> {
    let fields = value.0.as_ref().map_err(|_| DecodeError::InvalidResponse)?;
    let code = integer(fields.code.as_ref())?.ok_or(DecodeError::InvalidResponse)?;
    if !matches!(fields.success, Some(Scalar::Boolean))
        || !(0..=8).contains(&code)
        || !matches!(fields.message.as_ref(), Some(Scalar::String(_)))
        || (fields.fault_code.is_some() && integer(fields.fault_code.as_ref())?.is_none())
        || (fields.server_response.is_some() && !matches!(fields.server_response.as_ref(), Some(Scalar::String(_))))
    {
        return Err(DecodeError::InvalidResponse);
    }
    Ok(())
}

fn named_asset_type(value: &str) -> i32 {
    match value {
        "texture" => 0,
        "sound" => 1,
        "callcard" | "callingcard" => 2,
        "landmark" => 3,
        "script" => 4,
        "clothing" => 5,
        "object" => 6,
        "notecard" => 7,
        "category" => 8,
        "lsltext" => 10,
        "lslbyte" => 11,
        "txtr_tga" => 12,
        "bodypart" => 13,
        "snd_wav" => 17,
        "img_tga" => 18,
        "jpeg" => 19,
        "animatn" => 20,
        "gesture" => 21,
        "simstate" => 22,
        "link" => 24,
        "link_f" => 25,
        "widget" => 40,
        "person" => 45,
        "mesh" => 49,
        "settings" => 56,
        "material" => 57,
        "gltf" => 58,
        "glbin" | "gltf_binary" => 59,
        "invalid" => 255,
        "-1" => -1,
        _ => 255,
    }
}

fn named_inventory_type(value: &str) -> i32 {
    match value {
        "texture" => 0,
        "sound" => 1,
        "callcard" | "callingcard" => 2,
        "landmark" => 3,
        "object" => 6,
        "notecard" => 7,
        "category" => 8,
        "root" | "root_category" => 9,
        "script" | "lsl" => 10,
        "snapshot" => 15,
        "attach" | "attachment" => 17,
        "wearable" => 18,
        "animation" => 19,
        "gesture" => 20,
        "mesh" => 22,
        "widget" => 23,
        "person" => 24,
        "settings" => 25,
        "material" => 26,
        "gltf" => 27,
        "glbin" | "gltf_binary" => 28,
        _ => 255,
    }
}

pub fn decode_ais_children(bytes: &[u8], expected_folder: Uuid, expected_owner: Uuid) -> Result<Category, DecodeError> {
    let AisCategoryValue(category) = decode_llsd(bytes)?;
    let category = validate_category_tree(category?, expected_owner, None, 0, &mut ParseCounts::default())?;
    if category.id != expected_folder {
        return Err(DecodeError::InvalidResponse);
    }
    Ok(category)
}

pub fn decode_ais_item(bytes: &[u8], expected_item: Uuid, expected_owner: Uuid) -> Result<Item, DecodeError> {
    let AisItemValue(item) = decode_llsd(bytes)?;
    let item = item?;
    validate_item(&item, expected_owner, None)?;
    if item.id != expected_item || item.parent_id.is_none_or(|parent| parent.is_nil()) {
        return Err(DecodeError::InvalidResponse);
    }
    Ok(item)
}

pub fn decode_ais_edit_response(bytes: &[u8], expected_owner: Uuid) -> Result<AisEditResponse, DecodeError> {
    decode_llsd::<AisEditFields>(bytes)?.into_response(expected_owner)
}

pub fn decode_created_category_response(bytes: &[u8]) -> Result<CreatedCategoryResponse, DecodeError> {
    let fields = decode_llsd::<CreatedCategoryFields>(bytes)?;
    let id = uuid_fields(&[fields.folder_id.as_ref()])?.filter(|id| !id.is_nil()).ok_or(DecodeError::InvalidResponse)?;
    let parent_id = uuid_fields(&[fields.parent_id.as_ref()])?.filter(|id| !id.is_nil()).ok_or(DecodeError::InvalidResponse)?;
    let preferred_folder_type = integer(fields.type_.as_ref())?.unwrap_or(-1);
    let name = string(fields.name, "")?;
    if name.len() > MAX_NAME_BYTES {
        return Err(DecodeError::InvalidResponse);
    }
    Ok(CreatedCategoryResponse { id, parent_id, preferred_folder_type, name })
}

pub fn decode_fetch_inventory_descendents2(bytes: &[u8], requested: &[RequestedFolder]) -> Result<Vec<FolderDescendents>, DecodeError> {
    if requested.is_empty() {
        return Err(DecodeError::InvalidResponse);
    }
    let fields = decode_llsd::<FetchDescendentsFields>(bytes)?;
    let folders = fields.folders.ok_or(DecodeError::InvalidResponse)?.0?;
    if folders.len() != requested.len() {
        return Err(DecodeError::InvalidResponse);
    }
    let mut by_id = std::collections::HashMap::with_capacity(folders.len());
    for FolderValue(fields) in folders {
        let fields = fields?;
        let id = uuid_fields(&[fields.folder_id.as_ref()])?.ok_or(DecodeError::InvalidResponse)?;
        if by_id.insert(id, fields).is_some() {
            return Err(DecodeError::InvalidResponse);
        }
    }
    requested
        .iter()
        .map(|requested| {
            let folder = by_id.remove(&requested.id).ok_or(DecodeError::InvalidResponse)?;
            parse_folder_descendents(folder, *requested)
        })
        .collect()
}

pub fn decode_fetch_inventory2(bytes: &[u8], expected_owner: Uuid, requested_items: &[Uuid]) -> Result<Vec<Item>, DecodeError> {
    let fields = decode_llsd::<FetchItemsFields>(bytes)?;
    let items = fields.items.ok_or(DecodeError::InvalidResponse)?.0?;
    if items.len() > requested_items.len() {
        return Err(DecodeError::InvalidResponse);
    }
    let requested = requested_items.iter().copied().collect::<HashSet<_>>();
    let mut returned = HashSet::with_capacity(items.len());
    for item in &items {
        validate_item(item, expected_owner, None)?;
        if item.parent_id.is_none_or(|parent| parent.is_nil()) || !requested.contains(&item.id) || !returned.insert(item.id) {
            return Err(DecodeError::InvalidResponse);
        }
    }
    Ok(items)
}
