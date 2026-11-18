use math::{Quat, Vec3};
use protocol::{
    ChatType, LLUdpError,
    lludp::{Message, MessageId, Packet},
    messages::{
        AgentUpdate, ChatFromViewer, RegionHandshake,
        agent::agent_update::AgentData,
        chat::{chat_from_viewer, chat_from_viewer::ChatData},
        region::region_handshake::{RegionInfo1, RegionInfo2, RegionInfo3, RegionInfo4},
    },
};
use test_support::{lludp::vectors, parse_hex_fixture};
use uuid::Uuid;

fn fixture(hex: &str) -> Vec<u8> {
    parse_hex_fixture(hex).expect("valid protocol fixture")
}

fn chat_message() -> Message {
    Message::ChatFromViewer(ChatFromViewer {
        agent_data: chat_from_viewer::AgentData { agent_id: Uuid::nil(), session_id: Uuid::nil() },
        chat_data: ChatData { message: "hello".into(), chat_type: ChatType::Normal, channel: 42 },
    })
}

fn session_id() -> Uuid {
    Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").expect("valid fixture UUID")
}

fn agent_id() -> Uuid {
    Uuid::parse_str("ffeeddcc-bbaa-9988-7766-554433221100").expect("valid fixture UUID")
}

fn region_handshake(extended: bool) -> Message {
    Message::RegionHandshake(RegionHandshake {
        region_info1: RegionInfo1 { region_flags: 1, sim_access: 13, sim_name: "Test Region".into(), ..RegionInfo1::default() },
        region_info2: extended.then_some(RegionInfo2::default()),
        region_info3: extended.then_some(RegionInfo3::default()),
        region_info4: Vec::new(),
    })
}

#[test]
fn region_handshake_zerocode() {
    let packet_bytes = fixture(vectors::REGION_HANDSHAKE);
    let decoded = Packet::decode(&packet_bytes).expect("decode fixture");

    assert_eq!(decoded.sequence(), 0x0102_0304);
    assert!(decoded.is_reliable());
    assert!(decoded.message().requires_trusted_circuit());
    assert_eq!(decoded.message(), &region_handshake(true));
    assert!(decoded.trailing_message_bytes().is_empty());
    assert_eq!(decoded.encode().expect("re-encode handshake fixture"), packet_bytes);

    let mut truncated = packet_bytes;
    truncated.truncate(truncated.len() - 2);
    assert_eq!(Packet::decode(&truncated), Err(LLUdpError::TruncatedPacket));
}

#[test]
fn region_handshake_fixtures() {
    let cases = [
        (
            "non-ASCII strings",
            vectors::REGION_HANDSHAKE_STRINGS,
            Message::RegionHandshake(RegionHandshake {
                region_info1: RegionInfo1 { region_flags: 1, sim_access: 13, sim_name: "Test Región".into(), ..RegionInfo1::default() },
                region_info2: Some(RegionInfo2::default()),
                region_info3: Some(RegionInfo3 {
                    colo_name: "Láb".into(),
                    product_sku: "SKU-42".into(),
                    product_name: "Radía test".into(),
                    ..RegionInfo3::default()
                }),
                region_info4: Vec::new(),
            }),
        ),
        (
            "RegionInfo4",
            vectors::REGION_HANDSHAKE_INFO4,
            Message::RegionHandshake(RegionHandshake {
                region_info1: RegionInfo1 { region_flags: 1, sim_access: 13, sim_name: "Test Region".into(), ..RegionInfo1::default() },
                region_info2: Some(RegionInfo2::default()),
                region_info3: Some(RegionInfo3::default()),
                region_info4: vec![
                    RegionInfo4 { region_flags_extended: 0x0102_0304_0506_0708, region_protocols: 0x1112_1314_1516_1718 },
                    RegionInfo4 { region_flags_extended: 0x2122_2324_2526_2728, region_protocols: 0x3132_3334_3536_3738 },
                ],
            }),
        ),
        ("legacy", vectors::REGION_HANDSHAKE_LEGACY, region_handshake(false)),
    ];
    for (case, hex, message) in cases {
        let bytes = fixture(hex);
        let packet = Packet::decode(&bytes).expect(case);

        assert_eq!(packet.message(), &message, "{case}");
        assert!(packet.trailing_message_bytes().is_empty(), "{case}");
        assert_eq!(packet.encode().expect(case), bytes, "{case}");
    }
}

#[test]
fn region_handshake_invalid_utf8() {
    let bytes = fixture(vectors::REGION_HANDSHAKE_INVALID_UTF8);

    assert_eq!(Packet::decode(&bytes), Err(LLUdpError::InvalidUtf8));
}

