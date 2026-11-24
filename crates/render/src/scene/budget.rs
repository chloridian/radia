use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

pub const MEMORY_BYTES: usize = 256 * 1024 * 1024;
const MIN_MESH_JOBS: usize = 2;
const MAX_MESH_JOBS: usize = 6;

pub(in crate::scene) fn max_mesh_jobs() -> usize {
    static MAX: OnceLock<usize> = OnceLock::new();
    *MAX.get_or_init(|| thread::available_parallelism().map(usize::from).unwrap_or(4).clamp(MIN_MESH_JOBS, MAX_MESH_JOBS))
}

#[derive(Clone, Default)]
pub struct Budget(Arc<Accounting>);

impl Budget {
    pub fn charged_bytes(&self) -> usize {
        self.0.used.load(Ordering::Acquire)
    }

    pub fn high_water_bytes(&self) -> usize {
        self.0.peak.load(Ordering::Acquire)
    }

    pub(in crate::scene) fn reserve(&self, bytes: usize) -> Option<Lease> {
        let before = self
            .0
            .used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| used.checked_add(bytes).filter(|total| *total <= MEMORY_BYTES))
            .ok()?;
        self.0.peak.fetch_max(before + bytes, Ordering::AcqRel);
        Some(Lease { budget: self.clone(), bytes })
    }

    pub(in crate::scene) fn mesh_work(&self) -> Option<MeshWork> {
        let limit = max_mesh_jobs();
        self.0.mesh_jobs.try_update(Ordering::AcqRel, Ordering::Acquire, |jobs| (jobs < limit).then_some(jobs + 1)).ok()?;
        let Some(lease) = self.reserve(4 * 1024 * 1024) else {
            self.0.mesh_jobs.fetch_sub(1, Ordering::AcqRel);
            return None;
        };
        Some(MeshWork { lease, budget: self.clone() })
    }
}

#[derive(Default)]
struct Accounting {
    used: AtomicUsize,
    peak: AtomicUsize,
    mesh_jobs: AtomicUsize,
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
