use std::{assert_matches, fmt::Write, hint::black_box};

use llsd::{Error, Limits, Value, binary, notation, xml};
use test_support::allocation::{self, CountingAllocator, live_bytes};
use uuid::Uuid;

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

fn measure<T>(operation: impl FnOnce() -> T) -> (T, i128, i128) {
    let (value, allocations) = allocation::measure(operation);
    (value, allocations.live_bytes, allocations.peak_bytes)
}

#[derive(Debug, PartialEq, serde::Deserialize)]
struct UuidField {
    #[serde(with = "llsd::uuid")]
    id: Uuid,
}

#[test]
fn xml_decoding_releases_allocations() {
    let pairs = 131_068;
    let mut large_map = String::from("<llsd><map><key>id</key><integer>1</integer><key>events</key><array/><key>extra</key><map>");
    for index in 0..pairs {
        write!(large_map, "<key>k{index}</key><integer>0</integer>").expect("append XML pair");
    }
    large_map.push_str("</map></map></llsd>");
    let mut flat_array = String::from("<llsd><array>");
    for _ in 0..131_073 {
        flat_array.push_str("<undef/>");
    }
    flat_array.push_str("</array></llsd>");
    let binary = format!("<llsd><binary>{}</binary></llsd>", "AAAA".repeat(4 * 1024 * 1024));
    let split_text = format!("<llsd><string>{}<![CDATA[{}]]></string></llsd>", "x".repeat(100_001), "x".repeat(99_999));
    let mut node_boundary = String::from("<llsd><map>");
    for index in 0..144 {
        write!(node_boundary, "<key>k{index}</key><map><key>v</key><string>text</string></map>").expect("append nested map");
    }
    node_boundary.push_str("</map></llsd>");
    let limits = Limits { max_bytes: 32 * 1024 * 1024, max_values: 262_144, max_container_items: 262_144, ..Limits::default() };
    for (name, input) in [
        ("empty", "<llsd><undef/></llsd>".to_owned()),
        ("depth limit", format!("<llsd>{}<integer>1</integer>{}</llsd>", "<array>".repeat(64), "</array>".repeat(64))),
        ("entities", format!("<llsd><string>{}</string></llsd>", "&#x1f600;&amp;".repeat(4096))),
        ("binary attributes", format!("<llsd><binary {} encoding=\"base64\">AAAA</binary></llsd>", (0..1024).map(|i| format!("x{i}=\"ok\" ")).collect::<String>())),
        ("EventQueue shape", "<llsd><map><key>id</key><integer>1</integer><key>events</key><array><map><key>message</key><string>ParcelProperties</string><key>body</key><map><key>name</key><string>parcel</string></map></map></array></map></llsd>".to_owned()),
        ("large extra map", large_map),
        ("flat array growth", flat_array),
        ("large binary", binary),
        ("split scalar growth", split_text),
        ("normalized line endings", format!("<llsd><string>{}</string></llsd>", "ab\r\n".repeat(50_000))),
        ("nested map nodes", node_boundary),
    ] {
        let baseline = live_bytes();
        let (value, retained, peak) = measure(|| xml::from_slice_with_limits::<Value>(black_box(input.as_bytes()), limits).expect("decode measured XML"));
        assert!(peak >= retained);
        drop(value);
        assert_eq!(live_bytes(), baseline, "decoded storage must return for {name}");
        eprintln!("{name}: input={} B, retained={retained} B, decode_peak={peak} B", input.len());
    }
}

#[test]
fn oversized_keys_stay_bounded() {
    type Decode = fn(&[u8], Limits) -> Result<Value, Error>;
    let large = "x".repeat(1024 * 1024);
    let binary_map = |key: &str| {
        let mut bytes = b"{\0\0\0\x01k".to_vec();
        bytes.extend_from_slice(&(key.len() as u32).to_be_bytes());
        bytes.extend_from_slice(key.as_bytes());
        bytes.extend_from_slice(b"!}");
        bytes
    };
    let cases: [(&str, Decode, Vec<u8>, Vec<u8>); 3] = [
        (
            "XML",
            xml::from_slice_with_limits,
            b"<llsd><map><key>xxx</key><undef/></map></llsd>".to_vec(),
            format!("<llsd><map><key>{large}</key><undef/></map></llsd>").into_bytes(),
        ),
        ("notation", notation::from_slice_with_limits, b"{'xxx':!}".to_vec(), format!("{{'{large}':!}}").into_bytes()),
        ("binary", binary::from_slice_with_limits, binary_map("xxx"), binary_map(&large)),
    ];
    for (name, decode, small, large) in cases {
        let limits = Limits { max_map_key_bytes: 2, ..Limits::default() };
        let (small_result, small_live, small_peak) = measure(|| decode(&small, limits));
        let (large_result, large_live, large_peak) = measure(|| decode(&large, limits));
        assert_matches!(small_result, Err(Error::LimitExceeded("map key bytes")), "small key for {name}");
        assert_matches!(large_result, Err(Error::LimitExceeded("map key bytes")), "large key for {name}");
        assert_eq!((small_live, large_live), (0, 0), "failed decode retained storage for {name}");
        assert!(large_peak <= small_peak + 4096, "oversized key was copied for {name}: {small_peak} -> {large_peak}");
    }
}

