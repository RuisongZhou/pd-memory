//! Heartbeat indexing pipeline tests (offline, in-memory stores).
//!
//! Covers the classify → create → upsert flow, focus-turn selection, prefix
//! diffing, redaction, batch accounting, manifest rebuild-once, session_keys
//! filtering and replay-on-failure semantics.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use pd_memory_core::config::MemoryConfig;
use pd_memory_core::error::{MemoryError, Result};
use pd_memory_core::extraction::LlmExtractor;
use pd_memory_core::indexing::{run_heartbeat, IndexerDeps};
use pd_memory_core::ports::{Clock, MemoryStore, QueueStore, Redactor, StoreRoots, Telemetry};
use pd_memory_core::types::*;
use pd_memory_testkit::ScriptedModelProvider;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeQueueStore {
    records: Mutex<Vec<L0SessionRecord>>,
}

impl FakeQueueStore {
    fn new() -> Self {
        Self::default()
    }

    fn insert(&self, record: L0SessionRecord) {
        self.records.lock().unwrap().push(record);
    }

    fn pending_count(&self) -> usize {
        self.records
            .lock()
            .unwrap()
            .iter()
            .filter(|record| !record.indexed)
            .count()
    }
}

impl QueueStore for FakeQueueStore {
    fn enqueue(&self, record: &L0SessionRecord) -> Result<()> {
        self.records.lock().unwrap().push(record.clone());
        Ok(())
    }

    fn pending(
        &self,
        session_keys: Option<&[String]>,
        limit: usize,
    ) -> Result<Vec<L0SessionRecord>> {
        let mut records: Vec<L0SessionRecord> = self
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|record| !record.indexed)
            .cloned()
            .collect();
        if let Some(keys) = session_keys {
            records.retain(|record| keys.iter().any(|key| key == &record.session_key));
        }
        records.sort_by(|left, right| {
            left.timestamp
                .cmp(&right.timestamp)
                .then_with(|| left.created_at.cmp(&right.created_at))
        });
        records.truncate(limit);
        Ok(records)
    }

    fn pending_session_keys(&self) -> Result<Vec<String>> {
        let records = self.records.lock().unwrap();
        let mut keys: Vec<(String, String)> = Vec::new();
        for record in records.iter().filter(|record| !record.indexed) {
            match keys.iter_mut().find(|(key, _)| key == &record.session_key) {
                Some((_, earliest)) => {
                    if record.timestamp < *earliest {
                        *earliest = record.timestamp.clone();
                    }
                }
                None => keys.push((record.session_key.clone(), record.timestamp.clone())),
            }
        }
        keys.sort_by(|left, right| left.1.cmp(&right.1));
        Ok(keys.into_iter().map(|(key, _)| key).collect())
    }

    fn mark_indexed(&self, ids: &[String]) -> Result<()> {
        let mut records = self.records.lock().unwrap();
        for record in records.iter_mut() {
            if ids.contains(&record.l0_index_id) {
                record.indexed = true;
            }
        }
        Ok(())
    }

    fn count_pending_turns(&self) -> Result<usize> {
        // The reference counts pending L0 rows despite the name (spec §6.2).
        Ok(self.pending_count())
    }

    fn recent(&self, limit: usize) -> Result<Vec<L0SessionRecord>> {
        let mut records: Vec<L0SessionRecord> =
            self.records.lock().unwrap().iter().cloned().collect();
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
        let mut records: Vec<L0SessionRecord> = self
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|record| record.session_key == session_key)
            .cloned()
            .collect();
        records.sort_by(|left, right| {
            left.timestamp
                .cmp(&right.timestamp)
                .then_with(|| left.created_at.cmp(&right.created_at))
        });
        if records.len() > limit {
            records = records.split_off(records.len() - limit);
        }
        Ok(records)
    }

    fn clear(&self, session_keys: Option<&[String]>) -> Result<usize> {
        let mut records = self.records.lock().unwrap();
        let before = records.len();
        match session_keys {
            Some(keys) => {
                records.retain(|record| !keys.iter().any(|key| key == &record.session_key))
            }
            None => records.clear(),
        }
        Ok(before - records.len())
    }
}

