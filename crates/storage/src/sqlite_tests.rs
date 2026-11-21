use std::{
    assert_matches, env,
    error::Error,
    fs, io,
    io::Write,
    process::{Command, Stdio},
    sync::mpsc,
};

use futures::executor::block_on;
use rusqlite::{Connection, Transaction};
use tempfile::tempdir;
use test_support::process::TestProcess;

use super::*;

fn migrate(connection: &mut Connection) -> Result<(), MigrationError> {
    connection.execute_batch("CREATE TABLE IF NOT EXISTS values_table (value INTEGER PRIMARY KEY)")?;
    Ok(())
}

fn versioned_migration(version: i64) -> impl Fn(&mut Connection) -> Result<(), MigrationError> + Send + Sync + 'static {
    move |connection| {
        let current: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if current == 0 {
            connection.execute_batch("CREATE TABLE versioned (value INTEGER PRIMARY KEY)")?;
            connection.pragma_update(None, "user_version", version)?;
            Ok(())
        } else if current == version {
            Ok(())
        } else {
            Err(MigrationError::SchemaMismatch)
        }
    }
}

fn insert(value: i64) -> Result<SqliteBatch, SqliteError> {
    SqliteBatch::new(1, 8, move |transaction| {
        transaction.execute("INSERT INTO values_table(value) VALUES (?1)", [value])?;
        Ok(())
    })
}

#[test]
fn batch_bounds_are_enforced() {
    assert_matches!(
        SqliteBatch::new(MAX_BATCH_ROWS + 1, 1, |_: &Transaction<'_>| Ok(())),
        Err(SqliteError::BatchTooLarge { rows, bytes: 1 }) if rows == MAX_BATCH_ROWS + 1
    );
    assert_matches!(
        SqliteBatch::new(1, MAX_BATCH_BYTES + 1, |_: &Transaction<'_>| Ok(())),
        Err(SqliteError::BatchTooLarge { rows: 1, bytes }) if bytes == MAX_BATCH_BYTES + 1
    );
}

#[test]
fn garbage_database_is_recreated() -> Result<(), Box<dyn Error>> {
    let directory = tempdir()?;
    let path = directory.path().join("inventory.sqlite");
    fs::write(&path, b"not sqlite data")?;

    let (db, status) = SqliteDb::open(&path, migrate)?;

    assert_eq!(status, OpenStatus::Recreated(RecoveryReason::Corrupt));
    let receipt = db.read(0, |transaction| transaction.query_row("SELECT COUNT(*) FROM values_table", [], |row| row.get::<_, i64>(0)))?;
    assert_eq!(block_on(receipt.wait())?, 0);
    db.shutdown()?;
    Ok(())
}

#[test]
fn removal_requires_closed_database() -> Result<(), Box<dyn Error>> {
    let directory = tempdir()?;
    let path = directory.path().join("inventory.sqlite");
    let (db, _) = SqliteDb::open(&path, migrate)?;

    assert!(!SqliteDb::remove_if_idle(&path)?);
    assert!(path.exists());

    db.shutdown()?;
    assert!(SqliteDb::remove_if_idle(&path)?);
    assert!(!path.exists());
    Ok(())
}

#[test]
fn schema_mismatch_is_recreated() -> Result<(), Box<dyn Error>> {
    let directory = tempdir()?;
    let path = directory.path().join("inventory.sqlite");
    let (first, status) = SqliteDb::open(&path, versioned_migration(1))?;
    assert_eq!(status, OpenStatus::Created);
    first.shutdown()?;

    let (second, status) = SqliteDb::open(&path, versioned_migration(2))?;

    assert_eq!(status, OpenStatus::Recreated(RecoveryReason::SchemaMismatch));
    let receipt = second.read(0, |transaction| transaction.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0)))?;
    assert_eq!(block_on(receipt.wait())?, 2);
    second.shutdown()?;
    Ok(())
}

#[test]
fn full_queue_admits_control_work() -> Result<(), Box<dyn Error>> {
    let directory = tempdir()?;
    let (db, _) = SqliteDb::open(directory.path().join("inventory.sqlite"), migrate)?;
    let (started, started_rx) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let blocker = SqliteBatch::new(1, 8, move |transaction| {
        transaction.execute("INSERT INTO values_table(value) VALUES (0)", [])?;
        started.send(()).map_err(|_| rusqlite::Error::InvalidQuery)?;
        release_rx.recv().map_err(|_| rusqlite::Error::InvalidQuery)?;
        Ok(())
    })?;
    let blocker_receipt = db.submit_batches(vec![blocker])?;
    started_rx.recv()?;

    let mut queued = Vec::new();
    for value in 1..MAX_QUEUED_BATCHES {
        queued.push(db.submit_batches(vec![insert(value as i64)?])?);
    }
    assert_matches!(db.submit_batches(vec![insert(1000)?]), Err(SqliteError::QueueFull));

    let control = SqliteBatch::new(1, 8, |transaction| {
        transaction.execute("UPDATE values_table SET value=10000 WHERE value=0", [])?;
        Ok(())
    })?;
    let control_receipt = db.submit_control_batch(control)?;
    assert_matches!(db.submit_batches(vec![insert(1001)?]), Err(SqliteError::QueueFull));

    release.send(())?;
    block_on(blocker_receipt.wait())?;
    for receipt in queued {
        block_on(receipt.wait())?;
    }
    block_on(control_receipt.wait())?;
    let read = db.read(0, |transaction| transaction.query_row("SELECT COUNT(*) FROM values_table", [], |row| row.get::<_, i64>(0)))?;
    assert_eq!(block_on(read.wait())?, MAX_QUEUED_BATCHES as i64);
    db.shutdown()?;
    Ok(())
}