#[test]
fn xml_binary_text_stays_bounded() {
    let limits = Limits { max_binary_bytes: 1, ..Limits::default() };
    let small = b"<llsd><binary>AA==</binary></llsd>";
    let whitespace = format!("<llsd><binary>{}AA==</binary></llsd>", " \t\r\n".repeat(1024 * 1024));
    let (small_result, _, small_peak) = measure(|| xml::from_slice_with_limits::<Value>(small, limits));
    let (large_result, _, large_peak) = measure(|| xml::from_slice_with_limits::<Value>(whitespace.as_bytes(), limits));

    assert_eq!(small_result.expect("decode one byte"), Value::Binary(vec![0].into()));
    assert_eq!(large_result.expect("decode whitespace-wrapped one byte"), Value::Binary(vec![0].into()));
    assert!(large_peak <= small_peak + 4096, "binary whitespace was copied: {small_peak} -> {large_peak}");

    let small_oversized = b"<llsd><binary>AAAAA</binary></llsd>";
    let oversized = format!("<llsd><binary>{}</binary></llsd>", "A".repeat(4 * 1024 * 1024));
    let (small_result, _, small_peak) = measure(|| xml::from_slice_with_limits::<Value>(small_oversized, limits));
    let (large_result, _, large_peak) = measure(|| xml::from_slice_with_limits::<Value>(oversized.as_bytes(), limits));

    assert_matches!(small_result, Err(Error::LimitExceeded("binary bytes")));
    assert_matches!(large_result, Err(Error::LimitExceeded("binary bytes")));
    assert!(large_peak <= small_peak + 4096, "oversized base64 text was copied: {small_peak} -> {large_peak}");
}

#[test]
fn notation_uuid_keeps_spelling() {
    let canonical = "12345678-90ab-cdef-0123-456789abcdef";
    let expected = Uuid::parse_str(canonical).expect("valid UUID");
    let limits = Limits { max_string_bytes: 0, ..Limits::default() };
    for spelling in [canonical, "1234567890abcdef0123456789abcdef"] {
        let encoded = format!("u{spelling}");
        assert_eq!(
            notation::from_slice_with_limits::<String>(encoded.as_bytes(), limits).expect("decode UUID text"),
            spelling,
            "UUID text changed for {spelling}"
        );
        assert_eq!(
            notation::from_slice_with_limits::<Value>(encoded.as_bytes(), limits).expect("decode UUID value"),
            Value::Uuid(expected),
            "UUID value changed for {spelling}"
        );

        let encoded_field = format!("{{'id':u{spelling}}}");
        assert_eq!(
            notation::from_slice_with_limits::<UuidField>(encoded_field.as_bytes(), limits).expect("decode typed UUID"),
            UuidField { id: expected },
            "typed UUID changed for {spelling}"
        );
    }
}

#[test]
fn invalid_uuid_tokens_stay_bounded() {
    let limits = Limits { max_string_bytes: 1, ..Limits::default() };
    let large = format!("u{}", "x".repeat(1024 * 1024));
    let baseline = live_bytes();
    let (small_result, _, small_peak) = measure(|| notation::from_slice_with_limits::<Value>(b"unot-a-uuid", limits));
    let (large_result, _, large_peak) = measure(|| notation::from_slice_with_limits::<Value>(large.as_bytes(), limits));

    assert_matches!(&small_result, Err(Error::InvalidValue(_)));
    assert_matches!(&large_result, Err(Error::InvalidValue(_)));
    assert!(large_peak <= small_peak + 4096, "oversized UUID token was copied: {small_peak} -> {large_peak}");
    drop((small_result, large_result));
    assert_eq!(live_bytes(), baseline);

    assert_matches!(notation::from_slice_with_limits::<Value>(b"u\xff", limits), Err(Error::InvalidUtf8));
    assert_matches!(notation::from_slice_with_limits::<UuidField>(b"{'id':unot-a-uuid}", limits), Err(Error::InvalidValue(_)));
    assert_matches!(notation::from_slice_with_limits::<UuidField>(b"{'id':u\xff}", limits), Err(Error::InvalidUtf8));
}