struct FakeMemoryStore {
    roots: StoreRoots,
    candidates: Vec<MemoryCandidate>,
    entries: Vec<MemoryManifestEntry>,
    rebuild_count: usize,
    project_meta: Option<ProjectMetaRecord>,
    fail_session: Option<String>,
    fail_captured_at: Option<String>,
}

impl FakeMemoryStore {
    fn new(root: &std::path::Path) -> Self {
        Self {
            roots: StoreRoots {
                memory_dir: root.join("workspace").join("memory"),
                global_dir: root.join("global"),
                workspace_dir: root.join("workspace"),
            },
            candidates: Vec::new(),
            entries: Vec::new(),
            rebuild_count: 0,
            project_meta: None,
            fail_session: None,
            fail_captured_at: None,
        }
    }

    fn written_kinds(&self) -> Vec<MemoryRecordType> {
        self.candidates
            .iter()
            .map(|candidate| candidate.kind)
            .collect()
    }

    fn unsupported(method: &str) -> MemoryError {
        MemoryError::Unsupported(format!("fake memory store does not implement {method}"))
    }
}

impl MemoryStore for FakeMemoryStore {
    fn roots(&self) -> &StoreRoots {
        &self.roots
    }

    fn ensure_layout(&mut self) -> Result<()> {
        Ok(())
    }

    fn list_entries(&self, _options: &MemoryListOptions) -> Result<Vec<MemoryManifestEntry>> {
        Ok(self.entries.clone())
    }

    fn get_records(&self, _ids: &[String], _max_lines: usize) -> Result<Vec<MemoryFileRecord>> {
        Ok(Vec::new())
    }

    fn upsert_candidate(&mut self, candidate: &MemoryCandidate) -> Result<MemoryManifestEntry> {
        if (candidate.source_session_key == self.fail_session && self.fail_session.is_some())
            || (candidate.captured_at == self.fail_captured_at && self.fail_captured_at.is_some())
        {
            return Err(MemoryError::Other("scripted persistence failure".into()));
        }
        let number = self.candidates.len() + 1;
        let file = format!("note-{number}.md");
        let relative_path = format!("{}/{file}", candidate.kind.directory());
        let entry = MemoryManifestEntry {
            frontmatter: MemoryFileFrontmatter {
                name: candidate.name.clone(),
                description: candidate.description.clone(),
                kind: candidate.kind,
                scope: candidate.scope,
                captured_at: candidate.captured_at.clone(),
                source_session_key: candidate.source_session_key.clone(),
                updated_at: "2026-10-18T10:00:00.000Z".into(),
                ..Default::default()
            },
            file,
            relative_path: relative_path.clone(),
            absolute_path: format!("/fake/{relative_path}"),
        };
        self.candidates.push(candidate.clone());
        self.entries.push(entry.clone());
        Ok(entry)
    }

    fn upsert_user_profile(&mut self, _candidate: &MemoryCandidate) -> Result<MemoryManifestEntry> {
        Err(Self::unsupported("upsert_user_profile"))
    }

    fn rebuild_manifest(&mut self) -> Result<bool> {
        self.rebuild_count += 1;
        Ok(true)
    }

    fn manifest_text(&self) -> Result<Option<String>> {
        Ok(None)
    }

    fn get_project_meta(&self) -> Result<Option<ProjectMetaRecord>> {
        Ok(self.project_meta.clone())
    }

    fn set_project_meta(&mut self, meta: &ProjectMetaRecord) -> Result<()> {
        self.project_meta = Some(meta.clone());
        Ok(())
    }

    fn list_project_metas(&self) -> Result<Vec<ProjectMetaRecord>> {
        Ok(self.project_meta.iter().cloned().collect())
    }

    fn get_user_summary(&self) -> Result<MemoryUserSummary> {
        Ok(MemoryUserSummary::default())
    }

    fn overview(&self, _last_dream_at: Option<&str>) -> Result<MemoryOverview> {
        Ok(MemoryOverview::default())
    }

    fn edit_entry(
        &mut self,
        _id: &str,
        _name: &str,
        _description: &str,
        _fields: Option<&MemoryEntryEditFields>,
    ) -> Result<MemoryManifestEntry> {
        Err(Self::unsupported("edit_entry"))
    }

    fn delete_entries(&mut self, _ids: &[String]) -> Result<usize> {
        Ok(0)
    }

