use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};

#[cfg(test)]
use network::Grid;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use storage::{
    default_cache_dir,
    sqlite::{
        MAX_BATCH_BYTES, MAX_BATCH_ROWS, MigrationError, OpenStatus, SqliteBatch, SqliteDb, SqliteError, SqliteReceipt, StorageClass,
    },
};
#[cfg(test)]
use tokio::runtime::Handle;
use uuid::Uuid;

use super::{
    CacheScope, EncodedEntry, MAX_DATABASE_PAGES, MAX_ENTRY_BATCH_ROWS, MAX_GLOBAL_PRUNE_ROWS, MAX_OBJECT_CACHE_ENTRY_BYTES,
    MAX_OVERRIDE_BATCH_ROWS, MAX_REGION_BYTES, MAX_REGION_DELETE_BATCHES, MAX_REGION_ENTRIES, MAX_REGION_OVERRIDE_BYTES,
    MAX_REGION_OVERRIDES, MAX_TOTAL_BYTES, MAX_TOTAL_ENTRIES, OverrideWrite, SCHEMA_VERSION, StoredEntry, StoredOverride, StoredScope,
};
#[cfg(test)]
use super::{CacheWriteWorker, ObjectCacheEvent, ObjectCacheSession};

const REGION_SCOPES_SCHEMA: &str = "CREATE TABLE region_scopes (
    grid_id TEXT NOT NULL,
    region_id BLOB NOT NULL CHECK(length(region_id) = 16),
    cache_id BLOB NOT NULL CHECK(length(cache_id) = 16),
    last_used INTEGER NOT NULL,
    PRIMARY KEY (grid_id, region_id, cache_id)
) WITHOUT ROWID";
const OBJECT_ENTRIES_SCHEMA: &str = "CREATE TABLE object_entries (
    grid_id TEXT NOT NULL,
    region_id BLOB NOT NULL CHECK(length(region_id) = 16),
    cache_id BLOB NOT NULL CHECK(length(cache_id) = 16),
    local_id INTEGER NOT NULL CHECK(local_id > 0 AND local_id <= 4294967295),
    crc INTEGER NOT NULL CHECK(crc >= 0 AND crc <= 4294967295),
    update_kind INTEGER NOT NULL CHECK(update_kind IN (0, 1)),
    update_flags INTEGER NOT NULL CHECK(update_flags >= 0 AND update_flags <= 4294967295),
    payload BLOB NOT NULL CHECK(length(payload) >= 64 AND length(payload) <= 10000),
    PRIMARY KEY (grid_id, region_id, cache_id, local_id)
) WITHOUT ROWID";
const PBR_OVERRIDES_SCHEMA: &str = "CREATE TABLE pbr_overrides (
    grid_id TEXT NOT NULL,
    region_id BLOB NOT NULL CHECK(length(region_id) = 16),
    cache_id BLOB NOT NULL CHECK(length(cache_id) = 16),
    local_id INTEGER NOT NULL CHECK(local_id > 0 AND local_id <= 4294967295),
    object_id BLOB NOT NULL CHECK(length(object_id) = 16),
    payload BLOB NOT NULL CHECK(length(payload) >= 1 AND length(payload) <= 7168),
    PRIMARY KEY (grid_id, region_id, cache_id, local_id)
) WITHOUT ROWID";
const REGION_SCOPES_DELETE_OVERRIDES_TRIGGER: &str = "CREATE TRIGGER region_scopes_delete_overrides AFTER DELETE ON region_scopes BEGIN
    DELETE FROM pbr_overrides WHERE grid_id = OLD.grid_id AND region_id = OLD.region_id AND cache_id = OLD.cache_id;
END";
const OBJECT_CACHE_TOTALS_SCHEMA: &str = "CREATE TABLE object_cache_totals (
    singleton INTEGER NOT NULL PRIMARY KEY CHECK(singleton = 1),
    entry_count INTEGER NOT NULL CHECK(entry_count >= 0),
    payload_bytes INTEGER NOT NULL CHECK(payload_bytes >= 0)
) WITHOUT ROWID";
const OBJECT_ENTRIES_TOTALS_INSERT_TRIGGER: &str = "CREATE TRIGGER object_entries_totals_insert AFTER INSERT ON object_entries BEGIN
    UPDATE object_cache_totals SET entry_count = entry_count + 1, payload_bytes = payload_bytes + length(NEW.payload)
    WHERE singleton = 1;
END";
const OBJECT_ENTRIES_TOTALS_DELETE_TRIGGER: &str = "CREATE TRIGGER object_entries_totals_delete AFTER DELETE ON object_entries BEGIN
    UPDATE object_cache_totals SET entry_count = entry_count - 1, payload_bytes = payload_bytes - length(OLD.payload)
    WHERE singleton = 1;
END";
const OBJECT_ENTRIES_TOTALS_UPDATE_TRIGGER: &str = "CREATE TRIGGER object_entries_totals_update AFTER UPDATE OF payload ON object_entries
WHEN length(OLD.payload) != length(NEW.payload) BEGIN
    UPDATE object_cache_totals SET payload_bytes = payload_bytes + length(NEW.payload) - length(OLD.payload)
    WHERE singleton = 1;
