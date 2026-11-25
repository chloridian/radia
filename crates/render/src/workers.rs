use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerKind {
    Texture,
    Mesh,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WorkerStats {
    pub capacity: usize,
    pub texture_running: usize,
    pub texture_queued: usize,
    pub texture_busy_ns: u64,
    pub mesh_running: usize,
    pub mesh_queued: usize,
    pub mesh_busy_ns: u64,
}

struct Shared {
    capacity: usize,
    active: AtomicUsize,
    texture_active: AtomicUsize,
    mesh_active: AtomicUsize,
    texture_queued: AtomicUsize,
    mesh_queued: AtomicUsize,
    texture_busy_ns: AtomicU64,
    mesh_busy_ns: AtomicU64,
}

#[derive(Clone)]
pub struct WorkerBudget(Arc<Shared>);

impl WorkerBudget {
    pub fn new(capacity: usize) -> Self {
        Self(Arc::new(Shared {
            capacity: capacity.max(1),
            active: AtomicUsize::new(0),
            texture_active: AtomicUsize::new(0),
            mesh_active: AtomicUsize::new(0),
            texture_queued: AtomicUsize::new(0),
            mesh_queued: AtomicUsize::new(0),
            texture_busy_ns: AtomicU64::new(0),
            mesh_busy_ns: AtomicU64::new(0),
        }))
    }

    pub fn capacity(&self) -> usize {
        self.0.capacity
    }

    pub fn active(&self) -> usize {
        self.0.active.load(Ordering::Acquire)
    }

    pub fn active_for(&self, kind: WorkerKind) -> usize {
        match kind {
            WorkerKind::Texture => self.0.texture_active.load(Ordering::Acquire),
            WorkerKind::Mesh => self.0.mesh_active.load(Ordering::Acquire),
        }
    }

    pub fn try_acquire(&self, kind: WorkerKind) -> Option<WorkerPermit> {
        let limit = self.0.capacity;
        self.0.active.try_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < limit).then_some(n + 1)).ok()?;
        match kind {
            WorkerKind::Texture => self.0.texture_active.fetch_add(1, Ordering::AcqRel),
            WorkerKind::Mesh => self.0.mesh_active.fetch_add(1, Ordering::AcqRel),
        };
        Some(WorkerPermit { shared: Arc::clone(&self.0), kind })
    }

    pub fn set_queued(&self, kind: WorkerKind, depth: usize) {
        match kind {
            WorkerKind::Texture => self.0.texture_queued.store(depth, Ordering::Relaxed),
            WorkerKind::Mesh => self.0.mesh_queued.store(depth, Ordering::Relaxed),
        }
    }

    pub fn record_busy(&self, kind: WorkerKind, elapsed: Duration) {
        let nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        match kind {
            WorkerKind::Texture => self.0.texture_busy_ns.fetch_add(nanos, Ordering::Relaxed),
            WorkerKind::Mesh => self.0.mesh_busy_ns.fetch_add(nanos, Ordering::Relaxed),
        };
    }

    pub fn stats(&self) -> WorkerStats {
        WorkerStats {
            capacity: self.0.capacity,
            texture_running: self.0.texture_active.load(Ordering::Acquire),
            texture_queued: self.0.texture_queued.load(Ordering::Relaxed),
            texture_busy_ns: self.0.texture_busy_ns.load(Ordering::Relaxed),
            mesh_running: self.0.mesh_active.load(Ordering::Acquire),
            mesh_queued: self.0.mesh_queued.load(Ordering::Relaxed),
            mesh_busy_ns: self.0.mesh_busy_ns.load(Ordering::Relaxed),
        }
    }
}

pub struct WorkerPermit {
    shared: Arc<Shared>,
    kind: WorkerKind,
}

impl Drop for WorkerPermit {
    fn drop(&mut self) {
        match self.kind {
            WorkerKind::Texture => self.shared.texture_active.fetch_sub(1, Ordering::AcqRel),
            WorkerKind::Mesh => self.shared.mesh_active.fetch_sub(1, Ordering::AcqRel),
        };
        self.shared.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_budget_flexes_between_kinds() {
        let budget = WorkerBudget::new(4);
        let mut textures = (0..4).map(|_| budget.try_acquire(WorkerKind::Texture).expect("texture slot")).collect::<Vec<_>>();
        assert!(budget.try_acquire(WorkerKind::Texture).is_none());
        assert!(budget.try_acquire(WorkerKind::Mesh).is_none());
        assert_eq!(budget.stats().texture_running, 4);
        assert_eq!(budget.stats().mesh_running, 0);

        textures.pop();
        textures.pop();
        let mesh_one = budget.try_acquire(WorkerKind::Mesh).expect("mesh takes freed slot");
        let mesh_two = budget.try_acquire(WorkerKind::Mesh).expect("mesh takes second freed slot");
        assert!(budget.try_acquire(WorkerKind::Texture).is_none());
        assert_eq!(budget.stats().texture_running, 2);
        assert_eq!(budget.stats().mesh_running, 2);

        drop(textures);
        let mesh_three = budget.try_acquire(WorkerKind::Mesh).expect("mesh expands into full budget");
        let mesh_four = budget.try_acquire(WorkerKind::Mesh).expect("mesh takes all four slots");
        assert_eq!(budget.stats().texture_running, 0);
        assert_eq!(budget.stats().mesh_running, 4);

        drop((mesh_one, mesh_two, mesh_three, mesh_four));
        assert_eq!(budget.active(), 0);
    }

    #[test]
    fn stats_track_busy_and_queue() {
        let budget = WorkerBudget::new(2);
        budget.set_queued(WorkerKind::Texture, 5);
        budget.set_queued(WorkerKind::Mesh, 3);
        budget.record_busy(WorkerKind::Texture, Duration::from_millis(12));
        budget.record_busy(WorkerKind::Mesh, Duration::from_millis(7));
        let stats = budget.stats();
        assert_eq!(stats.capacity, 2);
        assert_eq!(stats.texture_queued, 5);
        assert_eq!(stats.mesh_queued, 3);
        assert_eq!(stats.texture_busy_ns, 12_000_000);
        assert_eq!(stats.mesh_busy_ns, 7_000_000);
    }
}
