use std::time::{Duration, Instant};

use crate::inspection::{FrameSpike, FrameSpikeRing};

const SMOOTHING_ALPHA: f64 = 0.15;
const STALE_FRAME_GAP: Duration = Duration::from_secs(1);
const MEDIAN_WINDOW: usize = 64;
const FRAME_WINDOW: usize = 128;
const MIN_MEDIAN_SAMPLES: usize = 4;

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
    pub(super) frame_p50_ms: f64,
    pub(super) frame_p99_ms: f64,
    pub(super) game_ms: f64,
    pub(super) render_ms: f64,
}

pub(super) struct FrameTimes {
    last_presented: Option<Instant>,
    frametime_ms: Option<f64>,
    game_ms: Option<f64>,
    render_ms: Option<f64>,
    recent_work_ms: [f64; MEDIAN_WINDOW],
    recent_head: usize,
    recent_len: usize,
    recent_frame_ms: [f64; FRAME_WINDOW],
    frame_head: usize,
    frame_len: usize,
    spikes: FrameSpikeRing,
}

impl Default for FrameTimes {
    fn default() -> Self {
        Self {
            last_presented: None,
            frametime_ms: None,
            game_ms: None,
            render_ms: None,
            recent_work_ms: [0.0; MEDIAN_WINDOW],
            recent_head: 0,
            recent_len: 0,
            recent_frame_ms: [0.0; FRAME_WINDOW],
            frame_head: 0,
            frame_len: 0,
            spikes: FrameSpikeRing::default(),
        }
    }
}

impl FrameTimes {
    pub(super) fn record(&mut self, presented: Instant, game_work: Duration, render_work: Duration, gpu_timings_ms: Option<(f64, f64)>) {
        let game_ms = game_work.as_secs_f64() * 1000.0;
        let render_ms = render_work.as_secs_f64() * 1000.0;
        let work_ms = game_ms + render_ms;
        let (present_delta_ms, frame_sample_ms) = if let Some(previous) = self.last_presented {
            let delta = presented.saturating_duration_since(previous);
            let delta_ms = delta.as_secs_f64() * 1000.0;
            if delta > Duration::ZERO && delta <= STALE_FRAME_GAP {
                self.frametime_ms = Some(smooth(self.frametime_ms, delta_ms));
                (delta_ms, Some(delta_ms))
            } else if delta > STALE_FRAME_GAP {
                self.frametime_ms = Some(work_ms);
                (delta_ms, Some(work_ms))
            } else {
                (delta_ms, None)
            }
        } else {
            self.frametime_ms = Some(work_ms);
            (work_ms, Some(work_ms))
        };
        if let Some(sample_ms) = frame_sample_ms {
            *self.recent_frame_ms.get_mut(self.frame_head).expect("frame_head is within FRAME_WINDOW") = sample_ms;
            self.frame_head = (self.frame_head + 1) % FRAME_WINDOW;
            if self.frame_len < FRAME_WINDOW {
                self.frame_len += 1;
            }
        }
        if let Some(median) = self.median_work_ms()
            && median > 0.0
            && work_ms > 2.0 * median
        {
            self.spikes.push(FrameSpike { work_ms, frame_ms: present_delta_ms, game_ms, render_ms, gpu_timings_ms });
        }
        *self.recent_work_ms.get_mut(self.recent_head).expect("recent_head is within MEDIAN_WINDOW") = work_ms;
        self.recent_head = (self.recent_head + 1) % MEDIAN_WINDOW;
        if self.recent_len < MEDIAN_WINDOW {
            self.recent_len += 1;
        }
        self.last_presented = Some(presented);
        self.game_ms = Some(smooth(self.game_ms, game_ms));
        self.render_ms = Some(smooth(self.render_ms, render_ms));
    }

    pub(super) fn metrics(&self) -> Option<FrameMetrics> {
        let frametime_ms = self.frametime_ms?;
        let fps = if frametime_ms > 0.0 { 1000.0 / frametime_ms } else { 0.0 };
        let frame_p50_ms = self.percentile_frame_ms(0.50).unwrap_or(frametime_ms);
        let frame_p99_ms = self.percentile_frame_ms(0.99).unwrap_or(frametime_ms);
        Some(FrameMetrics { fps, frametime_ms, frame_p50_ms, frame_p99_ms, game_ms: self.game_ms?, render_ms: self.render_ms? })
    }

