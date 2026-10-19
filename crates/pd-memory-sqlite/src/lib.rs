//! SQLite control plane for `pd-memory` (optional, desktop parity).
//!
//! Mirrors the two tables of the TypeScript implementation
//! (`core/storage/sqlite.ts`):
//!
//! - `l0_sessions(l0_index_id PRIMARY KEY, session_key, timestamp, messages_json, source, indexed, created_at)`
//! - `pipeline_state(state_key PRIMARY KEY, state_json, updated_at)`
//!
//! The pure-Rust JSON control plane (`pd_memory_core::control_plane::FileControlPlane`)
//! is the default everywhere, including Android. This crate exists for hosts that
//! want the exact PilotDeck storage topology or need SQL ergonomics for bulk
//! inspection. It is deliberately **not** part of the default dependency graph of
//! `pd-memory-core` / `pd-memory-ffi`.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use pd_memory_core::error::{MemoryError, Result};
use pd_memory_core::ports::{format_iso, QueueStore, StateStore};
use pd_memory_core::types::L0SessionRecord;

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
CREATE TABLE IF NOT EXISTS l0_sessions (
  l0_index_id TEXT PRIMARY KEY,
  session_key TEXT NOT NULL,
  timestamp TEXT NOT NULL,
  messages_json TEXT NOT NULL,
  source TEXT NOT NULL,
  indexed INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_l0_sessions_session ON l0_sessions(session_key);
CREATE INDEX IF NOT EXISTS idx_l0_sessions_pending ON l0_sessions(indexed, timestamp);
CREATE TABLE IF NOT EXISTS pipeline_state (
  state_key TEXT PRIMARY KEY,
  state_json TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
"#;

/// rusqlite-backed control plane.
pub struct SqliteControlPlane {
    db: Mutex<Connection>,
}

impl std::fmt::Debug for SqliteControlPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteControlPlane").finish_non_exhaustive()
    }
}

impl SqliteControlPlane {
    /// Open (and initialise) a database file.
    pub fn open(db_path: impl AsRef<Path>) -> Result<Self> {
        let path = db_path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| MemoryError::io(parent, error))?;
        }
        let connection = Connection::open(path).map_err(|error| MemoryError::Corrupt {
            path: path.to_path_buf(),
            message: format!("cannot open sqlite control plane: {error}"),
        })?;
        Self::configure(connection)
    }

    /// Open an in-memory database (tests).
    pub fn open_in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory().map_err(|error| MemoryError::Corrupt {
            path: "<memory>".into(),
            message: format!("cannot open in-memory sqlite: {error}"),
        })?;
        Self::configure(connection)
    }

    fn configure(connection: Connection) -> Result<Self> {
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(sqlite_error)?;
        connection.execute_batch(SCHEMA).map_err(sqlite_error)?;
        Ok(Self {
            db: Mutex::new(connection),
        })
    }

    fn conn(&self) -> Result<MutexGuard<'_, Connection>> {
        self.db
            .lock()
            .map_err(|_| MemoryError::Other("sqlite connection mutex poisoned".into()))
    }
}

fn sqlite_error(error: rusqlite::Error) -> MemoryError {
    MemoryError::Corrupt {
        path: "<sqlite>".into(),
        message: error.to_string(),
    }
}

impl QueueStore for SqliteControlPlane {
    fn enqueue(&self, record: &L0SessionRecord) -> Result<()> {
        let messages_json = serde_json::to_string(&record.messages)?;
        let created_at = if record.created_at.is_empty() {
            format_iso(chrono::Utc::now())
        } else {
            record.created_at.clone()
        };
        self.conn()?
            .execute(
                "INSERT INTO l0_sessions (
                    l0_index_id, session_key, timestamp, messages_json, source, indexed, created_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(l0_index_id) DO UPDATE SET
                    session_key = excluded.session_key,
                    timestamp = excluded.timestamp,
                    messages_json = excluded.messages_json,
                    source = excluded.source,
                    indexed = excluded.indexed,
                    created_at = excluded.created_at",
                params![
                    record.l0_index_id,
                    record.session_key,
                    record.timestamp,
                    messages_json,
                    record.source,
                    i64::from(record.indexed),
                    created_at,
                ],
            )
            .map_err(sqlite_error)?;
        Ok(())
    }

