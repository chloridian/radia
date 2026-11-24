use network::{
    CircuitMotionStatus, CircuitStats,
    capabilities::{CapabilityDiagnostics, CapabilityLastStatus},
    default_throttle_rates,
};
use serde_json::{Value, json};
use world::Region;

use super::{ToolContent, ToolFuture, ToolSpec};
use crate::{inspection::ViewerInspectionHandle, mcp::MAX_JSON_RESPONSE_BYTES};

const DEFAULT_LIMIT: usize = 32;
const MAX_LIMIT: usize = 100;

pub const SPEC: ToolSpec = ToolSpec {
    name: "network",
    description: "Return per-region circuit packet stats, throttle settings, EventQueueGet state, and capability statuses without URLs or session IDs.",
    input_schema,
    handler,
    max_response_bytes: MAX_JSON_RESPONSE_BYTES,
};

fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT }
        }
    })
}

struct RegionNetworkSnapshot {
    region: Region,
    region_name: Option<String>,
    is_primary: bool,
    circuit: CircuitMotionStatus,
    stats: CircuitStats,
    capabilities: Option<CapabilityDiagnostics>,
}

fn format_capability_status(status: CapabilityLastStatus) -> Value {
    match status {
        CapabilityLastStatus::Http(code) => Value::from(code.to_string()),
        CapabilityLastStatus::Cancelled => Value::from("cancelled"),
        CapabilityLastStatus::SessionClosed => Value::from("session_closed"),
        CapabilityLastStatus::Transport(kind) => Value::from(format!("{kind:?}")),
        CapabilityLastStatus::Error => Value::from("error"),
    }
}

fn format_network_snapshot(regions: &[RegionNetworkSnapshot], limit: usize) -> Value {
    let throttle = default_throttle_rates();
    let throttle_json = json!({
        "resend_bps": throttle.resend_bits_per_second,
        "land_bps": throttle.land_bits_per_second,
        "wind_bps": throttle.wind_bits_per_second,
        "cloud_bps": throttle.cloud_bits_per_second,
        "task_bps": throttle.task_bits_per_second,
        "texture_bps": throttle.texture_bits_per_second,
        "asset_bps": throttle.asset_bits_per_second,
    });
    let entries = regions
        .iter()
        .take(limit)
        .map(|entry| {
            let stats = entry.stats;
            let event_queue = entry.capabilities.as_ref().map(|caps| {
                json!({
                    "state": caps.event_queue.state.as_str(),
                    "last_status": caps.event_queue.last_status,
                })
            });
            let (capabilities, has_more_capabilities) = match &entry.capabilities {
                Some(caps) => {
                    let list = caps
                        .capabilities
                        .iter()
                        .take(limit)
                        .map(|cap| {
                            json!({
                                "name": cap.name.as_ref(),
                                "granted": cap.granted,
                                "last_status": cap.last_status.map(format_capability_status),
                            })
                        })
                        .collect::<Vec<_>>();
                    (list, caps.capabilities.len() > limit)
                }
                None => (Vec::new(), false),
            };
            json!({
                "region": entry.region.get(),
                "name": entry.region_name,
                "is_primary": entry.is_primary,
                "connected": entry.circuit.connected,
                "ping_ms": entry.circuit.measured_ping_delay.map(|d| d.as_secs_f64() * 1000.0),
                "circuit": {
                    "packets_in": stats.packets_in,
                    "packets_out": stats.packets_out,
                    "packets_in_per_sec": stats.packets_in_per_sec,
                    "packets_out_per_sec": stats.packets_out_per_sec,
                    "unacked": stats.unacked,
                    "resent": stats.resent,
                    "throttle_sent": stats.throttle_sent,
                    "throttle_rates": throttle_json,
                },
                "event_queue": event_queue,
                "capabilities": capabilities,
                "has_more_capabilities": has_more_capabilities,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "active_session": true,
        "regions": entries,
        "has_more": regions.len() > limit,
    })
}

fn handler(handle: ViewerInspectionHandle, args: Value) -> ToolFuture {
    Box::pin(async move {
        let Some(session) = handle.session() else {
            return Ok(ToolContent::Json(json!({ "active_session": false })));
        };
        let limit =
            args.get("limit").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
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
        Ok(ToolContent::Json(format_network_snapshot(&snapshots, limit)))
    })
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

        let output = format_network_snapshot(&regions, 1);
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
