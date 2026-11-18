pub mod complete_ping_check;
pub mod packet_ack;
pub mod start_ping_check;
pub mod use_circuit_code;

pub use self::{
    complete_ping_check::CompletePingCheck, packet_ack::PacketAck, start_ping_check::StartPingCheck, use_circuit_code::UseCircuitCode,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PingId {
    pub ping_id: u8,
}