    fn pending(
        &self,
        session_keys: Option<&[String]>,
        limit: usize,
    ) -> Result<Vec<L0SessionRecord>> {
        let conn = self.conn()?;
        let mut records = Vec::new();
        let limit = limit.max(1) as i64;
        match session_keys {
            Some(keys) if !keys.is_empty() => {
                let placeholders = vec!["?"; keys.len()].join(", ");
                let sql = format!(
                    "SELECT l0_index_id, session_key, timestamp, messages_json, source, indexed, created_at
                     FROM l0_sessions WHERE indexed = 0 AND session_key IN ({placeholders})
                     ORDER BY timestamp ASC, created_at ASC LIMIT ?"
                );
                let mut statement = conn.prepare(&sql).map_err(sqlite_error)?;
                let mut args: Vec<&dyn rusqlite::ToSql> =
                    keys.iter().map(|key| key as &dyn rusqlite::ToSql).collect();
                args.push(&limit);
                let rows = statement
                    .query_map(args.as_slice(), map_session_row)
                    .map_err(sqlite_error)?;
                for row in rows {
                    records.push(row.map_err(sqlite_error)??);
                }
            }
            _ => {
                let mut statement = conn
                    .prepare(
                        "SELECT l0_index_id, session_key, timestamp, messages_json, source, indexed, created_at
                         FROM l0_sessions WHERE indexed = 0
                         ORDER BY timestamp ASC, created_at ASC LIMIT ?1",
                    )
                    .map_err(sqlite_error)?;
                let rows = statement
                    .query_map(params![limit], map_session_row)
                    .map_err(sqlite_error)?;
                for row in rows {
                    records.push(row.map_err(sqlite_error)??);
                }
            }
        }
        Ok(records)
    }

    fn pending_session_keys(&self) -> Result<Vec<String>> {
        let conn = self.conn()?;
        let mut statement = conn
            .prepare(
                "SELECT session_key, MIN(timestamp) AS first_timestamp
                 FROM l0_sessions WHERE indexed = 0
                 GROUP BY session_key ORDER BY first_timestamp ASC LIMIT 50",
            )
            .map_err(sqlite_error)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sqlite_error)?;
        let mut keys = Vec::new();
        for row in rows {
            keys.push(row.map_err(sqlite_error)?);
        }
        Ok(keys)
    }

    fn mark_indexed(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let conn = self.conn()?;
        let tx = conn.unchecked_transaction().map_err(sqlite_error)?;
        for id in ids {
            tx.execute(
                "UPDATE l0_sessions SET indexed = 1 WHERE l0_index_id = ?1",
                params![id],
            )
            .map_err(sqlite_error)?;
        }
        tx.commit().map_err(sqlite_error)?;
        Ok(())
    }

    fn count_pending_turns(&self) -> Result<usize> {
        let count: i64 = self
            .conn()?
            .query_row(
                "SELECT COUNT(*) FROM l0_sessions WHERE indexed = 0",
                [],
                |row| row.get(0),
            )
            .map_err(sqlite_error)?;
        Ok(count.max(0) as usize)
    }

    fn recent(&self, limit: usize) -> Result<Vec<L0SessionRecord>> {
        let conn = self.conn()?;
        let mut statement = conn
            .prepare(
                "SELECT l0_index_id, session_key, timestamp, messages_json, source, indexed, created_at
                 FROM l0_sessions ORDER BY timestamp DESC, created_at DESC LIMIT ?1",
            )
            .map_err(sqlite_error)?;
        let rows = statement
            .query_map(params![limit.max(1) as i64], map_session_row)
            .map_err(sqlite_error)?;
        let mut records = Vec::new();
        for row in rows {
            records.push(row.map_err(sqlite_error)??);
        }
        Ok(records)
    }

    fn recent_for_session(&self, session_key: &str, limit: usize) -> Result<Vec<L0SessionRecord>> {
        let conn = self.conn()?;
        let mut statement = conn
            .prepare(
                "SELECT l0_index_id, session_key, timestamp, messages_json, source, indexed, created_at
                 FROM l0_sessions WHERE session_key = ?1
                 ORDER BY timestamp DESC, created_at DESC LIMIT ?2",
            )
            .map_err(sqlite_error)?;
        let rows = statement
            .query_map(params![session_key, limit.max(1) as i64], map_session_row)
            .map_err(sqlite_error)?;
        let mut records = Vec::new();
        for row in rows {
            records.push(row.map_err(sqlite_error)??);
        }
        records.reverse();
        Ok(records)
    }

    fn clear(&self, session_keys: Option<&[String]>) -> Result<usize> {
        let conn = self.conn()?;
        match session_keys {
            Some(keys) if !keys.is_empty() => {
                let placeholders = vec!["?"; keys.len()].join(", ");
                let sql = format!("DELETE FROM l0_sessions WHERE session_key IN ({placeholders})");
                let args: Vec<&dyn rusqlite::ToSql> =
                    keys.iter().map(|key| key as &dyn rusqlite::ToSql).collect();
                conn.execute(&sql, args.as_slice()).map_err(sqlite_error)
            }
            _ => conn
                .execute("DELETE FROM l0_sessions", [])
                .map_err(sqlite_error),
        }
    }
}