#[test]
fn panic_resolves_queued_receipt() -> Result<(), Box<dyn Error>> {
    let directory = tempdir()?;
    let (db, _) = SqliteDb::open(directory.path().join("inventory.sqlite"), migrate)?;
    let control = db.control_handle();
    let (started, started_rx) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let panicking = SqliteBatch::new(1, 8, move |_transaction| {
        started.send(()).map_err(|_| rusqlite::Error::InvalidQuery)?;
        release_rx.recv().map_err(|_| rusqlite::Error::InvalidQuery)?;
        panic!("test batch panic");
    })?;
    let panicking_receipt = db.submit_batches(vec![panicking])?;
    started_rx.recv()?;

    let queued_receipt = db.submit_batches(vec![insert(1)?])?;
    release.send(())?;

    assert_matches!(block_on(panicking_receipt.wait()), Err(SqliteError::WorkerStopped));
    assert_matches!(db.shutdown(), Err(SqliteError::WorkerStopped));
    assert_matches!(futures::FutureExt::now_or_never(queued_receipt.wait()), Some(Err(SqliteError::WorkerStopped)));
    assert_matches!(control.submit_control_batch(insert(2)?), Err(SqliteError::WorkerStopped));
    Ok(())
}

#[test]
fn second_process_write_is_busy() -> Result<(), Box<dyn Error>> {
    let directory = tempdir()?;
    let path = directory.path().join("inventory.sqlite");
    let (db, _) = SqliteDb::open(&path, migrate)?;
    let mut command = Command::new(env::current_exe()?);
    command
        .arg("--exact")
        .arg("sqlite::tests::busy_process_helper")
        .arg("--nocapture")
        .env("RADIA_SQLITE_BUSY_PATH", &path)
        .stderr(Stdio::inherit());
    let mut child = TestProcess::spawn(&mut command)?;
    child.wait_for_stdout("sqlite write locked")?;

    let receipt = db.submit_batches(vec![insert(55)?])?;
    assert_matches!(block_on(receipt.wait()), Err(SqliteError::Busy));

    child.write_stdin(b"continue\n")?;
    assert!(child.wait()?.success(), "child process failed; stdout: {}", child.stdout());
    db.shutdown()?;
    Ok(())
}

#[test]
fn repair_waits_for_other_process() -> Result<(), Box<dyn Error>> {
    let directory = tempdir()?;
    let path = directory.path().join("inventory.sqlite");
    let (initial, _) = SqliteDb::open(&path, versioned_migration(1))?;
    initial.shutdown()?;
    let mut command = Command::new(env::current_exe()?);
    command
        .arg("--exact")
        .arg("sqlite::tests::maintenance_process_helper")
        .arg("--nocapture")
        .env("RADIA_SQLITE_MAINTENANCE_PATH", &path)
        .stderr(Stdio::inherit());
    let mut child = TestProcess::spawn(&mut command)?;
    child.wait_for_stdout("sqlite maintenance held")?;

    assert_matches!(SqliteDb::open(&path, versioned_migration(2)), Err(SqliteError::Busy));
    assert!(path.exists(), "failed repair removed a database another process holds open");

    child.write_stdin(b"continue\n")?;
    assert!(child.wait()?.success(), "child process failed; stdout: {}", child.stdout());
    let (repaired, status) = SqliteDb::open(&path, versioned_migration(2))?;
    assert_eq!(status, OpenStatus::Recreated(RecoveryReason::SchemaMismatch));
    repaired.shutdown()?;
    Ok(())
}

