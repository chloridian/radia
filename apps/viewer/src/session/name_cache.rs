use std::{
    collections::HashMap,
    error::Error,
    fmt::{self, Display, Formatter},
    io,
    path::Path,
};

use network::{AgentSession, Grid};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use storage::{
    default_cache_dir,
    sqlite::{
        MAX_BATCH_BYTES, MAX_BATCH_ROWS, MigrationError, OpenStatus, SqliteBatch, SqliteDb, SqliteError, SqliteReceipt, StorageClass,
    },
};
use uuid::Uuid;

const SCHEMA_VERSION: i64 = 1;
const SQLITE_PAGE_SIZE: i64 = 4096;
const MAX_DATABASE_BYTES: usize = 32 * 1024 * 1024;
const MAX_DATABASE_PAGES: i64 = (MAX_DATABASE_BYTES as i64) / SQLITE_PAGE_SIZE;
pub(crate) const MAX_NAME_BYTES: usize = 256;
const MAX_AVATAR_ENTRIES_PER_GRID: usize = 50_000;
const MAX_GROUP_ENTRIES_PER_GRID: usize = 10_000;
const MAX_VERIFIED_LOGINS_PER_GRID: usize = 64;
const MAX_UPSERT_BATCH_ROWS: usize = 250;
const AVATAR_NAMES_SCHEMA: &str = "CREATE TABLE avatar_names (
    grid_id TEXT NOT NULL,
    agent_id BLOB NOT NULL CHECK(length(agent_id) = 16),
    username TEXT NOT NULL,
    first_name TEXT NOT NULL,
    last_name TEXT NOT NULL,
    display_name TEXT,
    is_default_display_name INTEGER NOT NULL CHECK(is_default_display_name IN (0, 1)),
    next_update_ms INTEGER NOT NULL CHECK(next_update_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= 0),
    PRIMARY KEY (grid_id, agent_id)
) WITHOUT ROWID";
const GROUP_NAMES_SCHEMA: &str = "CREATE TABLE group_names (
    grid_id TEXT NOT NULL,
    group_id BLOB NOT NULL CHECK(length(group_id) = 16),
    name TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= 0),
    PRIMARY KEY (grid_id, group_id)
) WITHOUT ROWID";
const VERIFIED_LOGINS_SCHEMA: &str = "CREATE TABLE verified_logins (
    grid_id TEXT NOT NULL,
    normalized_username TEXT NOT NULL,
    agent_id BLOB NOT NULL CHECK(length(agent_id) = 16),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= 0),
    PRIMARY KEY (grid_id, normalized_username)
) WITHOUT ROWID";
const SCHEMA: [(&str, &str, &str); 3] = [
    ("table", "avatar_names", AVATAR_NAMES_SCHEMA),
    ("table", "group_names", GROUP_NAMES_SCHEMA),
    ("table", "verified_logins", VERIFIED_LOGINS_SCHEMA),
];

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct AvatarId(Uuid);

impl AvatarId {
    pub(crate) fn new(id: Uuid) -> Option<Self> {
        (!id.is_nil()).then_some(Self(id))
    }

    pub(crate) const fn get(self) -> Uuid {
        self.0
    }

    pub(crate) const fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct GroupId(Uuid);

impl GroupId {
    pub(crate) fn new(id: Uuid) -> Option<Self> {
        (!id.is_nil()).then_some(Self(id))
    }

    pub(crate) const fn get(self) -> Uuid {
        self.0
    }

    pub(crate) const fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CachedAvatarName {
    pub(crate) agent_id: AvatarId,
    pub(crate) username: String,
    pub(crate) first_name: String,
    pub(crate) last_name: String,
    pub(crate) display_name: Option<String>,
    pub(crate) is_default_display_name: bool,
    pub(crate) next_update_ms: u64,
    pub(crate) updated_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CachedGroupName {
    pub(crate) group_id: GroupId,
    pub(crate) name: String,
    pub(crate) updated_at_ms: u64,
}

#[derive(Debug)]
pub(crate) enum NameCacheError {
    InvalidInput(&'static str),
    Storage(SqliteError),
}

impl Display for NameCacheError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(reason) => write!(formatter, "invalid name cache entry: {reason}"),
            Self::Storage(error) => Display::fmt(error, formatter),
        }
    }
}

impl Error for NameCacheError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidInput(_) => None,
            Self::Storage(error) => Some(error),
        }
    }
}