END";
const PBR_OVERRIDES_TOTALS_INSERT_TRIGGER: &str = "CREATE TRIGGER pbr_overrides_totals_insert AFTER INSERT ON pbr_overrides BEGIN
    UPDATE object_cache_totals SET entry_count = entry_count + 1, payload_bytes = payload_bytes + length(NEW.payload)
    WHERE singleton = 1;
END";
const PBR_OVERRIDES_TOTALS_DELETE_TRIGGER: &str = "CREATE TRIGGER pbr_overrides_totals_delete AFTER DELETE ON pbr_overrides BEGIN
    UPDATE object_cache_totals SET entry_count = entry_count - 1, payload_bytes = payload_bytes - length(OLD.payload)
    WHERE singleton = 1;
END";
const PBR_OVERRIDES_TOTALS_UPDATE_TRIGGER: &str = "CREATE TRIGGER pbr_overrides_totals_update AFTER UPDATE OF payload ON pbr_overrides
WHEN length(OLD.payload) != length(NEW.payload) BEGIN
    UPDATE object_cache_totals SET payload_bytes = payload_bytes + length(NEW.payload) - length(OLD.payload)
    WHERE singleton = 1;
END";
const SCHEMA: [(&str, &str, &str); 11] = [
    ("table", "region_scopes", REGION_SCOPES_SCHEMA),
    ("table", "object_entries", OBJECT_ENTRIES_SCHEMA),
    ("table", "pbr_overrides", PBR_OVERRIDES_SCHEMA),
    ("table", "object_cache_totals", OBJECT_CACHE_TOTALS_SCHEMA),
    ("trigger", "region_scopes_delete_overrides", REGION_SCOPES_DELETE_OVERRIDES_TRIGGER),
    ("trigger", "object_entries_totals_insert", OBJECT_ENTRIES_TOTALS_INSERT_TRIGGER),
    ("trigger", "object_entries_totals_delete", OBJECT_ENTRIES_TOTALS_DELETE_TRIGGER),
    ("trigger", "object_entries_totals_update", OBJECT_ENTRIES_TOTALS_UPDATE_TRIGGER),
    ("trigger", "pbr_overrides_totals_insert", PBR_OVERRIDES_TOTALS_INSERT_TRIGGER),
    ("trigger", "pbr_overrides_totals_delete", PBR_OVERRIDES_TOTALS_DELETE_TRIGGER),
    ("trigger", "pbr_overrides_totals_update", PBR_OVERRIDES_TOTALS_UPDATE_TRIGGER),
];

pub(super) struct PendingSubmission {
    pub(super) batches: Vec<SqliteBatch>,
    pub(super) rows: usize,
    pub(super) bytes: usize,
}
pub(super) fn open_default() -> Result<(SqliteDb, OpenStatus), String> {
    let root = default_cache_dir().map_err(|error| error.to_string())?;
    open_at(root.join("objects.sqlite")).map_err(|error| error.to_string())
}

pub(super) fn open_at(path: impl AsRef<Path>) -> Result<(SqliteDb, OpenStatus), SqliteError> {
    SqliteDb::open(path, StorageClass::Cache, migrate)
}

pub(super) fn migrate(connection: &mut Connection) -> Result<(), MigrationError> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match version {
        0 => {
            let transaction = connection.savepoint()?;
            for (_, _, sql) in SCHEMA {
                transaction.execute_batch(sql)?;
            }
            transaction.execute("INSERT INTO object_cache_totals (singleton, entry_count, payload_bytes) VALUES (1, 0, 0)", [])?;
            transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            transaction.commit()?;
        }
        SCHEMA_VERSION => validate_current_schema(connection)?,
        version if version > SCHEMA_VERSION => return Err(MigrationError::UnsupportedSchemaVersion),
        _ => return Err(MigrationError::SchemaMismatch),
    }
    connection.pragma_update(None, "max_page_count", MAX_DATABASE_PAGES)?;
    Ok(())
}

