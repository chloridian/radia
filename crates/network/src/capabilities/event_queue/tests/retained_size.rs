use std::fmt::Write as _;

use llsd::{Binary, Uri};

use super::*;

#[test]
fn counts_owned_buffer_capacity() {
    let mut uri_text = String::with_capacity(4096);
    uri_text.push_str("uri");
    let uri_capacity = uri_text.capacity();
    let uri = Value::Uri(Uri::from(uri_text));
    assert_eq!(uri.estimated_heap_bytes(), uri_capacity);
    std::assert_matches!(&uri, Value::Uri(value) if value.as_str() == "uri");

    let mut bytes = Vec::with_capacity(4096);
    bytes.extend_from_slice(b"bin");
    let binary_capacity = bytes.capacity();
    let binary = Value::Binary(Binary::from(bytes));
    assert_eq!(binary.estimated_heap_bytes(), binary_capacity);
    std::assert_matches!(&binary, Value::Binary(value) if value.as_slice() == b"bin");
}

const EVENT_BATCH_PREFIX: &str = "<llsd><map><key>id</key><integer>5</integer><key>events</key><array>";
const EVENT_BATCH_SUFFIX: &str = "</array></map></llsd>";
const EVENT_STRING_OPEN: &str = "<map><key>message</key><string>Large</string><key>body</key><string>";
const EVENT_STRING_CLOSE: &str = "</string></map>";

fn event_batch_with_strings(event_count: usize, value_bytes: usize) -> Vec<u8> {
    let event_capacity = event_count * (EVENT_STRING_OPEN.len() + value_bytes + EVENT_STRING_CLOSE.len());
    let mut body = Vec::with_capacity(EVENT_BATCH_PREFIX.len() + event_capacity + EVENT_BATCH_SUFFIX.len());
    body.extend_from_slice(EVENT_BATCH_PREFIX.as_bytes());
    for _ in 0..event_count {
        body.extend_from_slice(EVENT_STRING_OPEN.as_bytes());
        body.resize(body.len() + value_bytes, b'x');
        body.extend_from_slice(EVENT_STRING_CLOSE.as_bytes());
    }
    body.extend_from_slice(EVENT_BATCH_SUFFIX.as_bytes());
    body
}

fn report_decode(
    label: &str,
    body_len: usize,
    body_capacity: usize,
    live: i128,
    peak: i128,
    elapsed_ns: u128,
    retained_output_bytes: usize,
    outcome: &str,
) -> i128 {
    let capacity_plus_peak = body_capacity as i128 + peak;
    println!(
        "event_queue_decode measurement=single_poll label={label} raw_len={body_len} raw_capacity={body_capacity} additional_live_bytes={live} additional_peak_bytes={peak} elapsed_ns={elapsed_ns} retained_output_bytes={retained_output_bytes} outcome={outcome} raw_capacity_plus_measured_peak={capacity_plus_peak}"
    );
    capacity_plus_peak
}

