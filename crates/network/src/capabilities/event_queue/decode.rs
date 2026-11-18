use std::fmt;

use llsd::{Limits, Value, xml};
use serde::{
    Deserialize, Deserializer,
    de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor},
};
use tokio::{sync::OwnedSemaphorePermit, task::JoinError};

use crate::{
    capabilities::budget::{
        MAX_EVENT_QUEUE_BATCH_RETAINED_BYTES, MAX_EVENT_QUEUE_RESPONSE_BYTES, MAX_LLSD_NESTING_DEPTH, MAX_LLSD_XML_ELEMENTS,
        capability_llsd_limits, spawn_blocking_with_permit,
    },
    event::{CapabilityEvent, EventEnvelope},
};

const MAX_EVENT_QUEUE_BATCH_EVENTS: usize = 1024;
const MAX_EVENT_QUEUE_CONTAINER_ITEMS: usize = super::MAX_QUEUED_CAPABILITY_EVENT_BYTES / size_of::<Value>();

#[derive(Debug)]
pub(super) enum BatchDecodeError {
    Decode(String),
    Task(JoinError),
}

impl fmt::Display for BatchDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(error) => formatter.write_str(error),
            Self::Task(error) => write!(formatter, "EventQueueGet decoder task failed: {error}"),
        }
    }
}

enum BatchProblem {
    InvalidEvents,
    InvalidEvent,
    InvalidMessage,
    TooManyEvents,
    RetainedBytes,
}

impl fmt::Display for BatchProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEvents => formatter.write_str("EventQueueGet response returned an invalid events array."),
            Self::InvalidEvent => formatter.write_str("EventQueueGet event was not a map."),
            Self::InvalidMessage => formatter.write_str("EventQueueGet event omitted a valid message name."),
            Self::TooManyEvents => write!(formatter, "EventQueueGet batch exceeded the {MAX_EVENT_QUEUE_BATCH_EVENTS}-event limit."),
            Self::RetainedBytes => {
                write!(formatter, "EventQueueGet batch exceeded the {MAX_EVENT_QUEUE_BATCH_RETAINED_BYTES}-byte retained-data limit.")
            }
        }
    }
}

struct DecodedBatch {
    id: i32,
    events: Vec<CapabilityEvent>,
}

impl<'de> Deserialize<'de> for DecodedBatch {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(BatchVisitor)
    }
}

struct BatchVisitor;

impl<'de> Visitor<'de> for BatchVisitor {
    type Value = DecodedBatch;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an EventQueueGet response map")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut fields: A) -> Result<Self::Value, A::Error> {
        let mut id = None;
        let mut events = None;
        while let Some(key) = fields.next_key::<String>()? {
            match key.as_str() {
                "id" => {
                    drop(id.take());
                    id = Some(fields.next_value_seed(ValueSeed(super::MAX_QUEUED_CAPABILITY_EVENT_BYTES))?);
                }
                "events" => {
                    drop(events.take());
                    events = Some(fields.next_value_seed(EventsVisitor)?);
                }
                _ => {
                    fields.next_value::<IgnoredAny>()?;
                }
            }
        }
        let Some(Value::Integer(id)) = id else {
            return Err(de::Error::custom("EventQueueGet response omitted a valid id."));
        };
        let events =
            events.ok_or_else(|| de::Error::custom("EventQueueGet response omitted its events array."))?.map_err(de::Error::custom)?;
        Ok(DecodedBatch { id, events })
    }
}

struct EventsVisitor;

