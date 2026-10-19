//! Pure-Rust control plane: L0 queue + pipeline state (dev A).
//!
//! Default on Android and in single-process CLI usage. Layout inside the
//! workspace directory:
//!
//! ```text
//! <workspace>/
//!   control/
//!     l0_sessions.jsonl   # append-only capture queue (one JSON object per line)
//!     pipeline_state.json # KV state, atomically rewritten
//!     .lock               # single-writer lock
//! ```
//!
//! Invariants:
//!
//! * enqueue is idempotent by `l0_index_id` (an existing record is replaced in
//!   place, mirroring SQLite `INSERT ... ON CONFLICT DO UPDATE`);
//! * every read tolerates a torn trailing JSONL line (an interrupted append);
//!   the next mutation repairs the file by rewriting it cleanly;
//! * state writes are full atomic rewrites (`tmp` + `rename`);
//! * mutations are serialised by a cross-platform advisory lock file created
//!   with `create_new`; a lock older than [`LOCK_STALE_AFTER`] is taken over.
//!
//! Reads do not take the lock: JSONL appends are line-atomic enough and the
//! state file is replaced by rename, so a reader can never observe a partial
//! state document.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::error::{MemoryError, Result};
use crate::ports::{QueueStore, StateStore};
use crate::types::{L0SessionRecord, Timestamp};

/// A lock file older than this is considered abandoned and is taken over.
pub const LOCK_STALE_AFTER: Duration = Duration::from_secs(5);
/// How long a mutation waits for a live lock before failing with
/// [`MemoryError::Locked`].
const LOCK_RETRY_TIMEOUT: Duration = Duration::from_secs(2);
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(10);

/// JSONL/JSON implementation of the two control-plane ports.
#[derive(Debug)]
pub struct FileControlPlane {
    pub workspace_dir: PathBuf,
    control_dir: PathBuf,
    sessions_path: PathBuf,
    state_path: PathBuf,
    lock_path: PathBuf,
}

impl FileControlPlane {
    /// Create (or open) the control plane for a workspace directory.
    pub fn new(workspace_dir: impl Into<PathBuf>) -> Result<Self> {
        let workspace_dir = workspace_dir.into();
        let control_dir = workspace_dir.join("control");
        fs::create_dir_all(&control_dir).map_err(|error| MemoryError::io(&control_dir, error))?;
        Ok(Self {
            sessions_path: control_dir.join("l0_sessions.jsonl"),
            state_path: control_dir.join("pipeline_state.json"),
            lock_path: control_dir.join(".lock"),
            control_dir,
            workspace_dir,
        })
    }

    /// Directory that holds the control plane files.
    pub fn control_dir(&self) -> &Path {
        &self.control_dir
    }

    fn acquire_lock(&self) -> Result<ControlLock> {
        ControlLock::acquire(&self.lock_path)
    }

    /// Read every valid JSONL record plus a flag telling whether the file ends
    /// with a torn (interrupted) trailing line.
    ///
    /// A malformed *trailing* line is ignored; a malformed line anywhere else
    /// is corruption.
    fn read_records(&self) -> Result<(Vec<L0SessionRecord>, bool)> {
        let raw = match fs::read_to_string(&self.sessions_path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Vec::new(), false))
            }
            Err(error) => return Err(MemoryError::io(&self.sessions_path, error)),
        };
        let torn = !raw.is_empty() && !raw.ends_with('\n');
        let lines: Vec<&str> = raw.split('\n').collect();
        let mut records: Vec<L0SessionRecord> = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<L0SessionRecord>(trimmed) {
                Ok(record) => records.push(record),
                Err(error) => {
                    let is_trailing = lines[index + 1..].iter().all(|line| line.trim().is_empty());
                    if is_trailing {
                        return Ok((records, true));
                    }
                    return Err(MemoryError::Corrupt {
                        path: self.sessions_path.clone(),
                        message: format!("invalid l0_sessions JSON on line {}: {error}", index + 1),
                    });
                }
            }
        }
        Ok((records, torn))
    }

    fn write_records(&self, records: &[L0SessionRecord]) -> Result<()> {
        let mut contents = String::new();
        for record in records {
            contents.push_str(&serde_json::to_string(record)?);
            contents.push('\n');
        }
        atomic_write(&self.sessions_path, contents.as_bytes())
    }

    fn append_record(&self, record: &L0SessionRecord) -> Result<()> {
        let line = serde_json::to_string(record)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.sessions_path)
            .map_err(|error| MemoryError::io(&self.sessions_path, error))?;
        file.write_all(line.as_bytes())
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.flush())
            .map_err(|error| MemoryError::io(&self.sessions_path, error))
    }

    fn read_state_map(&self) -> Result<serde_json::Map<String, Value>> {
        let raw = match fs::read_to_string(&self.state_path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(serde_json::Map::new())
            }
            Err(error) => return Err(MemoryError::io(&self.state_path, error)),
        };
        if raw.trim().is_empty() {
            return Ok(serde_json::Map::new());
        }
        match serde_json::from_str::<Value>(&raw)? {
            Value::Object(map) => Ok(map),
            _ => Err(MemoryError::Corrupt {
                path: self.state_path.clone(),
                message: "pipeline_state.json must be a JSON object".into(),
            }),
        }
    }

    fn write_state_map(&self, map: &serde_json::Map<String, Value>) -> Result<()> {
        let mut contents = serde_json::to_string_pretty(&Value::Object(map.clone()))?;
        contents.push('\n');
        atomic_write(&self.state_path, contents.as_bytes())
    }
}