fn validate_current_schema(connection: &Connection) -> Result<(), MigrationError> {
    let mut statement = connection.prepare("SELECT type, name, sql FROM sqlite_master WHERE type IN ('table', 'trigger')")?;
    let objects = statement
        .query_map([], |row| Ok(((row.get::<_, String>(0)?, row.get::<_, String>(1)?), row.get::<_, Option<String>>(2)?)))?
        .collect::<Result<HashMap<_, _>, _>>()?;
    if SCHEMA.iter().any(|(kind, name, expected)| {
        objects
            .get(&(kind.to_string(), name.to_string()))
            .and_then(Option::as_deref)
            .is_none_or(|actual| normalize_schema_sql(actual) != normalize_schema_sql(expected))
    }) {
        return Err(MigrationError::SchemaMismatch);
    }
    drop(statement);

    let actual: (i64, i64) = connection.query_row(
        "SELECT COUNT(*), COALESCE(SUM(length(payload)), 0) FROM (
            SELECT payload FROM object_entries UNION ALL SELECT payload FROM pbr_overrides
         )",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let totals_rows: i64 = connection.query_row("SELECT COUNT(*) FROM object_cache_totals", [], |row| row.get(0))?;
    let stored: Option<(i64, i64)> = connection
        .query_row("SELECT entry_count, payload_bytes FROM object_cache_totals WHERE singleton = 1", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()?;
    if totals_rows != 1 || stored != Some(actual) {
        return Err(MigrationError::SchemaMismatch);
    }
    Ok(())
}

fn normalize_schema_sql(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ").trim_end_matches(';').to_owned()
}

#[cfg(test)]
pub(super) fn open_for_test(path: impl AsRef<Path>, grid: Grid) -> ObjectCacheSession {
    let (db, status) = open_at(path).expect("open isolated object cache database");
    let mut cache = ObjectCacheSession::new(grid);
    let db = Arc::new(db);
    let writer = CacheWriteWorker::start(Arc::clone(&db), Handle::current()).expect("start isolated cache writer");
    assert!(cache.handle_event(ObjectCacheEvent::Ready { db, status, writer }).is_none());
    cache
}

pub(super) fn queue_scope_load(db: &SqliteDb, scope: CacheScope) -> Result<SqliteReceipt<StoredScope>, SqliteError> {
    db.read(0, move |transaction| {
        Ok(StoredScope { entries: load_entries(transaction, &scope)?, overrides: load_overrides(transaction, &scope)? })
    })
}

fn load_entries(transaction: &Transaction<'_>, scope: &CacheScope) -> Result<HashMap<u32, StoredEntry>, rusqlite::Error> {
    let (row_count, bytes) = scope_stats(transaction, scope)?;
    if row_count > MAX_REGION_ENTRIES || bytes > MAX_REGION_BYTES {
        return Ok(HashMap::new());
    }
    let mut statement = transaction.prepare(
        "SELECT local_id, crc, update_kind, update_flags, length(payload),
                CASE WHEN length(payload) <= ?1 THEN payload ELSE NULL END
         FROM object_entries WHERE grid_id = ?2 AND region_id = ?3 AND cache_id = ?4 ORDER BY local_id",
    )?;
    let row_iter = statement.query_map(
        params![
            MAX_OBJECT_CACHE_ENTRY_BYTES as i64,
            scope.grid_id,
            scope.region_id.as_bytes().as_slice(),
            scope.cache_id.as_bytes().as_slice()
        ],
        |row| {
            Ok(StoredEntry {
                local_id: row.get(0)?,
                crc: row.get(1)?,
                update_kind: row.get(2)?,
                update_flags: row.get(3)?,
                payload_bytes: row.get(4)?,
                payload: row.get::<_, Option<Vec<u8>>>(5)?.map(Arc::from),
            })
        },
    )?;
    let mut entries = HashMap::with_capacity(row_count);
    for row in row_iter {
        let entry = row?;
        if let Ok(local_id) = u32::try_from(entry.local_id) {
            entries.insert(local_id, entry);
        }
    }
    Ok(entries)
}

fn load_overrides(transaction: &Transaction<'_>, scope: &CacheScope) -> Result<HashMap<u32, StoredOverride>, rusqlite::Error> {
    let (row_count, bytes) = scope_override_stats(transaction, scope)?;
    if row_count > MAX_REGION_OVERRIDES || bytes > MAX_REGION_OVERRIDE_BYTES {
        return Ok(HashMap::new());
    }
    let mut statement = transaction
        .prepare("SELECT local_id, object_id, payload FROM pbr_overrides WHERE grid_id = ?1 AND region_id = ?2 AND cache_id = ?3")?;
    let rows = statement
        .query_map(params![scope.grid_id, scope.region_id.as_bytes().as_slice(), scope.cache_id.as_bytes().as_slice()], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, Vec<u8>>(2)?))
        })?;
    let mut overrides = HashMap::with_capacity(row_count);
    for row in rows {
        let (local_id, object_id, payload) = row?;
        if let (Ok(local_id), Ok(object_id)) = (u32::try_from(local_id), Uuid::from_slice(&object_id)) {
            overrides.insert(local_id, StoredOverride { object_id, payload: Arc::from(payload) });
        }
    }
    Ok(overrides)
}