    fn set_deprecated(&mut self, _ids: &[String], _deprecated: bool) -> Result<usize> {
        Ok(0)
    }

    fn clear(&mut self, _scope: ClearMemoryScope) -> Result<MemoryTransferCounts> {
        Ok(MemoryTransferCounts::default())
    }

    fn export_bundle(&self) -> Result<MemoryExportBundle> {
        Ok(MemoryExportBundle::default())
    }

    fn import_bundle(&mut self, _bundle: &MemoryExportBundle) -> Result<MemoryImportResult> {
        Err(Self::unsupported("import_bundle"))
    }

    fn create_stage(&self, _tag: &str) -> Result<Box<dyn MemoryStore>> {
        Err(Self::unsupported("create_stage"))
    }

    fn commit_stage(
        &mut self,
        _stage: &dyn MemoryStore,
        _before: &DreamRuntimeState,
        _after: &DreamRuntimeState,
    ) -> Result<()> {
        Err(Self::unsupported("commit_stage"))
    }

    fn dispose_stage(&self, _stage: Box<dyn MemoryStore>) {}

    fn rollback_last_dream(&mut self, _current: &DreamRuntimeState) -> Result<DreamRollbackResult> {
        Err(Self::unsupported("rollback_last_dream"))
    }

    fn last_dream_snapshot(&self) -> Result<Option<Value>> {
        Ok(None)
    }
}

#[derive(Default)]
struct RecordingTelemetry {
    events: Mutex<Vec<(String, Value)>>,
}

impl RecordingTelemetry {
    fn names(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|(name, _)| name.clone())
            .collect()
    }
}

impl Telemetry for RecordingTelemetry {
    fn event(&self, name: &str, attributes: Value) {
        self.events
            .lock()
            .unwrap()
            .push((name.to_string(), attributes));
    }
}

struct IdentityRedactor;

impl Redactor for IdentityRedactor {
    fn redact(&self, text: &str) -> String {
        text.to_string()
    }
}

struct ReplacingRedactor {
    needle: &'static str,
    replacement: &'static str,
}

impl Redactor for ReplacingRedactor {
    fn redact(&self, text: &str) -> String {
        text.replace(self.needle, self.replacement)
    }
}

struct FixedClock(DateTime<Utc>);

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

fn fixed_clock() -> FixedClock {
    FixedClock(
        DateTime::parse_from_rfc3339("2026-10-18T10:00:00.000Z")
            .expect("valid timestamp")
            .with_timezone(&Utc),
    )
}

// ---------------------------------------------------------------------------
// Fixtures / helpers
// ---------------------------------------------------------------------------

fn config() -> MemoryConfig {
    MemoryConfig::for_data_root(std::env::temp_dir().join("pd-memory-heartbeat-tests"))
}

fn message(role: &str, content: &str) -> MemoryMessage {
    MemoryMessage::new(role, content)
}

fn session_record(
    session_key: &str,
    timestamp: &str,
    messages: Vec<MemoryMessage>,
) -> L0SessionRecord {
    L0SessionRecord {
        l0_index_id: format!("{session_key}_{timestamp}_raw"),
        session_key: session_key.to_string(),
        timestamp: timestamp.to_string(),
        messages,
        source: "test".into(),
        indexed: false,
        created_at: timestamp.to_string(),
    }
}

fn user_turn_for(label: &str) -> Vec<MemoryMessage> {
    vec![
        message(
            "user",
            &format!("我们继续推进 {label} 项目，目前在做索引层"),
        ),
        message("assistant", "好的，我记下了"),
    ]
}

fn classify_response(kind: &str) -> String {
    json!({
        "should_store": true,
        "labels": [{"type": kind, "reason": "durable fact", "evidence": "quote"}]
    })
    .to_string()
}

fn create_response(kind: &str) -> String {
    json!({
        "skip": false,
        "reason": "",
        "name": format!("{kind} note"),
        "description": format!("{kind} description"),
        "markdown": format!("## Summary\n{kind} body"),
    })
    .to_string()
}

fn create_schema(kind: &str) -> &'static str {
    match kind {
        "user" => "memory_create_user",
        "project" => "memory_create_project",
        _ => "memory_create_feedback",
    }
}

