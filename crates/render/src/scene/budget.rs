use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use utils::hardware::Hardware;

use crate::limits::worker_split;

const MIB: usize = 1024 * 1024;
const DEFAULT_MEMORY_BYTES: usize = 256 * MIB;
const DEFAULT_MESH_JOBS: usize = 4;

pub const UNBOUNDED_MEMORY_BYTES: usize = usize::MAX;
pub const MIN_CAPPED_MEMORY_BYTES: usize = DEFAULT_MEMORY_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub memory_bytes: usize,
    pub mesh_jobs: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { memory_bytes: DEFAULT_MEMORY_BYTES, mesh_jobs: DEFAULT_MESH_JOBS }
    }
}

impl Limits {
    pub fn derive(hardware: &Hardware) -> Self {
        let (_, mesh_jobs) = worker_split(hardware.cores);
        Self { memory_bytes: UNBOUNDED_MEMORY_BYTES, mesh_jobs }
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
        Self(Arc::new(Accounting {
            used: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            mesh_jobs: AtomicUsize::new(0),
            memory_bytes: limits.memory_bytes,
            mesh_jobs_limit: limits.mesh_jobs.max(1),
        }))
    }

    pub fn limits(&self) -> Limits {
        Limits { memory_bytes: self.0.memory_bytes, mesh_jobs: self.0.mesh_jobs_limit }
    }

    pub fn memory_bytes(&self) -> usize {
        self.0.memory_bytes
    }

    pub fn mesh_jobs(&self) -> usize {
        self.0.mesh_jobs_limit
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
        let limit = self.0.mesh_jobs_limit;
        self.0.mesh_jobs.try_update(Ordering::AcqRel, Ordering::Acquire, |jobs| (jobs < limit).then_some(jobs + 1)).ok()?;
        let Some(lease) = self.reserve(4 * 1024 * 1024) else {
            self.0.mesh_jobs.fetch_sub(1, Ordering::AcqRel);
            return None;
        };
        Some(MeshWork { lease, budget: self.clone() })
    }
}

struct Accounting {
    used: AtomicUsize,
    peak: AtomicUsize,
    mesh_jobs: AtomicUsize,
    memory_bytes: usize,
    mesh_jobs_limit: usize,
}

pub(in crate::scene) struct MeshWork {
    pub(in crate::scene) lease: Lease,
    budget: Budget,
}

impl Drop for MeshWork {
    fn drop(&mut self) {
        self.budget.0.mesh_jobs.fetch_sub(1, Ordering::AcqRel);
    }
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