impl<'de> DeserializeSeed<'de> for EventsVisitor {
    type Value = Result<Vec<CapabilityEvent>, BatchProblem>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for EventsVisitor {
    type Value = Result<Vec<CapabilityEvent>, BatchProblem>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an EventQueueGet events array")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut values: A) -> Result<Self::Value, A::Error> {
        let mut events = Vec::new();
        let mut event_data_bytes = 0usize;
        let mut problem = None;
        loop {
            if problem.is_some() {
                while values.next_element::<IgnoredAny>()?.is_some() {}
                break;
            }
            let Some(event) = values.next_element_seed(EventVisitor)? else { break };
            let event = match event {
                Ok(event) => event,
                Err(error) => {
                    problem = Some(error);
                    events.clear();
                    continue;
                }
            };
            if events.len() == MAX_EVENT_QUEUE_BATCH_EVENTS {
                problem = Some(BatchProblem::TooManyEvents);
                events.clear();
                continue;
            }
            event_data_bytes = event_data_bytes.saturating_add(event.retained_bytes.saturating_sub(size_of::<CapabilityEvent>()));
            if batch_retained_size(event_data_bytes, events.len() + 1) > MAX_EVENT_QUEUE_BATCH_RETAINED_BYTES {
                problem = Some(BatchProblem::RetainedBytes);
                events.clear();
                continue;
            }
            if events.len() == events.capacity() {
                let capacity = events.capacity().saturating_mul(2).clamp(4, MAX_EVENT_QUEUE_BATCH_EVENTS);
                events
                    .try_reserve_exact(capacity - events.len())
                    .map_err(|_| de::Error::custom("Could not grow the EventQueueGet batch."))?;
            }
            events.push(event);
        }
        Ok(match problem {
            Some(error) => Err(error),
            None => Ok(events.into_boxed_slice().into_vec()),
        })
    }

    fn visit_map<A: MapAccess<'de>>(self, mut fields: A) -> Result<Self::Value, A::Error> {
        while fields.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(Err(BatchProblem::InvalidEvents))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(Err(BatchProblem::InvalidEvents))
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(Err(BatchProblem::InvalidEvents))
    }
    fn visit_i32<E: de::Error>(self, _: i32) -> Result<Self::Value, E> {
        Ok(Err(BatchProblem::InvalidEvents))
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(Err(BatchProblem::InvalidEvents))
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(Err(BatchProblem::InvalidEvents))
    }
    fn visit_bytes<E: de::Error>(self, _: &[u8]) -> Result<Self::Value, E> {
        Ok(Err(BatchProblem::InvalidEvents))
    }
}

struct EventVisitor;

impl<'de> DeserializeSeed<'de> for EventVisitor {
    type Value = Result<CapabilityEvent, BatchProblem>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for EventVisitor {
    type Value = Result<CapabilityEvent, BatchProblem>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an EventQueueGet event map")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut fields: A) -> Result<Self::Value, A::Error> {
        let heap_limit = super::MAX_QUEUED_CAPABILITY_EVENT_BYTES.saturating_sub(size_of::<CapabilityEvent>());
        let mut message: Option<String> = None;
        let mut body = Value::Undefined;
        while let Some(key) = fields.next_key::<String>()? {
            match key.as_str() {
                "message" => {
                    drop(message.take());
                    let limit = heap_limit.saturating_sub(body.estimated_heap_bytes());
                    if let Value::String(value) = fields.next_value_seed(ValueSeed(limit))? {
                        message = Some(value);
                    }
                }
                "body" => {
                    drop(body);
                    let limit = heap_limit.saturating_sub(message.as_ref().map_or(0, String::capacity));
                    body = fields.next_value_seed(ValueSeed(limit))?;
                }
                _ => {
                    fields.next_value::<IgnoredAny>()?;
                }
            }
        }
        let Some(message) = message else { return Ok(Err(BatchProblem::InvalidMessage)) };
        let retained_bytes = queued_event_retained_size(message.capacity(), &body);
        if retained_bytes > super::MAX_QUEUED_CAPABILITY_EVENT_BYTES {
            return Err(de::Error::custom(format!(
                "EventQueueGet event exceeded the {}-byte limit.",
                super::MAX_QUEUED_CAPABILITY_EVENT_BYTES
            )));
        }
        Ok(Ok(CapabilityEvent { message, body, retained_bytes }))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut values: A) -> Result<Self::Value, A::Error> {
        while values.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Err(BatchProblem::InvalidEvent))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(Err(BatchProblem::InvalidEvent))
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(Err(BatchProblem::InvalidEvent))
    }
    fn visit_i32<E: de::Error>(self, _: i32) -> Result<Self::Value, E> {
        Ok(Err(BatchProblem::InvalidEvent))
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(Err(BatchProblem::InvalidEvent))
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(Err(BatchProblem::InvalidEvent))
    }
    fn visit_bytes<E: de::Error>(self, _: &[u8]) -> Result<Self::Value, E> {
        Ok(Err(BatchProblem::InvalidEvent))
    }
}