fn push_note_flow(provider: &ScriptedModelProvider, kind: &str) {
    provider.push(Some("memory_classify"), classify_response(kind));
    provider.push(Some(create_schema(kind)), create_response(kind));
}

#[allow(clippy::too_many_arguments)]
fn run_batch(
    config: &MemoryConfig,
    store: &mut FakeMemoryStore,
    queue: &FakeQueueStore,
    provider: &Arc<ScriptedModelProvider>,
    clock: &FixedClock,
    telemetry: &RecordingTelemetry,
    redactor: &dyn Redactor,
    session_keys: Option<Vec<String>>,
) -> Result<HeartbeatStats> {
    let extractor = LlmExtractor::new(provider.clone());
    let temp = tempfile::tempdir().unwrap();
    let state = pd_memory_core::control_plane::FileControlPlane::new(temp.path()).unwrap();
    run_heartbeat(
        IndexerDeps {
            config,
            store,
            queue,
            state: &state,
            extractor: &extractor,
            clock,
            telemetry,
            redactor,
            session_keys,
        },
        config.heartbeat_batch_size,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn heartbeat_classifies_creates_and_upserts_notes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = FakeMemoryStore::new(temp.path());
    let queue = FakeQueueStore::new();
    let record = session_record("s1", "2026-10-18T09:12:33.412Z", user_turn_for("Boreal"));
    let expected_timestamp = record.timestamp.clone();
    queue.insert(record);

    let provider = Arc::new(ScriptedModelProvider::new());
    push_note_flow(&provider, "project");
    let clock = fixed_clock();
    let telemetry = RecordingTelemetry::default();

    let stats = run_batch(
        &config(),
        &mut store,
        &queue,
        &provider,
        &clock,
        &telemetry,
        &IdentityRedactor,
        None,
    )
    .expect("heartbeat succeeds");

    assert_eq!(stats.processed_sessions, 1);
    assert_eq!(stats.processed_turns, 1);
    assert_eq!(stats.written_files, 1);
    assert_eq!(stats.skipped_turns, 0);
    assert!(stats.errors.is_empty());
    assert_eq!(stats.started_at, clock.now_iso());

    assert_eq!(store.written_kinds(), vec![MemoryRecordType::Project]);
    let candidate = &store.candidates[0];
    assert_eq!(candidate.scope, MemoryScope::Project);
    assert_eq!(candidate.name, "project note");
    assert_eq!(candidate.body, "## Summary\nproject body");
    assert_eq!(
        candidate.captured_at.as_deref(),
        Some(expected_timestamp.as_str())
    );
    assert_eq!(candidate.source_session_key.as_deref(), Some("s1"));
    assert_eq!(store.rebuild_count, 1);
    assert_eq!(queue.pending_count(), 0);

    let names = telemetry.names();
    for expected in [
        "memory.index.batch_start",
        "memory.index.classified",
        "memory.index.note_written",
        "memory.index.session_done",
        "memory.index.finish",
    ] {
        assert!(
            names.iter().any(|name| name == expected),
            "missing telemetry event {expected} in {names:?}"
        );
    }
}

#[test]
fn heartbeat_multiple_sessions_rebuild_manifest_once() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = FakeMemoryStore::new(temp.path());
    let queue = FakeQueueStore::new();
    queue.insert(session_record(
        "s1",
        "2026-10-18T09:12:33.412Z",
        user_turn_for("Boreal"),
    ));
    queue.insert(session_record(
        "s2",
        "2026-10-18T09:13:33.412Z",
        user_turn_for("Orion"),
    ));

    let provider = Arc::new(ScriptedModelProvider::new());
    push_note_flow(&provider, "project");
    push_note_flow(&provider, "feedback");
    let telemetry = RecordingTelemetry::default();

    let stats = run_batch(
        &config(),
        &mut store,
        &queue,
        &provider,
        &fixed_clock(),
        &telemetry,
        &IdentityRedactor,
        None,
    )
    .expect("heartbeat succeeds");

    assert_eq!(stats.processed_sessions, 2);
    assert_eq!(stats.processed_turns, 2);
    assert_eq!(stats.written_files, 2);
    assert_eq!(store.rebuild_count, 1);
    assert_eq!(queue.pending_count(), 0);
}

