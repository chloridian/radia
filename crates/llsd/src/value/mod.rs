mod binary;
mod date;
mod de;
mod uri;
pub mod uuid;

use std::{collections::BTreeMap, mem::size_of};

use ::uuid::Uuid;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub use self::binary::Binary;
pub use self::date::Date;
pub use self::uri::Uri;

pub(crate) const UUID_NEWTYPE: &str = "llsd::UUID";
pub(crate) const URI_NEWTYPE: &str = "llsd::URI";
pub(crate) const DATE_NEWTYPE: &str = "llsd::Date";
pub(crate) const BINARY_NEWTYPE: &str = "llsd::Binary";
pub(crate) const VALUE_ENUM: &str = "Value";

pub type Map = BTreeMap<String, Value>;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Undefined,
    Boolean(bool),
    Integer(i32),
    Real(f64),
    Uuid(Uuid),
    String(String),
    Date(Date),
    Uri(Uri),
    Binary(Binary),
    Array(Vec<Self>),
    Map(Map),
}

impl Value {
    pub fn deserialize_with_heap_limit<'de, D: Deserializer<'de>>(
        deserializer: D,
        max_heap_bytes: usize,
    ) -> std::result::Result<Self, D::Error> {
        de::deserialize_with_heap_limit(deserializer, max_heap_bytes)
    }

    pub fn estimated_heap_bytes(&self) -> usize {
        match self {
            Self::String(value) => value.capacity(),
            Self::Uri(value) => value.capacity(),
            Self::Binary(value) => value.capacity(),
            Self::Array(values) => values
                .capacity()
                .saturating_mul(size_of::<Value>())
                .saturating_add(values.iter().fold(0usize, |size, value| size.saturating_add(value.estimated_heap_bytes()))),
            Self::Map(values) => map_heap_bytes(values.len()).saturating_add(
                values
                    .iter()
                    .fold(0usize, |size, (key, value)| size.saturating_add(key.capacity()).saturating_add(value.estimated_heap_bytes())),
            ),
            Self::Undefined | Self::Boolean(_) | Self::Integer(_) | Self::Real(_) | Self::Uuid(_) | Self::Date(_) => 0,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Boolean(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_integer(&self) -> Option<i32> {
        match self {
            Self::Integer(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_real(&self) -> Option<f64> {
        match self {
            Self::Real(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_finite_f32(&self) -> Option<f32> {
        let value = match self {
            Self::Integer(value) => *value as f32,
            Self::Real(value) => *value as f32,
            _ => return None,
        };
        value.is_finite().then_some(value)
    }

    pub fn as_uuid(&self) -> Option<Uuid> {
        match self {
            Self::Uuid(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            Self::Uri(value) => Some(value.as_str()),
            _ => None,
        }
    }

    pub fn as_uri(&self) -> Option<&Uri> {
        match self {
            Self::Uri(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_date(&self) -> Option<Date> {
        match self {
            Self::Date(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_binary(&self) -> Option<&[u8]> {
        match self {
            Self::Binary(value) => Some(value.as_slice()),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Self]> {
        match self {
            Self::Array(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&Map> {
        match self {
            Self::Map(value) => Some(value),
            _ => None,
        }
    }
}

impl Serialize for Value {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Undefined => serializer.serialize_unit(),
            Self::Boolean(value) => serializer.serialize_bool(*value),
            Self::Integer(value) => serializer.serialize_i32(*value),
            Self::Real(value) => serializer.serialize_f64(*value),
            Self::Uuid(value) => serializer.serialize_newtype_struct(UUID_NEWTYPE, &value.to_string()),
            Self::String(value) => serializer.serialize_str(value),
            Self::Date(value) => value.serialize(serializer),
            Self::Uri(value) => value.serialize(serializer),
            Self::Binary(value) => value.serialize(serializer),
            Self::Array(value) => value.serialize(serializer),
            Self::Map(value) => value.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        de::deserialize_with_heap_limit(deserializer, usize::MAX)
    }
}

fn map_heap_bytes(len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let entry_size = size_of::<(String, Value)>();
    len.saturating_mul(entry_size.saturating_mul(3))
        .saturating_add(entry_size.saturating_mul(11))
        .saturating_add(size_of::<usize>().saturating_mul(12))
}

#[cfg(test)]
mod tests {
    use super::Value;

    #[test]
    fn finite_numeric_conversion() {
        for (value, expected) in [
            (Value::Integer(i32::MIN), -2_147_483_648.0),
            (Value::Integer(i32::MAX), 2_147_483_648.0),
            (Value::Real(0.25), 0.25),
            (Value::Real(f64::from(f32::MAX)), f32::MAX),
            (Value::Real(-f64::from(f32::MAX)), -f32::MAX),
            (Value::Real(-0.0), -0.0),
        ] {
            assert_eq!(value.as_finite_f32().map(f32::to_bits), Some(expected.to_bits()), "{value:?}");
        }
    }

    #[test]
    fn rejects_non_finite_conversion() {
        for value in [
            Value::Undefined,
            Value::Boolean(true),
            Value::String("1".into()),
            Value::Real(f64::NAN),
            Value::Real(f64::INFINITY),
            Value::Real(f64::NEG_INFINITY),
            Value::Real(f64::MAX),
            Value::Real(-f64::MAX),
        ] {
            assert_eq!(value.as_finite_f32(), None, "{value:?}");
        }
    }
}
