use std::{
    collections::btree_map::Entry,
    fmt,
    mem::{self, size_of},
};

use ::uuid::Uuid;
use serde::{
    Deserialize, Deserializer,
    de::{DeserializeSeed, EnumAccess, Error as DeError, MapAccess, SeqAccess, VariantAccess, Visitor},
};

use super::{Binary, Map, Uri, VALUE_ENUM, Value, map_heap_bytes};

const HEAP_LIMIT_ERROR: &str = "LLSD heap limit exceeded";

struct HeapBudget {
    max_bytes: usize,
    used_bytes: usize,
}

impl HeapBudget {
    fn new(max_bytes: usize) -> Self {
        Self { max_bytes, used_bytes: 0 }
    }

    fn charge<E: DeError>(&mut self, bytes: usize) -> std::result::Result<(), E> {
        let Some(used_bytes) = self.used_bytes.checked_add(bytes) else {
            return Err(heap_limit_error());
        };
        if used_bytes > self.max_bytes {
            return Err(heap_limit_error());
        }
        self.used_bytes = used_bytes;
        Ok(())
    }

    fn release(&mut self, bytes: usize) {
        self.used_bytes -= bytes;
    }

    fn remaining(&self) -> usize {
        self.max_bytes - self.used_bytes
    }
}

struct ValueBuilder<'a> {
    budget: &'a mut HeapBudget,
}

impl<'de> DeserializeSeed<'de> for ValueBuilder<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error> {
        deserializer.deserialize_enum(VALUE_ENUM, &[], self)
    }
}

impl<'de> Visitor<'de> for ValueBuilder<'_> {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an LLSD value")
    }

    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> std::result::Result<Self::Value, A::Error> {
        let (kind, variant) = data.variant::<ValueKind>()?;
        match kind {
            ValueKind::Undefined => {
                variant.unit_variant()?;
                Ok(Value::Undefined)
            }
            ValueKind::Boolean => variant.newtype_variant().map(Value::Boolean),
            ValueKind::Integer => variant.newtype_variant().map(Value::Integer),
            ValueKind::Real => variant.newtype_variant().map(Value::Real),
            ValueKind::Uuid => {
                let value = variant.newtype_variant::<String>()?;
                Uuid::parse_str(&value).map(Value::Uuid).map_err(A::Error::custom)
            }
            ValueKind::String => {
                let value = variant.newtype_variant::<String>()?;
                self.budget.charge::<A::Error>(value.capacity())?;
                Ok(Value::String(value))
            }
            ValueKind::Date => variant.newtype_variant().map(Value::Date),
            ValueKind::Uri => {
                let value = variant.newtype_variant::<Uri>()?;
                self.budget.charge::<A::Error>(value.capacity())?;
                Ok(Value::Uri(value))
            }
            ValueKind::Binary => {
                let value = variant.newtype_variant::<Binary>()?;
                self.budget.charge::<A::Error>(value.capacity())?;
                Ok(Value::Binary(value))
            }
            ValueKind::Array => variant.newtype_variant_seed(ArrayBuilder { budget: self.budget }),
            ValueKind::Map => variant.newtype_variant_seed(MapBuilder { budget: self.budget }),
        }
    }
}

#[derive(Deserialize)]
#[serde(field_identifier)]
enum ValueKind {
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

struct ArrayBuilder<'a> {
    budget: &'a mut HeapBudget,
}

impl<'de> DeserializeSeed<'de> for ArrayBuilder<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for ArrayBuilder<'_> {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an LLSD array")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> std::result::Result<Self::Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = access.next_element_seed(ArrayElementBuilder { budget: self.budget, values: &mut values })? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }
}

struct ArrayElementBuilder<'a> {
    budget: &'a mut HeapBudget,
    values: &'a mut Vec<Value>,
}

impl<'de> DeserializeSeed<'de> for ArrayElementBuilder<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error> {
        let Self { budget, values } = self;
        reserve_array::<D::Error>(values, budget)?;
        ValueBuilder { budget }.deserialize(deserializer)
    }
}

struct MapBuilder<'a> {
    budget: &'a mut HeapBudget,
}

impl<'de> DeserializeSeed<'de> for MapBuilder<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for MapBuilder<'_> {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an LLSD map")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> std::result::Result<Self::Value, A::Error> {
        let mut values = Map::new();
        while let Some(key) = access.next_key::<String>()? {
            let len = values.len();
            match values.entry(key) {
                Entry::Occupied(mut entry) => {
                    let old_value = mem::replace(entry.get_mut(), Value::Undefined);
                    self.budget.release(old_value.estimated_heap_bytes());
                    drop(old_value);
                    *entry.get_mut() = access.next_value_seed(ValueBuilder { budget: self.budget })?;
                }
                Entry::Vacant(entry) => {
                    let next_len = len.checked_add(1).ok_or_else(heap_limit_error)?;
                    let entry_heap = map_heap_bytes(next_len).saturating_sub(map_heap_bytes(len));
                    let added_heap = entry_heap.checked_add(entry.key().capacity()).ok_or_else(heap_limit_error)?;
                    self.budget.charge::<A::Error>(added_heap)?;
                    entry.insert(access.next_value_seed(ValueBuilder { budget: self.budget })?);
                }
            }
        }
        Ok(Value::Map(values))
    }
}

fn reserve_array<E: DeError>(values: &mut Vec<Value>, budget: &mut HeapBudget) -> std::result::Result<(), E> {
    if values.len() < values.capacity() {
        return Ok(());
    }
    let current_capacity = values.capacity();
    let min_capacity = values.len().checked_add(1).ok_or_else(heap_limit_error)?;
    let value_size = size_of::<Value>();
    let max_capacity = current_capacity.saturating_add(budget.remaining() / value_size);
    if max_capacity < min_capacity {
        return Err(heap_limit_error());
    }
    let growth_capacity = if current_capacity == 0 { 4 } else { current_capacity.saturating_mul(2) }.max(min_capacity);
    let target_capacity = growth_capacity.min(max_capacity);
    let current_bytes = current_capacity.checked_mul(value_size).ok_or_else(heap_limit_error)?;
    let target_bytes = target_capacity.checked_mul(value_size).ok_or_else(heap_limit_error)?;
    let added_bytes = target_bytes.checked_sub(current_bytes).ok_or_else(heap_limit_error)?;
    budget.charge::<E>(added_bytes)?;

    let additional = target_capacity.checked_sub(values.len()).ok_or_else(heap_limit_error)?;
    if values.try_reserve_exact(additional).is_err() {
        budget.release(added_bytes);
        return Err(E::custom("LLSD allocation failed"));
    }

    let actual_bytes = values.capacity().checked_mul(value_size).ok_or_else(heap_limit_error)?;
    if actual_bytes > target_bytes {
        budget.charge::<E>(actual_bytes - target_bytes)?;
    } else {
        budget.release(target_bytes - actual_bytes);
    }
    Ok(())
}

pub(super) fn deserialize_with_heap_limit<'de, D: Deserializer<'de>>(
    deserializer: D,
    max_heap_bytes: usize,
) -> std::result::Result<Value, D::Error> {
    let mut budget = HeapBudget::new(max_heap_bytes);
    ValueBuilder { budget: &mut budget }.deserialize(deserializer)
}

fn heap_limit_error<E: DeError>() -> E {
    E::custom(HEAP_LIMIT_ERROR)
}
