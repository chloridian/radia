use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use utils::hardware::Hardware;

use crate::{
    limits::auto_worker_threads,
    workers::{WorkerBudget, WorkerKind, WorkerPermit},
};

const MIB: usize = 1024 * 1024;
const DEFAULT_MEMORY_BYTES: usize = 256 * MIB;
const DEFAULT_WORKER_THREADS: usize = 4;

pub const UNBOUNDED_MEMORY_BYTES: usize = usize::MAX;
pub const MIN_CAPPED_MEMORY_BYTES: usize = DEFAULT_MEMORY_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub memory_bytes: usize,
    pub worker_threads: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { memory_bytes: DEFAULT_MEMORY_BYTES, worker_threads: DEFAULT_WORKER_THREADS }
    }
}

impl Limits {
    pub fn derive(hardware: &Hardware) -> Self {
        Self { memory_bytes: UNBOUNDED_MEMORY_BYTES, worker_threads: auto_worker_threads(hardware.cores) }
    }
}

#[derive(Clone)]
pub struct Budget(Arc<Accounting>);

impl Default for Budget {
    fn default() -> Self {
        Self::new(Limits::default())
    }
}

impl Budget {
    pub fn new(limits: Limits) -> Self {
        Self::with_workers(limits, WorkerBudget::new(limits.worker_threads))
    }

    pub fn with_workers(limits: Limits, workers: WorkerBudget) -> Self {
        Self(Arc::new(Accounting { used: AtomicUsize::new(0), peak: AtomicUsize::new(0), workers, memory_bytes: limits.memory_bytes }))
    }

    pub fn limits(&self) -> Limits {
        Limits { memory_bytes: self.0.memory_bytes, worker_threads: self.0.workers.capacity() }
    }

    pub fn memory_bytes(&self) -> usize {
        self.0.memory_bytes
    }

    pub fn worker_threads(&self) -> usize {
        self.0.workers.capacity()
    }

    pub fn workers(&self) -> WorkerBudget {
        self.0.workers.clone()
    }

    pub fn charged_bytes(&self) -> usize {
        self.0.used.load(Ordering::Acquire)
    }

    pub fn high_water_bytes(&self) -> usize {
        self.0.peak.load(Ordering::Acquire)
    }

    pub(in crate::scene) fn reserve(&self, bytes: usize) -> Option<Lease> {
        let limit = self.0.memory_bytes;
        let before = self
            .0
            .used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| used.checked_add(bytes).filter(|total| *total <= limit))
            .ok()?;
        self.0.peak.fetch_max(before + bytes, Ordering::AcqRel);
        Some(Lease { budget: self.clone(), bytes })
    }

    pub(in crate::scene) fn mesh_work(&self) -> Option<MeshWork> {
        let permit = self.0.workers.try_acquire(WorkerKind::Mesh)?;
        let Some(lease) = self.reserve(4 * 1024 * 1024) else {
            drop(permit);
            return None;
        };
        Some(MeshWork { lease, _permit: permit })
    }
}

struct Accounting {
    used: AtomicUsize,
    peak: AtomicUsize,
    workers: WorkerBudget,
    memory_bytes: usize,
}

pub(in crate::scene) struct MeshWork {
    pub(in crate::scene) lease: Lease,
    _permit: WorkerPermit,
}

pub(in crate::scene) struct Lease {
    budget: Budget,
    bytes: usize,
}

impl Lease {
    pub(in crate::scene) fn split(&mut self, bytes: usize) -> Option<Self> {
        self.bytes = self.bytes.checked_sub(bytes)?;
        Some(Self { budget: self.budget.clone(), bytes })
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.budget.0.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
