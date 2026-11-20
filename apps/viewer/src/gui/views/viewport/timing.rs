use std::time::{Duration, Instant};

const FRAME_WORK_SAMPLES: usize = 120;

pub(super) struct FrameTimes {
    samples: [Duration; FRAME_WORK_SAMPLES],
    next: usize,
    len: usize,
}

impl Default for FrameTimes {
    fn default() -> Self {
        Self { samples: [Duration::ZERO; FRAME_WORK_SAMPLES], next: 0, len: 0 }
    }
}

pub(super) struct Summary {
    pub(super) p50: Duration,
    pub(super) p95: Duration,
}

impl FrameTimes {
    pub(super) fn record(&mut self, frame_work: Duration) {
        self.samples[self.next] = frame_work;
        self.next = (self.next + 1) % FRAME_WORK_SAMPLES;
        self.len = (self.len + 1).min(FRAME_WORK_SAMPLES);
    }

    pub(super) fn summary(&self) -> Option<Summary> {
        if self.len == 0 {
            return None;
        }
        let mut ordered = self.samples;
        ordered[..self.len].sort_unstable();
        Some(Summary { p50: percentile(&ordered[..self.len], 50)?, p95: percentile(&ordered[..self.len], 95)? })
    }
}

fn percentile(values: &[Duration], percentile: usize) -> Option<Duration> {
    let rank = values.len().checked_mul(percentile)?.div_ceil(100).max(1);
    values.get(rank - 1).copied()
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
    fn percentiles_use_nearest_rank() {
        for (frames, expected) in [(0, None), (20, Some((10, 19))), (121, Some((61, 115)))] {
            let mut times = FrameTimes::default();
            for millis in 1..=frames {
                times.record(Duration::from_millis(millis));
            }
            let summary = times.summary().map(|summary| (summary.p50, summary.p95));
            let expected = expected.map(|(p50, p95)| (Duration::from_millis(p50), Duration::from_millis(p95)));
            assert_eq!(summary, expected, "{frames} frames");
        }
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