pub(super) fn make_write_batches(scope: CacheScope, entries: Arc<[EncodedEntry]>, last_used: i64) -> Result<Vec<SqliteBatch>, SqliteError> {
    let chunk_count = entries.len().div_ceil(MAX_ENTRY_BATCH_ROWS);
    let mut batches = Vec::with_capacity(chunk_count + 3);
    for chunk_index in 0..chunk_count {
        let start = chunk_index * MAX_ENTRY_BATCH_ROWS;
        let chunk_rows = entries.len().saturating_sub(start).min(MAX_ENTRY_BATCH_ROWS);
        let include_scope = chunk_index == 0;
        let bytes = entries
            .iter()
            .skip(start)
            .take(chunk_rows)
            .map(|entry| entry.payload.len().saturating_add(64))
            .sum::<usize>()
            .saturating_add(usize::from(include_scope) * 64);
        let rows = chunk_rows.saturating_add(usize::from(include_scope));
        let entries_for_batch = Arc::clone(&entries);
        let scope_for_batch = scope.clone();
        let batch = SqliteBatch::new(rows, bytes, move |transaction| {
            if include_scope {
                upsert_scope(transaction, &scope_for_batch, last_used)?;
            }
            write_entry_chunk(transaction, &scope_for_batch, &entries_for_batch, start, chunk_rows)
        })?;
        batches.push(batch);
    }
    let region_prune =
        SqliteBatch::new(256, 256, move |transaction| prune_scope(transaction, &scope, MAX_REGION_ENTRIES, MAX_REGION_BYTES))?;
    let global_prune_first = SqliteBatch::new(256, 256, prune_global_chunk)?;
    let global_prune_second = SqliteBatch::new(256, 256, prune_global_chunk)?;
    batches.extend([region_prune, global_prune_first, global_prune_second]);
    Ok(batches)
}

fn write_entry_chunk(
    transaction: &Transaction<'_>,
    scope: &CacheScope,
    entries: &[EncodedEntry],
    start: usize,
    count: usize,
) -> Result<(), rusqlite::Error> {
    for entry in entries.iter().skip(start).take(count) {
        transaction.execute(
            "INSERT INTO object_entries (grid_id, region_id, cache_id, local_id, crc, update_kind, update_flags, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (grid_id, region_id, cache_id, local_id) DO UPDATE SET
                crc = excluded.crc,
                update_kind = excluded.update_kind,
                update_flags = excluded.update_flags,
                payload = excluded.payload
             WHERE object_entries.crc != excluded.crc
                OR object_entries.update_kind != excluded.update_kind
                OR object_entries.update_flags != excluded.update_flags
                OR object_entries.payload != excluded.payload",
            params![
                scope.grid_id.as_str(),
                scope.region_id.as_bytes().as_slice(),
                scope.cache_id.as_bytes().as_slice(),
                i64::from(entry.local_id),
                i64::from(entry.crc),
                entry.update_kind,
                i64::from(entry.update_flags),
                entry.payload.as_ref(),
            ],
        )?;
    }
    Ok(())
}

pub(super) fn make_scope_delete_submission(scope: CacheScope) -> PendingSubmission {
    let mut batches = Vec::with_capacity(MAX_REGION_DELETE_BATCHES);
    for _ in 0..MAX_REGION_DELETE_BATCHES {
        let scope = scope.clone();
        let batch = SqliteBatch::new(256, 256, move |transaction| delete_scope_chunk(transaction, &scope))
            .expect("scope delete batch fits SQLite limits");
        batches.push(batch);
    }
    PendingSubmission { batches, rows: MAX_REGION_DELETE_BATCHES * 256, bytes: MAX_REGION_DELETE_BATCHES * 256 }
}

pub(super) fn make_delete_entries_batches(scope: CacheScope, local_ids: Arc<[u32]>) -> Vec<SqliteBatch> {
    let rows_per_batch = MAX_BATCH_ROWS.saturating_sub(1).min(MAX_BATCH_BYTES.saturating_sub(64) / 4).max(1);
    local_ids
        .chunks(rows_per_batch)
        .enumerate()
        .map(|(index, ids)| {
            let start = index * rows_per_batch;
            let count = ids.len();
            let scope = scope.clone();
            let local_ids = Arc::clone(&local_ids);
            SqliteBatch::new(count.saturating_add(1), count.saturating_mul(4).saturating_add(64), move |transaction| {
                for local_id in local_ids.iter().skip(start).take(count) {
                    transaction.execute(
                        "DELETE FROM object_entries WHERE grid_id = ?1 AND region_id = ?2 AND cache_id = ?3 AND local_id = ?4",
                        params![
                            scope.grid_id,
                            scope.region_id.as_bytes().as_slice(),
                            scope.cache_id.as_bytes().as_slice(),
                            i64::from(*local_id)
                        ],
                    )?;
                }
                remove_empty_scope(transaction, &scope)
            })
            .expect("cache entry delete batch fits SQLite limits")
        })
        .collect()
}

pub(super) fn make_access_submission(scope: CacheScope, stale: HashSet<u32>, last_used: i64) -> PendingSubmission {
    let rows = stale.len().saturating_add(1).min(256);
    let bytes = stale.len().saturating_mul(4).saturating_add(64);
    let batch = SqliteBatch::new(rows, bytes, move |transaction| {
        for local_id in stale {
            transaction.execute(
                "DELETE FROM object_entries WHERE grid_id = ?1 AND region_id = ?2 AND cache_id = ?3 AND local_id = ?4",
                params![scope.grid_id, scope.region_id.as_bytes().as_slice(), scope.cache_id.as_bytes().as_slice(), i64::from(local_id)],
            )?;
        }
        transaction.execute(
            "UPDATE region_scopes SET last_used = MAX(last_used, ?4) WHERE grid_id = ?1 AND region_id = ?2 AND cache_id = ?3",
            params![scope.grid_id, scope.region_id.as_bytes().as_slice(), scope.cache_id.as_bytes().as_slice(), last_used],
        )?;
        remove_empty_scope(transaction, &scope)
    })
    .expect("cache access batch fits SQLite limits");
    PendingSubmission { batches: vec![batch], rows, bytes }
}