impl QueueStore for FileControlPlane {
    fn enqueue(&self, record: &L0SessionRecord) -> Result<()> {
        let _lock = self.acquire_lock()?;
        let (mut records, torn) = self.read_records()?;
        if let Some(existing) = records
            .iter_mut()
            .find(|existing| existing.l0_index_id == record.l0_index_id)
        {
            *existing = record.clone();
            return self.write_records(&records);
        }
        if torn {
            // Repair the torn append before adding the new record.
            records.push(record.clone());
            return self.write_records(&records);
        }
        self.append_record(record)
    }

    fn pending(
        &self,
        session_keys: Option<&[String]>,
        limit: usize,
    ) -> Result<Vec<L0SessionRecord>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let (records, _) = self.read_records()?;
        let mut pending: Vec<L0SessionRecord> = records
            .into_iter()
            .filter(|record| !record.indexed)
            .filter(|record| match session_keys {
                Some(keys) => keys.iter().any(|key| key == &record.session_key),
                None => true,
            })
            .collect();
        pending.sort_by(|left, right| {
            left.timestamp
                .cmp(&right.timestamp)
                .then_with(|| left.created_at.cmp(&right.created_at))
        });
        pending.truncate(limit);
        Ok(pending)
    }

    fn pending_session_keys(&self) -> Result<Vec<String>> {
        let (records, _) = self.read_records()?;
        let mut first_timestamps: Vec<(String, String)> = Vec::new();
        for record in records {
            if record.indexed {
                continue;
            }
            match first_timestamps
                .iter_mut()
                .find(|(key, _)| key == &record.session_key)
            {
                Some((_, timestamp)) => {
                    if record.timestamp < *timestamp {
                        *timestamp = record.timestamp;
                    }
                }
                None => first_timestamps.push((record.session_key, record.timestamp)),
            }
        }
        first_timestamps
            .sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));
        Ok(first_timestamps.into_iter().map(|(key, _)| key).collect())
    }

    fn mark_indexed(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let _lock = self.acquire_lock()?;
        let (mut records, torn) = self.read_records()?;
        let mut changed = torn;
        for record in records.iter_mut() {
            if !record.indexed && ids.iter().any(|id| id == &record.l0_index_id) {
                record.indexed = true;
                changed = true;
            }
        }
        if changed {
            self.write_records(&records)?;
        }
        Ok(())
    }

    fn count_pending_turns(&self) -> Result<usize> {
        let (records, _) = self.read_records()?;
        Ok(records.iter().filter(|record| !record.indexed).count())
    }

    fn recent(&self, limit: usize) -> Result<Vec<L0SessionRecord>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let (mut records, _) = self.read_records()?;
        records.sort_by(|left, right| {
            right
                .timestamp
                .cmp(&left.timestamp)
                .then_with(|| right.created_at.cmp(&left.created_at))
        });
        records.truncate(limit);
        Ok(records)
    }

    fn recent_for_session(&self, session_key: &str, limit: usize) -> Result<Vec<L0SessionRecord>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let (records, _) = self.read_records()?;
        let mut session_records: Vec<L0SessionRecord> = records
            .into_iter()
            .filter(|record| record.session_key == session_key)
            .collect();
        session_records.sort_by(|left, right| {
            right
                .timestamp
                .cmp(&left.timestamp)
                .then_with(|| right.created_at.cmp(&left.created_at))
        });
        session_records.truncate(limit);
        Ok(session_records)
    }

    fn clear(&self, session_keys: Option<&[String]>) -> Result<usize> {
        let _lock = self.acquire_lock()?;
        let (records, torn) = self.read_records()?;
        let total = records.len();
        let kept: Vec<L0SessionRecord> = records
            .into_iter()
            .filter(|record| match session_keys {
                Some(keys) => !keys.iter().any(|key| key == &record.session_key),
                None => false,
            })
            .collect();
        let removed = total - kept.len();
        if removed > 0 || torn {
            self.write_records(&kept)?;
        }
        Ok(removed)
    }
}