#[test]
fn heartbeat_indexes_user_turn_without_assistant_reply() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = FakeMemoryStore::new(temp.path());
    let queue = FakeQueueStore::new();
    queue.insert(session_record(
        "s1",
        "2026-10-18T09:12:33.412Z",
        vec![message("user", "只有用户消息，没有回复")],
    ));
    let provider = Arc::new(ScriptedModelProvider::new());
    push_note_flow(&provider, "feedback");
    let telemetry = RecordingTelemetry::default();

    let stats = run_batch(
        &config(),
        &mut store,
        &queue,
        &provider,
        &fixed_clock(),
        &telemetry,
        &IdentityRedactor,
        None,
    )
    .expect("heartbeat succeeds");

    assert_eq!(stats.processed_sessions, 1);
    assert_eq!(stats.processed_turns, 1);
    assert_eq!(stats.skipped_turns, 0);
    assert_eq!(stats.written_files, 1);
    assert_eq!(provider.request_count(), 2);
    assert_eq!(queue.pending_count(), 0);
    assert_eq!(store.rebuild_count, 1);
    assert!(telemetry
        .names()
        .iter()
        .any(|name| name == "memory.index.note_written"));
}

#[test]
fn heartbeat_model_failure_marks_record_indexed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = FakeMemoryStore::new(temp.path());
    let queue = FakeQueueStore::new();
    queue.insert(session_record(
        "s1",
        "2026-10-18T09:12:33.412Z",
        user_turn_for("Boreal"),
    ));
    let provider = Arc::new(ScriptedModelProvider::new()); // no scripted responses
    let telemetry = RecordingTelemetry::default();

    let stats = run_batch(
        &config(),
        &mut store,
        &queue,
        &provider,
        &fixed_clock(),
        &telemetry,
        &IdentityRedactor,
        None,
    )
    .unwrap();
    assert!(stats.errors.is_empty());
    assert_eq!(stats.processed_sessions, 1);
    assert_eq!(queue.pending_count(), 0);
    assert!(store.candidates.is_empty());
}

#[test]
fn heartbeat_replays_failed_sessions_on_next_run() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = FakeMemoryStore::new(temp.path());
    let queue = FakeQueueStore::new();
    queue.insert(session_record(
        "s1",
        "2026-10-18T09:12:33.412Z",
        user_turn_for("Boreal"),
    ));
    queue.insert(session_record(
        "s2",
        "2026-10-18T09:13:33.412Z",
        user_turn_for("Orion"),
    ));
    let provider = Arc::new(ScriptedModelProvider::new());
    push_note_flow(&provider, "project");
    push_note_flow(&provider, "project");
    store.fail_session = Some("s2".into());
    let telemetry = RecordingTelemetry::default();
    let clock = fixed_clock();

    let stats = run_batch(
        &config(),
        &mut store,
        &queue,
        &provider,
        &clock,
        &telemetry,
        &IdentityRedactor,
        None,
    )
    .expect("persistence failures are reported in batch stats");
    assert_eq!(stats.errors.len(), 1);
    assert_eq!(store.candidates.len(), 1);
    assert_eq!(queue.pending_count(), 1);
    assert!(!queue.records.lock().unwrap()[1].indexed);

    // Second run only replays the failed record.
    store.fail_session = None;
    push_note_flow(&provider, "feedback");
    let stats = run_batch(
        &config(),
        &mut store,
        &queue,
        &provider,
        &clock,
        &telemetry,
        &IdentityRedactor,
        None,
    )
    .expect("replay succeeds");
    assert_eq!(stats.processed_sessions, 1);
    assert_eq!(stats.written_files, 1);
    assert_eq!(store.candidates.len(), 2);
    assert_eq!(
        store.candidates[1].source_session_key.as_deref(),
        Some("s2")
    );
    assert_eq!(queue.pending_count(), 0);
}

