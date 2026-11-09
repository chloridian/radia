use std::{fmt::Write as _, hint::black_box, mem::size_of, time::Instant};

use protocol::inventory::{RequestedFolder, decode_ais_children, decode_fetch_inventory_descendents2};
use protocol::lludp::{FieldValue, Message, Packet};
use test_support::{
    allocation::{self, CountingAllocator, measure},
    lludp::vectors,
    parse_hex_fixture,
};
use uuid::Uuid;

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

#[test]
#[ignore = "manual inventory decode working-set measurement"]
fn inventory_working_sets() {
    const RESPONSE_BYTES: usize = 32 * 1024 * 1024;
    let owner = Uuid::from_u128(99);
    let folder = Uuid::from_u128(1);
    let mut body = String::with_capacity(RESPONSE_BYTES);
    write!(body, "<llsd><map><key>category_id</key><uuid>{folder}</uuid><key>agent_id</key><uuid>{owner}</uuid><key>name</key><string>root</string><key>ignored</key><map>").expect("fixture text");
    for index in 0..1_000_000 {
        write!(body, "<key>k{index:06}</key><undef/>").expect("fixture key");
    }
    body.push_str("</map></map></llsd>");
    assert!(body.len() <= RESPONSE_BYTES);
    let baseline = allocation::live_bytes();
    let started = Instant::now();
    let (decoded, allocations) = measure(|| decode_ais_children(body.as_bytes(), folder, owner));
    let elapsed = started.elapsed();
    let decoded = decoded.expect("unknown fields fit current wire limits");
    assert_eq!(decoded.id, folder);
    assert_eq!(decoded.name, "root");
    assert!(decoded.children.is_none());
    drop(decoded);
    assert_eq!(allocation::live_bytes(), baseline, "category output releases all decoded allocations");
    println!(
        "inventory_decode case=dense_unknown_map raw_bytes={} raw_capacity={} live_bytes={} peak_bytes={} elapsed_us={}",
        body.len(),
        body.capacity(),
        allocations.live_bytes,
        allocations.peak_bytes,
        elapsed.as_micros()
    );
    drop(body);

    let requested = (100..100_100).map(Uuid::from_u128).collect::<Vec<_>>();
    let mut body = String::with_capacity(RESPONSE_BYTES);
    write!(body, "<llsd><map><key>folders</key><array><map><key>folder_id</key><uuid>{folder}</uuid><key>owner_id</key><uuid>{owner}</uuid><key>version</key><integer>1</integer><key>descendents</key><integer>100000</integer><key>categories</key><array/><key>items</key><array>").expect("fixture folder");
    for id in &requested {
        write!(body, "<map><key>item_id</key><uuid>{id}</uuid><key>owner_id</key><uuid>{owner}</uuid><key>parent_id</key><uuid>{folder}</uuid><key>name</key><string>x</string></map>").expect("fixture item");
    }
    body.push_str("</array></map></array></map></llsd>");
    assert!(body.len() <= RESPONSE_BYTES);
    let baseline = allocation::live_bytes();
    let started = Instant::now();
    let (decoded, allocations) =
        measure(|| decode_fetch_inventory_descendents2(body.as_bytes(), &[RequestedFolder { id: folder, owner_id: owner }]));
    let elapsed = started.elapsed();
    let decoded = decoded.expect("large item response fits current wire limits");
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].id, folder);
    assert_eq!(decoded[0].items.len(), requested.len());
    assert!(decoded[0].items.iter().zip(&requested).all(|(item, id)| item.id == *id && item.parent_id == Some(folder) && item.name == "x"));
    drop(decoded);
    assert_eq!(allocation::live_bytes(), baseline, "item output releases all decoded allocations");
    println!(
        "inventory_decode case=folder_100k_items raw_bytes={} raw_capacity={} live_bytes={} peak_bytes={} elapsed_us={}",
        body.len(),
        body.capacity(),
        allocations.live_bytes,
        allocations.peak_bytes,
        elapsed.as_micros()
    );
}

