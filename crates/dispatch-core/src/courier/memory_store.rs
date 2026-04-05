use super::{BuiltinMemoryPutEntry, CourierError, CourierSession, current_unix_timestamp};
use crate::manifest::MountKind;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct MemoryEntry {
    pub(super) namespace: String,
    pub(super) key: String,
    pub(super) value: String,
    pub(super) updated_at: u64,
}

fn map_memory_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryEntry> {
    Ok(MemoryEntry {
        namespace: row.get(0)?,
        key: row.get(1)?,
        value: row.get(2)?,
        updated_at: row.get::<_, i64>(3)? as u64,
    })
}

pub(super) fn memory_mount_path(session: &CourierSession) -> Option<&Path> {
    session
        .resolved_mounts
        .iter()
        .find(|mount| mount.kind == MountKind::Memory && mount.driver == "sqlite")
        .map(|mount| Path::new(&mount.target_path))
}

fn require_memory_mount_path(session: &CourierSession) -> Result<&Path, CourierError> {
    memory_mount_path(session).ok_or_else(|| CourierError::MissingMemoryMount {
        parcel_digest: session.parcel_digest.clone(),
    })
}

fn ensure_memory_sqlite(connection: &Connection, path: &Path) -> Result<(), CourierError> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS dispatch_memory (
                parcel_digest TEXT NOT NULL,
                namespace TEXT NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY(parcel_digest, namespace, key)
            );",
        )
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "create_memory_table",
            source,
        })
}

pub(super) fn memory_get(
    session: &CourierSession,
    namespace: &str,
    key: &str,
) -> Result<Option<MemoryEntry>, CourierError> {
    let path = require_memory_mount_path(session)?;
    let connection = Connection::open(path).map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "open_memory_get",
        source,
    })?;
    ensure_memory_sqlite(&connection, path)?;
    connection
        .query_row(
            concat!(
                "SELECT namespace, key, value, updated_at ",
                "FROM dispatch_memory ",
                "WHERE parcel_digest = ?1 AND namespace = ?2 AND key = ?3"
            ),
            params![session.parcel_digest, namespace, key],
            map_memory_entry,
        )
        .map(Some)
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            source => Err(CourierError::SqliteMount {
                path: path.display().to_string(),
                operation: "query_memory_get",
                source,
            }),
        })
}

pub(super) fn memory_put(
    session: &CourierSession,
    namespace: &str,
    key: &str,
    value: &str,
) -> Result<bool, CourierError> {
    let path = require_memory_mount_path(session)?;
    let mut connection = Connection::open(path).map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "open_memory_put",
        source,
    })?;
    ensure_memory_sqlite(&connection, path)?;
    let tx = connection
        .transaction()
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "begin_memory_put",
            source,
        })?;
    let existed = tx
        .query_row(
            concat!(
                "SELECT EXISTS(",
                "SELECT 1 FROM dispatch_memory ",
                "WHERE parcel_digest = ?1 AND namespace = ?2 AND key = ?3",
                ")"
            ),
            params![session.parcel_digest, namespace, key],
            |row| row.get::<_, i64>(0),
        )
        .map(|value| value != 0)
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "query_memory_put_exists",
            source,
        })?;
    tx.execute(
        concat!(
            "INSERT INTO dispatch_memory ",
            "(parcel_digest, namespace, key, value, updated_at) ",
            "VALUES (?1, ?2, ?3, ?4, ?5) ",
            "ON CONFLICT(parcel_digest, namespace, key) DO UPDATE SET ",
            "value = excluded.value, ",
            "updated_at = excluded.updated_at"
        ),
        params![
            session.parcel_digest,
            namespace,
            key,
            value,
            current_unix_timestamp() as i64,
        ],
    )
    .map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "upsert_memory_put",
        source,
    })?;
    tx.commit().map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "commit_memory_put",
        source,
    })?;
    Ok(existed)
}

pub(super) fn memory_delete(
    session: &CourierSession,
    namespace: &str,
    key: &str,
) -> Result<bool, CourierError> {
    let path = require_memory_mount_path(session)?;
    let connection = Connection::open(path).map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "open_memory_delete",
        source,
    })?;
    ensure_memory_sqlite(&connection, path)?;
    let deleted = connection
        .execute(
            concat!(
                "DELETE FROM dispatch_memory ",
                "WHERE parcel_digest = ?1 AND namespace = ?2 AND key = ?3"
            ),
            params![session.parcel_digest, namespace, key],
        )
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "delete_memory_entry",
            source,
        })?;
    Ok(deleted > 0)
}

pub(super) fn memory_list(
    session: &CourierSession,
    namespace: &str,
    prefix: Option<&str>,
) -> Result<Vec<MemoryEntry>, CourierError> {
    let path = require_memory_mount_path(session)?;
    let connection = Connection::open(path).map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "open_memory_list",
        source,
    })?;
    ensure_memory_sqlite(&connection, path)?;
    let prefix_like = super::escape_sql_like_prefix(prefix.unwrap_or_default());
    let mut statement = connection
        .prepare(concat!(
            "SELECT namespace, key, value, updated_at ",
            "FROM dispatch_memory ",
            "WHERE parcel_digest = ?1 AND namespace = ?2 AND key LIKE ?3 ESCAPE '\\' ",
            "ORDER BY key ASC"
        ))
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "prepare_memory_list",
            source,
        })?;
    let rows = statement
        .query_map(
            params![session.parcel_digest, namespace, prefix_like],
            map_memory_entry,
        )
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "query_memory_list",
            source,
        })?;
    let mut entries = Vec::new();
    for entry in rows {
        entries.push(entry.map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "read_memory_list",
            source,
        })?);
    }
    Ok(entries)
}