pub(super) fn make_override_submission(scope: CacheScope, writes: Vec<OverrideWrite>, last_used: i64) -> PendingSubmission {
    let mut submission = PendingSubmission { batches: Vec::new(), rows: 0, bytes: 0 };
    let mut writes = writes.into_iter().peekable();
    while writes.peek().is_some() {
        let chunk = writes.by_ref().take(MAX_OVERRIDE_BATCH_ROWS).collect::<Vec<_>>();
        let rows = chunk.len().saturating_add(2);
        let bytes = chunk.iter().map(|write| write.payload_bytes().saturating_add(64)).sum::<usize>().saturating_add(128);
        let scope = scope.clone();
        let batch = SqliteBatch::new(rows, bytes, move |transaction| write_override_chunk(transaction, &scope, &chunk, last_used))
            .expect("override batch fits SQLite limits");
        submission.batches.push(batch);
        submission.rows = submission.rows.saturating_add(rows);
        submission.bytes = submission.bytes.saturating_add(bytes);
    }
    submission
}

fn write_override_chunk(
    transaction: &Transaction<'_>,
    scope: &CacheScope,
    writes: &[OverrideWrite],
    last_used: i64,
) -> Result<(), rusqlite::Error> {
    if writes.iter().any(|write| matches!(write, OverrideWrite::Upsert(..))) {
        upsert_scope(transaction, scope, last_used)?;
    }
    let (mut rows, mut bytes) = scope_override_stats(transaction, scope)?;
    for write in writes {
        let local_id = i64::from(write.local_id());
        let existing = transaction
            .query_row(
                "SELECT length(payload) FROM pbr_overrides WHERE grid_id = ?1 AND region_id = ?2 AND cache_id = ?3 AND local_id = ?4",
                params![scope.grid_id, scope.region_id.as_bytes().as_slice(), scope.cache_id.as_bytes().as_slice(), local_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .map(|length| usize::try_from(length).unwrap_or(usize::MAX));
        let rows_without = rows.saturating_sub(usize::from(existing.is_some()));
        let bytes_without = bytes.saturating_sub(existing.unwrap_or(0));
        let fits = rows_without < MAX_REGION_OVERRIDES && bytes_without.saturating_add(write.payload_bytes()) <= MAX_REGION_OVERRIDE_BYTES;
        match write {
            OverrideWrite::Upsert(_, stored) if fits => {
                transaction.execute(
                    "INSERT INTO pbr_overrides (grid_id, region_id, cache_id, local_id, object_id, payload)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT (grid_id, region_id, cache_id, local_id) DO UPDATE SET
                        object_id = excluded.object_id,
                        payload = excluded.payload
                     WHERE pbr_overrides.object_id != excluded.object_id OR pbr_overrides.payload != excluded.payload",
                    params![
                        scope.grid_id,
                        scope.region_id.as_bytes().as_slice(),
                        scope.cache_id.as_bytes().as_slice(),
                        local_id,
                        stored.object_id.as_bytes().as_slice(),
                        stored.payload.as_ref(),
                    ],
                )?;
                rows = rows_without.saturating_add(1);
                bytes = bytes_without.saturating_add(stored.payload.len());
            }
            OverrideWrite::Upsert(..) | OverrideWrite::Delete(_) => {
                if existing.is_some() {
                    transaction.execute(
                        "DELETE FROM pbr_overrides WHERE grid_id = ?1 AND region_id = ?2 AND cache_id = ?3 AND local_id = ?4",
                        params![scope.grid_id, scope.region_id.as_bytes().as_slice(), scope.cache_id.as_bytes().as_slice(), local_id],
                    )?;
                }
                rows = rows_without;
                bytes = bytes_without;
            }
        }
    }
    remove_empty_scope(transaction, scope)
}

fn upsert_scope(transaction: &Transaction<'_>, scope: &CacheScope, last_used: i64) -> Result<(), rusqlite::Error> {
    transaction.execute(
        "INSERT INTO region_scopes (grid_id, region_id, cache_id, last_used) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (grid_id, region_id, cache_id) DO UPDATE SET last_used = MAX(last_used, excluded.last_used)",
        params![scope.grid_id, scope.region_id.as_bytes().as_slice(), scope.cache_id.as_bytes().as_slice(), last_used],
    )?;
    Ok(())
}

fn prune_scope(transaction: &Transaction<'_>, scope: &CacheScope, max_rows: usize, max_bytes: usize) -> Result<(), rusqlite::Error> {
    let (rows, bytes) = scope_stats(transaction, scope)?;
    if rows <= max_rows && bytes <= max_bytes {
        return Ok(());
    }
    let mut statement = transaction.prepare(
        "SELECT local_id FROM object_entries WHERE grid_id = ?1 AND region_id = ?2 AND cache_id = ?3
         ORDER BY length(payload) DESC, local_id LIMIT 255",
    )?;
    let local_ids = statement
        .query_map(params![scope.grid_id, scope.region_id.as_bytes().as_slice(), scope.cache_id.as_bytes().as_slice()], |row| {
            row.get::<_, i64>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for local_id in local_ids {
        transaction.execute(
            "DELETE FROM object_entries WHERE grid_id = ?1 AND region_id = ?2 AND cache_id = ?3 AND local_id = ?4",
            params![scope.grid_id, scope.region_id.as_bytes().as_slice(), scope.cache_id.as_bytes().as_slice(), local_id],
        )?;
    }
    remove_empty_scope(transaction, scope)?;
    Ok(())
}

fn prune_global_chunk(transaction: &Transaction<'_>) -> Result<(), rusqlite::Error> {
    let (rows, bytes) = global_stats(transaction)?;
    let rows = i64::try_from(rows).unwrap_or(i64::MAX);
    let bytes = i64::try_from(bytes).unwrap_or(i64::MAX);
    if rows <= MAX_TOTAL_ENTRIES as i64 && bytes <= MAX_TOTAL_BYTES as i64 {
        return Ok(());
    }
    let mut remaining_rows = usize::try_from(rows).unwrap_or(usize::MAX);
    let mut remaining_bytes = usize::try_from(bytes).unwrap_or(usize::MAX);
    let mut deleted = 0;
    while (remaining_rows > MAX_TOTAL_ENTRIES || remaining_bytes > MAX_TOTAL_BYTES) && deleted < MAX_GLOBAL_PRUNE_ROWS {
        let oldest = transaction
            .query_row(
                "SELECT grid_id, region_id, cache_id FROM region_scopes
                 WHERE EXISTS (
                    SELECT 1 FROM object_entries e
                    WHERE e.grid_id = region_scopes.grid_id AND e.region_id = region_scopes.region_id AND e.cache_id = region_scopes.cache_id
                 ) OR EXISTS (
                    SELECT 1 FROM pbr_overrides o
                    WHERE o.grid_id = region_scopes.grid_id AND o.region_id = region_scopes.region_id AND o.cache_id = region_scopes.cache_id
                 )
                 ORDER BY last_used, grid_id, region_id, cache_id LIMIT 1",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, Vec<u8>>(2)?)),
            )
            .optional()?;
        let Some((grid_id, region_id, cache_id)) = oldest else { break };
        let limit = i64::try_from(MAX_GLOBAL_PRUNE_ROWS - deleted).unwrap_or(i64::MAX);
        let mut statement = transaction.prepare(
            "SELECT is_override, local_id, bytes FROM (
                SELECT 0 AS is_override, local_id, length(payload) AS bytes FROM object_entries
                WHERE grid_id = ?1 AND region_id = ?2 AND cache_id = ?3
                UNION ALL
                SELECT 1, local_id, length(payload) FROM pbr_overrides
                WHERE grid_id = ?1 AND region_id = ?2 AND cache_id = ?3
             )
             ORDER BY is_override, bytes DESC, local_id LIMIT ?4",
        )?;
        let rows = statement
            .query_map(params![grid_id, region_id, cache_id, limit], |row| {
                Ok((row.get::<_, bool>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        if rows.is_empty() {
            break;
        }
        for (is_override, local_id, payload_bytes) in &rows {
            let sql = if *is_override {
                "DELETE FROM pbr_overrides WHERE grid_id = ?1 AND region_id = ?2 AND cache_id = ?3 AND local_id = ?4"
            } else {
                "DELETE FROM object_entries WHERE grid_id = ?1 AND region_id = ?2 AND cache_id = ?3 AND local_id = ?4"
            };
            transaction.execute(sql, params![grid_id, region_id, cache_id, local_id])?;
            remaining_rows = remaining_rows.saturating_sub(1);
            remaining_bytes = remaining_bytes.saturating_sub(usize::try_from(*payload_bytes).unwrap_or(usize::MAX));
        }
        deleted += rows.len();
        remove_empty_scope_key(transaction, &grid_id, &region_id, &cache_id)?;
    }
    Ok(())
}

fn global_stats(transaction: &Transaction<'_>) -> Result<(usize, usize), rusqlite::Error> {
    let (rows, bytes): (i64, i64) =
        transaction.query_row("SELECT entry_count, payload_bytes FROM object_cache_totals WHERE singleton = 1", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    Ok((usize::try_from(rows).unwrap_or(usize::MAX), usize::try_from(bytes).unwrap_or(usize::MAX)))
}

fn delete_scope_chunk(transaction: &Transaction<'_>, scope: &CacheScope) -> Result<(), rusqlite::Error> {
    let mut statement = transaction.prepare(
        "SELECT local_id FROM object_entries WHERE grid_id = ?1 AND region_id = ?2 AND cache_id = ?3 ORDER BY local_id LIMIT 255",
    )?;
    let local_ids = statement
        .query_map(params![scope.grid_id, scope.region_id.as_bytes().as_slice(), scope.cache_id.as_bytes().as_slice()], |row| {
            row.get::<_, i64>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for local_id in &local_ids {
        transaction.execute(
            "DELETE FROM object_entries WHERE grid_id = ?1 AND region_id = ?2 AND cache_id = ?3 AND local_id = ?4",
            params![scope.grid_id, scope.region_id.as_bytes().as_slice(), scope.cache_id.as_bytes().as_slice(), local_id],
        )?;
    }
    if local_ids.len() < 255 {
        transaction.execute(
            "DELETE FROM region_scopes WHERE grid_id = ?1 AND region_id = ?2 AND cache_id = ?3",
            params![scope.grid_id, scope.region_id.as_bytes().as_slice(), scope.cache_id.as_bytes().as_slice()],
        )?;
    }
    Ok(())
}

fn remove_empty_scope(transaction: &Transaction<'_>, scope: &CacheScope) -> Result<(), rusqlite::Error> {
    remove_empty_scope_key(transaction, &scope.grid_id, scope.region_id.as_bytes(), scope.cache_id.as_bytes())
}

fn remove_empty_scope_key(transaction: &Transaction<'_>, grid_id: &str, region_id: &[u8], cache_id: &[u8]) -> Result<(), rusqlite::Error> {
    transaction.execute(
        "DELETE FROM region_scopes WHERE grid_id = ?1 AND region_id = ?2 AND cache_id = ?3
         AND NOT EXISTS (
            SELECT 1 FROM object_entries e
            WHERE e.grid_id = region_scopes.grid_id AND e.region_id = region_scopes.region_id AND e.cache_id = region_scopes.cache_id
         )
         AND NOT EXISTS (
            SELECT 1 FROM pbr_overrides o
            WHERE o.grid_id = region_scopes.grid_id AND o.region_id = region_scopes.region_id AND o.cache_id = region_scopes.cache_id
         )",
        params![grid_id, region_id, cache_id],
    )?;
    Ok(())
}

fn scope_override_stats(transaction: &Transaction<'_>, scope: &CacheScope) -> Result<(usize, usize), rusqlite::Error> {
    let (rows, bytes): (i64, i64) = transaction.query_row(
        "SELECT COUNT(*), COALESCE(SUM(length(payload)), 0) FROM pbr_overrides
         WHERE grid_id = ?1 AND region_id = ?2 AND cache_id = ?3",
        params![scope.grid_id, scope.region_id.as_bytes().as_slice(), scope.cache_id.as_bytes().as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok((usize::try_from(rows).unwrap_or(usize::MAX), usize::try_from(bytes).unwrap_or(usize::MAX)))
}

fn scope_stats(transaction: &Transaction<'_>, scope: &CacheScope) -> Result<(usize, usize), rusqlite::Error> {
    let (rows, bytes): (i64, i64) = transaction.query_row(
        "SELECT COUNT(*), COALESCE(SUM(length(payload)), 0) FROM object_entries
         WHERE grid_id = ?1 AND region_id = ?2 AND cache_id = ?3",
        params![scope.grid_id, scope.region_id.as_bytes().as_slice(), scope.cache_id.as_bytes().as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok((usize::try_from(rows).unwrap_or(usize::MAX), usize::try_from(bytes).unwrap_or(usize::MAX)))
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use storage::sqlite::RecoveryReason;

    use super::*;
    use crate::session::object_cache::test_support::{
        CACHE_ID, REGION_ID, SIMULATOR, cache_totals, cached_references, fixture_message, flush, open_cache, override_rows, vectors,
    };
    #[test]
    fn damaged_schema_recreates_database() {
        for (case, foreign_version, damage) in [
            ("version -1", Some(-1), ""),
            ("missing totals trigger", None, "DROP TRIGGER object_entries_totals_insert;"),
            (
                "no-op totals trigger",
                None,
                "DROP TRIGGER object_entries_totals_insert;
                 CREATE TRIGGER object_entries_totals_insert AFTER INSERT ON object_entries BEGIN SELECT 1; END;",
            ),
            ("stale totals", None, "UPDATE object_cache_totals SET entry_count = 1, payload_bytes = 64 WHERE singleton = 1;"),
        ] {
            let directory = tempfile::tempdir().expect("create isolated cache directory");
            let path = directory.path().join("objects.sqlite");
            if let Some(version) = foreign_version {
                let connection = Connection::open(&path).expect("create isolated SQLite database");
                connection.execute_batch("CREATE TABLE object_entries (local_id INTEGER);").expect("create foreign schema");
                connection.pragma_update(None, "user_version", version).expect("set foreign schema version");
            } else {
                let (db, _) = open_at(&path).expect("create current cache schema");
                db.shutdown().expect("close initial database");
                let (db, status) = open_at(&path).expect("reopen current cache schema");
                assert_eq!(status, OpenStatus::Existing, "{case}");
                db.shutdown().expect("close reopened database");
                Connection::open(&path).expect("open current cache schema").execute_batch(damage).expect("damage current schema");
            }

            let (db, status) = open_at(&path).expect("recreate damaged database");

            assert_matches!(status, OpenStatus::Recreated(RecoveryReason::SchemaMismatch), "{case}");
            db.shutdown().expect("close recreated database");
            let connection = Connection::open(&path).expect("reopen recreated database");
            let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0)).expect("read schema version");
            assert_eq!(version, SCHEMA_VERSION, "{case}");
        }
    }

    #[test]
    fn newer_schema_preserves_contents() {
        for future_version in [SCHEMA_VERSION + 1, 7] {
            let directory = tempfile::tempdir().expect("create isolated cache directory");
            let path = directory.path().join("objects.sqlite");
            let connection = Connection::open(&path).expect("create isolated SQLite database");
            connection
                .execute_batch(
                    "CREATE TABLE future_entries (local_id INTEGER PRIMARY KEY); INSERT INTO future_entries (local_id) VALUES (42);",
                )
                .expect("create future schema");
            connection.pragma_update(None, "user_version", future_version).expect("set future schema version");
            drop(connection);

            assert_matches!(open_at(&path), Err(SqliteError::UnsupportedSchemaVersion));

            let connection = Connection::open(&path).expect("reopen preserved database");
            let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0)).expect("read schema version");
            let local_id: i64 = connection.query_row("SELECT local_id FROM future_entries", [], |row| row.get(0)).expect("read future row");
            assert_eq!(version, future_version);
            assert_eq!(local_id, 42);
        }
    }

    #[tokio::test]
    async fn override_writes_respect_scope_limits() {
        let directory = tempfile::tempdir().expect("create isolated cache directory");
        let mut cache = open_cache(directory.path().join("objects.sqlite"));
        let scope = cache.identities.get(&SIMULATOR).expect("simulator has a cache scope").clone();
        let upserts = |count: u32, bytes: usize| {
            (1..=count)
                .map(|local_id| {
                    let object_id = Uuid::from_u128(u128::from(local_id));
                    OverrideWrite::Upsert(local_id, StoredOverride { object_id, payload: Arc::from(vec![b'x'; bytes]) })
                })
                .collect::<Vec<_>>()
        };

        let rows = u32::try_from(MAX_REGION_OVERRIDES).expect("override row limit fits u32") + 1;
        assert!(cache.enqueue(make_override_submission(scope.clone(), upserts(rows, 8), 1)));
        flush(&mut cache).await;
        assert_eq!(override_rows(cache.db.as_ref().expect("cache database is open")).await.len(), MAX_REGION_OVERRIDES);

        assert!(cache.enqueue(make_override_submission(scope, upserts(200, 7_168), 2)));
        flush(&mut cache).await;
        let (rows, bytes, total_rows, total_bytes) = cache_totals(cache.db.as_ref().expect("cache database is open")).await;
        assert!(usize::try_from(rows).expect("row count fits usize") <= MAX_REGION_OVERRIDES);
        assert!(usize::try_from(bytes).expect("byte count fits usize") <= MAX_REGION_OVERRIDE_BYTES);
        assert!(bytes > i64::try_from(MAX_REGION_OVERRIDE_BYTES).expect("byte limit fits i64") - 7_168 - 8 * 2_048);
        assert_eq!((rows, bytes), (total_rows, total_bytes));
        cache.close().await;
    }

    #[tokio::test]
    async fn totals_track_updates_and_deletes() {
        let directory = tempfile::tempdir().expect("create isolated cache directory");
        let mut cache = open_cache(directory.path().join("objects.sqlite"));
        let full = fixture_message(vectors::OBJECT_UPDATE);
        cache.record_update(SIMULATOR, full.clone());
        flush(&mut cache).await;

        let db = cache.db.as_ref().expect("cache database is open");
        let before = cache_totals(db).await;
        assert!(before.0 > 1);
        assert_eq!(before.0, before.2);
        assert_eq!(before.1, before.3);

        let (_, references) = cached_references(&full);
        let updated_local_id = references[0].local_id;
        let deleted_local_id = references[1].local_id;
        let update = SqliteBatch::new(1, 65, move |transaction| {
            transaction.execute(
                "UPDATE object_entries SET payload = zeroblob(65)
             WHERE grid_id = ?1 AND region_id = ?2 AND cache_id = ?3 AND local_id = ?4",
                params![Grid::Agni.id(), REGION_ID.as_bytes().as_slice(), CACHE_ID.as_bytes().as_slice(), i64::from(updated_local_id)],
            )?;
            transaction.execute(
                "DELETE FROM object_entries
             WHERE grid_id = ?1 AND region_id = ?2 AND cache_id = ?3 AND local_id = ?4",
                params![Grid::Agni.id(), REGION_ID.as_bytes().as_slice(), CACHE_ID.as_bytes().as_slice(), i64::from(deleted_local_id)],
            )?;
            Ok(())
        })
        .expect("stats trigger batch fits SQLite limits");
        cache.db.as_ref().expect("cache database is open").submit_batches(vec![update]).expect("queue stats trigger batch");
        flush(&mut cache).await;
        let after = cache_totals(cache.db.as_ref().expect("cache database is open")).await;
        assert_eq!(after.0, before.0 - 1);
        assert_ne!(after.1, before.1);
        assert_eq!(after.0, after.2);
        assert_eq!(after.1, after.3);
        cache.close().await;
    }
}
