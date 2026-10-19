//! End-to-end indexing through the real engine + file-backed store.
//!
//! These tests exercise the write pipeline (capture → queue → heartbeat →
//! Markdown + `MEMORY.md`) against the default `FileMemoryStore` /
//! `FileControlPlane`, with a scripted model provider (offline, deterministic).

use std::sync::Arc;

use pd_memory_core::config::MemoryConfig;
use pd_memory_core::engine::{FlushOptions, MemoryEngine};
use pd_memory_core::types::MemoryMessage;
use pd_memory_testkit::ScriptedModelProvider;
use serde_json::json;

fn classify_project_response() -> String {
    json!({
        "should_store": true,
        "labels": [{"type": "project", "reason": "durable project fact", "evidence": "quote"}]
    })
    .to_string()
}

fn create_project_response() -> String {
    json!({
        "skip": false,
        "reason": "",
        "name": "Boreal",
        "description": "本地知识库整理工具",
        "markdown": "## Summary\n本地知识库整理工具\n\n## Current Stage\n设计阶段",
    })
    .to_string()
}

fn open_engine(temp: &tempfile::TempDir, provider: &Arc<ScriptedModelProvider>) -> MemoryEngine {
    let mut config = MemoryConfig::for_data_root(temp.path().join("data"));
    config.workspace_id = Some("test-workspace".into());
    MemoryEngine::open(config, provider.clone()).expect("engine opens")
}

fn capture_one_turn(engine: &MemoryEngine) {
    engine
        .capture_turn(
            &[
                MemoryMessage::user("我们项目叫 Boreal，目标是整理本地知识库，目前还在设计阶段"),
                MemoryMessage::assistant("好的，我记下了"),
            ],
            "session-1",
            Some("2026-10-18T09:12:33.412Z".into()),
            "test",
        )
        .expect("capture succeeds");
}

#[test]
fn engine_flush_writes_markdown_and_manifest() {
    let temp = tempfile::tempdir().expect("tempdir");
    let provider = Arc::new(ScriptedModelProvider::new());
    provider.push(Some("memory_classify"), classify_project_response());
    provider.push(Some("memory_create_project"), create_project_response());
    let engine = open_engine(&temp, &provider);

    capture_one_turn(&engine);
    let overview = engine.overview().expect("overview");
    assert_eq!(overview.pending_sessions, 1);
    assert_eq!(
        engine.snapshot(10).unwrap().overview.pending_dialogue_turns,
        1
    );

    let stats = engine
        .flush(FlushOptions::default())
        .expect("flush succeeds");
    assert_eq!(stats.processed_sessions, 1);
    assert_eq!(stats.processed_turns, 1);
    assert_eq!(stats.written_files, 1);

    let roots = engine.store_roots().expect("roots");
    assert!(roots.memory_dir.join("MEMORY.md").is_file());
    let project_files: Vec<_> = std::fs::read_dir(roots.memory_dir.join("Project"))
        .expect("project dir")
        .filter_map(|entry| entry.ok())
        .collect();
    assert_eq!(project_files.len(), 1);
    let content = std::fs::read_to_string(project_files[0].path()).expect("project file read");
    assert!(content.contains("type: project"), "{content}");
    assert!(
        content.contains("captured_at: 2026-10-18T09:12:33.412Z"),
        "{content}"
    );
    assert!(
        content.contains("source_session_key: session-1"),
        "{content}"
    );
    assert!(content.contains("## Current Stage"), "{content}");

    let entries = engine.list(&Default::default()).expect("entries list");
    assert_eq!(entries.len(), 1);
    assert!(entries[0].relative_path.starts_with("Project/"));

    let records = engine
        .get(&[entries[0].relative_path.clone()], 100)
        .expect("get record");
    assert_eq!(records.len(), 1);
    assert!(
        records[0].content.contains("本地知识库"),
        "record content: {:?}",
        records[0].content
    );

    // The L0 queue is drained and a no-op flush writes nothing.
    let overview = engine.overview().expect("overview after flush");
    assert_eq!(overview.pending_sessions, 0);
    let stats = engine.flush(FlushOptions::default()).expect("no-op flush");
    assert_eq!(stats.written_files, 0);
}

#[test]
fn engine_flush_marks_model_failure_indexed_and_updates_capture_cursor() {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedModelProvider::new());
    provider.push(Some("memory_classify"), classify_project_response());
    let engine = open_engine(&temp, &provider);
    capture_one_turn(&engine);
    let stats = engine.flush(FlushOptions::default()).unwrap();
    assert_eq!(stats.written_files, 0);
    assert!(stats.errors.is_empty());
    assert_eq!(engine.overview().unwrap().pending_sessions, 0);
    assert_eq!(
        engine.state_get("lastIndexedAt").unwrap(),
        Some(json!("2026-10-18T09:12:33.412Z"))
    );
    assert_eq!(
        engine.state_get("lastIndexedCursor:session-1").unwrap(),
        Some(json!("2026-10-18T09:12:33.412Z"))
    );
    assert_eq!(
        engine
            .flush(FlushOptions::default())
            .unwrap()
            .processed_turns,
        0
    );
    assert_eq!(provider.request_count(), 2);
}

#[test]
fn capture_matches_reference_facade_without_requeuing_duplicates() {
    let temp = tempfile::tempdir().expect("tempdir");
    let provider = Arc::new(ScriptedModelProvider::new());
    let engine = open_engine(&temp, &provider);
    let empty = engine
        .capture_turn(&[], "session-1", None, "test")
        .expect("empty input");
    assert!(!empty.captured);
    assert!(empty.normalized_messages.is_empty());
    assert_eq!(
        engine
            .overview()
            .expect("empty overview")
            .pending_dialogue_turns,
        0
    );

    capture_one_turn(&engine);
    capture_one_turn(&engine);
    assert_eq!(
        engine
            .overview()
            .expect("pending overview")
            .pending_dialogue_turns,
        1
    );
    provider.push(
        Some("memory_classify"),
        r#"{"should_store":false,"labels":[]}"#,
    );
    engine.flush(FlushOptions::default()).expect("flush");
    let messages = [
        MemoryMessage::user("我们项目叫 Boreal，目标是整理本地知识库，目前还在设计阶段"),
        MemoryMessage::assistant("好的，我记下了"),
    ];
    let duplicate = engine
        .capture_turn(&messages, "session-1", None, "test")
        .expect("duplicate capture");
    assert!(duplicate.captured);
    assert_eq!(duplicate.normalized_messages, messages);
    assert_eq!(
        engine
            .overview()
            .expect("drained overview")
            .pending_dialogue_turns,
        0
    );
    assert_eq!(
        engine
            .flush(FlushOptions::default())
            .expect("no-op flush")
            .written_files,
        0
    );
    assert_eq!(provider.request_count(), 1);
    let queue_path = engine
        .store_roots()
        .expect("roots")
        .workspace_dir
        .join("control/l0_sessions.jsonl");
    let queue = std::fs::read_to_string(queue_path).expect("queue");
    let rows: Vec<_> = queue.lines().collect();
    assert_eq!(rows.len(), 1);
    let row: serde_json::Value = serde_json::from_str(rows[0]).expect("L0 record");
    assert_eq!(row["indexed"], true);
}