impl From<SqliteError> for NameCacheError {
    fn from(error: SqliteError) -> Self {
        Self::Storage(error)
    }
}

#[derive(Debug)]
pub(crate) struct NameCache {
    db: SqliteDb,
}

impl NameCache {
    pub(crate) fn open_default() -> Result<(Self, OpenStatus), NameCacheError> {
        let root = default_cache_dir().map_err(|error| SqliteError::Unavailable(io::Error::other(error.to_string())))?;
        Self::open(root.join("names.sqlite"))
    }

    pub(crate) fn open(path: impl AsRef<Path>) -> Result<(Self, OpenStatus), NameCacheError> {
        let (db, status) = SqliteDb::open(path, StorageClass::Cache, migrate)?;
        Ok((Self { db }, status))
    }

    pub(crate) fn shutdown(self) -> Result<(), NameCacheError> {
        self.db.shutdown().map_err(NameCacheError::from)
    }

    pub(crate) fn upsert_avatars(&self, grid: Grid, entries: &[CachedAvatarName]) -> Result<SqliteReceipt<()>, NameCacheError> {
        self.upsert_avatars_with_limit(grid, entries, MAX_AVATAR_ENTRIES_PER_GRID)
    }

    fn upsert_avatars_with_limit(
        &self,
        grid: Grid,
        entries: &[CachedAvatarName],
        max_entries: usize,
    ) -> Result<SqliteReceipt<()>, NameCacheError> {
        let grid_id = grid.id();
        let mut validated = Vec::with_capacity(entries.len());
        for entry in entries {
            validate_avatar_entry(entry)?;
            validated.push(entry.clone());
        }
        let mut batches = Vec::new();
        for chunk in validated.chunks(MAX_UPSERT_BATCH_ROWS) {
            let chunk_vec = chunk.to_vec();
            let bytes = chunk_vec.iter().map(avatar_entry_bytes).sum::<usize>().min(MAX_BATCH_BYTES);
            let rows = chunk_vec.len().min(MAX_BATCH_ROWS);
            let batch = SqliteBatch::new(rows, bytes, move |transaction| {
                for entry in &chunk_vec {
                    upsert_avatar_row(transaction, grid_id, entry)?;
                }
                Ok(())
            })?;
            batches.push(batch);
        }
        let prune_batch = SqliteBatch::new(1, 64, move |transaction| prune_avatars_for_grid(transaction, grid_id, max_entries))?;
        batches.push(prune_batch);
        self.db.submit_batches(batches).map_err(NameCacheError::from)
    }

    pub(crate) fn upsert_groups(&self, grid: Grid, entries: &[CachedGroupName]) -> Result<SqliteReceipt<()>, NameCacheError> {
        self.upsert_groups_with_limit(grid, entries, MAX_GROUP_ENTRIES_PER_GRID)
    }

    fn upsert_groups_with_limit(
        &self,
        grid: Grid,
        entries: &[CachedGroupName],
        max_entries: usize,
    ) -> Result<SqliteReceipt<()>, NameCacheError> {
        let grid_id = grid.id();
        let mut validated = Vec::with_capacity(entries.len());
        for entry in entries {
            validate_group_entry(entry)?;
            validated.push(entry.clone());
        }
        let mut batches = Vec::new();
        for chunk in validated.chunks(MAX_UPSERT_BATCH_ROWS) {
            let chunk_vec = chunk.to_vec();
            let bytes = chunk_vec.iter().map(group_entry_bytes).sum::<usize>().min(MAX_BATCH_BYTES);
            let rows = chunk_vec.len().min(MAX_BATCH_ROWS);
            let batch = SqliteBatch::new(rows, bytes, move |transaction| {
                for entry in &chunk_vec {
                    upsert_group_row(transaction, grid_id, entry)?;
                }
                Ok(())
            })?;
            batches.push(batch);
        }
        let prune_batch = SqliteBatch::new(1, 64, move |transaction| prune_groups_for_grid(transaction, grid_id, max_entries))?;
        batches.push(prune_batch);
        self.db.submit_batches(batches).map_err(NameCacheError::from)
    }