#[test]
fn decode_allocations() {
    let mut wire = [0; 43];
    wire[4] = 1;
    wire[6..10].copy_from_slice(&[0xff, 0xff, 0, 98]);
    wire[42] = u8::MAX;

    let preflight = Packet::preflight(&wire).expect("preflight omitted ObjectData records");
    drop(Packet::decode(&wire).expect("warm decoder before measuring"));
    let baseline_bytes = allocation::live_bytes();
    let (packet, allocations) = measure(|| black_box(Packet::decode(black_box(&wire)).expect("decode omitted ObjectData records")));
    let live_bytes = allocations.live_bytes;
    let peak_bytes = allocations.peak_bytes;
    let allocation_calls = allocations.allocation_calls;
    let retained_bytes = packet.retained_capacity_bytes();
    let Message::Schema(message) = packet.message() else { panic!("ObjectShape must remain schema-backed") };
    let blocks = message.blocks();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[1].instances.len(), usize::from(u8::MAX));

    let mut missing_fields = 0;
    for (instance_index, instance) in blocks[1].instances.iter().enumerate() {
        for field_index in 0..instance.fields.len() {
            if message.field_was_supplied(1, instance_index, field_index) == Some(false) {
                missing_fields += 1;
            }
        }
    }

    assert_eq!(missing_fields, 4_845);
    assert_eq!(live_bytes, i128::try_from(retained_bytes).expect("retained capacity fits i128"));
    assert!(retained_bytes <= preflight.retained_capacity_upper_bound(), "retained storage exceeds preflight admission");
    assert!(peak_bytes <= preflight.peak_decode_allocation_bytes() as i128, "decode peak exceeds preflight admission");
    assert!(preflight.peak_decode_allocation_bytes() >= preflight.retained_capacity_upper_bound());

    drop(packet);
    assert_eq!(allocation::live_bytes(), baseline_bytes);

    eprintln!(
        "43-byte ObjectShape: {missing_fields} padded fields, {} B of padded FieldValue storage, {} B retained, {} B peak ({} allocations, {} reallocations)",
        missing_fields * size_of::<FieldValue>(),
        retained_bytes,
        peak_bytes,
        allocation_calls,
        allocations.reallocation_calls,
    );

    for (name, fixture) in [
        ("zero-coded AgentUpdate", vectors::AGENT_UPDATE_ZEROCODED),
        ("zero-coded ObjectUpdate", vectors::OBJECT_UPDATE),
        ("RegionHandshake strings", vectors::REGION_HANDSHAKE_STRINGS),
        ("packed ObjectUpdateCompressed", vectors::OBJECT_UPDATE_COMPRESSED_TYPED),
    ] {
        let wire = parse_hex_fixture(fixture).expect("parse allocation fixture");
        let preflight = Packet::preflight(&wire).expect("preflight allocation fixture");
        drop(Packet::decode(&wire).expect("warm typed decoder"));
        let baseline = allocation::live_bytes();
        let (packet, allocations) = measure(|| black_box(Packet::decode(black_box(&wire)).expect("decode allocation fixture")));
        let live = allocations.live_bytes;
        let peak = allocations.peak_bytes;

        assert_eq!(
            live,
            i128::try_from(packet.retained_capacity_bytes()).expect("retained capacity fits i128"),
            "retained capacity for {name}"
        );
        assert!(
            live <= i128::try_from(preflight.retained_capacity_upper_bound()).expect("retained bound fits i128"),
            "retained bound for {name}"
        );
        assert!(
            peak <= i128::try_from(preflight.peak_decode_allocation_bytes()).expect("peak bound fits i128"),
            "decode peak bound for {name}: measured={peak}, bound={}",
            preflight.peak_decode_allocation_bytes()
        );
        drop(packet);
        assert_eq!(allocation::live_bytes(), baseline, "allocation return for {name}");
        eprintln!("{name}: {} B wire, {live} B retained, {peak} B decode peak", wire.len());
    }
}