    pub(super) fn spikes(&self) -> FrameSpikeRing {
        self.spikes
    }

    fn percentile_frame_ms(&self, percentile: f64) -> Option<f64> {
        if self.frame_len == 0 {
            return None;
        }
        let mut scratch = self.recent_frame_ms;
        let slice = scratch.get_mut(..self.frame_len).expect("frame_len is at most FRAME_WINDOW");
        let rank = ((self.frame_len as f64 * percentile).ceil() as usize).saturating_sub(1).min(self.frame_len - 1);
        let (_, value, _) = slice.select_nth_unstable_by(rank, f64::total_cmp);
        Some(*value)
    }

    fn median_work_ms(&self) -> Option<f64> {
        if self.recent_len < MIN_MEDIAN_SAMPLES {
            return None;
        }
        let mut scratch = self.recent_work_ms;
        let slice = scratch.get_mut(..self.recent_len).expect("recent_len is at most MEDIAN_WINDOW");
        let (_, median, _) = slice.select_nth_unstable_by(self.recent_len / 2, f64::total_cmp);
        Some(*median)
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
    use crate::inspection::FRAME_SPIKE_CAPACITY;

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
    }

    #[test]
    fn tracks_interval_and_work() {
        let mut times = FrameTimes::default();
        assert!(times.metrics().is_none());

        let start = Instant::now();
        times.record(start, Duration::from_millis(2), Duration::from_millis(4), None);
        times.record(start + Duration::from_millis(16), Duration::from_millis(2), Duration::from_millis(4), None);
        let metrics = times.metrics().expect("metrics available after presenting");
        assert!(metrics.fps > 0.0);
        assert!((metrics.game_ms - 2.0).abs() < 0.01);
        assert!((metrics.render_ms - 4.0).abs() < 0.01);
    }

    #[test]
    fn records_frame_spikes_in_bounded_ring() {
        let mut times = FrameTimes::default();
        let mut now = Instant::now();
        for _ in 0..8 {
            times.record(now, Duration::from_millis(2), Duration::from_millis(4), Some((1.0, 2.0)));
            now += Duration::from_millis(16);
        }
        assert!(times.spikes().is_empty());

        now += Duration::from_millis(500);
        times.record(now, Duration::from_millis(2), Duration::from_millis(4), Some((1.0, 2.0)));
        assert!(times.spikes().is_empty(), "idle present gap with normal work must not record a spike");

        for spike_index in 0..40 {
            let render_ms = 12 + spike_index;
            now += Duration::from_millis(16);
            times.record(now, Duration::from_millis(5), Duration::from_millis(render_ms), Some((3.0, 4.0)));
            for _ in 0..3 {
                now += Duration::from_millis(16);
                times.record(now, Duration::from_millis(2), Duration::from_millis(4), Some((1.0, 2.0)));
            }
        }

        let spikes = times.spikes().iter().collect::<Vec<_>>();
        assert_eq!(spikes.len(), FRAME_SPIKE_CAPACITY);
        assert!((spikes[0].work_ms - 25.0).abs() < 0.1);
        assert!((spikes[31].work_ms - 56.0).abs() < 0.1);
        assert!((spikes[31].frame_ms - 16.0).abs() < 0.1);
        assert_eq!(spikes[31].gpu_timings_ms, Some((3.0, 4.0)));
    }

    #[test]
    fn frame_percentiles_from_fixed_samples() {
        let mut times = FrameTimes::default();
        let mut now = Instant::now();
        times.record(now, Duration::from_millis(1), Duration::ZERO, None);
        for sample_ms in 2..=100_u64 {
            now += Duration::from_millis(sample_ms);
            times.record(now, Duration::from_millis(1), Duration::from_millis(1), None);
        }
        let metrics = times.metrics().expect("metrics available");
        assert!((metrics.frame_p50_ms - 50.0).abs() < 0.01, "p50 was {}", metrics.frame_p50_ms);
        assert!((metrics.frame_p99_ms - 99.0).abs() < 0.01, "p99 was {}", metrics.frame_p99_ms);
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
