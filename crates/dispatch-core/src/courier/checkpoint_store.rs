use super::{CourierError, CourierSession, current_unix_timestamp, ensure_session_sqlite};
use crate::manifest::MountKind;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct CheckpointEntry {
    pub(super) name: String,
    pub(super) value: String,
    pub(super) updated_at: u64,
}

fn map_checkpoint_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<CheckpointEntry> {
    Ok(CheckpointEntry {
        name: row.get(0)?,
        value: row.get(1)?,
        updated_at: row.get::<_, i64>(2)? as u64,
    })
}

fn session_mount_path(session: &CourierSession) -> Option<&Path> {
    session
        .resolved_mounts
        .iter()
        .find(|mount| mount.kind == MountKind::Session && mount.driver == "sqlite")
        .map(|mount| Path::new(&mount.target_path))
}

fn require_session_mount_path(session: &CourierSession) -> Result<&Path, CourierError> {
    session_mount_path(session).ok_or_else(|| CourierError::MissingSessionMount {
        parcel_digest: session.parcel_digest.clone(),
    })
}

pub(super) fn checkpoint_get(
    session: &CourierSession,
    name: &str,
) -> Result<Option<CheckpointEntry>, CourierError> {
    let path = require_session_mount_path(session)?;
    let connection = Connection::open(path).map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "open_checkpoint_get",
        source,
    })?;
    ensure_session_sqlite(&connection, path)?;
    connection
        .query_row(
            concat!(
                "SELECT checkpoint_name, value, updated_at ",
                "FROM dispatch_checkpoints ",
                "WHERE session_id = ?1 AND parcel_digest = ?2 AND checkpoint_name = ?3"
            ),
            params![session.id, session.parcel_digest, name],
            map_checkpoint_entry,
        )
        .map(Some)
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            source => Err(CourierError::SqliteMount {
                path: path.display().to_string(),
                operation: "query_checkpoint_get",
                source,
            }),
        })
}

pub(super) fn checkpoint_put(
    session: &CourierSession,
    name: &str,
    value: &str,
) -> Result<bool, CourierError> {
    let path = require_session_mount_path(session)?;
    let mut connection = Connection::open(path).map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "open_checkpoint_put",
        source,
    })?;
    ensure_session_sqlite(&connection, path)?;
    let tx = connection
        .transaction()
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "begin_checkpoint_put",
            source,
        })?;
    let existed = tx
        .query_row(
            concat!(
                "SELECT EXISTS(",
                "SELECT 1 FROM dispatch_checkpoints ",
                "WHERE session_id = ?1 AND parcel_digest = ?2 AND checkpoint_name = ?3",
                ")"
            ),
            params![session.id, session.parcel_digest, name],
            |row| row.get::<_, i64>(0),
        )
        .map(|value| value != 0)
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "query_checkpoint_put_exists",
            source,
        })?;
    tx.execute(
        concat!(
            "INSERT INTO dispatch_checkpoints ",
            "(session_id, parcel_digest, checkpoint_name, value, updated_at) ",
            "VALUES (?1, ?2, ?3, ?4, ?5) ",
            "ON CONFLICT(session_id, parcel_digest, checkpoint_name) DO UPDATE SET ",
            "value = excluded.value, ",
            "updated_at = excluded.updated_at"
        ),
        params![
            session.id,
            session.parcel_digest,
            name,
            value,
            current_unix_timestamp() as i64,
        ],
    )
    .map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "upsert_checkpoint_put",
        source,
    })?;
    tx.commit().map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "commit_checkpoint_put",
        source,
    })?;
    Ok(existed)
}

pub(super) fn checkpoint_delete(
    session: &CourierSession,
    name: &str,
) -> Result<bool, CourierError> {
    let path = require_session_mount_path(session)?;
    let connection = Connection::open(path).map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "open_checkpoint_delete",
        source,
    })?;
    ensure_session_sqlite(&connection, path)?;
    let deleted = connection
        .execute(
            concat!(
                "DELETE FROM dispatch_checkpoints ",
                "WHERE session_id = ?1 AND parcel_digest = ?2 AND checkpoint_name = ?3"
            ),
            params![session.id, session.parcel_digest, name],
        )
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "delete_checkpoint",
            source,
        })?;
    Ok(deleted > 0)
}

pub(super) fn checkpoint_list(
    session: &CourierSession,
    prefix: Option<&str>,
) -> Result<Vec<CheckpointEntry>, CourierError> {
    let path = require_session_mount_path(session)?;
    let connection = Connection::open(path).map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "open_checkpoint_list",
        source,
    })?;
    ensure_session_sqlite(&connection, path)?;
    let prefix_like = super::escape_sql_like_prefix(prefix.unwrap_or_default());
    let mut statement = connection
        .prepare(concat!(
            "SELECT checkpoint_name, value, updated_at ",
            "FROM dispatch_checkpoints ",
            "WHERE session_id = ?1 AND parcel_digest = ?2 AND checkpoint_name LIKE ?3 ESCAPE '\\' ",
            "ORDER BY checkpoint_name ASC"
        ))
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "prepare_checkpoint_list",
            source,
        })?;
    let rows = statement
        .query_map(
            params![session.id, session.parcel_digest, prefix_like],
            map_checkpoint_entry,
        )
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "query_checkpoint_list",
            source,
        })?;
    let mut entries = Vec::new();
    for entry in rows {
        entries.push(entry.map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "read_checkpoint_list",
            source,
        })?);
    }
    Ok(entries)
}
