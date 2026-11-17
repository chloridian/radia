use std::time::{Duration, Instant};

pub struct Summary<T> {
    interval: Duration,
    pending: Option<T>,
    reported_at: Option<Instant>,
}

impl<T: Default> Summary<T> {
    pub const fn new(interval: Duration) -> Self {
        Self { interval, pending: None, reported_at: None }
    }

    pub fn record(&mut self, now: Instant, update: impl FnOnce(&mut T)) -> Option<T> {
        update(self.pending.get_or_insert_with(T::default));
        if self.reported_at.is_some_and(|at| now.saturating_duration_since(at) < self.interval) {
            return None;
        }
        self.reported_at = Some(now);
        self.pending.take()
    }

    pub fn flush(&mut self) -> Option<T> {
        self.pending.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTERVAL: Duration = Duration::from_secs(10);

    fn count(summary: &mut Summary<u64>, now: Instant) -> Option<u64> {
        summary.record(now, |count| *count += 1)
    }

    #[test]
    fn first_record_reports_immediately() {
        let mut summary = Summary::new(INTERVAL);
        assert_eq!(count(&mut summary, Instant::now()), Some(1));
    }

    #[test]
    fn records_within_interval_accumulate() {
        let start = Instant::now();
        let mut summary = Summary::new(INTERVAL);
        assert_eq!(count(&mut summary, start), Some(1));
        assert_eq!(count(&mut summary, start + Duration::from_secs(1)), None);
        assert_eq!(count(&mut summary, start + INTERVAL - Duration::from_millis(1)), None);
        assert_eq!(count(&mut summary, start + INTERVAL), Some(3));
        assert_eq!(count(&mut summary, start + INTERVAL + Duration::from_secs(1)), None);
        assert_eq!(count(&mut summary, start + INTERVAL * 2 + Duration::from_secs(5)), Some(2));
    }

    #[test]
    fn interval_restarts_at_each_report() {
        let start = Instant::now();
        let mut summary = Summary::new(INTERVAL);
        assert_eq!(count(&mut summary, start + Duration::from_secs(3)), Some(1));
        assert_eq!(count(&mut summary, start + INTERVAL), None);
        assert_eq!(count(&mut summary, start + INTERVAL + Duration::from_secs(3)), Some(2));
    }

    #[test]
    fn earlier_clock_reading_stays_suppressed() {
        let start = Instant::now() + Duration::from_secs(60);
        let mut summary = Summary::new(INTERVAL);
        assert_eq!(count(&mut summary, start), Some(1));
        assert_eq!(count(&mut summary, start - Duration::from_secs(30)), None);
        assert_eq!(count(&mut summary, start + INTERVAL), Some(2));
    }

    #[test]
    fn flush_drains_pending_regardless_of_interval() {
        let start = Instant::now();
        let mut summary = Summary::new(INTERVAL);
        assert_eq!(summary.flush(), None);
        assert_eq!(count(&mut summary, start), Some(1));
        assert_eq!(summary.flush(), None);
        assert_eq!(count(&mut summary, start + Duration::from_secs(1)), None);
        assert_eq!(count(&mut summary, start + Duration::from_secs(2)), None);
        assert_eq!(summary.flush(), Some(2));
        assert_eq!(summary.flush(), None);
        assert_eq!(count(&mut summary, start + Duration::from_secs(3)), None);
        assert_eq!(count(&mut summary, start + INTERVAL), Some(2));
    }
}