impl StateStore for FileControlPlane {
    fn get(&self, key: &str) -> Result<Option<Value>> {
        Ok(self.read_state_map()?.get(key).cloned())
    }

    fn set(&self, key: &str, value: Value) -> Result<()> {
        let _lock = self.acquire_lock()?;
        let mut map = self.read_state_map()?;
        map.insert(key.to_string(), value);
        self.write_state_map(&map)
    }

    fn remove(&self, key: &str) -> Result<()> {
        let _lock = self.acquire_lock()?;
        let mut map = self.read_state_map()?;
        if map.remove(key).is_some() {
            self.write_state_map(&map)?;
        }
        Ok(())
    }

    fn entries(&self) -> Result<Vec<(String, Value)>> {
        let map = self.read_state_map()?;
        let mut entries: Vec<(String, Value)> = map.into_iter().collect();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(entries)
    }
}

/// ISO timestamp helper used by several control-plane implementations.
pub fn now_iso() -> Timestamp {
    crate::ports::format_iso(chrono::Utc::now())
}

// ---------------------------------------------------------------------------
// Lock file
// ---------------------------------------------------------------------------

struct ControlLock {
    path: PathBuf,
}

impl ControlLock {
    fn acquire(path: &Path) -> Result<Self> {
        let deadline = SystemTime::now() + LOCK_RETRY_TIMEOUT;
        loop {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    let _ = writeln!(file, "pid={} ts={}", std::process::id(), now_millis());
                    return Ok(Self {
                        path: path.to_path_buf(),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if is_lock_stale(path) {
                        let _ = fs::remove_file(path);
                        continue;
                    }
                    if SystemTime::now() >= deadline {
                        return Err(MemoryError::Locked(path.display().to_string()));
                    }
                    std::thread::sleep(LOCK_RETRY_INTERVAL);
                }
                Err(error) => return Err(MemoryError::io(path, error)),
            }
        }
    }
}

impl Drop for ControlLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// A lock is stale when either its mtime is older than [`LOCK_STALE_AFTER`]
/// or (on Linux/Android) the recorded owner pid no longer exists.
fn is_lock_stale(path: &Path) -> bool {
    let mtime_stale = match fs::metadata(path).and_then(|metadata| metadata.modified()) {
        Ok(modified) => SystemTime::now()
            .duration_since(modified)
            .map(|age| age >= LOCK_STALE_AFTER)
            .unwrap_or(false),
        // A lock file that vanished between the failed create and this check is
        // not stale: the retry loop will simply create it.
        Err(_) => return false,
    };
    if mtime_stale {
        return true;
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        if let Some(pid) = read_lock_pid(path) {
            let proc = PathBuf::from("/proc").join(pid.to_string());
            if !proc.exists() {
                return true;
            }
        }
    }
    false
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn read_lock_pid(path: &Path) -> Option<u32> {
    let content = fs::read_to_string(path).ok()?;
    for field in content.split_whitespace() {
        if let Some(value) = field.strip_prefix("pid=") {
            return value.parse().ok();
        }
    }
    None
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Atomic write
// ---------------------------------------------------------------------------

fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    fs::create_dir_all(&parent).map_err(|error| MemoryError::io(&parent, error))?;
    let temp = unique_temp_path(path);
    fs::write(&temp, contents).map_err(|error| MemoryError::io(&temp, error))?;
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(MemoryError::io(path, error));
    }
    Ok(())
}

