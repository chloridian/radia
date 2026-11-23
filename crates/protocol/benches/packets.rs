use std::{
    fmt::{self, Display, Formatter},
    hint::black_box,
};

use divan::counter::{BytesCount, ItemsCount};
use protocol::lludp::{Message, MessageId, Packet};
use test_support::{lludp::vectors, parse_hex_fixture};

#[derive(Clone, Copy, Debug)]
enum PacketData {
    Hex(&'static str),
    UnknownFallback,
    OversizedZeroCoded,
}

#[derive(Clone, Copy, Debug)]
struct PacketCase {
    name: &'static str,
    data: PacketData,
    objects: Option<u64>,
}

const OBJECT_CASES: &[PacketCase] = &[
    PacketCase { name: "update_full", data: PacketData::Hex(vectors::OBJECT_UPDATE), objects: Some(2) },
    PacketCase { name: "update_compressed", data: PacketData::Hex(vectors::OBJECT_UPDATE_COMPRESSED_TYPED), objects: Some(1) },
    PacketCase { name: "update_terse", data: PacketData::Hex(vectors::IMPROVED_TERSE_OBJECT_UPDATE), objects: Some(1) },
    PacketCase { name: "update_terse_avatar", data: PacketData::Hex(vectors::IMPROVED_TERSE_OBJECT_UPDATE_AVATAR), objects: Some(1) },
    PacketCase { name: "update_cached", data: PacketData::Hex(vectors::OBJECT_UPDATE_CACHED), objects: Some(2) },
    PacketCase { name: "kill_object", data: PacketData::Hex(vectors::KILL_OBJECT), objects: Some(2) },
];

const PACKET_CASES: &[PacketCase] = &[
    PacketCase { name: "packet_ack", data: PacketData::Hex(vectors::PACKET_ACK), objects: None },
    PacketCase { name: "appended_acks", data: PacketData::Hex(vectors::APPENDED_ACKS), objects: None },
    PacketCase { name: "request_multiple_objects", data: PacketData::Hex(vectors::REQUEST_MULTIPLE_OBJECTS), objects: None },
    PacketCase { name: "zerocode_at_limit", data: PacketData::Hex(vectors::ZEROCODE_PACKET_AT_LIMIT), objects: None },
    PacketCase { name: "schema_fallback", data: PacketData::Hex(vectors::COARSE_LOCATION_UPDATE), objects: None },
    PacketCase { name: "unknown_fallback", data: PacketData::UnknownFallback, objects: None },
];

fn fixture(hex: &str) -> Vec<u8> {
    parse_hex_fixture(hex).expect("valid packet fixture")
}

fn wire(case: PacketCase) -> Vec<u8> {
    match case.data {
        PacketData::Hex(hex) => fixture(hex),
        PacketData::UnknownFallback => {
            Packet::new(7, Message::Unknown { id: MessageId::high(0x7e), payload: (0..256).map(|byte| byte as u8).collect() })
                .with_zero_coding(false)
                .encode()
                .expect("encode unknown fixture")
        }
        PacketData::OversizedZeroCoded => fixture(vectors::TRANSFER_REQUEST_LARGE_ZEROCODED),
    }
}

#[divan::bench(args = OBJECT_CASES)]
fn lludp_objects_encode(bencher: divan::Bencher, case: PacketCase) {
    let packet = Packet::decode(&wire(case)).expect("decode object packet fixture");
    bencher
        .counter(ItemsCount::new(case.objects.expect("object packet has a count")))
        .bench(|| black_box(packet.encode().expect("encode object packet")));
}

#[divan::bench(args = OBJECT_CASES)]
fn lludp_objects_decode(bencher: divan::Bencher, case: PacketCase) {
    let wire = wire(case);
    bencher
        .counter(ItemsCount::new(case.objects.expect("object packet has a count")))
        .bench(|| black_box(Packet::decode(black_box(wire.as_slice())).expect("decode object packet")));
}

#[divan::bench(args = PACKET_CASES)]
fn lludp_packets_encode(bencher: divan::Bencher, case: PacketCase) {
    let packet = Packet::decode(&wire(case)).expect("decode packet fixture");
    let byte_count = wire(case).len();
    bencher.counter(BytesCount::new(byte_count)).bench(|| black_box(packet.encode().expect("encode packet")));
}

#[divan::bench(args = PACKET_CASES)]
fn lludp_packets_decode(bencher: divan::Bencher, case: PacketCase) {
    let input = wire(case);
    bencher.counter(BytesCount::new(input.len())).bench(|| black_box(Packet::decode(black_box(input.as_slice())).expect("decode packet")));
}

#[divan::bench]
fn lludp_reject_zerocode_limit(bencher: divan::Bencher) {
    let wire = wire(PacketCase { name: "oversized", data: PacketData::OversizedZeroCoded, objects: None });
    assert!(Packet::decode(&wire).is_err(), "invalid packet fixture must be rejected");
    bencher.counter(BytesCount::new(wire.len())).bench(|| black_box(Packet::decode(black_box(wire.as_slice()))));
}

impl Display for PacketCase {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}
