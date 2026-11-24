use crate::{
    LLUdpError,
    lludp::{
        codec::{Reader, Writer},
        message::{AllocationStrategy, TypedMessage, allocation},
        types::MessageId,
    },
    schema::ids::SIM_STATS,
};

#[derive(Clone, Debug, PartialEq)]
pub struct SimStats {
    pub region: SimStatsRegion,
    pub stats: Vec<SimStatEntry>,
    pub pid: Option<i32>,
    pub region_info: Vec<SimStatsRegionInfo>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SimStatsRegion {
    pub region_x: u32,
    pub region_y: u32,
    pub region_flags: u32,
    pub object_capacity: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SimStatEntry {
    pub stat_id: u32,
    pub stat_value: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SimStatsRegionInfo {
    pub region_flags_extended: Option<u64>,
}

impl TypedMessage for SimStats {
    const ID: MessageId = SIM_STATS;
    const ALLOCATION: AllocationStrategy = AllocationStrategy::SchemaDerived;
    const INBOUND_ONLY: bool = true;

    fn decode(reader: &mut Reader<'_>) -> Result<Self, LLUdpError> {
        let region = SimStatsRegion {
            region_x: reader.u32()?,
            region_y: reader.u32()?,
            region_flags: reader.u32()?,
            object_capacity: reader.u32()?,
        };
        let stat_count = usize::from(reader.u8()?);
        let mut stats = Vec::with_capacity(stat_count);
        for _ in 0..stat_count {
            stats.push(SimStatEntry { stat_id: reader.u32()?, stat_value: reader.f32()? });
        }
        let pid = reader.optional_fixed(4, |reader| reader.i32())?;
        let region_info_count = reader.terminal_variable_count()?;
        let mut region_info = Vec::with_capacity(region_info_count);
        for _ in 0..region_info_count {
            region_info.push(SimStatsRegionInfo { region_flags_extended: reader.optional_fixed(8, |reader| reader.u64())? });
        }
        Ok(Self { region, stats, pid, region_info })
    }

    fn encode(&self, writer: &mut Writer<'_>) -> Result<(), LLUdpError> {
        writer.u32(self.region.region_x);
        writer.u32(self.region.region_y);
        writer.u32(self.region.region_flags);
        writer.u32(self.region.object_capacity);
        writer.u8(u8::try_from(self.stats.len()).map_err(|_| LLUdpError::VariableFieldTooLong)?);
        for entry in &self.stats {
            writer.u32(entry.stat_id);
            writer.f32(entry.stat_value);
        }
        if let Some(pid) = self.pid {
            writer.i32(pid);
        } else if !self.region_info.is_empty() {
            return Err(LLUdpError::InvalidMessage);
        } else {
            return Ok(());
        }
        writer.u8(u8::try_from(self.region_info.len()).map_err(|_| LLUdpError::VariableFieldTooLong)?);
        for info in &self.region_info {
            let Some(flags) = info.region_flags_extended else { return Ok(()) };
            writer.u64(flags);
        }
        Ok(())
    }

    fn retained_capacity_bytes(&self) -> usize {
        let mut bytes = allocation::capacity_bytes(self.stats.capacity(), size_of::<SimStatEntry>());
        allocation::add_capacity(&mut bytes, self.region_info.capacity(), size_of::<SimStatsRegionInfo>());
        bytes
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;
    use crate::lludp::{Message, Packet};

    #[test]
    fn round_trip_sim_stats() {
        let original = SimStats {
            region: SimStatsRegion { region_x: 1000, region_y: 2000, region_flags: 0x0010_0001, object_capacity: 20_000 },
            stats: vec![
                SimStatEntry { stat_id: 0, stat_value: 1.0 },
                SimStatEntry { stat_id: 1, stat_value: 45.0 },
                SimStatEntry { stat_id: 11, stat_value: 1234.0 },
            ],
            pid: Some(4242),
            region_info: vec![SimStatsRegionInfo { region_flags_extended: Some(0x0000_0002_0000_0000) }],
        };
        let packet = Packet::new(7, Message::SimStats(original.clone()));
        let encoded = packet.encode().expect("encode SimStats");
        let decoded = Packet::decode(&encoded).expect("decode SimStats");
        assert_matches!(decoded.message(), Message::SimStats(stats) if stats == &original);
    }

    #[test]
    fn decodes_without_trailing_region_info() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&1000_u32.to_le_bytes());
        payload.extend_from_slice(&2000_u32.to_le_bytes());
        payload.extend_from_slice(&0_u32.to_le_bytes());
        payload.extend_from_slice(&15_000_u32.to_le_bytes());
        payload.push(1);
        payload.extend_from_slice(&1_u32.to_le_bytes());
        payload.extend_from_slice(&44.5_f32.to_le_bytes());
        payload.extend_from_slice(&99_i32.to_le_bytes());

        let mut reader = Reader::new(&payload);
        let decoded = SimStats::decode(&mut reader).expect("decode SimStats without RegionInfo");
        assert_eq!(decoded.region.object_capacity, 15_000);
        assert_eq!(decoded.stats, vec![SimStatEntry { stat_id: 1, stat_value: 44.5 }]);
        assert_eq!(decoded.pid, Some(99));
        assert!(decoded.region_info.is_empty());
    }

    #[test]
    fn rejects_truncated_stat_entry() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0_u32.to_le_bytes());
        payload.extend_from_slice(&0_u32.to_le_bytes());
        payload.extend_from_slice(&0_u32.to_le_bytes());
        payload.extend_from_slice(&0_u32.to_le_bytes());
        payload.push(1);
        payload.extend_from_slice(&1_u32.to_le_bytes());

        let mut reader = Reader::new(&payload);
        assert!(SimStats::decode(&mut reader).is_err());
    }
}
