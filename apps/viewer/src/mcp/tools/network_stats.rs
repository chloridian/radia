use network::{
    CircuitMotionStatus, CircuitStats,
    capabilities::{CapabilityDiagnostics, CapabilityLastStatus, EventQueueState},
    default_throttle_rates,
};
use rmcp::{
    ErrorData,
    handler::server::wrapper::{Json, Parameters},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use world::Region;

use crate::mcp::server::Handler;

const DEFAULT_LIMIT: usize = 32;
const MAX_LIMIT: usize = 100;

#[derive(Deserialize, JsonSchema)]
pub struct NetworkArgs {
    #[schemars(description = "Maximum number of regions and capabilities per region to return (1..=100).")]
    pub limit: Option<usize>,
}

#[derive(Clone, Copy, Serialize, JsonSchema)]
pub struct ThrottleRatesOutput {
    pub resend_bps: f32,
    pub land_bps: f32,
    pub wind_bps: f32,
    pub cloud_bps: f32,
    pub task_bps: f32,
    pub texture_bps: f32,
    pub asset_bps: f32,
}

#[derive(Serialize, JsonSchema)]
pub struct CircuitStatsOutput {
    pub packets_in: u64,
    pub packets_out: u64,
    pub packets_in_per_sec: u32,
    pub packets_out_per_sec: u32,
    pub unacked: usize,
    pub resent: u64,
    pub throttle_sent: bool,
    pub throttle_rates: ThrottleRatesOutput,
}

#[derive(Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventQueueStateOutput {
    Starting,
    Connected,
    Backoff,
    Failed,
    Stopped,
}

#[derive(Serialize, JsonSchema)]
pub struct EventQueueOutput {
    pub state: EventQueueStateOutput,
    pub last_status: Option<u16>,
}

#[derive(Serialize, JsonSchema)]
pub struct CapabilityOutput {
    pub name: String,
    pub granted: bool,
    pub last_status: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct RegionNetworkOutput {
    pub region: u64,
    pub name: Option<String>,
    pub is_primary: bool,
    pub connected: bool,
    pub ping_ms: Option<f64>,
    pub circuit: CircuitStatsOutput,
    pub event_queue: Option<EventQueueOutput>,
    pub capabilities: Vec<CapabilityOutput>,
    pub has_more_capabilities: bool,
}

#[derive(Serialize, JsonSchema)]
pub struct NetworkOutput {
    pub active_session: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub regions: Option<Vec<RegionNetworkOutput>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
}

struct RegionNetworkSnapshot {
    region: Region,
    region_name: Option<String>,
    is_primary: bool,
    circuit: CircuitMotionStatus,
    stats: CircuitStats,
    capabilities: Option<CapabilityDiagnostics>,
}

#[tool_router(router = network_router, vis = "pub(super)")]
impl Handler {
    #[tool(
        name = "network",
        title = "Network Diagnostics",
        description = "Return per-region circuit packet stats, throttle settings, EventQueueGet state, and capability statuses without URLs or session IDs.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn network(&self, Parameters(args): Parameters<NetworkArgs>) -> Result<Json<NetworkOutput>, ErrorData> {
        let Some(session) = self.handle.session() else {
            return Ok(Json(NetworkOutput { active_session: false, regions: None, has_more: None }));
        };
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let handles = session.diagnostics.network.borrow().clone();
        let snapshots = handles
            .into_iter()
            .map(|entry| RegionNetworkSnapshot {
                region: entry.region,
                region_name: entry.region_name,
                is_primary: entry.is_primary,
                circuit: *entry.circuit.borrow(),
                stats: *entry.stats.borrow(),
                capabilities: entry.capabilities.as_ref().map(|client| client.diagnostics()),
            })
            .collect::<Vec<_>>();
        Ok(Json(format_network_snapshot(&snapshots, limit)))
    }
}

fn format_capability_status(status: CapabilityLastStatus) -> String {
    match status {
        CapabilityLastStatus::Http(code) => code.to_string(),
        CapabilityLastStatus::Cancelled => "cancelled".to_owned(),
        CapabilityLastStatus::SessionClosed => "session_closed".to_owned(),
        CapabilityLastStatus::Transport(kind) => kind.as_str().to_owned(),
        CapabilityLastStatus::Error => "error".to_owned(),
    }
}

