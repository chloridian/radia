use std::fmt;

use serde::{Deserialize, Deserializer, Serializer, de, de::Visitor};
use uuid::Uuid;

use crate::value::UUID_NEWTYPE;

struct UuidVisitor;

impl<'de> Visitor<'de> for UuidVisitor {
    type Value = Uuid;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a UUID")
    }

    fn visit_newtype_struct<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        let value = String::deserialize(deserializer)?;
        Uuid::parse_str(&value).map_err(de::Error::custom)
    }
}

pub fn serialize<S: Serializer>(value: &Uuid, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_newtype_struct(UUID_NEWTYPE, &value.to_string())
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Uuid, D::Error> {
    deserializer.deserialize_newtype_struct(UUID_NEWTYPE, UuidVisitor)
}
