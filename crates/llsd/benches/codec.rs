use std::{collections::BTreeMap, fmt::Debug};

use divan::black_box;
use llsd::{Binary, Map, Uri, Value, binary as llsd_binary, notation as llsd_notation, xml as llsd_xml};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

#[derive(Debug, Deserialize, Serialize)]
#[serde(transparent)]
struct CapabilityMap(BTreeMap<String, Uri>);

#[derive(Debug, Deserialize, Serialize)]
struct EventBatch {
    id: i32,
    events: Vec<EventMessage>,
}

#[derive(Debug, Deserialize, Serialize)]
struct EventMessage {
    message: String,
    body: Value,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(transparent)]
struct CapabilityValue(Value);

#[derive(Debug, Deserialize, Serialize)]
#[serde(transparent)]
struct EventBatchValue(Value);

#[derive(Debug, Deserialize, Serialize)]
#[serde(transparent)]
struct InventoryBatch(Value);

#[derive(Debug, Deserialize, Serialize)]
#[serde(transparent)]
struct NestedArray(Value);

trait CodecFixture: Serialize + DeserializeOwned + Debug + Send + Sync + 'static {
    fn fixture() -> Self;
}

impl CodecFixture for CapabilityMap {
    fn fixture() -> Self {
        capabilities()
    }
}

impl CodecFixture for CapabilityValue {
    fn fixture() -> Self {
        Self(capability_value(&capabilities()))
    }
}

impl CodecFixture for EventBatch {
    fn fixture() -> Self {
        event_batch()
    }
}

impl CodecFixture for EventBatchValue {
    fn fixture() -> Self {
        Self(event_batch_value(&event_batch()))
    }
}

impl CodecFixture for InventoryBatch {
    fn fixture() -> Self {
        Self(inventory_batch())
    }
}

impl CodecFixture for NestedArray {
    fn fixture() -> Self {
        Self(nested_array_value(60))
    }
}

fn capabilities() -> CapabilityMap {
    const NAMES: &[&str] = &[
        "ChatSessionRequest",
        "CopyInventoryFromNotecard",
        "CreateInventoryCategory",
        "EventQueueGet",
        "FetchInventory2",
        "GetAgentDisplayName",
        "GetAvatarPickerSearch",
        "GetDisplayNames",
        "GetExperienceInfo",
        "GetExperiences",
        "GetMesh",
        "GetObjectCost",
        "GetObjectPhysicsData",
        "GetTexture",
        "GroupMemberData",
        "MapLayer",
        "MapLayerGod",
        "NewFileAgentInventory",
        "RemoveInventoryObjects",
        "RequestTextureDownload",
        "Seed",
        "UpdateGestureAgentInventory",
        "UpdateInventoryItem",
        "UploadAgentInventory",
        "UploadAgentProfileImage",
        "UploadAgentPreferences",
        "UploadBakedTexture",
        "ViewerAsset",
        "ViewerStats",
        "UntrustedSimulatorMessage",
    ];

    CapabilityMap(
        NAMES
            .iter()
            .enumerate()
            .map(|(index, name)| ((*name).to_owned(), Uri::from(format!("https://cap.example.invalid/region/{index:04}"))))
            .collect(),
    )
}

fn capability_value(capabilities: &CapabilityMap) -> Value {
    Value::Map(capabilities.0.iter().map(|(name, uri)| (name.clone(), Value::Uri(uri.clone()))).collect())
}

fn event_batch() -> EventBatch {
    const TYPES: &[&str] = &["ObjectUpdate", "ChatFromSimulator", "ParcelProperties", "CoarseLocationUpdate"];

    let events = (0..48)
        .map(|index| {
            let mut body = Map::new();
            body.insert("local_id".into(), Value::Integer(index));
            body.insert("name".into(), Value::String(format!("Object {index:03}")));
            body.insert(
                "position".into(),
                Value::Array(vec![
                    Value::Real(index as f64 * 1.25),
                    Value::Real(index as f64 * -0.5),
                    Value::Real(20.0 + f64::from(index)),
                ]),
            );
            body.insert("flags".into(), Value::Integer(index % 16));
            body.insert("visible".into(), Value::Boolean(index % 3 != 0));
            body.insert("payload".into(), Value::Binary(Binary::new(vec![(index % 251) as u8; 24])));

            EventMessage { message: TYPES[index as usize % TYPES.len()].to_owned(), body: Value::Map(body) }
        })
        .collect();

    EventBatch { id: 8192, events }
}