fn format_network_snapshot(regions: &[RegionNetworkSnapshot], limit: usize) -> NetworkOutput {
    let throttle = default_throttle_rates();
    let throttle_rates = ThrottleRatesOutput {
        resend_bps: throttle.resend_bits_per_second,
        land_bps: throttle.land_bits_per_second,
        wind_bps: throttle.wind_bits_per_second,
        cloud_bps: throttle.cloud_bits_per_second,
        task_bps: throttle.task_bits_per_second,
        texture_bps: throttle.texture_bits_per_second,
        asset_bps: throttle.asset_bits_per_second,
    };
    let entries = regions
        .iter()
        .take(limit)
        .map(|entry| {
            let stats = entry.stats;
            let event_queue = entry.capabilities.as_ref().map(|caps| EventQueueOutput {
                state: match caps.event_queue.state {
                    EventQueueState::Starting => EventQueueStateOutput::Starting,
                    EventQueueState::Connected => EventQueueStateOutput::Connected,
                    EventQueueState::Backoff => EventQueueStateOutput::Backoff,
                    EventQueueState::Failed => EventQueueStateOutput::Failed,
                    EventQueueState::Stopped => EventQueueStateOutput::Stopped,
                },
                last_status: caps.event_queue.last_status,
            });
            let (capabilities, has_more_capabilities) = match &entry.capabilities {
                Some(caps) => {
                    let list = caps
                        .capabilities
                        .iter()
                        .take(limit)
                        .map(|cap| CapabilityOutput {
                            name: cap.name.to_string(),
                            granted: cap.granted,
                            last_status: cap.last_status.map(format_capability_status),
                        })
                        .collect();
                    (list, caps.capabilities.len() > limit)
                }
                None => (Vec::new(), false),
            };
            RegionNetworkOutput {
                region: entry.region.get(),
                name: entry.region_name.clone(),
                is_primary: entry.is_primary,
                connected: entry.circuit.connected,
                ping_ms: entry.circuit.measured_ping_delay.map(|d| d.as_secs_f64() * 1000.0),
                circuit: CircuitStatsOutput {
                    packets_in: stats.packets_in,
                    packets_out: stats.packets_out,
                    packets_in_per_sec: stats.packets_in_per_sec,
                    packets_out_per_sec: stats.packets_out_per_sec,
                    unacked: stats.unacked,
                    resent: stats.resent,
                    throttle_sent: stats.throttle_sent,
                    throttle_rates,
                },
                event_queue,
                capabilities,
                has_more_capabilities,
            }
        })
        .collect();
    NetworkOutput { active_session: true, regions: Some(entries), has_more: Some(regions.len() > limit) }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    use network::{
        CircuitMotionStatus, CircuitStats,
        capabilities::{CapabilityDiagnostics, CapabilityLastStatus, CapabilityRequestStatus, EventQueueState, EventQueueStatus},
    };
    use world::{World, WorldLimits};

    use super::{RegionNetworkSnapshot, format_network_snapshot};

    #[test]
    fn network_output_omits_urls_and_session_ids() {
        let mut world = World::new(WorldLimits {
            max_regions: 2,
            max_entities: 8,
            max_entities_per_region: 8,
            max_tombstones_per_region: 8,
            max_tombstones: 8,
            max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(8),
            max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(8),
            max_journal_entries: 8,
            max_journal_changes: 32,
            max_region_name_bytes: 64,
            max_recovery_targets: 4,
        });
        let region = world.register_region().expect("register region");
        let regions = vec![RegionNetworkSnapshot {
            region,
            region_name: Some("Ahern".to_owned()),
            is_primary: true,
            circuit: CircuitMotionStatus {
                last_packet_received_at: Instant::now(),
                measured_ping_delay: Some(Duration::from_millis(45)),
                connected: true,
            },
            stats: CircuitStats {
                packets_in: 120,
                packets_out: 80,
                packets_in_per_sec: 12,
                packets_out_per_sec: 8,
                unacked: 2,
                resent: 1,
                throttle_sent: true,
            },
            capabilities: Some(CapabilityDiagnostics {
                event_queue: EventQueueStatus { state: EventQueueState::Connected, last_status: Some(200) },
                capabilities: vec![
                    CapabilityRequestStatus {
                        name: Arc::from("FetchInventory2"),
                        granted: true,
                        last_status: Some(CapabilityLastStatus::Http(200)),
                    },
                    CapabilityRequestStatus {
                        name: Arc::from("GetMesh2"),
                        granted: true,
                        last_status: Some(CapabilityLastStatus::Http(503)),
                    },
                ],
            }),
        }];

        let output = serde_json::to_value(format_network_snapshot(&regions, 1)).expect("serialize network");
        let text = output.to_string();
        assert!(!text.contains("http://"));
        assert!(!text.contains("https://"));
        assert!(!text.contains("seed"));
        assert!(!text.contains("session_id"));
        assert!(!text.contains("secure_session_id"));
        assert_eq!(output["regions"][0]["event_queue"]["state"], "connected");
        assert_eq!(output["regions"][0]["has_more_capabilities"], true);
        assert_eq!(output["regions"][0]["capabilities"].as_array().unwrap().len(), 1);
    }
}