fn map_session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<L0SessionRecord>> {
    let messages_json: String = row.get(3)?;
    let messages = serde_json::from_str(&messages_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(Ok(L0SessionRecord {
        l0_index_id: row.get(0)?,
        session_key: row.get(1)?,
        timestamp: row.get(2)?,
        messages,
        source: row.get(4)?,
        indexed: row.get::<_, i64>(5)? != 0,
        created_at: row.get(6)?,
    }))
}

impl StateStore for SqliteControlPlane {
    fn get(&self, key: &str) -> Result<Option<Value>> {
        let conn = self.conn()?;
        let raw: Option<String> = conn
            .query_row(
                "SELECT state_json FROM pipeline_state WHERE state_key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(sqlite_error)?;
        match raw {
            Some(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(MemoryError::from),
            None => Ok(None),
        }
    }

    fn set(&self, key: &str, value: Value) -> Result<()> {
        let payload = serde_json::to_string(&value)?;
        self.conn()?
            .execute(
                "INSERT INTO pipeline_state (state_key, state_json, updated_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(state_key) DO UPDATE SET
                    state_json = excluded.state_json,
                    updated_at = excluded.updated_at",
                params![key, payload, format_iso(chrono::Utc::now())],
            )
            .map_err(sqlite_error)?;
        Ok(())
    }

    fn remove(&self, key: &str) -> Result<()> {
        self.conn()?
            .execute(
                "DELETE FROM pipeline_state WHERE state_key = ?1",
                params![key],
            )
            .map_err(sqlite_error)?;
        Ok(())
    }

    fn entries(&self) -> Result<Vec<(String, Value)>> {
        let conn = self.conn()?;
        let mut statement = conn
            .prepare("SELECT state_key, state_json FROM pipeline_state ORDER BY state_key ASC")
            .map_err(sqlite_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sqlite_error)?;
        let mut entries = Vec::new();
        for row in rows {
            let (key, raw) = row.map_err(sqlite_error)?;
            let value = serde_json::from_str(&raw)?;
            entries.push((key, value));
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pd_memory_core::types::MemoryMessage;

    fn record(id: &str, session: &str, timestamp: &str) -> L0SessionRecord {
        L0SessionRecord {
            l0_index_id: id.into(),
            session_key: session.into(),
            timestamp: timestamp.into(),
            messages: vec![MemoryMessage::user("hello")],
            source: "test".into(),
            indexed: false,
            created_at: timestamp.into(),
        }
    }

    #[test]
    fn enqueue_is_idempotent() {
        let plane = SqliteControlPlane::open_in_memory().unwrap();
        plane
            .enqueue(&record("a", "s1", "2026-01-01T00:00:00.000Z"))
            .unwrap();
        plane
            .enqueue(&record("a", "s1", "2026-01-01T00:00:00.000Z"))
            .unwrap();
        assert_eq!(plane.count_pending_turns().unwrap(), 1);
    }

    #[test]
    fn pending_and_mark_indexed() {
        let plane = SqliteControlPlane::open_in_memory().unwrap();
        plane
            .enqueue(&record("a", "s1", "2026-01-01T00:00:00.000Z"))
            .unwrap();
        plane
            .enqueue(&record("b", "s2", "2026-01-02T00:00:00.000Z"))
            .unwrap();
        let pending = plane.pending(None, 10).unwrap();
        assert_eq!(pending.len(), 2);
        plane.mark_indexed(&["a".into()]).unwrap();
        assert_eq!(plane.count_pending_turns().unwrap(), 1);
        assert_eq!(
            plane.pending_session_keys().unwrap(),
            vec!["s2".to_string()]
        );
    }

    #[test]
    fn state_roundtrip() {
        let plane = SqliteControlPlane::open_in_memory().unwrap();
        assert!(plane.get("k").unwrap().is_none());
        plane.set("k", serde_json::json!({"a": 1})).unwrap();
        assert_eq!(plane.get("k").unwrap().unwrap()["a"], 1);
        assert_eq!(plane.entries().unwrap().len(), 1);
        plane.remove("k").unwrap();
        assert!(plane.get("k").unwrap().is_none());
    }

    #[test]
    fn recent_for_session_returns_chronological_tail() {
        let plane = SqliteControlPlane::open_in_memory().unwrap();
        for (index, id) in ["a", "b", "c"].iter().enumerate() {
            plane
                .enqueue(&record(
                    id,
                    "s1",
                    &format!("2026-01-0{}T00:00:00.000Z", index + 1),
                ))
                .unwrap();
        }
        let recent = plane.recent_for_session("s1", 2).unwrap();
        assert_eq!(
            recent
                .iter()
                .map(|record| record.l0_index_id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "c"]
        );
    }
}
