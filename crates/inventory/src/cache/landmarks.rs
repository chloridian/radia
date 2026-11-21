use std::path::Path;

use rusqlite::{OptionalExtension, Transaction, params};
use storage::{
    default_cache_dir,
    sqlite::{MAX_BATCH_BYTES, MAX_BATCH_ROWS, OpenStatus, SqliteBatch, SqliteDb, StorageClass},
};
use uuid::Uuid;

use super::{InventoryCacheError, schema, valid_identity};

pub const MAX_LOGIN_LANDMARKS_PER_AGENT: usize = 512;
pub const MAX_LOGIN_LANDMARK_NAME_BYTES: usize = 512;
pub const MAX_LOGIN_LANDMARK_SLURL_BYTES: usize = 1024;
pub const MAX_LOGIN_LANDMARK_IMPORT_SOURCE_KEY_BYTES: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoginLandmark {
    pub name: String,
    pub slurl: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoginLandmarkImportSnapshot {
    pub grid_id: String,
    pub agent_id: Uuid,
    pub landmarks: Vec<LoginLandmark>,
}

#[derive(Debug)]
pub struct LoginLandmarkCache {
    db: SqliteDb,
}

impl LoginLandmarkCache {
    pub fn open() -> Result<(Self, OpenStatus), InventoryCacheError> {
        let root = default_cache_dir()?;
        Self::open_at_root(root)
    }

    pub fn open_at_root(cache_root: impl AsRef<Path>) -> Result<(Self, OpenStatus), InventoryCacheError> {
        let path = cache_root.as_ref().join("inventory.sqlite");
        let (db, status) = SqliteDb::open(path, StorageClass::Cache, |connection| schema::migrate(connection, None))?;
        Ok((Self { db }, status))
    }

    pub fn path(&self) -> &Path {
        self.db.path()
    }

    pub async fn load(&self, grid_id: &str, agent_id: Uuid) -> Result<Vec<LoginLandmark>, InventoryCacheError> {
        if !valid_identity(grid_id) || agent_id.is_nil() {
            return Err(InventoryCacheError::InvalidIdentity);
        }
        let grid_id = grid_id.to_owned();
        let receipt = self.db.read(0, move |transaction| {
            let mut statement = transaction
                .prepare("SELECT name, slurl FROM login_landmarks WHERE grid_id = ?1 AND agent_id = ?2 ORDER BY ordinal ASC LIMIT ?3")?;
            let rows = statement
                .query_map(params![grid_id, agent_id.as_bytes().as_slice(), MAX_LOGIN_LANDMARKS_PER_AGENT as i64], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
            let mut landmarks = Vec::new();
            for row in rows {
                let (name, slurl) = row?;
                let landmark = LoginLandmark { name, slurl };
                if validate_landmark(&landmark).is_ok() {
                    landmarks.push(landmark);
                }
            }
            Ok(landmarks)
        })?;
        receipt.wait().await.map_err(Into::into)
    }

    pub async fn replace(&self, grid_id: &str, agent_id: Uuid, landmarks: &[LoginLandmark]) -> Result<(), InventoryCacheError> {
        if !valid_identity(grid_id) || agent_id.is_nil() {
            return Err(InventoryCacheError::InvalidIdentity);
        }
        validate_landmarks(landmarks)?;
        let grid_id = grid_id.to_owned();
        let landmarks = landmarks.to_vec();
        let rows = landmarks.len().saturating_add(1).min(MAX_BATCH_ROWS);
        let bytes = landmarks_bytes(&landmarks).saturating_add(64).min(MAX_BATCH_BYTES);
        let batch = SqliteBatch::new(rows, bytes, move |transaction: &Transaction<'_>| {
            transaction.execute(
                "DELETE FROM login_landmarks WHERE grid_id = ?1 AND agent_id = ?2",
                params![grid_id, agent_id.as_bytes().as_slice()],
            )?;
            let mut statement =
                transaction.prepare("INSERT INTO login_landmarks (grid_id, agent_id, ordinal, name, slurl) VALUES (?1, ?2, ?3, ?4, ?5)")?;
            for (ordinal, landmark) in landmarks.iter().enumerate() {
                statement.execute(params![
                    grid_id,
                    agent_id.as_bytes().as_slice(),
                    ordinal as i64,
                    landmark.name.as_str(),
                    landmark.slurl.as_str(),
                ])?;
            }
            Ok(())
        })?;
        self.db.submit_batches(vec![batch])?.wait().await?;
        Ok(())
    }

    pub async fn has_import_marker(&self, source_key: &str) -> Result<bool, InventoryCacheError> {
        validate_source_key(source_key)?;
        let source_key = source_key.to_owned();
        let receipt = self.db.read(0, move |transaction| {
            let exists = transaction
                .query_row("SELECT 1 FROM login_landmarks_import WHERE source_key = ?1", [source_key], |_| Ok(()))
                .optional()?
                .is_some();
            Ok(exists)
        })?;
        receipt.wait().await.map_err(Into::into)
    }

    pub async fn import_if_absent(&self, source_key: &str, snapshots: &[LoginLandmarkImportSnapshot]) -> Result<(), InventoryCacheError> {
        validate_source_key(source_key)?;
        for snapshot in snapshots {
            if !valid_identity(&snapshot.grid_id) || snapshot.agent_id.is_nil() {
                return Err(InventoryCacheError::InvalidIdentity);
            }
            validate_landmarks(&snapshot.landmarks)?;
        }
        let source_key = source_key.to_owned();
        let snapshots = snapshots.to_vec();
        let total_rows = snapshots
            .iter()
            .map(|snapshot| snapshot.landmarks.len().saturating_add(1))
            .sum::<usize>()
            .saturating_add(1)
            .min(MAX_BATCH_ROWS);
        let total_bytes = snapshots
            .iter()
            .map(|snapshot| landmarks_bytes(&snapshot.landmarks).saturating_add(64))
            .sum::<usize>()
            .saturating_add(source_key.len())
            .saturating_add(64)
            .min(MAX_BATCH_BYTES);
        let batch = SqliteBatch::new(total_rows, total_bytes, move |transaction: &Transaction<'_>| {
            let already_imported = transaction
                .query_row("SELECT 1 FROM login_landmarks_import WHERE source_key = ?1", [&source_key], |_| Ok(()))
                .optional()?
                .is_some();
            if already_imported {
                return Ok(());
            }
            let mut exists_stmt = transaction.prepare("SELECT 1 FROM login_landmarks WHERE grid_id = ?1 AND agent_id = ?2 LIMIT 1")?;
            let mut insert_stmt =
                transaction.prepare("INSERT INTO login_landmarks (grid_id, agent_id, ordinal, name, slurl) VALUES (?1, ?2, ?3, ?4, ?5)")?;
            for snapshot in &snapshots {
                let has_existing = exists_stmt
                    .query_row(params![snapshot.grid_id.as_str(), snapshot.agent_id.as_bytes().as_slice()], |_| Ok(()))
                    .optional()?
                    .is_some();
                if has_existing {
                    continue;
                }
                for (ordinal, landmark) in snapshot.landmarks.iter().enumerate() {
                    insert_stmt.execute(params![
                        snapshot.grid_id.as_str(),
                        snapshot.agent_id.as_bytes().as_slice(),
                        ordinal as i64,
                        landmark.name.as_str(),
                        landmark.slurl.as_str(),
                    ])?;
                }
            }
            transaction.execute("INSERT INTO login_landmarks_import (source_key) VALUES (?1)", [source_key])?;
            Ok(())
        })?;
        self.db.submit_batches(vec![batch])?.wait().await?;
        Ok(())
    }

    pub async fn clear_all(&self) -> Result<(), InventoryCacheError> {
        let batch = SqliteBatch::new(1, 64, |transaction: &Transaction<'_>| {
            transaction.execute("DELETE FROM login_landmarks", [])?;
            Ok(())
        })?;
        self.db.submit_batches(vec![batch])?.wait().await?;
        Ok(())
    }

    pub fn close(self) -> Result<(), InventoryCacheError> {
        self.db.shutdown().map_err(Into::into)
    }
}