    pub(crate) fn record_verified_login(
        &self,
        grid: Grid,
        entered_username: &str,
        session: &AgentSession,
        now_ms: u64,
    ) -> Result<SqliteReceipt<()>, NameCacheError> {
        self.record_verified_login_fields(
            grid,
            entered_username,
            session.agent_id,
            &session.first_name,
            &session.last_name,
            now_ms,
            MAX_VERIFIED_LOGINS_PER_GRID,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn record_verified_login_fields(
        &self,
        grid: Grid,
        entered_username: &str,
        raw_agent_id: Uuid,
        first_name: &str,
        last_name: &str,
        now_ms: u64,
        max_verified: usize,
    ) -> Result<SqliteReceipt<()>, NameCacheError> {
        let agent_id = AvatarId::new(raw_agent_id).ok_or(NameCacheError::InvalidInput("nil agent id"))?;
        let normalized_username = normalize_username(entered_username).ok_or(NameCacheError::InvalidInput("invalid login username"))?;
        if !validate_name(first_name) || !validate_name(last_name) {
            return Err(NameCacheError::InvalidInput("invalid session avatar name"));
        }
        let canonical_username =
            canonical_session_username(first_name, last_name).ok_or(NameCacheError::InvalidInput("invalid canonical username"))?;
        let updated_at_ms = validate_timestamp(now_ms)?;
        let avatar_entry = CachedAvatarName {
            agent_id,
            username: canonical_username,
            first_name: first_name.trim().to_owned(),
            last_name: last_name.trim().to_owned(),
            display_name: None,
            is_default_display_name: false,
            next_update_ms: 0,
            updated_at_ms: now_ms,
        };
        validate_avatar_entry(&avatar_entry)?;
        let grid_id = grid.id();
        let bytes = avatar_entry_bytes(&avatar_entry).saturating_add(normalized_username.len()).saturating_add(64).min(MAX_BATCH_BYTES);
        let batch = SqliteBatch::new(3, bytes, move |transaction| {
            transaction.execute(
                "INSERT INTO verified_logins (grid_id, normalized_username, agent_id, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (grid_id, normalized_username) DO UPDATE SET
                    agent_id = excluded.agent_id,
                    updated_at_ms = excluded.updated_at_ms",
                params![grid_id, normalized_username, agent_id.as_bytes().as_slice(), updated_at_ms],
            )?;
            upsert_avatar_row(transaction, grid_id, &avatar_entry)?;
            prune_verified_logins_for_grid(transaction, grid_id, max_verified)?;
            prune_avatars_for_grid(transaction, grid_id, MAX_AVATAR_ENTRIES_PER_GRID)
        })?;
        self.db.submit_batches(vec![batch]).map_err(NameCacheError::from)
    }

    pub(crate) fn load_avatars(&self, grid: Grid, ids: &[AvatarId]) -> Result<SqliteReceipt<Vec<CachedAvatarName>>, NameCacheError> {
        let grid_id = grid.id();
        let ids = ids.to_vec();
        self.db
            .read(0, move |transaction| {
                let mut statement = transaction.prepare(
                    "SELECT agent_id, username, first_name, last_name, display_name, is_default_display_name, next_update_ms, updated_at_ms
                     FROM avatar_names WHERE grid_id = ?1 AND agent_id = ?2",
                )?;
                let mut results = Vec::with_capacity(ids.len());
                for id in ids {
                    let row = statement
                        .query_row(params![grid_id, id.as_bytes().as_slice()], |row| {
                            Ok((
                                row.get::<_, Vec<u8>>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, Option<String>>(4)?,
                                row.get::<_, i64>(5)?,
                                row.get::<_, i64>(6)?,
                                row.get::<_, i64>(7)?,
                            ))
                        })
                        .optional()?;
                    if let Some((agent_bytes, username, first_name, last_name, display_name, is_default, next_update_ms, updated_at_ms)) =
                        row
                        && let Some(entry) = decode_avatar_row(
                            &agent_bytes,
                            username,
                            first_name,
                            last_name,
                            display_name,
                            is_default,
                            next_update_ms,
                            updated_at_ms,
                        )
                    {
                        results.push(entry);
                    }
                }
                Ok(results)
            })
            .map_err(NameCacheError::from)
    }

    pub(crate) fn load_groups(&self, grid: Grid, ids: &[GroupId]) -> Result<SqliteReceipt<Vec<CachedGroupName>>, NameCacheError> {
        let grid_id = grid.id();
        let ids = ids.to_vec();
        self.db
            .read(0, move |transaction| {
                let mut statement =
                    transaction.prepare("SELECT group_id, name, updated_at_ms FROM group_names WHERE grid_id = ?1 AND group_id = ?2")?;
                let mut results = Vec::with_capacity(ids.len());
                for id in ids {
                    let row = statement
                        .query_row(params![grid_id, id.as_bytes().as_slice()], |row| {
                            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
                        })
                        .optional()?;
                    if let Some((group_bytes, name, updated_at_ms)) = row
                        && let Some(entry) = decode_group_row(&group_bytes, name, updated_at_ms)
                    {
                        results.push(entry);
                    }
                }
                Ok(results)
            })
            .map_err(NameCacheError::from)
    }

    pub(crate) fn seed_verified_logins(
        &self,
        entries: &[(Grid, String, AvatarId)],
        now_ms: u64,
    ) -> Result<SqliteReceipt<()>, NameCacheError> {
        let updated_at_ms = validate_timestamp(now_ms)?;
        let mut candidates: HashMap<(&'static str, String), Option<AvatarId>> = HashMap::new();
        for (grid, username, agent_id) in entries {
            let Some(normalized) = normalize_username(username) else {
                continue;
            };
            let key = (grid.id(), normalized);
            candidates
                .entry(key)
                .and_modify(|existing| {
                    if *existing != Some(*agent_id) {
                        *existing = None;
                    }
                })
                .or_insert(Some(*agent_id));
        }
        let mut unambiguous = Vec::new();
        let mut grids = Vec::new();
        for ((grid_id, normalized), agent_id) in candidates {
            if let Some(agent_id) = agent_id {
                if !grids.contains(&grid_id) {
                    grids.push(grid_id);
                }
                unambiguous.push((grid_id, normalized, agent_id));
            }
        }
        let rows = unambiguous.len().saturating_add(grids.len()).clamp(1, MAX_BATCH_ROWS);
        let bytes = unambiguous
            .iter()
            .map(|(_, normalized, _)| 48usize.saturating_add(normalized.len()))
            .sum::<usize>()
            .saturating_add(64)
            .min(MAX_BATCH_BYTES);
        let batch = SqliteBatch::new(rows, bytes, move |transaction| {
            for (grid_id, normalized, agent_id) in &unambiguous {
                transaction.execute(
                    "INSERT OR IGNORE INTO verified_logins (grid_id, normalized_username, agent_id, updated_at_ms)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![grid_id, normalized.as_str(), agent_id.as_bytes().as_slice(), updated_at_ms],
                )?;
            }
            for grid_id in &grids {
                prune_verified_logins_for_grid(transaction, grid_id, MAX_VERIFIED_LOGINS_PER_GRID)?;
            }
            Ok(())
        })?;
        self.db.submit_batches(vec![batch]).map_err(NameCacheError::from)
    }

    pub(crate) fn lookup_verified_agent_id(&self, grid: Grid, username: &str) -> Result<SqliteReceipt<Option<AvatarId>>, NameCacheError> {
        let grid_id = grid.id();
        let normalized = normalize_username(username);
        self.db
            .read(0, move |transaction| {
                let Some(normalized) = normalized else {
                    return Ok(None);
                };
                let row = transaction
                    .query_row(
                        "SELECT agent_id FROM verified_logins WHERE grid_id = ?1 AND normalized_username = ?2",
                        params![grid_id, normalized],
                        |row| row.get::<_, Vec<u8>>(0),
                    )
                    .optional()?;
                Ok(row.and_then(|bytes| Uuid::from_slice(&bytes).ok()).and_then(AvatarId::new))
            })
            .map_err(NameCacheError::from)
    }
}

pub(crate) fn normalize_username(username: &str) -> Option<String> {
    let trimmed = username.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_NAME_BYTES || trimmed.chars().any(char::is_control) {
        return None;
    }
    let mut parts = trimmed.split(|character: char| character.is_whitespace() || character == '.').filter(|part| !part.is_empty());
    let first = parts.next()?;
    let rest: Vec<&str> = parts.collect();
    let normalized =
        if rest.is_empty() { format!("{first} resident").to_lowercase() } else { format!("{first} {}", rest.join(" ")).to_lowercase() };
    (normalized.len() <= MAX_NAME_BYTES).then_some(normalized)
}

fn canonical_session_username(first_name: &str, last_name: &str) -> Option<String> {
    let first = first_name.trim();
    let last = last_name.trim();
    if first.is_empty() || last.is_empty() {
        return None;
    }
    let canonical = if last.eq_ignore_ascii_case("resident") { first.to_lowercase() } else { format!("{first}.{last}").to_lowercase() };
    validate_name(&canonical).then_some(canonical)
}

fn validate_name(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= MAX_NAME_BYTES && !value.chars().any(char::is_control)
}

fn validate_timestamp(ms: u64) -> Result<i64, NameCacheError> {
    i64::try_from(ms).map_err(|_| NameCacheError::InvalidInput("timestamp exceeds i64"))
}

fn validate_avatar_entry(entry: &CachedAvatarName) -> Result<(), NameCacheError> {
    if !validate_name(&entry.username) {
        return Err(NameCacheError::InvalidInput("invalid avatar username"));
    }
    if !validate_name(&entry.first_name) || !validate_name(&entry.last_name) {
        return Err(NameCacheError::InvalidInput("invalid avatar legacy name"));
    }
    if let Some(display_name) = entry.display_name.as_deref()
        && !validate_name(display_name)
    {
        return Err(NameCacheError::InvalidInput("invalid avatar display name"));
    }
    validate_timestamp(entry.next_update_ms)?;
    validate_timestamp(entry.updated_at_ms)?;
    Ok(())
}

fn validate_group_entry(entry: &CachedGroupName) -> Result<(), NameCacheError> {
    if !validate_name(&entry.name) {
        return Err(NameCacheError::InvalidInput("invalid group name"));
    }
    validate_timestamp(entry.updated_at_ms)?;
    Ok(())
}

fn avatar_entry_bytes(entry: &CachedAvatarName) -> usize {
    64usize
        .saturating_add(entry.username.len())
        .saturating_add(entry.first_name.len())
        .saturating_add(entry.last_name.len())
        .saturating_add(entry.display_name.as_deref().map_or(0, str::len))
}

fn group_entry_bytes(entry: &CachedGroupName) -> usize {
    48usize.saturating_add(entry.name.len())
}

fn upsert_avatar_row(transaction: &Transaction<'_>, grid_id: &str, entry: &CachedAvatarName) -> Result<(), rusqlite::Error> {
    transaction.execute(
        "INSERT INTO avatar_names (
            grid_id, agent_id, username, first_name, last_name, display_name, is_default_display_name, next_update_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (grid_id, agent_id) DO UPDATE SET
            username = excluded.username,
            first_name = excluded.first_name,
            last_name = excluded.last_name,
            display_name = excluded.display_name,
            is_default_display_name = excluded.is_default_display_name,
            next_update_ms = excluded.next_update_ms,
            updated_at_ms = excluded.updated_at_ms",
        params![
            grid_id,
            entry.agent_id.as_bytes().as_slice(),
            entry.username.as_str(),
            entry.first_name.as_str(),
            entry.last_name.as_str(),
            entry.display_name.as_deref(),
            i64::from(entry.is_default_display_name),
            entry.next_update_ms as i64,
            entry.updated_at_ms as i64,
        ],
    )?;
    Ok(())
}

fn upsert_group_row(transaction: &Transaction<'_>, grid_id: &str, entry: &CachedGroupName) -> Result<(), rusqlite::Error> {
    transaction.execute(
        "INSERT INTO group_names (grid_id, group_id, name, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (grid_id, group_id) DO UPDATE SET
            name = excluded.name,
            updated_at_ms = excluded.updated_at_ms",
        params![grid_id, entry.group_id.as_bytes().as_slice(), entry.name.as_str(), entry.updated_at_ms as i64],
    )?;
    Ok(())
}

fn prune_avatars_for_grid(transaction: &Transaction<'_>, grid_id: &str, max_entries: usize) -> Result<(), rusqlite::Error> {
    transaction.execute(
        "DELETE FROM avatar_names
         WHERE grid_id = ?1
           AND agent_id IN (
               SELECT agent_id FROM avatar_names
               WHERE grid_id = ?1
               ORDER BY updated_at_ms DESC, agent_id DESC
               LIMIT -1 OFFSET ?2
           )",
        params![grid_id, max_entries as i64],
    )?;
    Ok(())
}

fn prune_groups_for_grid(transaction: &Transaction<'_>, grid_id: &str, max_entries: usize) -> Result<(), rusqlite::Error> {
    transaction.execute(
        "DELETE FROM group_names
         WHERE grid_id = ?1
           AND group_id IN (
               SELECT group_id FROM group_names
               WHERE grid_id = ?1
               ORDER BY updated_at_ms DESC, group_id DESC
               LIMIT -1 OFFSET ?2
           )",
        params![grid_id, max_entries as i64],
    )?;
    Ok(())
}

fn prune_verified_logins_for_grid(transaction: &Transaction<'_>, grid_id: &str, max_entries: usize) -> Result<(), rusqlite::Error> {
    transaction.execute(
        "DELETE FROM verified_logins
         WHERE grid_id = ?1
           AND normalized_username IN (
               SELECT normalized_username FROM verified_logins
               WHERE grid_id = ?1
               ORDER BY updated_at_ms DESC, normalized_username DESC
               LIMIT -1 OFFSET ?2
           )",
        params![grid_id, max_entries as i64],
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_avatar_row(
    agent_bytes: &[u8],
    username: String,
    first_name: String,
    last_name: String,
    display_name: Option<String>,
    is_default: i64,
    next_update_ms: i64,
    updated_at_ms: i64,
) -> Option<CachedAvatarName> {
    let agent_id = Uuid::from_slice(agent_bytes).ok().and_then(AvatarId::new)?;
    if !validate_name(&username) || !validate_name(&first_name) || !validate_name(&last_name) {
        return None;
    }
    if let Some(display_name) = display_name.as_deref()
        && !validate_name(display_name)
    {
        return None;
    }
    let is_default_display_name = match is_default {
        0 => false,
        1 => true,
        _ => return None,
    };
    let next_update_ms = u64::try_from(next_update_ms).ok()?;
    let updated_at_ms = u64::try_from(updated_at_ms).ok()?;
    Some(CachedAvatarName {
        agent_id,
        username,
        first_name,
        last_name,
        display_name,
        is_default_display_name,
        next_update_ms,
        updated_at_ms,
    })
}

fn decode_group_row(group_bytes: &[u8], name: String, updated_at_ms: i64) -> Option<CachedGroupName> {
    let group_id = Uuid::from_slice(group_bytes).ok().and_then(GroupId::new)?;
    if !validate_name(&name) {
        return None;
    }
    let updated_at_ms = u64::try_from(updated_at_ms).ok()?;
    Some(CachedGroupName { group_id, name, updated_at_ms })
}

fn migrate(connection: &mut Connection) -> Result<(), MigrationError> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match version {
        0 => {
            let transaction = connection.savepoint()?;
            for (_, _, sql) in SCHEMA {
                transaction.execute_batch(sql)?;
            }
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
    let mut statement = connection.prepare("SELECT type, name, sql FROM sqlite_master WHERE type = 'table'")?;
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
    Ok(())
}

fn normalize_schema_sql(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ").trim_end_matches(';').to_owned()
}

#[cfg(test)]
mod tests {
    use std::{assert_matches, fs, slice};

    use network::Grid;
    use rusqlite::Connection;
    use storage::sqlite::{OpenStatus, RecoveryReason, SqliteError};
    use tempfile::tempdir;
    use uuid::Uuid;

    use super::*;

    #[tokio::test]
    async fn round_trip_isolates_grids() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("names.sqlite");
        let (cache, status) = NameCache::open(&path).expect("open name cache");
        assert_eq!(status, OpenStatus::Created);

        let avatar_id = AvatarId::new(Uuid::from_u128(1)).expect("non-nil avatar id");
        let group_id = GroupId::new(Uuid::from_u128(2)).expect("non-nil group id");
        let avatar = CachedAvatarName {
            agent_id: avatar_id,
            username: "ada.lovelace".to_owned(),
            first_name: "Ada".to_owned(),
            last_name: "Lovelace".to_owned(),
            display_name: Some("Lady Ada".to_owned()),
            is_default_display_name: false,
            next_update_ms: 1_700_000_999_000,
            updated_at_ms: 1_700_000_000_000,
        };
        let group = CachedGroupName { group_id, name: "Builders".to_owned(), updated_at_ms: 1_700_000_123_000 };

        cache.upsert_avatars(Grid::Agni, slice::from_ref(&avatar)).expect("queue avatar").wait().await.expect("write avatar");
        cache.upsert_groups(Grid::Agni, slice::from_ref(&group)).expect("queue group").wait().await.expect("write group");
        cache.shutdown().expect("shutdown");

        let (reopened, status) = NameCache::open(&path).expect("reopen name cache");
        assert_eq!(status, OpenStatus::Existing);

        let agni_avatars = reopened.load_avatars(Grid::Agni, &[avatar_id]).expect("queue load").wait().await.expect("load avatars");
        assert_eq!(agni_avatars, vec![avatar]);

        let aditi_avatars = reopened.load_avatars(Grid::Aditi, &[avatar_id]).expect("queue load").wait().await.expect("load avatars");
        assert!(aditi_avatars.is_empty());

        let agni_groups = reopened.load_groups(Grid::Agni, &[group_id]).expect("queue load").wait().await.expect("load groups");
        assert_eq!(agni_groups, vec![group]);

        let aditi_groups = reopened.load_groups(Grid::Aditi, &[group_id]).expect("queue load").wait().await.expect("load groups");
        assert!(aditi_groups.is_empty());

        reopened.shutdown().expect("shutdown");
    }

    #[tokio::test]
    async fn verified_login_normalizes_username() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("names.sqlite");
        let (cache, _) = NameCache::open(&path).expect("open name cache");

        let first_agent = Uuid::from_u128(10);
        let second_agent = Uuid::from_u128(11);
        let single_name_agent = Uuid::from_u128(12);

        cache
            .record_verified_login_fields(Grid::Agni, "First Last", first_agent, "First", "Last", 100, MAX_VERIFIED_LOGINS_PER_GRID)
            .expect("queue login")
            .wait()
            .await
            .expect("write login");

        let looked_up = cache.lookup_verified_agent_id(Grid::Agni, "first.last").expect("queue lookup").wait().await.expect("lookup");
        assert_eq!(looked_up, AvatarId::new(first_agent));

        cache
            .record_verified_login_fields(Grid::Agni, "first.last", second_agent, "First", "Last", 200, MAX_VERIFIED_LOGINS_PER_GRID)
            .expect("queue reauth")
            .wait()
            .await
            .expect("write reauth");

        let updated = cache.lookup_verified_agent_id(Grid::Agni, "  FIRST   LAST ").expect("queue lookup").wait().await.expect("lookup");
        assert_eq!(updated, AvatarId::new(second_agent));

        cache
            .record_verified_login_fields(Grid::Agni, "Solo", single_name_agent, "Solo", "Resident", 300, MAX_VERIFIED_LOGINS_PER_GRID)
            .expect("queue resident login")
            .wait()
            .await
            .expect("write resident login");

        let resident_lookup =
            cache.lookup_verified_agent_id(Grid::Agni, "Solo Resident").expect("queue lookup").wait().await.expect("lookup");
        assert_eq!(resident_lookup, AvatarId::new(single_name_agent));

        let dot_resident_lookup =
            cache.lookup_verified_agent_id(Grid::Agni, "solo.resident").expect("queue lookup").wait().await.expect("lookup");
        assert_eq!(dot_resident_lookup, AvatarId::new(single_name_agent));

        let baseline = cache
            .load_avatars(Grid::Agni, &[AvatarId::new(single_name_agent).expect("non-nil")])
            .expect("queue load")
            .wait()
            .await
            .expect("load baseline");
        assert_eq!(baseline.len(), 1);
        assert_eq!(baseline[0].username, "solo");
        assert_eq!(baseline[0].first_name, "Solo");
        assert_eq!(baseline[0].last_name, "Resident");
        assert_eq!(baseline[0].display_name, None);

        cache.shutdown().expect("shutdown");
    }

    #[test]
    fn corrupt_heals_newer_preserved() {
        let dir = tempdir().expect("tempdir");

        let corrupt_path = dir.path().join("corrupt.sqlite");
        fs::write(&corrupt_path, b"not a valid sqlite database").expect("write corrupt file");
        let (cache, status) = NameCache::open(&corrupt_path).expect("recreate corrupt db");
        assert_eq!(status, OpenStatus::Recreated(RecoveryReason::Corrupt));
        cache.shutdown().expect("shutdown");

        let mismatch_path = dir.path().join("mismatch.sqlite");
        {
            let connection = Connection::open(&mismatch_path).expect("open raw sqlite");
            connection
                .execute_batch("CREATE TABLE avatar_names (grid_id TEXT); PRAGMA user_version = 1;")
                .expect("write mismatched schema");
        }
        let (cache, status) = NameCache::open(&mismatch_path).expect("recreate mismatched db");
        assert_eq!(status, OpenStatus::Recreated(RecoveryReason::SchemaMismatch));
        cache.shutdown().expect("shutdown");

        let newer_path = dir.path().join("newer.sqlite");
        {
            let connection = Connection::open(&newer_path).expect("open raw sqlite");
            connection
                .execute_batch("CREATE TABLE sentinel (id INTEGER PRIMARY KEY); PRAGMA user_version = 99;")
                .expect("write newer schema");
        }
        let error = NameCache::open(&newer_path).expect_err("reject newer schema version");
        assert_matches!(error, NameCacheError::Storage(SqliteError::UnsupportedSchemaVersion));
        assert!(newer_path.exists());
        let connection = Connection::open(&newer_path).expect("reopen newer db");
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0)).expect("read user_version");
        assert_eq!(version, 99);
    }

    #[tokio::test]
    async fn invalid_rejected_corrupt_discarded() {
        assert_eq!(AvatarId::new(Uuid::nil()), None);
        assert_eq!(GroupId::new(Uuid::nil()), None);

        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("names.sqlite");
        let (cache, _) = NameCache::open(&path).expect("open name cache");

        let avatar_id = AvatarId::new(Uuid::from_u128(20)).expect("non-nil");
        let group_id = GroupId::new(Uuid::from_u128(21)).expect("non-nil");

        let empty_avatar = CachedAvatarName {
            agent_id: avatar_id,
            username: "   ".to_owned(),
            first_name: "Ada".to_owned(),
            last_name: "Lovelace".to_owned(),
            display_name: None,
            is_default_display_name: false,
            next_update_ms: 0,
            updated_at_ms: 1,
        };
        assert_matches!(cache.upsert_avatars(Grid::Agni, &[empty_avatar]), Err(NameCacheError::InvalidInput(_)));

        let oversized_group = CachedGroupName { group_id, name: "a".repeat(MAX_NAME_BYTES + 1), updated_at_ms: 1 };
        assert_matches!(cache.upsert_groups(Grid::Agni, &[oversized_group]), Err(NameCacheError::InvalidInput(_)));

        assert_matches!(
            cache
                .record_verified_login_fields(Grid::Agni, "Ada Lovelace", Uuid::nil(), "Ada", "Lovelace", 1, MAX_VERIFIED_LOGINS_PER_GRID,),
            Err(NameCacheError::InvalidInput(_))
        );

        cache.shutdown().expect("shutdown");

        {
            let connection = Connection::open(&path).expect("open raw sqlite");
            connection
                .execute(
                    "INSERT INTO group_names (grid_id, group_id, name, updated_at_ms) VALUES (?1, ?2, ?3, ?4)",
                    params![Grid::Agni.id(), group_id.as_bytes().as_slice(), "x".repeat(MAX_NAME_BYTES + 10), 1_i64],
                )
                .expect("insert oversized group row");
        }

        let (reopened, _) = NameCache::open(&path).expect("reopen name cache");
        let loaded = reopened.load_groups(Grid::Agni, &[group_id]).expect("queue load").wait().await.expect("load groups");
        assert!(loaded.is_empty());
        reopened.shutdown().expect("shutdown");
    }

    #[tokio::test]
    async fn grid_pruning_isolates_grids() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("names.sqlite");
        let (cache, _) = NameCache::open(&path).expect("open name cache");

        let agni_entries: Vec<CachedGroupName> = (1_u128..=4)
            .map(|index| CachedGroupName {
                group_id: GroupId::new(Uuid::from_u128(index)).expect("non-nil"),
                name: format!("Group {index}"),
                updated_at_ms: (index * 100) as u64,
            })
            .collect();
        let aditi_group = CachedGroupName {
            group_id: GroupId::new(Uuid::from_u128(99)).expect("non-nil"),
            name: "Aditi Group".to_owned(),
            updated_at_ms: 1,
        };

        cache
            .upsert_groups_with_limit(Grid::Aditi, slice::from_ref(&aditi_group), 2)
            .expect("queue aditi")
            .wait()
            .await
            .expect("write aditi");
        cache.upsert_groups_with_limit(Grid::Agni, &agni_entries, 2).expect("queue agni").wait().await.expect("write agni");

        let agni_ids: Vec<GroupId> = agni_entries.iter().map(|entry| entry.group_id).collect();
        let remaining_agni = cache.load_groups(Grid::Agni, &agni_ids).expect("queue load").wait().await.expect("load agni");
        assert_eq!(remaining_agni.len(), 2);
        assert_eq!(remaining_agni[0].group_id, agni_entries[2].group_id);
        assert_eq!(remaining_agni[1].group_id, agni_entries[3].group_id);

        let remaining_aditi =
            cache.load_groups(Grid::Aditi, &[aditi_group.group_id]).expect("queue load").wait().await.expect("load aditi");
        assert_eq!(remaining_aditi, vec![aditi_group]);

        cache.shutdown().expect("shutdown");
    }
}