#[test]
fn heartbeat_respects_session_keys_filter() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = FakeMemoryStore::new(temp.path());
    let queue = FakeQueueStore::new();
    queue.insert(session_record(
        "s1",
        "2026-10-18T09:12:33.412Z",
        user_turn_for("Boreal"),
    ));
    queue.insert(session_record(
        "s2",
        "2026-10-18T09:13:33.412Z",
        user_turn_for("Orion"),
    ));
    let provider = Arc::new(ScriptedModelProvider::new());
    push_note_flow(&provider, "project");
    let telemetry = RecordingTelemetry::default();

    let stats = run_batch(
        &config(),
        &mut store,
        &queue,
        &provider,
        &fixed_clock(),
        &telemetry,
        &IdentityRedactor,
        Some(vec!["s2".to_string()]),
    )
    .expect("heartbeat succeeds");

    assert_eq!(stats.processed_sessions, 1);
    assert_eq!(store.candidates.len(), 1);
    assert_eq!(
        store.candidates[0].source_session_key.as_deref(),
        Some("s2")
    );
    assert_eq!(queue.pending_count(), 1);
}

#[test]
fn heartbeat_redacts_message_content_before_prompts() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = FakeMemoryStore::new(temp.path());
    let queue = FakeQueueStore::new();
    queue.insert(session_record(
        "s1",
        "2026-10-18T09:12:33.412Z",
        vec![
            message("user", "请记住我的 key 是 secret-token 不要泄露"),
            message("assistant", "好的"),
        ],
    ));
    let provider = Arc::new(ScriptedModelProvider::new());
    push_note_flow(&provider, "project");
    let telemetry = RecordingTelemetry::default();
    let redactor = ReplacingRedactor {
        needle: "secret-token",
        replacement: "[redacted]",
    };

    run_batch(
        &config(),
        &mut store,
        &queue,
        &provider,
        &fixed_clock(),
        &telemetry,
        &redactor,
        None,
    )
    .expect("heartbeat succeeds");

    let log = provider.log.lock().unwrap();
    assert!(log.len() >= 2);
    for request in log.iter() {
        assert!(!request.user.contains("secret-token"), "raw secret leaked");
        assert!(!request.system.contains("secret-token"));
    }
    assert!(log[0].user.contains("[redacted]"));
}

#[test]
fn heartbeat_prefix_diff_indexes_only_new_turns() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = FakeMemoryStore::new(temp.path());
    let queue = FakeQueueStore::new();

    // Already indexed snapshot: [u1, a1].
    let mut indexed = session_record(
        "s1",
        "2026-10-18T09:00:00.000Z",
        vec![
            message("user", "第一轮：先聊项目背景"),
            message("assistant", "收到"),
        ],
    );
    indexed.indexed = true;
    queue.insert(indexed);

    // New cumulative snapshot: [u1, a1, u2, a2].
    queue.insert(session_record(
        "s1",
        "2026-10-18T09:01:00.000Z",
        vec![
            message("user", "第一轮：先聊项目背景"),
            message("assistant", "收到"),
            message("user", "第二轮：现在开始做索引层"),
            message("assistant", "好的"),
        ],
    ));

    let provider = Arc::new(ScriptedModelProvider::new());
    push_note_flow(&provider, "project");
    let telemetry = RecordingTelemetry::default();

    let stats = run_batch(
        &config(),
        &mut store,
        &queue,
        &provider,
        &fixed_clock(),
        &telemetry,
        &IdentityRedactor,
        None,
    )
    .expect("heartbeat succeeds");

    assert_eq!(stats.processed_turns, 1);
    let classify_requests: Vec<_> = provider
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.schema_name == "memory_classify")
        .cloned()
        .collect();
    assert_eq!(classify_requests.len(), 1);
    let prompt: Value = serde_json::from_str(&classify_requests[0].user).expect("prompt json");
    assert!(prompt["focus_user_turn"]["content"]
        .as_str()
        .expect("content")
        .contains("第二轮"));
    assert_eq!(store.candidates.len(), 1);
    assert_eq!(
        store.candidates[0].captured_at.as_deref(),
        Some("2026-10-18T09:01:00.000Z")
    );
}

#[test]
fn heartbeat_user_note_flow_writes_global_scope() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = FakeMemoryStore::new(temp.path());
    let queue = FakeQueueStore::new();
    queue.insert(session_record(
        "s1",
        "2026-10-18T09:12:33.412Z",
        vec![
            message("user", "我是算法工程师，长期做推理优化"),
            message("assistant", "明白"),
        ],
    ));
    let provider = Arc::new(ScriptedModelProvider::new());
    push_note_flow(&provider, "user");
    let telemetry = RecordingTelemetry::default();

    let stats = run_batch(
        &config(),
        &mut store,
        &queue,
        &provider,
        &fixed_clock(),
        &telemetry,
        &IdentityRedactor,
        None,
    )
    .expect("heartbeat succeeds");

    assert_eq!(stats.written_files, 1);
    assert_eq!(store.candidates[0].kind, MemoryRecordType::User);
    assert_eq!(store.candidates[0].scope, MemoryScope::Global);
}