#[test]
fn decode_working_sets() {
    let mut largest = ("", 0_i128);

    let empty_body = event_batch_with_strings(1, 0);
    let value_bytes = MAX_EVENT_QUEUE_RESPONSE_BYTES - empty_body.len();
    drop(empty_body);
    let body = event_batch_with_strings(1, value_bytes);
    assert_eq!(body.len(), MAX_EVENT_QUEUE_RESPONSE_BYTES);
    let (result, live, peak, elapsed_ns) = crate::fixtures::allocation::measure(|| decode_event_batch(&body));
    let total = report_decode("near_32mib_string", body.len(), body.capacity(), live, peak, elapsed_ns, 0, "reject_scalar_limit");
    if total > largest.1 {
        largest = ("near_32mib_string", total);
    }
    let Err(error) = result else {
        panic!("a near-limit string must be rejected by the retained event limit");
    };
    assert!(error.contains("string bytes limit exceeded"), "{error}");
    assert!((0..=4096).contains(&live), "rejected decode retained unexpected allocations: {live} bytes");
    let ((), released, _, _) = crate::fixtures::allocation::measure(|| drop(error));
    assert_eq!(released, -live, "rejected decode output should release its allocations");
    drop(body);

    let map_entries = MAX_LLSD_XML_ELEMENTS - 6;
    let map_prefix = "<llsd><map><key>id</key><integer>5</integer><key>events</key><array><map><key>message</key><string>Keys</string><key>body</key><map>";
    let map_suffix = "</map></map></array></map></llsd>";
    let mut xml = String::with_capacity(map_prefix.len() + map_entries * 38 + map_suffix.len());
    xml.push_str(map_prefix);
    for index in 0..map_entries {
        write!(&mut xml, "<key>k{index:06}</key><integer>1</integer>").expect("write map entry to String");
    }
    xml.push_str(map_suffix);
    let body = xml.into_bytes();
    assert!(body.len() <= MAX_EVENT_QUEUE_RESPONSE_BYTES);
    let (result, live, peak, elapsed_ns) = crate::fixtures::allocation::measure(|| decode_event_batch(&body));
    let total = report_decode("large_key_map", body.len(), body.capacity(), live, peak, elapsed_ns, 0, "reject_event_heap_limit");
    if total > largest.1 {
        largest = ("large_key_map", total);
    }
    let Err(error) = result else {
        panic!("a value-budget-sized unique-key map must be rejected by the retained event limit");
    };
    assert!(error.contains("heap limit"), "{error}");
    assert!(peak <= (2 * MAX_QUEUED_CAPABILITY_EVENT_BYTES) as i128, "large map was built before rejection: peak={peak}");
    assert!((0..=4096).contains(&live), "rejected decode retained unexpected allocations: {live} bytes");
    let ((), released, _, _) = crate::fixtures::allocation::measure(|| drop(error));
    assert_eq!(released, -live, "rejected decode output should release its allocations");
    drop(body);

    let nested_map_count = MAX_LLSD_XML_ELEMENTS / 32 - 1;
    let regular_map_entries = 31;
    let surrounding_values = 3 + 3;
    let extra_last_map_entries = MAX_LLSD_XML_ELEMENTS - surrounding_values - nested_map_count * (regular_map_entries + 1);
    let container_item_limit = MAX_QUEUED_CAPABILITY_EVENT_BYTES / std::mem::size_of::<Value>();
    assert!(nested_map_count < container_item_limit);
    assert!(regular_map_entries + extra_last_map_entries < container_item_limit);
    assert_eq!(surrounding_values + nested_map_count * (regular_map_entries + 1) + extra_last_map_entries, MAX_LLSD_XML_ELEMENTS);

    let dense_prefix = "<llsd><map><key>id</key><integer>5</integer><key>events</key><array><map><key>message</key><string>Dense</string><key>body</key><array>";
    let dense_suffix = "</array></map></array></map></llsd>";
    let dense_entry_count = nested_map_count * regular_map_entries + extra_last_map_entries;
    let dense_capacity = dense_prefix.len()
        + dense_suffix.len()
        + nested_map_count * "<map></map>".len()
        + dense_entry_count * "<key>k00</key><integer>1</integer>".len();
    let mut xml = String::with_capacity(dense_capacity);
    xml.push_str(dense_prefix);
    for map_index in 0..nested_map_count {
        xml.push_str("<map>");
        let map_entries = regular_map_entries + if map_index + 1 == nested_map_count { extra_last_map_entries } else { 0 };
        for key_index in 0..map_entries {
            write!(&mut xml, "<key>k{key_index:02}</key><integer>1</integer>").expect("write nested map entry to String");
        }
        xml.push_str("</map>");
    }
    xml.push_str(dense_suffix);
    let body = xml.into_bytes();
    assert!(body.len() <= MAX_EVENT_QUEUE_RESPONSE_BYTES);
    let (result, live, peak, elapsed_ns) = crate::fixtures::allocation::measure(|| decode_event_batch(&body));
    let total =
        report_decode("near_global_values_small_maps", body.len(), body.capacity(), live, peak, elapsed_ns, 0, "reject_event_heap_limit");
    if total > largest.1 {
        largest = ("near_global_values_small_maps", total);
    }
    let Err(error) = result else {
        panic!("an event with aggregate small-map data should exceed the retained event limit");
    };
    assert!(error.contains("heap limit"), "{error}");
    assert!(peak <= (2 * MAX_QUEUED_CAPABILITY_EVENT_BYTES) as i128, "nested maps were built before rejection: peak={peak}");
    assert!((0..=4096).contains(&live), "rejected decode retained unexpected allocations: {live} bytes");
    let ((), released, _, _) = crate::fixtures::allocation::measure(|| drop(error));
    assert_eq!(released, -live, "rejected decode output should release its allocations");
    drop(body);

    let oversized_event_count = 1025;
    let messages = vec!["x"; oversized_event_count];
    let body = crate::fixtures::http::event_batch_body(5, &messages).into_bytes();
    let (result, live, peak, elapsed_ns) = crate::fixtures::allocation::measure(|| decode_event_batch(&body));
    let total = report_decode("oversized_event_array", body.len(), body.capacity(), live, peak, elapsed_ns, 0, "reject_event_count");
    if total > largest.1 {
        largest = ("oversized_event_array", total);
    }
    let Err(error) = result else {
        panic!("a batch with 1,025 events must be rejected after LLSD decoding");
    };
    assert!(error.contains("1024-event limit"), "{error}");
    assert!(peak <= (2 * MAX_QUEUED_CAPABILITY_EVENT_BYTES) as i128, "oversized events array was retained: peak={peak}");
    assert!((0..=4096).contains(&live), "rejected decode retained unexpected allocations: {live} bytes");
    let ((), released, _, _) = crate::fixtures::allocation::measure(|| drop(error));
    assert_eq!(released, -live, "rejected decode output should release its allocations");
    drop(body);

    let ignored_map_entries = container_item_limit - 1;
    let ignored_prefix = "<llsd><map><key>id</key><integer>5</integer><key>events</key><array><map><key>message</key><string>Small</string><key>body</key><undef/></map></array><key>ignored</key><map>";
    let ignored_suffix = "</map></map></llsd>";
    let ignored_capacity = ignored_prefix.len() + ignored_suffix.len() + ignored_map_entries * "<key>k0000</key><integer>1</integer>".len();
    let mut xml = String::with_capacity(ignored_capacity);
    xml.push_str(ignored_prefix);
    for index in 0..ignored_map_entries {
        write!(&mut xml, "<key>k{index:04}</key><integer>1</integer>").expect("write ignored envelope map entry to String");
    }
    xml.push_str(ignored_suffix);
    let body = xml.into_bytes();
    assert!(body.len() <= MAX_EVENT_QUEUE_RESPONSE_BYTES);
    let (result, live, peak, elapsed_ns) = crate::fixtures::allocation::measure(|| decode_event_batch(&body));
    let Ok((id, events)) = result else {
        panic!("a small event should be accepted when its envelope has an ignored map field");
    };
    assert_eq!(id, 5);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].message, "Small");
    std::assert_matches!(&events[0].body, Value::Undefined);
    let retained_bytes = capability_event_batch_retained_size(&events, events.capacity());
    assert!(peak <= (2 * MAX_QUEUED_CAPABILITY_EVENT_BYTES) as i128, "ignored envelope map was retained: peak={peak}");
    let total = report_decode(
        "ignored_envelope_key_map",
        body.len(),
        body.capacity(),
        live,
        peak,
        elapsed_ns,
        retained_bytes,
        "accepted_ignored_field",
    );
    if total > largest.1 {
        largest = ("ignored_envelope_key_map", total);
    }
    let ((), released, _, _) = crate::fixtures::allocation::measure(|| drop(events));
    assert_eq!(released, -live, "accepted decoded output should release its allocations");
    drop(body);

    for (label, padded, ignored_binary) in
        [("near_8mib_batch", false, false), ("max_response_near_8mib_batch", true, false), ("max_response_ignored_binary", true, true)]
    {
        let mut body = event_batch_with_strings(32, 254 * 1024);
        if ignored_binary {
            body.truncate(body.len() - "</map></llsd>".len());
            body.extend_from_slice(b"<key>ignored</key><binary>");
            body.extend_from_slice("AAAA".repeat(MAX_QUEUED_CAPABILITY_EVENT_BYTES / 3).as_bytes());
            body.extend_from_slice(match MAX_QUEUED_CAPABILITY_EVENT_BYTES % 3 {
                0 => b"",
                1 => b"AA==",
                _ => b"AAA=",
            });
            body.extend_from_slice(b"</binary></map></llsd>");
        }
        if padded {
            body.resize(MAX_EVENT_QUEUE_RESPONSE_BYTES, b' ');
        }
        let (result, live, peak, elapsed_ns) = crate::fixtures::allocation::measure(|| decode_event_batch(&body));
        let Ok((id, events)) = result else {
            panic!("the representative batch should fit the retained batch limit: {label}");
        };
        assert_eq!(id, 5);
        assert_eq!(events.len(), 32);
        assert!(events.iter().all(|event| event.retained_bytes <= MAX_QUEUED_CAPABILITY_EVENT_BYTES));
        let retained_bytes = capability_event_batch_retained_size(&events, events.capacity());
        assert!(retained_bytes >= MAX_EVENT_QUEUE_BATCH_RETAINED_BYTES * 98 / 100, "retained={retained_bytes}");
        assert!(retained_bytes <= MAX_EVENT_QUEUE_BATCH_RETAINED_BYTES, "retained={retained_bytes}");
        let total = report_decode(label, body.len(), body.capacity(), live, peak, elapsed_ns, retained_bytes, "accepted");
        if total > largest.1 {
            largest = (label, total);
        }
        let ((), released, _, _) = crate::fixtures::allocation::measure(|| drop(events));
        assert_eq!(released, -live, "accepted decoded output should release its allocations");
    }

    println!("event_queue_decode_largest label={} raw_capacity_plus_measured_peak={}", largest.0, largest.1);
    println!(
        "event_queue_decode_model kind=identical_concurrent_polls poll_count={} source_label={} single_poll_raw_capacity_plus_measured_peak={} modeled_nine_poll_peak_bytes={}",
        MAX_CONCURRENT_EVENT_QUEUES,
        largest.0,
        largest.1,
        largest.1 * MAX_CONCURRENT_EVENT_QUEUES as i128
    );
    println!(
        "event_queue_decode_limits response_bytes={} batch_retained_bytes={} event_retained_bytes={} batch_events={} llsd_values={} container_items={} key_map_entries={} dense_nested_array_maps={} dense_regular_map_entries={} dense_final_map_entries={} ignored_envelope_map_entries={} rejected_array_events={} accepted_batch_events={}",
        MAX_EVENT_QUEUE_RESPONSE_BYTES,
        MAX_EVENT_QUEUE_BATCH_RETAINED_BYTES,
        MAX_QUEUED_CAPABILITY_EVENT_BYTES,
        1024,
        MAX_LLSD_XML_ELEMENTS,
        MAX_QUEUED_CAPABILITY_EVENT_BYTES / std::mem::size_of::<Value>(),
        map_entries,
        nested_map_count,
        regular_map_entries,
        regular_map_entries + extra_last_map_entries,
        ignored_map_entries,
        oversized_event_count,
        32
    );
}