fn unique_temp_path(path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "control".to_string());
    path.with_file_name(format!(".{name}.tmp-{nanos}-{counter}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{L0SessionRecord, MemoryMessage};

    fn record(id: &str, session: &str, timestamp: &str, indexed: bool) -> L0SessionRecord {
        L0SessionRecord {
            l0_index_id: id.to_string(),
            session_key: session.to_string(),
            timestamp: timestamp.to_string(),
            messages: vec![MemoryMessage::user("hello")],
            source: "test".into(),
            indexed,
            created_at: timestamp.to_string(),
        }
    }

    fn temp_plane() -> (tempfile::TempDir, FileControlPlane) {
        let dir = tempfile::tempdir().unwrap();
        let plane = FileControlPlane::new(dir.path()).unwrap();
        (dir, plane)
    }

    #[test]
    fn enqueue_is_idempotent() {
        let (_dir, plane) = temp_plane();
        let first = record("a", "s1", "t1", false);
        plane.enqueue(&first).unwrap();
        let mut second = first.clone();
        second.indexed = true;
        plane.enqueue(&second).unwrap();
        let all = plane.recent(10).unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].indexed);
    }

    #[test]
    fn tolerates_torn_trailing_line() {
        let (dir, plane) = temp_plane();
        plane.enqueue(&record("a", "s1", "t1", false)).unwrap();
        let sessions = dir.path().join("control").join("l0_sessions.jsonl");
        let mut raw = fs::read_to_string(&sessions).unwrap();
        raw.push_str(r#"{"l0_index_id":"broken""#);
        fs::write(&sessions, raw).unwrap();
        // The torn line is ignored when reading.
        let pending = plane.pending(None, 10).unwrap();
        assert_eq!(pending.len(), 1);
        // The next enqueue repairs the file, so both records are readable.
        plane.enqueue(&record("b", "s2", "t2", false)).unwrap();
        assert_eq!(plane.pending(None, 10).unwrap().len(), 2);
    }

    #[test]
    fn pending_orders_by_timestamp_and_filters() {
        let (_dir, plane) = temp_plane();
        plane
            .enqueue(&record("b", "s2", "2026-01-02", false))
            .unwrap();
        plane
            .enqueue(&record("a", "s1", "2026-01-01", false))
            .unwrap();
        plane
            .enqueue(&record("c", "s1", "2026-01-03", true))
            .unwrap();
        let pending = plane.pending(None, 10).unwrap();
        assert_eq!(
            pending
                .iter()
                .map(|item| item.l0_index_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        let filtered = plane.pending(Some(&["s1".to_string()]), 10).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(
            plane.pending_session_keys().unwrap(),
            vec!["s1".to_string(), "s2".to_string()]
        );
        assert_eq!(plane.count_pending_turns().unwrap(), 2);
    }

    #[test]
    fn marks_indexed_and_clears() {
        let (_dir, plane) = temp_plane();
        plane.enqueue(&record("a", "s1", "t1", false)).unwrap();
        plane.enqueue(&record("b", "s1", "t2", false)).unwrap();
        plane.mark_indexed(&["a".to_string()]).unwrap();
        assert_eq!(plane.count_pending_turns().unwrap(), 1);
        assert_eq!(plane.recent_for_session("s1", 10).unwrap().len(), 2);
        assert_eq!(plane.recent(1).unwrap()[0].l0_index_id, "b");
        assert_eq!(plane.clear(Some(&["s1".to_string()])).unwrap(), 2);
        assert!(plane.recent(10).unwrap().is_empty());
    }

    #[test]
    fn state_persists_across_handles() {
        let (dir, plane) = temp_plane();
        plane.set("lastDreamAt", serde_json::json!("t1")).unwrap();
        plane
            .set("indexingSettings", serde_json::json!({"x": 1}))
            .unwrap();
        plane.remove("indexingSettings").unwrap();
        let reopened = FileControlPlane::new(dir.path()).unwrap();
        assert_eq!(
            reopened.get("lastDreamAt").unwrap(),
            Some(serde_json::json!("t1"))
        );
        assert_eq!(reopened.get("indexingSettings").unwrap(), None);
        assert_eq!(reopened.entries().unwrap().len(), 1);
        // Missing state file reads as empty.
        let empty_dir = tempfile::tempdir().unwrap();
        let empty = FileControlPlane::new(empty_dir.path()).unwrap();
        assert!(empty.entries().unwrap().is_empty());
    }

    #[test]
    fn lock_file_is_released() {
        let (dir, plane) = temp_plane();
        plane.enqueue(&record("a", "s1", "t1", false)).unwrap();
        assert!(!dir.path().join("control").join(".lock").exists());
    }
}
