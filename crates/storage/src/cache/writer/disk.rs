use std::{
    fs, io,
    path::Path,
    time::{Duration, Instant},
};

use tempfile::NamedTempFile;
use utils::Summary;

use crate::cache::{Key, NamespaceTable, StorageError};

pub(super) const DATA_TEMP_PREFIX: &str = ".data-tmp-";
pub(super) const CLEANUP_FAILURE_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Default)]
pub(super) struct CleanupFailures {
    failed: u64,
    last_error: Option<io::Error>,
}

impl CleanupFailures {
    pub(super) fn report(self) {
        if let Some(last_error) = self.last_error {
            tracing::warn!(failed = self.failed, %last_error, "cache data cleanup failed");
        }
    }
}

pub(super) fn fail_temporary(temp: NamedTempFile, error: io::Error) -> StorageError {
    if error.kind() == io::ErrorKind::Interrupted {
        match temp.keep() {
            Ok((file, _)) => drop(file),
            Err(failure) => return StorageError::Io(failure.error),
        }
    }
    StorageError::Io(error)
}

pub(super) fn delete_data(root: &Path, namespaces: &NamespaceTable, key: &Key, failures: &mut Summary<CleanupFailures>) {
    let Some(path) = namespaces.path(root, *key) else {
        return;
    };
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => record_cleanup_failure(failures, error),
    }
    let mut directory = path.parent();
    while let Some(current) = directory {
        if current == root {
            break;
        }
        match fs::remove_dir(current) {
            Ok(()) => directory = current.parent(),
            Err(error) if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty) => break,
            Err(error) => {
                record_cleanup_failure(failures, error);
                break;
            }
        }
    }
}

fn record_cleanup_failure(failures: &mut Summary<CleanupFailures>, error: io::Error) {
    let due = failures.record(Instant::now(), |failures| {
        failures.failed = failures.failed.saturating_add(1);
        failures.last_error = Some(error);
    });
    if let Some(due) = due {
        due.report();
    }
}

pub(super) fn remove_path(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) }
}
