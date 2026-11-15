use std::{assert_matches, mem::size_of};

use llsd::{Binary, Date, Error, Map, Uri, Value, binary, notation, xml};
use serde::{Deserialize, Deserializer};
use test_support::allocation::{self, CountingAllocator, live_bytes};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

#[derive(Debug)]
struct Bounded<const BYTES: usize>(Value);

impl<'de, const BYTES: usize> Deserialize<'de> for Bounded<BYTES> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Value::deserialize_with_heap_limit(deserializer, BYTES).map(Self)
    }
}

#[test]
fn zero_budget_allows_empty() {
    type Encode = fn(&Value) -> llsd::Result<Vec<u8>>;
    type Decode = fn(&[u8]) -> llsd::Result<Bounded<0>>;
    let formats: [(&str, Encode, Decode); 3] = [
        ("XML", xml::to_vec, xml::from_slice),
        ("binary", binary::to_vec, binary::from_slice),
        ("notation", notation::to_vec, notation::from_slice),
    ];
    for expected in [
        Value::Undefined,
        Value::Boolean(true),
        Value::Integer(-7),
        Value::Real(1.25),
        Value::Uuid(Default::default()),
        Value::String(String::new()),
        Value::Date(Date::UNIX_EPOCH),
        Value::Uri(Uri::default()),
        Value::Binary(Binary::default()),
        Value::Array(Vec::new()),
        Value::Map(Map::new()),
    ] {
        for (name, encode, decode) in formats {
            let input = encode(&expected).expect("encode zero-heap value");
            assert_eq!(decode(&input).expect("zero-heap value").0, expected, "{name}");
        }
    }
}

#[test]
fn scalar_boundary_is_exact() {
    let input = "<llsd><string>small scalar</string></llsd>";
    let value = xml::from_str::<Bounded<12>>(input).expect("exact scalar boundary").0;
    assert_eq!(value, Value::String("small scalar".into()));
    assert_matches!(xml::from_str::<Bounded<11>>(input), Err(Error::InvalidValue(message)) if message == "LLSD heap limit exceeded");
}

#[test]
fn singleton_arrays_fit_budget() {
    const ITEMS: usize = 32;
    const BYTES: usize = 2 * ITEMS * size_of::<Value>();
    let mut input = vec![b'['];
    input.extend_from_slice(&(ITEMS as u32).to_be_bytes());
    for _ in 0..ITEMS {
        input.extend_from_slice(b"[\0\0\0\x01i\0\0\0\x01]");
    }
    input.push(b']');
    let value = binary::from_slice::<Bounded<BYTES>>(&input).expect("known singleton arrays fit their storage budget").0;
    let expected = Value::Array((0..ITEMS).map(|_| Value::Array(vec![Value::Integer(1)])).collect());
    assert_eq!(value, expected);
}

#[test]
fn duplicate_budget_is_released() {
    let text = "x".repeat(512);
    let pair = format!("<key>same</key><string>{text}</string>");
    let input = format!("<llsd><map>{}</map></llsd>", pair.repeat(3));
    let value = xml::from_str::<Bounded<2048>>(&input).expect("replaced values release their storage budget").0;
    assert_eq!(value, Value::Map(Map::from([("same".into(), Value::String(text))])));
}

#[test]
fn nested_budget_rejects_early() {
    let small = "<llsd><array><map><key>same</key><integer>1</integer></map></array></llsd>";
    let fields = (0..20_000).map(|index| format!("<key>key{index}</key><integer>1</integer>")).collect::<String>();
    let large = format!("<llsd><array><map>{fields}</map></array></llsd>");
    let baseline = live_bytes();
    let (small_result, small_allocations) = allocation::measure(|| xml::from_str::<Bounded<{ size_of::<Value>() }>>(small));
    let (large_result, large_allocations) = allocation::measure(|| xml::from_str::<Bounded<{ size_of::<Value>() }>>(&large));
    assert_matches!(&small_result, Err(Error::InvalidValue(message)) if message == "LLSD heap limit exceeded");
    assert_matches!(&large_result, Err(Error::InvalidValue(message)) if message == "LLSD heap limit exceeded");
    assert!(large_allocations.peak_bytes <= small_allocations.peak_bytes + 4096, "large nested value was allocated before rejection");
    drop((small_result, large_result));
    assert_eq!(live_bytes(), baseline);
}

#[test]
fn heap_estimate_covers_storage() {
    let baseline = live_bytes();
    let (value, allocations) = allocation::measure(|| {
        let mut text = String::with_capacity(4096);
        text.push_str("value");
        let mut uri = String::with_capacity(1024);
        uri.push_str("uri");
        let mut key = String::with_capacity(2048);
        key.push_str("key");
        let mut values = Vec::with_capacity(16);
        values.push(Value::String(text));
        values.push(Value::Uri(Uri::from(uri)));
        values.push(Value::Binary(Binary::from(Vec::with_capacity(8192))));
        Value::Map(Map::from([(key, Value::Array(values))]))
    });
    assert!(allocations.live_bytes >= 4096 + 8192);
    assert!(value.estimated_heap_bytes() as i128 >= allocations.live_bytes, "heap estimate undercounts retained allocations");
    drop(value);
    assert_eq!(live_bytes(), baseline);
}

#[test]
fn duplicate_keys_keep_last_value() {
    let expected = Value::Map(Map::from([("dup".into(), Value::Integer(2))]));
    assert_eq!(notation::from_str::<Value>("{'dup':i1,'dup':i2}").expect("notation duplicates"), expected);
    assert_eq!(
        xml::from_str::<Value>("<llsd><map><key>dup</key><integer>1</integer><key>dup</key><integer>2</integer></map></llsd>")
            .expect("XML duplicates"),
        expected
    );
    let mut bytes = b"{\0\0\0\x02".to_vec();
    for value in [1_i32, 2_i32] {
        bytes.extend_from_slice(b"k\0\0\0\x03dupi");
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    bytes.push(b'}');
    assert_eq!(binary::from_slice::<Value>(&bytes).expect("binary duplicates"), expected);
}