fn event_batch_value(batch: &EventBatch) -> Value {
    let events = batch
        .events
        .iter()
        .map(|event| {
            Value::Map(Map::from([("body".to_owned(), event.body.clone()), ("message".to_owned(), Value::String(event.message.clone()))]))
        })
        .collect();

    Value::Map(Map::from([("events".to_owned(), Value::Array(events)), ("id".to_owned(), Value::Integer(batch.id))]))
}

fn inventory_batch() -> Value {
    let items = (0..96)
        .map(|index| {
            Value::Map(Map::from([
                ("name".to_owned(), Value::String(format!("Inventory item {index:03}"))),
                ("description".to_owned(), Value::String("viewer-created inventory entry ".repeat(8))),
                ("asset_data".to_owned(), Value::Binary(Binary::new(vec![(index % 251) as u8; 512]))),
                ("permissions".to_owned(), Value::Integer(index % 16)),
            ]))
        })
        .collect();

    Value::Map(Map::from([("items".to_owned(), Value::Array(items)), ("version".to_owned(), Value::Integer(1))]))
}

fn nested_array_value(depth: usize) -> Value {
    (0..depth).fold(Value::Integer(1), |value, _| Value::Array(vec![value]))
}

#[divan::bench(types = [CapabilityMap, CapabilityValue, EventBatch, EventBatchValue, InventoryBatch, NestedArray])]
fn xml_encode<T: CodecFixture>(bencher: divan::Bencher) {
    let value = T::fixture();

    bencher.bench_local(|| {
        black_box(llsd_xml::to_vec(black_box(&value)).expect("encode LLSD XML"));
    });
}

#[divan::bench(types = [CapabilityMap, CapabilityValue, EventBatch, EventBatchValue, InventoryBatch, NestedArray])]
fn xml_decode<T: CodecFixture>(bencher: divan::Bencher) {
    let value = T::fixture();
    let input = llsd_xml::to_vec(&value).expect("encode benchmark fixture");

    bencher.bench_local(|| {
        black_box(llsd_xml::from_slice::<T>(black_box(&input)).expect("decode LLSD XML"));
    });
}

#[divan::bench(types = [CapabilityMap, CapabilityValue, EventBatch, EventBatchValue])]
fn xml_bounded_encode<T: CodecFixture>(bencher: divan::Bencher) {
    let value = T::fixture();
    let input = llsd_xml::to_vec(&value).expect("encode benchmark fixture");
    let max_bytes = input.len();

    bencher.bench_local(|| {
        black_box(llsd_xml::to_vec_with_max_bytes(black_box(&value), max_bytes).expect("bounded LLSD XML encode"));
    });
}

#[divan::bench(types = [CapabilityMap, CapabilityValue, EventBatch, EventBatchValue, InventoryBatch, NestedArray])]
fn binary_encode<T: CodecFixture>(bencher: divan::Bencher) {
    let value = T::fixture();

    bencher.bench_local(|| {
        black_box(llsd_binary::to_vec(black_box(&value)).expect("encode LLSD binary"));
    });
}

#[divan::bench(types = [CapabilityMap, CapabilityValue, EventBatch, EventBatchValue, InventoryBatch, NestedArray])]
fn binary_decode<T: CodecFixture>(bencher: divan::Bencher) {
    let value = T::fixture();
    let input = llsd_binary::to_vec(&value).expect("encode benchmark fixture");

    bencher.bench_local(|| {
        black_box(llsd_binary::from_slice::<T>(black_box(&input)).expect("decode LLSD binary"));
    });
}

#[divan::bench(types = [CapabilityMap, CapabilityValue, EventBatch, EventBatchValue, InventoryBatch, NestedArray])]
fn notation_encode<T: CodecFixture>(bencher: divan::Bencher) {
    let value = T::fixture();

    bencher.bench_local(|| {
        black_box(llsd_notation::to_vec(black_box(&value)).expect("encode LLSD notation"));
    });
}

#[divan::bench(types = [CapabilityMap, CapabilityValue, EventBatch, EventBatchValue, InventoryBatch, NestedArray])]
fn notation_decode<T: CodecFixture>(bencher: divan::Bencher) {
    let value = T::fixture();
    let input = llsd_notation::to_vec(&value).expect("encode benchmark fixture");

    bencher.bench_local(|| {
        black_box(llsd_notation::from_slice::<T>(black_box(&input)).expect("decode LLSD notation"));
    });
}
