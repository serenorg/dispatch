use super::{CourierError, CourierSession, MountKind};
use rusqlite::{Connection, params};
use std::path::Path;

pub(super) fn persist_session_mounts(session: &CourierSession) -> Result<(), CourierError> {
    for mount in &session.resolved_mounts {
        if mount.kind == MountKind::Session && mount.driver == "sqlite" {
            persist_session_sqlite(Path::new(&mount.target_path), session)?;
        }
    }
    Ok(())
}

fn persist_session_sqlite(path: &Path, session: &CourierSession) -> Result<(), CourierError> {
    let connection = Connection::open(path).map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "open",
        source,
    })?;
    ensure_session_sqlite(&connection, path)?;
    let payload = serde_json::to_string(session)
        .map_err(|error| CourierError::SerializeSession(error.to_string()))?;
    connection
        .execute(
            concat!(
                "INSERT INTO dispatch_sessions ",
                "(session_id, parcel_digest, entrypoint, turn_count, payload_json) ",
                "VALUES (?1, ?2, ?3, ?4, ?5) ",
                "ON CONFLICT(session_id) DO UPDATE SET ",
                "parcel_digest = excluded.parcel_digest, ",
                "entrypoint = excluded.entrypoint, ",
                "turn_count = excluded.turn_count, ",
                "payload_json = excluded.payload_json"
            ),
            params![
                session.id,
                session.parcel_digest,
                session.entrypoint,
                session.turn_count as i64,
                payload,
            ],
        )
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "upsert_session",
            source,
        })?;
    Ok(())
}

pub(super) fn ensure_session_sqlite(
    connection: &Connection,
    path: &Path,
) -> Result<(), CourierError> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS dispatch_sessions (
                session_id TEXT PRIMARY KEY,
                parcel_digest TEXT NOT NULL,
                entrypoint TEXT,
                turn_count INTEGER NOT NULL,
                payload_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS dispatch_checkpoints (
                session_id TEXT NOT NULL,
                parcel_digest TEXT NOT NULL,
                checkpoint_name TEXT NOT NULL,
                value TEXT NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY(session_id, parcel_digest, checkpoint_name)
            );",
        )
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "create_session_tables",
            source,
        })
}