#[test]
fn heartbeat_isolates_persistence_failure_to_one_record_in_the_same_session() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = FakeMemoryStore::new(temp.path());
    let queue = FakeQueueStore::new();
    let provider = Arc::new(ScriptedModelProvider::new());
    for (timestamp, text) in [
        ("2026-10-18T09:10:00.000Z", "First project fact"),
        ("2026-10-18T09:11:00.000Z", "Second project fact"),
        ("2026-10-18T09:12:00.000Z", "Third project fact"),
    ] {
        queue.insert(session_record(
            "same",
            timestamp,
            vec![MemoryMessage::user(text)],
        ));
        push_note_flow(&provider, "project");
    }
    store.fail_captured_at = Some("2026-10-18T09:11:00.000Z".into());
    let stats = run_batch(
        &config(),
        &mut store,
        &queue,
        &provider,
        &fixed_clock(),
        &RecordingTelemetry::default(),
        &IdentityRedactor,
        None,
    )
    .unwrap();
    assert_eq!(stats.processed_sessions, 2);
    assert_eq!(stats.written_files, 2);
    assert_eq!(stats.errors.len(), 1);
    let records = queue.records.lock().unwrap();
    assert_eq!(
        records.iter().map(|r| r.indexed).collect::<Vec<_>>(),
        vec![true, false, true]
    );
    assert_eq!(provider.request_count(), 6);
}

#[test]
fn configured_batch_size_is_not_silently_capped_at_thirty() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = FakeMemoryStore::new(temp.path());
    let queue = FakeQueueStore::new();
    let provider = Arc::new(ScriptedModelProvider::new());
    for n in 0..31 {
        queue.insert(session_record(
            &format!("s-{n}"),
            "2026-10-18T09:00:00.000Z",
            vec![MemoryMessage::user("project fact")],
        ));
        provider.push(
            Some("memory_classify"),
            r#"{"should_store":false,"labels":[]}"#,
        );
    }
    let mut config = config();
    config.heartbeat_batch_size = 31;
    let stats = run_batch(
        &config,
        &mut store,
        &queue,
        &provider,
        &fixed_clock(),
        &RecordingTelemetry::default(),
        &IdentityRedactor,
        None,
    )
    .unwrap();
    assert_eq!(stats.processed_sessions, 31);
    assert_eq!(queue.pending_count(), 0);
}

#[test]
fn prefix_lookup_keeps_the_previous_row_with_more_than_two_hundred_pending_rows() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = FakeMemoryStore::new(temp.path());
    let queue = FakeQueueStore::new();
    let mut previous = session_record(
        "same",
        "2026-10-18T09:00:00.000Z",
        vec![MemoryMessage::user("old fact")],
    );
    previous.indexed = true;
    queue.insert(previous);
    for n in 1..=201 {
        let at = chrono::DateTime::parse_from_rfc3339("2026-10-18T09:00:00.000Z").unwrap()
            + chrono::Duration::seconds(n);
        queue.insert(session_record(
            "same",
            &at.to_rfc3339(),
            vec![
                MemoryMessage::user("old fact"),
                MemoryMessage::assistant("noted"),
                MemoryMessage::user("new fact"),
            ],
        ));
    }
    let provider = Arc::new(ScriptedModelProvider::new());
    provider.push(
        Some("memory_classify"),
        r#"{"should_store":false,"labels":[]}"#,
    );
    let stats = run_batch(
        &config(),
        &mut store,
        &queue,
        &provider,
        &fixed_clock(),
        &RecordingTelemetry::default(),
        &IdentityRedactor,
        None,
    )
    .unwrap();
    assert_eq!(stats.processed_sessions, 201);
    assert_eq!(stats.processed_turns, 1);
    assert_eq!(provider.request_count(), 1);
}
