use std::{
    fmt,
    ops::Deref,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use tokio::sync::watch;

const MIB: usize = 1024 * 1024;
const DEFAULT_WORK_BYTES: usize = 512 * MIB;
const DEFAULT_TEXTURE_WORK_BYTES: usize = 192 * MIB;
const WORK_BYTES_PER_WORKER: usize = 128 * MIB;
pub(crate) const WORK_BYTES: usize = 32 * MIB;
#[cfg(test)]
pub(crate) const ASSET_WORK_BYTES: usize = DEFAULT_WORK_BYTES;
#[cfg(test)]
pub(crate) const TEXTURE_WORK_BYTES: usize = DEFAULT_TEXTURE_WORK_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub work_bytes: usize,
    pub texture_work_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { work_bytes: DEFAULT_WORK_BYTES, texture_work_bytes: DEFAULT_TEXTURE_WORK_BYTES }
    }
}

impl Limits {
    pub fn derive(workers: usize) -> Self {
        let work_bytes = workers.saturating_mul(WORK_BYTES_PER_WORKER).max(DEFAULT_WORK_BYTES);
        Self::from_work_bytes(work_bytes)
    }

    pub fn from_work_bytes(work_bytes: usize) -> Self {
        let work_bytes = work_bytes / MIB * MIB;
        let texture_work_bytes = (work_bytes * 3 / 8) / MIB * MIB;
        Self { work_bytes, texture_work_bytes }
    }
}

struct Accounting {
    charged: AtomicUsize,
    working: AtomicUsize,
    resident: AtomicUsize,
    high_water: AtomicUsize,
    work_bytes: usize,
    texture_work_bytes: usize,
    wake: watch::Sender<()>,
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
        let (wake, _) = watch::channel(());
        Self(Arc::new(Accounting {
            charged: AtomicUsize::new(0),
            working: AtomicUsize::new(0),
            resident: AtomicUsize::new(0),
            high_water: AtomicUsize::new(0),
            work_bytes: limits.work_bytes,
            texture_work_bytes: limits.texture_work_bytes.min(limits.work_bytes),
            wake,
        }))
    }

    pub fn limits(&self) -> Limits {
        Limits { work_bytes: self.0.work_bytes, texture_work_bytes: self.0.texture_work_bytes }
    }

    pub(crate) fn texture_work_bytes(&self) -> usize {
        self.0.texture_work_bytes
    }

    pub fn stats(&self) -> MemoryStats {
        MemoryStats {
            charged_bytes: self.0.charged.load(Ordering::Acquire),
            resident_bytes: self.0.resident.load(Ordering::Acquire),
            high_water_bytes: self.0.high_water.load(Ordering::Acquire),
        }
    }

    pub(crate) fn wake(&self) -> watch::Sender<()> {
        self.0.wake.clone()
    }

    pub(crate) fn reserve(&self, bytes: usize) -> Option<Reservation> {
        self.reserve_with_headroom(bytes, 0)
    }

    pub(crate) fn reserve_with_headroom(&self, bytes: usize, headroom: usize) -> Option<Reservation> {
        let limit = self.0.work_bytes;
        self.0
            .working
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|next| next.checked_add(headroom).is_some_and(|total| total <= limit))
            })
            .ok()?;
        let used = self.0.charged.fetch_add(bytes, Ordering::AcqRel);
        self.0.high_water.fetch_max(used + bytes, Ordering::AcqRel);
        Some(Reservation { budget: self.clone(), bytes })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryStats {
    pub charged_bytes: usize,
    pub resident_bytes: usize,
    pub high_water_bytes: usize,
}

pub(crate) struct Reservation {
    budget: Budget,
    bytes: usize,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.0.working.fetch_sub(self.bytes, Ordering::AcqRel);
        self.budget.0.charged.fetch_sub(self.bytes, Ordering::AcqRel);
        self.budget.0.wake.send_replace(());
    }
}

impl Reservation {
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn shrink_to_at_most(&mut self, max_bytes: usize) {
        let bytes = self.bytes.min(max_bytes);
        let released = self.bytes - bytes;
        self.budget.0.charged.fetch_sub(released, Ordering::AcqRel);
        self.budget.0.working.fetch_sub(released, Ordering::AcqRel);
        if released > 0 {
            self.budget.0.wake.send_replace(());
        }
        self.bytes = bytes;
    }

    pub(crate) fn resize(&mut self, bytes: usize) -> bool {
        if bytes > self.bytes {
            let Some(mut extra) = self.budget.reserve(bytes - self.bytes) else { return false };
            extra.bytes = 0;
        } else {
            self.budget.0.charged.fetch_sub(self.bytes - bytes, Ordering::AcqRel);
            self.budget.0.working.fetch_sub(self.bytes - bytes, Ordering::AcqRel);
            if bytes < self.bytes {
                self.budget.0.wake.send_replace(());
            }
        }
        self.bytes = bytes;
        true
    }

    pub(crate) fn split(&mut self, bytes: usize) -> Result<Self, ()> {
        if bytes > self.bytes {
            return Err(());
        }
        self.bytes -= bytes;
        Ok(Self { budget: self.budget.clone(), bytes })
    }
    pub(crate) fn retain(mut self, capacity: usize) -> Result<Resident, ()> {
        if capacity > self.bytes {
            return Err(());
        }
        self.budget.0.working.fetch_sub(capacity, Ordering::AcqRel);
        self.bytes -= capacity;
        self.budget.0.resident.fetch_add(capacity, Ordering::AcqRel);
        Ok(Resident { budget: self.budget.clone(), bytes: capacity })
    }

    pub(crate) fn finish(self, bytes: Vec<u8>) -> Result<Blob, ()> {
        let reservation = self.retain(bytes.capacity())?;
        Ok(Blob(Arc::new(BlobInner { bytes, _reservation: reservation })))
    }
}

pub(crate) struct Resident {
    budget: Budget,
    bytes: usize,
}

impl Drop for Resident {
    fn drop(&mut self) {
        self.budget.0.resident.fetch_sub(self.bytes, Ordering::AcqRel);
        self.budget.0.charged.fetch_sub(self.bytes, Ordering::AcqRel);
        self.budget.0.wake.send_replace(());
    }
}

struct BlobInner {
    bytes: Vec<u8>,
    _reservation: Resident,
}

#[derive(Clone)]
pub(crate) struct Blob(Arc<BlobInner>);

impl Deref for Blob {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0.bytes
    }
}

impl fmt::Debug for Blob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Blob").field("bytes", &self.len()).finish()
    }
}
