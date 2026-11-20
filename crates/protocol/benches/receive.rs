use std::hint::black_box;

use protocol::lludp::{Message, Packet};
use test_support::{lludp::vectors, parse_hex_fixture};

const RECEIVE_BATCH_LEN: usize = 1_000;

fn fixture(hex: &str) -> Vec<u8> {
    parse_hex_fixture(hex).expect("valid LLUDP fixture")
}

fn representative_packets() -> [Vec<u8>; 5] {
    let object_update = fixture(vectors::OBJECT_UPDATE);
    let terse_update = fixture(vectors::IMPROVED_TERSE_OBJECT_UPDATE);

    let compressed_update = Packet::decode(&fixture(vectors::OBJECT_UPDATE_COMPRESSED_TYPED))
        .expect("decode compressed object update fixture")
        .with_zero_coding(true)
        .encode()
        .expect("encode zero-coded compressed object update");
    assert!(
        Packet::decode(&compressed_update)
            .expect("decode zero-coded compressed object update")
            .is_zerocoded()
            .expect("read zero-coding flag")
    );

    let packet_ack = Packet::decode(&fixture(vectors::PACKET_ACK)).expect("decode PacketAck fixture");
    let appended_acks = Packet::decode(&fixture(vectors::APPENDED_ACKS)).expect("decode appended ACK fixture").appended_acks().to_vec();
    let packet_ack =
        Packet::new(0, packet_ack.into_message()).with_appended_acks(appended_acks).encode().expect("encode PacketAck with appended ACKs");

    let reliable_chat = Packet::decode(&fixture(vectors::CHAT_FROM_SIMULATOR))
        .expect("decode chat fixture")
        .with_reliable()
        .encode()
        .expect("encode reliable chat fixture");

    [object_update, terse_update, compressed_update, packet_ack, reliable_chat]
}

fn mixed_batch() -> Vec<Vec<u8>> {
    let packets = representative_packets();
    (0..RECEIVE_BATCH_LEN)
        .map(|index| {
            let mut wire = packets[index % packets.len()].clone();
            set_sequence(&mut wire, u32::try_from(index + 1).expect("batch sequence fits u32"));
            wire
        })
        .collect()
}

fn set_sequence(wire: &mut [u8], sequence: u32) {
    wire.get_mut(1..5).expect("LLUDP fixture has a sequence field").copy_from_slice(&sequence.to_be_bytes());
}

fn decode_received(wire: &[u8]) {
    let frame = Packet::inspect(black_box(wire)).expect("inspect receive fixture");
    let sequence = frame.sequence();
    let reliable = frame.is_reliable();
    let appended_acks = frame.appended_acks().fold(0u32, u32::wrapping_add);
    let message_id = frame.message_id().expect("read receive message ID");
    let preflighted = frame.preflighted().expect("preflight receive fixture");
    let preflight = preflighted.preflight();
    let packet = preflighted.decode().expect("decode receive fixture");
    let packet_acks = match packet.message() {
        Message::PacketAck(message) => message.packets.iter().fold(0u32, |sum, packet| sum.wrapping_add(packet.id)),
        _ => 0,
    };
    black_box((sequence, reliable, appended_acks, message_id, preflight, packet_acks));
    black_box(packet);
}

mod benches {
    use super::*;

    #[divan::bench]
    fn lludp_receive_pipeline_mixed_1000(bencher: divan::Bencher) {
        let batch = mixed_batch();
        bencher.counter(divan::counter::ItemsCount::new(RECEIVE_BATCH_LEN as u64)).bench_local(|| {
            for wire in &batch {
                decode_received(wire);
            }
        });
    }
}