#[test]
fn integer_arrays_avoid_tag_allocations() {
    const ITEMS: usize = 4096;

    let notation_input = format!("[{}]", vec!["i1"; ITEMS].join(","));
    let mut binary_input = vec![b'['];
    binary_input.extend((ITEMS as u32).to_be_bytes());
    for _ in 0..ITEMS {
        binary_input.push(b'i');
        binary_input.extend(1_i32.to_be_bytes());
    }
    binary_input.push(b']');

    let (notation_value, notation_allocations) =
        allocation::measure(|| notation::from_slice::<Value>(notation_input.as_bytes()).expect("decode notation integers"));
    let (binary_value, binary_allocations) =
        allocation::measure(|| binary::from_slice::<Value>(&binary_input).expect("decode binary integers"));

    assert_eq!(notation_value.as_array().map(<[Value]>::len), Some(ITEMS));
    assert_eq!(binary_value.as_array().map(<[Value]>::len), Some(ITEMS));
    assert!(notation_allocations.allocation_calls + notation_allocations.reallocation_calls < ITEMS / 8);
    assert!(binary_allocations.allocation_calls + binary_allocations.reallocation_calls < ITEMS / 8);
}

#[test]
fn binary_attributes_stay_bounded() {
    let limits = Limits { max_string_bytes: 6, max_binary_bytes: 1, ..Limits::default() };
    for (name, small, large, valid) in [
        (
            "numeric reference",
            "<llsd><binary encoding=\"&#x62;ase64\">AA==</binary></llsd>",
            format!("<llsd><binary encoding=\"&#x{}62;ase64\">AA==</binary></llsd>", "0".repeat(4 * 1024 * 1024)),
            true,
        ),
        (
            "unknown entity",
            "<llsd><binary encoding=\"&unknown;\">AA==</binary></llsd>",
            format!("<llsd><binary encoding=\"&{};\">AA==</binary></llsd>", "x".repeat(4 * 1024 * 1024)),
            false,
        ),
        (
            "normalized whitespace",
            "<llsd><binary encoding=\"\tbase64\">AA==</binary></llsd>",
            format!("<llsd><binary encoding=\"{}base64\">AA==</binary></llsd>", "\t".repeat(4 * 1024 * 1024)),
            false,
        ),
    ] {
        let baseline = live_bytes();
        let (small_result, small_allocations) = allocation::measure(|| xml::from_slice_with_limits::<Value>(small.as_bytes(), limits));
        let (large_result, large_allocations) = allocation::measure(|| xml::from_slice_with_limits::<Value>(large.as_bytes(), limits));
        if valid {
            let expected = Value::Binary(vec![0].into());
            assert_eq!(small_result.as_ref().expect("valid small encoding"), &expected);
            assert_eq!(large_result.as_ref().expect("valid large encoding"), &expected);
        } else {
            assert_matches!(&small_result, Err(Error::InvalidValue(_)));
            assert_matches!(&large_result, Err(Error::InvalidValue(_)));
        }
        assert!(
            large_allocations.peak_bytes <= small_allocations.peak_bytes + 4096,
            "binary attribute was copied for {name}: {} -> {}",
            small_allocations.peak_bytes,
            large_allocations.peak_bytes
        );
        drop((small_result, large_result));
        assert_eq!(live_bytes(), baseline);
    }
}

#[test]
fn binary_capacity_stays_bounded() {
    const BYTES: usize = 256 * 1024;
    let expected = Value::Binary(vec![0; BYTES].into());
    let input = xml::to_vec(&expected).expect("encode bounded binary input");
    let limits = Limits { max_binary_bytes: BYTES, ..Limits::default() };
    let baseline = live_bytes();
    let (result, allocations) = allocation::measure(|| xml::from_slice_with_limits::<Value>(&input, limits));
    assert_eq!(result.as_ref().expect("decode binary at the limit"), &expected);
    let encoded_bytes = BYTES.div_ceil(3) * 4;
    assert!(
        allocations.peak_bytes <= (encoded_bytes + BYTES + 4096) as i128,
        "binary parsing overreserved its encoded buffer: {}",
        allocations.peak_bytes
    );
    drop(result);
    assert_eq!(live_bytes(), baseline);
}
