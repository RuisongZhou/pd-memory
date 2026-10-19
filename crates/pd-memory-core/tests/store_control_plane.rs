//! FileControlPlane queue/state tests (task-1, dev A).

use std::fs;

use pd_memory_core::control_plane::FileControlPlane;
use pd_memory_core::ports::{QueueStore, StateStore};
use pd_memory_core::types::{L0SessionRecord, MemoryMessage};

fn record(id: &str, session: &str, timestamp: &str) -> L0SessionRecord {
    L0SessionRecord {
        l0_index_id: id.to_string(),
        session_key: session.to_string(),
        timestamp: timestamp.to_string(),
        messages: vec![MemoryMessage::user(format!("turn {id}"))],
        source: "test".to_string(),
        indexed: false,
        created_at: timestamp.to_string(),
    }
}

#[test]
fn enqueue_is_idempotent_by_l0_index_id() {
    let dir = tempfile::tempdir().unwrap();
    let plane = FileControlPlane::new(dir.path()).unwrap();
    plane
        .enqueue(&record("same-id", "s1", "2026-10-18T09:00:00.000Z"))
        .unwrap();
    let mut updated = record("same-id", "s1", "2026-10-18T09:00:00.000Z");
    updated.messages = vec![MemoryMessage::user("updated")];
    plane.enqueue(&updated).unwrap();
    let recent = plane.recent(10).unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].messages[0].content, "updated");
    // The JSONL file has exactly one line.
    let raw = fs::read_to_string(dir.path().join("control").join("l0_sessions.jsonl")).unwrap();
    assert_eq!(
        raw.lines().filter(|line| !line.trim().is_empty()).count(),
        1
    );
}

#[test]
fn pending_is_ordered_by_timestamp_and_filters_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let plane = FileControlPlane::new(dir.path()).unwrap();
    plane
        .enqueue(&record("b", "s2", "2026-10-18T10:00:00.000Z"))
        .unwrap();
    plane
        .enqueue(&record("a", "s1", "2026-10-18T09:00:00.000Z"))
        .unwrap();
    let mut indexed = record("c", "s1", "2026-10-18T11:00:00.000Z");
    indexed.indexed = true;
    plane.enqueue(&indexed).unwrap();

    let pending = plane.pending(None, 10).unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|item| item.l0_index_id.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b"]
    );
    assert_eq!(
        plane.pending(Some(&["s1".to_string()]), 10).unwrap().len(),
        1
    );
    assert_eq!(plane.pending(None, 0).unwrap().len(), 0);
    assert_eq!(
        plane.pending_session_keys().unwrap(),
        vec!["s1".to_string(), "s2".to_string()]
    );
    assert_eq!(plane.count_pending_turns().unwrap(), 2);
    assert_eq!(plane.recent_for_session("s1", 10).unwrap().len(), 2);
    assert_eq!(plane.recent(1).unwrap()[0].l0_index_id, "c");
}

#[test]
fn tolerates_torn_trailing_line() {
    let dir = tempfile::tempdir().unwrap();
    let plane = FileControlPlane::new(dir.path()).unwrap();
    plane
        .enqueue(&record("a", "s1", "2026-10-18T09:00:00.000Z"))
        .unwrap();
    let sessions = dir.path().join("control").join("l0_sessions.jsonl");
    let mut raw = fs::read_to_string(&sessions).unwrap();
    raw.push_str("{\"l0_index_id\":\"torn");
    fs::write(&sessions, raw).unwrap();
    // Reads ignore the torn trailing line.
    assert_eq!(plane.pending(None, 10).unwrap().len(), 1);
    // The next enqueue repairs the file before appending.
    plane
        .enqueue(&record("b", "s2", "2026-10-18T10:00:00.000Z"))
        .unwrap();
    assert_eq!(plane.pending(None, 10).unwrap().len(), 2);
}

#[test]
fn mark_indexed_and_clear() {
    let dir = tempfile::tempdir().unwrap();
    let plane = FileControlPlane::new(dir.path()).unwrap();
    plane
        .enqueue(&record("a", "s1", "2026-10-18T09:00:00.000Z"))
        .unwrap();
    plane
        .enqueue(&record("b", "s2", "2026-10-18T10:00:00.000Z"))
        .unwrap();
    plane.mark_indexed(&["a".to_string()]).unwrap();
    assert_eq!(plane.count_pending_turns().unwrap(), 1);
    assert_eq!(
        plane.pending_session_keys().unwrap(),
        vec!["s2".to_string()]
    );
    assert_eq!(plane.clear(None).unwrap(), 2);
    assert!(plane.recent(10).unwrap().is_empty());
    assert_eq!(plane.clear(Some(&["missing".to_string()])).unwrap(), 0);
}

#[test]
fn state_store_persists_atomically_and_orders_entries() {
    let dir = tempfile::tempdir().unwrap();
    let plane = FileControlPlane::new(dir.path()).unwrap();
    plane
        .set("lastDreamAt", serde_json::json!("2026-10-19T00:05:00.000Z"))
        .unwrap();
    plane
        .set("lastDreamStatus", serde_json::json!("success"))
        .unwrap();
    plane
        .set(
            "lastIndexedAt",
            serde_json::json!("2026-10-19T00:04:00.000Z"),
        )
        .unwrap();

    // Reopening the control plane sees the same state.
    let reopened = FileControlPlane::new(dir.path()).unwrap();
    assert_eq!(
        reopened.get("lastDreamStatus").unwrap(),
        Some(serde_json::json!("success"))
    );
    let entries = reopened.entries().unwrap();
    let keys: Vec<&str> = entries.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(
        keys,
        vec!["lastDreamAt", "lastDreamStatus", "lastIndexedAt"],
        "entries are ordered by key"
    );

    reopened.remove("lastDreamStatus").unwrap();
    assert_eq!(reopened.get("lastDreamStatus").unwrap(), None);
    // State file is valid JSON on disk.
    let raw = fs::read_to_string(dir.path().join("control").join("pipeline_state.json")).unwrap();
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(value.is_object());
    // Removing a missing key is a no-op.
    reopened.remove("lastDreamStatus").unwrap();
}

#[test]
fn missing_files_read_as_empty() {
    let dir = tempfile::tempdir().unwrap();
    let plane = FileControlPlane::new(dir.path()).unwrap();
    assert!(plane.entries().unwrap().is_empty());
    assert!(plane.recent(10).unwrap().is_empty());
    assert_eq!(plane.count_pending_turns().unwrap(), 0);
    assert!(plane.get("anything").unwrap().is_none());
}
