use std::sync::Arc;

use futures::lock;

pub(crate) type WriteLock = Arc<lock::Mutex<()>>;

pub(crate) fn write_lock() -> WriteLock {
    Arc::new(lock::Mutex::new(()))
}