fn validate_source_key(source_key: &str) -> Result<(), InventoryCacheError> {
    if source_key.is_empty() || source_key.len() > MAX_LOGIN_LANDMARK_IMPORT_SOURCE_KEY_BYTES || source_key.chars().any(char::is_control) {
        return Err(InventoryCacheError::InvalidLandmark("invalid import source key"));
    }
    Ok(())
}

fn validate_landmarks(landmarks: &[LoginLandmark]) -> Result<(), InventoryCacheError> {
    if landmarks.len() > MAX_LOGIN_LANDMARKS_PER_AGENT {
        return Err(InventoryCacheError::InvalidLandmark("too many login landmarks"));
    }
    for landmark in landmarks {
        validate_landmark(landmark)?;
    }
    Ok(())
}

fn validate_landmark(landmark: &LoginLandmark) -> Result<(), InventoryCacheError> {
    if landmark.name.is_empty() || landmark.name.len() > MAX_LOGIN_LANDMARK_NAME_BYTES || landmark.name.chars().any(char::is_control) {
        return Err(InventoryCacheError::InvalidLandmark("invalid login landmark name"));
    }
    if landmark.slurl.is_empty()
        || landmark.slurl.len() > MAX_LOGIN_LANDMARK_SLURL_BYTES
        || landmark.slurl.chars().any(char::is_control)
        || !landmark.slurl.starts_with("secondlife://")
    {
        return Err(InventoryCacheError::InvalidLandmark("invalid login landmark SLURL"));
    }
    Ok(())
}

fn landmarks_bytes(landmarks: &[LoginLandmark]) -> usize {
    landmarks.iter().map(|landmark| 48usize.saturating_add(landmark.name.len()).saturating_add(landmark.slurl.len())).sum()
}
