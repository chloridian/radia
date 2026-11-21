use std::time::{Duration, Instant};

const SMOOTHING_ALPHA: f64 = 0.15;
const STALE_FRAME_GAP: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MetricStatus {
    Green,
    Yellow,
    Red,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct FrameMetrics {
    pub(super) fps: f64,
    pub(super) frametime_ms: f64,
    pub(super) game_ms: f64,
    pub(super) render_ms: f64,
}

#[derive(Default)]
pub(super) struct FrameTimes {
    last_presented: Option<Instant>,
    frametime_ms: Option<f64>,
    game_ms: Option<f64>,
    render_ms: Option<f64>,
}

impl FrameTimes {
    pub(super) fn record(&mut self, presented: Instant, game_work: Duration, render_work: Duration) {
        let game_ms = game_work.as_secs_f64() * 1000.0;
        let render_ms = render_work.as_secs_f64() * 1000.0;
        if let Some(previous) = self.last_presented {
            let delta = presented.saturating_duration_since(previous);
            if delta > Duration::ZERO && delta <= STALE_FRAME_GAP {
                let sample_ms = delta.as_secs_f64() * 1000.0;
                self.frametime_ms = Some(smooth(self.frametime_ms, sample_ms));
            } else if delta > STALE_FRAME_GAP {
                self.frametime_ms = Some(game_ms + render_ms);
            }
        } else {
            self.frametime_ms = Some(game_ms + render_ms);
        }
        self.last_presented = Some(presented);
        self.game_ms = Some(smooth(self.game_ms, game_ms));
        self.render_ms = Some(smooth(self.render_ms, render_ms));
    }

    pub(super) fn metrics(&self) -> Option<FrameMetrics> {
        let frametime_ms = self.frametime_ms?;
        let fps = if frametime_ms > 0.0 { 1000.0 / frametime_ms } else { 0.0 };
        Some(FrameMetrics { fps, frametime_ms, game_ms: self.game_ms?, render_ms: self.render_ms? })
    }
}

fn smooth(previous: Option<f64>, sample: f64) -> f64 {
    match previous {
        Some(prev) => prev * (1.0 - SMOOTHING_ALPHA) + sample * SMOOTHING_ALPHA,
        None => sample,
    }
}

pub(super) fn fps_status(fps: f64) -> MetricStatus {
    if fps >= 60.0 {
        MetricStatus::Green
    } else if fps >= 45.0 {
        MetricStatus::Yellow
    } else {
        MetricStatus::Red
    }
}

pub(super) fn frame_time_status(ms: f64) -> MetricStatus {
    if ms <= 16.67 {
        MetricStatus::Green
    } else if ms <= 22.22 {
        MetricStatus::Yellow
    } else {
        MetricStatus::Red
    }
}

pub(super) fn vram_status(used_bytes: usize, budget_bytes: usize) -> MetricStatus {
    if budget_bytes == 0 {
        return MetricStatus::Red;
    }
    let ratio = used_bytes as f64 / budget_bytes as f64;
    if ratio < 0.70 {
        MetricStatus::Green
    } else if ratio <= 0.90 {
        MetricStatus::Yellow
    } else {
        MetricStatus::Red
    }
}

pub(super) fn ping_status(ms: f64) -> MetricStatus {
    if ms < 100.0 {
        MetricStatus::Green
    } else if ms <= 200.0 {
        MetricStatus::Yellow
    } else {
        MetricStatus::Red
    }
}

#[allow(dead_code)]
pub(super) fn packet_loss_status(percent: f64) -> MetricStatus {
    if percent <= 0.0 {
        MetricStatus::Green
    } else if percent <= 1.0 {
        MetricStatus::Yellow
    } else {
        MetricStatus::Red
    }
}

pub(super) fn format_bytes(bytes: usize) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let value = bytes as f64;
    if value >= GIB { format!("{:.2} GiB", value / GIB) } else { format!("{:.1} MiB", value / MIB) }
}

pub(super) fn format_count(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.2}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}K", count as f64 / 1_000.0)
    } else {
        count.to_string()
    }
}

pub(super) fn next_frame_deadline(deadline: Instant, completed: Instant, interval: Duration) -> Instant {
    if deadline > completed {
        return deadline;
    }
    let periods = completed.duration_since(deadline).as_nanos() / interval.as_nanos() + 1;
    let advance = Duration::from_nanos(interval.as_nanos().saturating_mul(periods).min(u64::MAX as u128) as u64);
    deadline.checked_add(advance).unwrap_or(completed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_thresholds_match_vw306_boundaries() {
        assert_eq!(fps_status(60.0), MetricStatus::Green);
        assert_eq!(fps_status(59.99), MetricStatus::Yellow);
        assert_eq!(fps_status(45.0), MetricStatus::Yellow);
        assert_eq!(fps_status(44.99), MetricStatus::Red);

        assert_eq!(frame_time_status(16.67), MetricStatus::Green);
        assert_eq!(frame_time_status(16.68), MetricStatus::Yellow);
        assert_eq!(frame_time_status(22.22), MetricStatus::Yellow);
        assert_eq!(frame_time_status(22.23), MetricStatus::Red);

        assert_eq!(vram_status(69, 100), MetricStatus::Green);
        assert_eq!(vram_status(70, 100), MetricStatus::Yellow);
        assert_eq!(vram_status(90, 100), MetricStatus::Yellow);
        assert_eq!(vram_status(91, 100), MetricStatus::Red);

        assert_eq!(ping_status(99.9), MetricStatus::Green);
        assert_eq!(ping_status(100.0), MetricStatus::Yellow);
        assert_eq!(ping_status(200.0), MetricStatus::Yellow);
        assert_eq!(ping_status(200.1), MetricStatus::Red);

        assert_eq!(packet_loss_status(0.0), MetricStatus::Green);
        assert_eq!(packet_loss_status(0.5), MetricStatus::Yellow);
        assert_eq!(packet_loss_status(1.0), MetricStatus::Yellow);
        assert_eq!(packet_loss_status(1.01), MetricStatus::Red);
    }

    #[test]
    fn frame_times_track_interval_and_work() {
        let mut times = FrameTimes::default();
        assert!(times.metrics().is_none());

        let start = Instant::now();
        times.record(start, Duration::from_millis(2), Duration::from_millis(4));
        times.record(start + Duration::from_millis(16), Duration::from_millis(2), Duration::from_millis(4));
        let metrics = times.metrics().expect("metrics available after presenting");
        assert!(metrics.fps > 0.0);
        assert!((metrics.game_ms - 2.0).abs() < 0.01);
        assert!((metrics.render_ms - 4.0).abs() < 0.01);
    }

    #[test]
    fn deadline_keeps_cadence_skips_missed() {
        let start = Instant::now();
        let interval = Duration::from_millis(16);
        assert_eq!(next_frame_deadline(start, start + Duration::from_millis(5), interval), start + interval);
        assert_eq!(next_frame_deadline(start, start + Duration::from_millis(37), interval), start + interval * 3);
        assert_eq!(next_frame_deadline(start + interval, start, interval), start + interval);
    }
}