#[test]
fn connection_settings_applied_on_create_reopen_recreate_and_discard() -> Result<(), Box<dyn Error>> {
    const TEST_MAX_PAGES: i64 = 4096;
    fn migrate_with_settings(connection: &mut Connection) -> Result<(), MigrationError> {
        connection.execute_batch("CREATE TABLE IF NOT EXISTS values_table (value INTEGER PRIMARY KEY)")?;
        connection.pragma_update(None, "max_page_count", TEST_MAX_PAGES)?;
        Ok(())
    }
    fn assert_settings(db: &SqliteDb) -> Result<(), Box<dyn Error>> {
        let receipt = db.read(0, |transaction| {
            let journal_mode: String = transaction.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
            let synchronous: i64 = transaction.pragma_query_value(None, "synchronous", |row| row.get(0))?;
            let busy_timeout: i64 = transaction.pragma_query_value(None, "busy_timeout", |row| row.get(0))?;
            let max_page_count: i64 = transaction.pragma_query_value(None, "max_page_count", |row| row.get(0))?;
            Ok((journal_mode, synchronous, busy_timeout, max_page_count))
        })?;
        let (journal_mode, synchronous, busy_timeout, max_page_count) = block_on(receipt.wait())?;
        assert!(journal_mode.eq_ignore_ascii_case("wal"));
        assert_eq!(synchronous, 1);
        assert_eq!(busy_timeout, BUSY_TIMEOUT.as_millis() as i64);
        assert_eq!(max_page_count, TEST_MAX_PAGES);
        Ok(())
    }

    let directory = tempdir()?;
    let path = directory.path().join("settings.sqlite");

    let (db, status) = SqliteDb::open(&path, migrate_with_settings)?;
    assert_eq!(status, OpenStatus::Created);
    assert_settings(&db)?;
    db.shutdown()?;

    let (db, status) = SqliteDb::open(&path, migrate_with_settings)?;
    assert_eq!(status, OpenStatus::Existing);
    assert_settings(&db)?;
    db.shutdown()?;

    fs::write(&path, b"not sqlite data")?;
    let (db, status) = SqliteDb::open(&path, migrate_with_settings)?;
    assert_eq!(status, OpenStatus::Recreated(RecoveryReason::Corrupt));
    assert_settings(&db)?;

    block_on(db.discard()?.wait())?;
    assert_settings(&db)?;
    db.shutdown()?;
    Ok(())
}

#[test]
fn newer_schema_version_fails_safely_and_preserves_contents() -> Result<(), Box<dyn Error>> {
    let directory = tempdir()?;
    let path = directory.path().join("future.sqlite");
    let connection = Connection::open(&path)?;
    connection.execute_batch("CREATE TABLE future_data (value INTEGER PRIMARY KEY); INSERT INTO future_data (value) VALUES (42);")?;
    connection.pragma_update(None, "user_version", 2_i64)?;
    drop(connection);

    let result = SqliteDb::open(&path, |connection| {
        let current: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if current > 1 { Err(MigrationError::UnsupportedSchemaVersion) } else { Ok(()) }
    });
    assert_matches!(result, Err(SqliteError::UnsupportedSchemaVersion));

    let connection = Connection::open(&path)?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let value: i64 = connection.query_row("SELECT value FROM future_data", [], |row| row.get(0))?;
    assert_eq!(version, 2);
    assert_eq!(value, 42);
    Ok(())
}

#[test]
fn busy_process_helper() -> Result<(), Box<dyn Error>> {
    let Some(path) = env::var_os("RADIA_SQLITE_BUSY_PATH") else {
        return Ok(());
    };
    let path = PathBuf::from(path);
    let (db, _) = SqliteDb::open(path, migrate)?;
    let batch = SqliteBatch::new(1, 8, |transaction: &Transaction<'_>| {
        transaction.execute("INSERT INTO values_table(value) VALUES (99)", [])?;
        println!("sqlite write locked");
        io::stdout().flush().map_err(|_| rusqlite::Error::InvalidQuery)?;
        let mut input = String::new();
        io::stdin().read_line(&mut input).map_err(|_| rusqlite::Error::InvalidQuery)?;
        Ok(())
    });
    let receipt = db.submit_batches(vec![batch?])?;
    block_on(receipt.wait())?;
    db.shutdown()?;
    Ok(())
}

#[test]
fn maintenance_process_helper() -> Result<(), Box<dyn Error>> {
    let Some(path) = env::var_os("RADIA_SQLITE_MAINTENANCE_PATH") else {
        return Ok(());
    };
    let (db, _) = SqliteDb::open(path, versioned_migration(1))?;
    let batch = SqliteBatch::new(1, 8, |transaction: &Transaction<'_>| {
        transaction.execute("INSERT INTO versioned(value) VALUES (1)", [])?;
        println!("sqlite maintenance held");
        io::stdout().flush().map_err(|_| rusqlite::Error::InvalidQuery)?;
        let mut input = String::new();
        io::stdin().read_line(&mut input).map_err(|_| rusqlite::Error::InvalidQuery)?;
        Ok(())
    });
    let receipt = db.submit_batches(vec![batch?])?;
    block_on(receipt.wait())?;
    db.shutdown()?;
    Ok(())
}
