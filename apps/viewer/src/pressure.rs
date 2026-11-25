use std::time::Duration;

use tokio::{
    runtime::Handle,
    sync::watch,
    time::{self, MissedTickBehavior},
};
pub use utils::hardware::Pressure;
use utils::hardware::available_ram_bytes;

const MIB: u64 = 1024 * 1024;
const LOW_ENTER_BYTES: u64 = 512 * MIB;
const LOW_LEAVE_BYTES: u64 = 612 * MIB;
const CRITICAL_ENTER_BYTES: u64 = 256 * MIB;
const CRITICAL_LEAVE_BYTES: u64 = 512 * MIB;
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

pub fn next_pressure(previous: Pressure, available_bytes: u64) -> Pressure {
    match previous {
        Pressure::Normal => {
            if available_bytes < CRITICAL_ENTER_BYTES {
                Pressure::Critical
            } else if available_bytes < LOW_ENTER_BYTES {
                Pressure::Low
            } else {
                Pressure::Normal
            }
        }
        Pressure::Low => {
            if available_bytes < CRITICAL_ENTER_BYTES {
                Pressure::Critical
            } else if available_bytes >= LOW_LEAVE_BYTES {
                Pressure::Normal
            } else {
                Pressure::Low
            }
        }
        Pressure::Critical => {
            if available_bytes >= LOW_LEAVE_BYTES {
                Pressure::Normal
            } else if available_bytes >= CRITICAL_LEAVE_BYTES {
                Pressure::Low
            } else {
                Pressure::Critical
            }
        }
    }
}

pub fn spawn_monitor(runtime: &Handle) -> watch::Receiver<Pressure> {
    let (sender, receiver) = watch::channel(Pressure::Normal);
    drop(runtime.spawn(async move {
        let mut interval = time::interval(SAMPLE_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut current = Pressure::Normal;
        loop {
            tokio::select! {
                _ = sender.closed() => return,
                _ = interval.tick() => {}
            }
            let Some(available) = available_ram_bytes() else { continue };
            let next = next_pressure(current, available);
            if next != current {
                match next {
                    Pressure::Low | Pressure::Critical => {
                        tracing::warn!(level = ?next, available_mib = available / MIB, "system memory pressure elevated");
                    }
                    Pressure::Normal => {
                        tracing::info!(available_mib = available / MIB, "system memory pressure returned to normal");
                    }
                }
                current = next;
                sender.send_replace(next);
            }
        }
    }));
    receiver
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_hysteresis() {
        let readings_mib: &[(u64, Pressure)] = &[
            (1024, Pressure::Normal),
            (512, Pressure::Normal),
            (511, Pressure::Low),
            (520, Pressure::Low),
            (611, Pressure::Low),
            (500, Pressure::Low),
            (256, Pressure::Low),
            (255, Pressure::Critical),
            (300, Pressure::Critical),
            (511, Pressure::Critical),
            (260, Pressure::Critical),
            (512, Pressure::Low),
            (500, Pressure::Low),
            (611, Pressure::Low),
            (612, Pressure::Normal),
            (512, Pressure::Normal),
            (200, Pressure::Critical),
            (612, Pressure::Normal),
        ];

        let mut level = Pressure::Normal;
        let mut actual = Vec::with_capacity(readings_mib.len());
        let expected = readings_mib.iter().map(|(_, expected)| *expected).collect::<Vec<_>>();
        for &(available_mib, _) in readings_mib {
            level = next_pressure(level, available_mib * MIB);
            actual.push(level);
        }
        assert_eq!(actual, expected);
    }
}