#[test]
fn handshake_blocks_pair() {
    let packet = Packet::new(
        1,
        Message::RegionHandshake(RegionHandshake {
            region_info1: RegionInfo1::default(),
            region_info2: Some(RegionInfo2::default()),
            region_info3: None,
            region_info4: Vec::new(),
        }),
    );
    assert_eq!(packet.encode(), Err(LLUdpError::InvalidMessage));
}

#[test]
fn agent_update_zero_coding() {
    let expected = fixture(vectors::AGENT_UPDATE_ZEROCODED);
    let message = Message::AgentUpdate(AgentUpdate {
        agent_data: AgentData {
            agent_id: Uuid::nil(),
            session_id: Uuid::nil(),
            body_rotation: Quat::IDENTITY,
            head_rotation: Quat::IDENTITY,
            state: 0,
            camera_center: Vec3::ZERO,
            camera_at_axis: Vec3::ZERO,
            camera_left_axis: Vec3::ZERO,
            camera_up_axis: Vec3::ZERO,
            far: 0.0,
            control_flags: 0,
            flags: 0,
        },
    });
    let packet = Packet::new(0x0102_0304, message.clone());

    assert_eq!(packet.encode().expect("encode packet"), expected);
    let decoded = Packet::decode(&expected).expect("decode fixture");
    assert_eq!(decoded.message(), &message);
    assert_eq!(decoded.encode().expect("re-encode fixture"), expected);
}

#[test]
fn initial_agent_update() {
    let expected = fixture(vectors::AGENT_UPDATE_INITIAL);
    let decoded = Packet::decode(&expected).expect("decode fixture");

    assert_eq!(decoded.sequence(), 3);
    assert!(decoded.is_reliable());
    assert_eq!(
        decoded.message(),
        &Message::AgentUpdate(AgentUpdate {
            agent_data: AgentData {
                agent_id: agent_id(),
                session_id: session_id(),
                body_rotation: Quat::IDENTITY,
                head_rotation: Quat::IDENTITY,
                state: 0,
                camera_center: Vec3::new(128.0, 128.0, 30.0),
                camera_at_axis: Vec3::new(1.0, 0.0, 0.0),
                camera_left_axis: Vec3::new(0.0, 1.0, 0.0),
                camera_up_axis: Vec3::new(0.0, 0.0, 1.0),
                far: 128.0,
                control_flags: 0,
                flags: 0,
            },
        })
    );
    assert_eq!(decoded.encode().expect("re-encode fixture"), expected);
}

#[test]
fn chat_zerocoding() {
    let cases = [("default", vectors::CHAT_FROM_VIEWER_NIL_IDS, false), ("requested", vectors::CHAT_FROM_VIEWER_ZEROCODED, true)];
    for (case, hex, zero_coded) in cases {
        let expected = fixture(hex);
        let message = chat_message();
        let mut packet = Packet::new(0x0102_0304, message.clone()).with_reliable();
        if zero_coded {
            packet = packet.with_zero_coding(true);
        }

        assert_eq!(packet.encode().expect(case), expected, "{case}");
        assert_eq!(expected[0] & 0x80 != 0, zero_coded, "{case}");
        assert_eq!(Packet::decode(&expected).expect(case).message(), &message, "{case}");
    }
}

#[test]
fn invalid_zerocoded_id() {
    let mut datagram = vec![0x80, 0, 0, 0, 1, 0];
    for _ in 0..33 {
        datagram.extend_from_slice(&[0, u8::MAX]);
    }
    datagram.push(1);

    assert!(datagram.len() < 0x2000);
    assert_eq!(Packet::decode(&datagram), Err(LLUdpError::InvalidMessageId));
}

#[test]
fn large_expansion_rejected() {
    let bytes = fixture(vectors::TRANSFER_REQUEST_LARGE_ZEROCODED);
    assert!(bytes.len() < 0x2000);
    assert_eq!(Packet::decode(&bytes), Err(LLUdpError::InvalidZeroCode));
}

#[test]
fn expansion_at_limit() {
    let bytes = fixture(vectors::ZEROCODE_PACKET_AT_LIMIT);
    assert!(bytes.len() < 0x2000);
    let decoded = Packet::decode(&bytes).expect("decode packet at viewer buffer limit");

    assert_eq!(decoded.message(), &Message::Unknown { id: MessageId::low(0xfff9), payload: vec![0; 8182] });
}

#[test]
fn extended_run() {
    let bytes = fixture(vectors::ZEROCODE_EXTENDED_RUN);
    let decoded = Packet::decode(&bytes).expect("decode extended zero-run fixture");

    assert_eq!(decoded.message(), &Message::Unknown { id: MessageId::low(0xfff9), payload: vec![0; 256] });
    assert_eq!(decoded.encode().expect("encode canonical zero run"), [0x80, 1, 2, 3, 4, 0, 0xff, 0xff, 0xff, 0xf9, 0, 0xff, 0, 1]);
}