struct ValueSeed(usize);

impl<'de> DeserializeSeed<'de> for ValueSeed {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        Value::deserialize_with_heap_limit(deserializer, self.0)
    }
}

pub(super) async fn decode_admitted_event_batch(
    body: Vec<u8>,
    event_queue_permit: OwnedSemaphorePermit,
    body_permit: OwnedSemaphorePermit,
) -> Result<(i32, Vec<CapabilityEvent>, OwnedSemaphorePermit), BatchDecodeError> {
    let (decoded, (event_queue_permit, body_permit)) =
        spawn_blocking_with_permit((event_queue_permit, body_permit), move || decode_event_batch(&body).map_err(BatchDecodeError::Decode))
            .await
            .map_err(BatchDecodeError::Task)?;
    drop(body_permit);
    decoded.map(|(id, events)| (id, events, event_queue_permit))
}

pub(super) fn decode_event_batch(body: &[u8]) -> Result<(i32, Vec<CapabilityEvent>), String> {
    let batch: DecodedBatch = xml::from_slice_with_limits(body, event_queue_llsd_limits()).map_err(|error| {
        let reason = match error {
            llsd::Error::LimitExceeded("input bytes") => {
                format!("EventQueueGet response exceeded the {MAX_EVENT_QUEUE_RESPONSE_BYTES}-byte limit.")
            }
            llsd::Error::LimitExceeded("nesting depth") => format!("LLSD nesting exceeded the {MAX_LLSD_NESTING_DEPTH}-level limit."),
            llsd::Error::LimitExceeded("value count") => format!("LLSD exceeded the {MAX_LLSD_XML_ELEMENTS}-element limit."),
            llsd::Error::LimitExceeded("container items") => {
                format!("LLSD container exceeded the {MAX_EVENT_QUEUE_CONTAINER_ITEMS}-item limit.")
            }
            llsd::Error::LimitExceeded(limit @ ("string bytes" | "map key bytes" | "binary bytes")) => format!(
                "LLSD {limit} limit exceeded; EventQueueGet event exceeded the {}-byte scalar limit.",
                super::MAX_QUEUED_CAPABILITY_EVENT_BYTES
            ),
            error => error.to_string(),
        };
        format!("Could not decode an EventQueueGet response: {reason}")
    })?;
    Ok((batch.id, batch.events))
}

fn event_queue_llsd_limits() -> Limits {
    let event_limit = super::MAX_QUEUED_CAPABILITY_EVENT_BYTES;
    Limits {
        max_string_bytes: event_limit,
        max_map_key_bytes: event_limit,
        max_binary_bytes: event_limit,
        max_container_items: MAX_EVENT_QUEUE_CONTAINER_ITEMS,
        ..capability_llsd_limits(MAX_EVENT_QUEUE_RESPONSE_BYTES)
    }
}

pub(super) fn queued_event_retained_size(message_capacity: usize, body: &Value) -> usize {
    size_of::<CapabilityEvent>().saturating_add(message_capacity).saturating_add(body.estimated_heap_bytes())
}

pub(super) fn capability_event_batch_retained_size(events: &[CapabilityEvent], event_capacity: usize) -> usize {
    let event_data_bytes =
        events.iter().fold(0usize, |total, event| total.saturating_add(event.retained_bytes.saturating_sub(size_of::<CapabilityEvent>())));
    batch_retained_size(event_data_bytes, event_capacity)
}

fn batch_retained_size(event_data_bytes: usize, event_capacity: usize) -> usize {
    size_of::<EventEnvelope>()
        .saturating_add(size_of::<OwnedSemaphorePermit>())
        .saturating_add(2 * size_of::<usize>())
        .saturating_add(event_capacity.saturating_mul(size_of::<CapabilityEvent>()))
        .saturating_add(event_data_bytes)
}