pub(super) fn memory_list_range(
    session: &CourierSession,
    namespace: &str,
    start_key: Option<&str>,
    end_key: Option<&str>,
    limit: Option<usize>,
) -> Result<Vec<MemoryEntry>, CourierError> {
    let path = require_memory_mount_path(session)?;
    let connection = Connection::open(path).map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "open_memory_list_range",
        source,
    })?;
    ensure_memory_sqlite(&connection, path)?;
    let mut query = String::from(concat!(
        "SELECT namespace, key, value, updated_at ",
        "FROM dispatch_memory ",
        "WHERE parcel_digest = ?1 AND namespace = ?2 "
    ));
    if start_key.is_some() {
        query.push_str("AND key >= ?3 ");
    }
    if end_key.is_some() {
        query.push_str(if start_key.is_some() {
            "AND key < ?4 "
        } else {
            "AND key < ?3 "
        });
    }
    query.push_str("ORDER BY key ASC");
    if let Some(limit) = limit {
        query.push_str(&format!(" LIMIT {}", limit));
    }

    let mut statement = connection
        .prepare(&query)
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "prepare_memory_list_range",
            source,
        })?;
    let rows = match (start_key, end_key) {
        (Some(start_key), Some(end_key)) => statement.query_map(
            params![session.parcel_digest, namespace, start_key, end_key],
            map_memory_entry,
        ),
        (Some(start_key), None) => statement.query_map(
            params![session.parcel_digest, namespace, start_key],
            map_memory_entry,
        ),
        (None, Some(end_key)) => statement.query_map(
            params![session.parcel_digest, namespace, end_key],
            map_memory_entry,
        ),
        (None, None) => {
            statement.query_map(params![session.parcel_digest, namespace], map_memory_entry)
        }
    }
    .map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "query_memory_list_range",
        source,
    })?;
    let mut entries = Vec::new();
    for entry in rows {
        entries.push(entry.map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "read_memory_list_range",
            source,
        })?);
    }
    Ok(entries)
}

pub(super) fn memory_delete_range(
    session: &CourierSession,
    namespace: &str,
    start_key: Option<&str>,
    end_key: Option<&str>,
) -> Result<usize, CourierError> {
    let path = require_memory_mount_path(session)?;
    let connection = Connection::open(path).map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "open_memory_delete_range",
        source,
    })?;
    ensure_memory_sqlite(&connection, path)?;
    let mut query = String::from(concat!(
        "DELETE FROM dispatch_memory ",
        "WHERE parcel_digest = ?1 AND namespace = ?2 "
    ));
    if start_key.is_some() {
        query.push_str("AND key >= ?3 ");
    }
    if end_key.is_some() {
        query.push_str(if start_key.is_some() {
            "AND key < ?4"
        } else {
            "AND key < ?3"
        });
    }
    let deleted = match (start_key, end_key) {
        (Some(start_key), Some(end_key)) => connection.execute(
            &query,
            params![session.parcel_digest, namespace, start_key, end_key],
        ),
        (Some(start_key), None) => {
            connection.execute(&query, params![session.parcel_digest, namespace, start_key])
        }
        (None, Some(end_key)) => {
            connection.execute(&query, params![session.parcel_digest, namespace, end_key])
        }
        (None, None) => connection.execute(&query, params![session.parcel_digest, namespace]),
    }
    .map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "delete_memory_range",
        source,
    })?;
    Ok(deleted)
}

pub(super) fn memory_put_many(
    session: &CourierSession,
    namespace: &str,
    entries: &[BuiltinMemoryPutEntry],
) -> Result<usize, CourierError> {
    let path = require_memory_mount_path(session)?;
    let mut connection = Connection::open(path).map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "open_memory_put_many",
        source,
    })?;
    ensure_memory_sqlite(&connection, path)?;
    let tx = connection
        .transaction()
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "begin_memory_put_many",
            source,
        })?;
    let mut replaced = 0usize;
    for entry in entries {
        let existed = tx
            .query_row(
                concat!(
                    "SELECT EXISTS(",
                    "SELECT 1 FROM dispatch_memory ",
                    "WHERE parcel_digest = ?1 AND namespace = ?2 AND key = ?3",
                    ")"
                ),
                params![session.parcel_digest, namespace, entry.key],
                |row| row.get::<_, i64>(0),
            )
            .map(|value| value != 0)
            .map_err(|source| CourierError::SqliteMount {
                path: path.display().to_string(),
                operation: "query_memory_put_many_exists",
                source,
            })?;
        if existed {
            replaced += 1;
        }
        tx.execute(
            concat!(
                "INSERT INTO dispatch_memory ",
                "(parcel_digest, namespace, key, value, updated_at) ",
                "VALUES (?1, ?2, ?3, ?4, ?5) ",
                "ON CONFLICT(parcel_digest, namespace, key) DO UPDATE SET ",
                "value = excluded.value, ",
                "updated_at = excluded.updated_at"
            ),
            params![
                session.parcel_digest,
                namespace,
                entry.key,
                entry.value,
                current_unix_timestamp() as i64,
            ],
        )
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "upsert_memory_put_many",
            source,
        })?;
    }
    tx.commit().map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "commit_memory_put_many",
        source,
    })?;
    Ok(replaced)
}
