//! The engine facade: one object the host (CLI / MCP / hooks / Android JNI)
//! talks to.
//!
//! Threading contract: `MemoryEngine` is `Send + Sync`. All mutable state is
//! behind mutexes, so an Android JNI worker thread and an MCP server thread can
//! share one engine safely. The engine is **blocking**: callers must not invoke
//! it on a UI thread.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use sha1::{Digest, Sha1};

use crate::config::MemoryConfig;
use crate::control_plane::FileControlPlane;
use crate::error::{MemoryError, Result};
use crate::extraction::LlmExtractor;
use crate::files::FileMemoryStore;
use crate::message::NormalizeOptions;
use crate::ports::{
    Clock, MemoryStore, ModelProvider, NoopTelemetry, QueueStore, Redactor, StateStore,
    SystemClock, Telemetry,
};
use crate::recall::{self, RecallCache, RetrieveOptions};
use crate::redact::DefaultRedactor;
use crate::types::*;
use crate::{dream, indexing, message, scheduler};

// Pipeline-state keys, matching the TypeScript implementation's vocabulary.
pub const KEY_SETTINGS: &str = "indexingSettings";
pub const KEY_LAST_INDEXED_AT: &str = "lastIndexedAt";
pub const KEY_LAST_DREAM_AT: &str = "lastDreamAt";
pub const KEY_LAST_DREAM_STATUS: &str = "lastDreamStatus";
pub const KEY_LAST_DREAM_SUMMARY: &str = "lastDreamSummary";
pub const KEY_AUTO_INDEX_ANCHOR: &str = "autoIndexAnchorAt";
pub const KEY_AUTO_DREAM_ANCHOR: &str = "autoDreamAnchorAt";
pub const KEY_WORKSPACE_DIR: &str = "workspaceDir";
pub const KEY_RETRIEVAL_TRACES: &str = "retrievalTraces";
pub const KEY_INDEX_TRACES: &str = "indexTraces";
pub const KEY_DREAM_TRACES: &str = "dreamTraces";
pub const TRACE_RING_LIMIT: usize = 30;

/// Options for [`MemoryEngine::flush`].
#[derive(Debug, Clone)]
pub struct FlushOptions {
    pub batch_size: Option<usize>,
    pub session_keys: Option<Vec<String>>,
    /// Free-form reason recorded in traces (`manual`, `hook:Stop`, ...).
    pub reason: String,
}

impl Default for FlushOptions {
    fn default() -> Self {
        Self {
            batch_size: None,
            session_keys: None,
            reason: "manual".into(),
        }
    }
}

/// Fully injected engine dependencies. Hosts either use [`MemoryEngine::open`]
/// (file-backed defaults) or build parts explicitly (e.g. Android with a
/// custom store, or tests with a mock model).
pub struct MemoryEngineOptions {
    pub config: MemoryConfig,
    pub store: Box<dyn MemoryStore>,
    pub queue: Box<dyn QueueStore>,
    pub state: Box<dyn StateStore>,
    pub model: Arc<dyn ModelProvider>,
    pub clock: Arc<dyn Clock>,
    pub telemetry: Arc<dyn Telemetry>,
    pub redactor: Arc<dyn Redactor>,
}

/// The memory engine.
pub struct MemoryEngine {
    config: MemoryConfig,
    store: Mutex<Box<dyn MemoryStore>>,
    queue: Box<dyn QueueStore>,
    state: Box<dyn StateStore>,
    model: Arc<dyn ModelProvider>,
    clock: Arc<dyn Clock>,
    telemetry: Arc<dyn Telemetry>,
    redactor: Arc<dyn Redactor>,
    extractor: LlmExtractor,
    recall_cache: Mutex<RecallCache>,
}

impl std::fmt::Debug for MemoryEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryEngine")
            .field("workspace_key", &self.config.workspace_key())
            .field("data_root", &self.config.data_root)
            .finish_non_exhaustive()
    }
}

impl MemoryEngine {
    /// Build an engine with explicit ports.
    pub fn new(options: MemoryEngineOptions) -> Result<Self> {
        options.config.validate()?;
        let MemoryEngineOptions {
            config,
            store,
            queue,
            state,
            model,
            clock,
            telemetry,
            redactor,
        } = options;
        let extractor = LlmExtractor::new(model.clone());
        let engine = Self {
            config,
            store: Mutex::new(store),
            queue,
            state,
            model,
            clock,
            telemetry,
            redactor,
            extractor,
            recall_cache: Mutex::new(RecallCache::new()),
        };
        {
            let mut store = engine.lock_store()?;
            store.ensure_layout()?;
        }
        engine.reconcile_workspace_dir()?;
        Ok(engine)
    }

    /// Convenience constructor for the default file-backed deployment.
    pub fn open(config: MemoryConfig, model: Arc<dyn ModelProvider>) -> Result<Self> {
        let layout = config.store_layout();
        let control_dir = layout.roots.workspace_dir.clone();
        let store: Box<dyn MemoryStore> = Box::new(FileMemoryStore::new(layout.roots.clone()));
        // The control plane is used as both queue and state; two handles over
        // the same directory are safe (each write is atomic and locked).
        let queue: Box<dyn QueueStore> = Box::new(FileControlPlane::new(control_dir.clone())?);
        let state: Box<dyn StateStore> = Box::new(FileControlPlane::new(control_dir)?);
        let redactor = Arc::new(DefaultRedactor::new(config.redaction.clone()));
        Self::new(MemoryEngineOptions {
            config,
            store,
            queue,
            state,
            model,
            clock: Arc::new(SystemClock),
            telemetry: Arc::new(NoopTelemetry),
            redactor,
        })
    }

    fn lock_store(&self) -> Result<std::sync::MutexGuard<'_, Box<dyn MemoryStore>>> {
        self.store
            .lock()
            .map_err(|_| MemoryError::Other("memory store mutex poisoned".into()))
    }

    pub fn config(&self) -> &MemoryConfig {
        &self.config
    }

    pub fn model(&self) -> &Arc<dyn ModelProvider> {
        &self.model
    }

    pub fn store_roots(&self) -> Result<crate::ports::StoreRoots> {
        Ok(self.lock_store()?.roots().clone())
    }

    fn reconcile_workspace_dir(&self) -> Result<()> {
        let workspace_dir = self.config.store_layout().roots.workspace_dir;
        self.state.set(
            KEY_WORKSPACE_DIR,
            json!(workspace_dir.to_string_lossy().to_string()),
        )
    }

    // -----------------------------------------------------------------
    // Capture / indexing
    // -----------------------------------------------------------------

    /// Accept one turn (no LLM call). Like PilotDeck's facade, nonempty input
    /// returns `captured = true` even when the queue skips a duplicate.
    pub fn capture_turn(
        &self,
        raw_messages: &[MemoryMessage],
        session_key: &str,
        timestamp: Option<Timestamp>,
        source: &str,
    ) -> Result<CaptureTurnResult> {
        let options = NormalizeOptions {
            include_assistant: self.config.include_assistant,
            max_message_chars: self.config.max_message_chars,
            capture_strategy: self.config.capture_strategy,
        };
        let normalized = message::normalize_messages(raw_messages, &options);
        let normalized: Vec<MemoryMessage> = normalized
            .into_iter()
            .map(|mut message| {
                message.content = self.redactor.redact(&message.content);
                message
            })
            .filter(|message| !message.content.trim().is_empty())
            .collect();
        if normalized.is_empty() {
            return Ok(CaptureTurnResult {
                captured: false,
                normalized_messages: normalized,
                session_key: session_key.to_string(),
            });
        }

        let timestamp = timestamp.unwrap_or_else(|| self.clock.now_iso());

        // Parity with `captureL0Session` (heartbeat.ts:550-575): de-duplicate
        // against the globally most recent L0 record of the same session.
        if let Some(recent) = self.queue.recent(1)?.first() {
            if recent.session_key == session_key
                && !message::has_new_content(&recent.messages, &normalized)
            {
                return Ok(CaptureTurnResult {
                    captured: true,
                    normalized_messages: normalized,
                    session_key: session_key.to_string(),
                });
            }
        }

        let payload = serde_json::to_string(&normalized)?;
        let l0_index_id = {
            // id = `${sessionKey || "session"}_${sha1(`${timestamp}:${payload}`)[:10]}_raw`
            let mut hasher = Sha1::new();
            hasher.update(format!("{timestamp}:{payload}").as_bytes());
            let hash = crate::config::format_hash10(&hex::encode(hasher.finalize()));
            let session_prefix = if session_key.trim().is_empty() {
                "session"
            } else {
                session_key
            };
            format!("{session_prefix}_{hash}_raw")
        };
        let record = L0SessionRecord {
            l0_index_id,
            session_key: session_key.to_string(),
            timestamp,
            messages: normalized.clone(),
            source: source.to_string(),
            indexed: false,
            created_at: self.clock.now_iso(),
        };
        self.queue.enqueue(&record)?;
        self.telemetry.event(
            "memory.capture",
            json!({
                "session_key": session_key,
                "source": source,
                "message_count": normalized.len(),
            }),
        );
        Ok(CaptureTurnResult {
            captured: true,
            normalized_messages: normalized,
            session_key: session_key.to_string(),
        })
    }

    /// Run one indexing batch (LLM extraction). Blocking; can take seconds.
    pub fn flush(&self, options: FlushOptions) -> Result<HeartbeatStats> {
        if options.reason == "manual" {
            self.reconcile_auto_index_anchor(
                Some(&self.clock.now_iso()),
                options.session_keys.as_deref(),
            )?;
        }
        let batch_size = options
            .batch_size
            .unwrap_or(self.config.heartbeat_batch_size);
        let stats = {
            let mut store = self.lock_store()?;
            let deps = indexing::IndexerDeps {
                config: &self.config,
                store: &mut **store,
                queue: self.queue.as_ref(),
                state: self.state.as_ref(),
                extractor: &self.extractor,
                clock: self.clock.as_ref(),
                telemetry: self.telemetry.as_ref(),
                redactor: self.redactor.as_ref(),
                session_keys: options.session_keys.clone(),
            };
            indexing::run_heartbeat(deps, batch_size)?
        };
        self.push_trace(
            KEY_INDEX_TRACES,
            json!({
                "reason": options.reason,
                "stats": stats,
                "at": self.clock.now_iso(),
            }),
        )?;
        self.reconcile_auto_index_anchor(None, options.session_keys.as_deref())?;
        let fallback = (stats.written_files > 0).then(|| self.clock.now_iso());
        self.reconcile_auto_dream_anchor(None, fallback.as_deref())?;
        self.reset_recall_cache();
        Ok(stats)
    }

    /// Evaluate scheduling anchors and run whatever is due.
    pub fn run_due_scheduled_maintenance(&self, reason: &str) -> Result<MaintenanceResult> {
        let settings = self.settings()?;
        let now_ms = self.clock.now().timestamp_millis();
        let mut overview = self.overview()?;
        self.reconcile_auto_index_anchor(None, None)?;
        let decision = scheduler::decide(
            overview.pending_dialogue_turns,
            overview.pending_sessions,
            0,
            self.read_anchor_ms(KEY_AUTO_INDEX_ANCHOR)?,
            None,
            &settings,
            now_ms,
        );

        let mut result = MaintenanceResult::default();
        if decision.index_due {
            let stats = self.flush(FlushOptions {
                batch_size: None,
                session_keys: None,
                reason: {
                    let reason = if reason.starts_with("scheduled") {
                        reason.to_string()
                    } else {
                        format!("scheduled:{reason}")
                    };
                    if decision.index_reason == "pending_threshold" {
                        format!("{reason}:pending_threshold")
                    } else {
                        reason
                    }
                },
            })?;
            result.index_ran = true;
            result.index_stats = Some(stats);
            overview = self.overview()?;
        }
        self.reconcile_auto_dream_anchor(None, None)?;
        let dream_due = scheduler::decide(
            0,
            0,
            overview.changed_files_since_last_dream,
            None,
            self.read_anchor_ms(KEY_AUTO_DREAM_ANCHOR)?,
            &settings,
            now_ms,
        )
        .dream_due;
        if dream_due {
            let dream_result = self.dream(DreamTrigger::Scheduled)?;
            result.dream_ran = true;
            result.dream_result = Some(dream_result);
        }
        Ok(result)
    }

    // -----------------------------------------------------------------
    // Retrieval
    // -----------------------------------------------------------------

    /// Retrieve memory through the model gate and manifest selection, then
    /// render PilotDeck's four-section system context.
    pub fn retrieve_context(
        &self,
        query: &str,
        options: RetrieveOptions,
    ) -> Result<RetrievalResult> {
        let mut result = {
            let store = self.lock_store()?;
            recall::retrieve(
                &self.config,
                &**store,
                self.model.as_ref(),
                self.clock.as_ref(),
                &self.recall_cache,
                query,
                &options,
                self.state_string(KEY_LAST_DREAM_AT)?.as_deref(),
            )?
        };
        result.system_context = result
            .context
            .as_deref()
            .and_then(recall::build_memory_recall_system_context);
        self.push_trace(
            KEY_RETRIEVAL_TRACES,
            json!({
                "query": query,
                "route": result.route,
                "selected": result.selected_ids,
                "cache_hit": result.cache_hit,
                "at": self.clock.now_iso(),
            }),
        )?;
        self.telemetry.event(
            "memory.retrieve",
            json!({
                "route": result.route,
                "selected": result.selected_ids.len(),
                "cache_hit": result.cache_hit,
            }),
        );
        Ok(result)
    }

    /// Explicit search using the same model gate as automatic recall.
    pub fn search(&self, query: &str, recent: &[MemoryMessage]) -> Result<RetrievalResult> {
        self.retrieve_context(
            query,
            RetrieveOptions {
                recent_messages: recent.to_vec(),
                workspace_hint: None,
                explicit: true,
            },
        )
    }

    // -----------------------------------------------------------------
    // Consolidation
    // -----------------------------------------------------------------

    pub fn dream(&self, trigger: DreamTrigger) -> Result<DreamRunResult> {
        if trigger == DreamTrigger::Manual {
            self.reconcile_auto_dream_anchor(Some(&self.clock.now_iso()), None)?;
        }
        let prep = self.flush(FlushOptions {
            batch_size: None,
            session_keys: None,
            reason: format!("{:?}_dream_prep", trigger).to_lowercase(),
        })?;
        let before_state = self.dream_runtime_state()?;
        let outcome: Result<DreamRunResult> = (|| {
            let mut store = self.lock_store()?;
            let mut stage = store.create_stage("dream")?;
            let outcome = dream::run_dream(
                &self.config,
                self.model.as_ref(),
                self.clock.as_ref(),
                self.telemetry.as_ref(),
                stage.as_mut(),
                trigger,
            );
            match outcome {
                Ok(result) => {
                    if !result.is_no_op {
                        let after_state = DreamRuntimeState {
                            last_dream_at: Some(result.finished_at.clone()),
                            last_dream_status: Some(DreamPipelineStatus::Success),
                            last_dream_summary: Some(result.summary.clone()),
                        };
                        if let Err(error) =
                            store.commit_stage(stage.as_ref(), &before_state, &after_state)
                        {
                            store.dispose_stage(stage);
                            return Err(error);
                        }
                    } else {
                        store.dispose_stage(stage);
                    }
                    Ok(result)
                }
                Err(error) => {
                    store.dispose_stage(stage);
                    Err(error)
                }
            }
        })();

        let result = outcome?;
        self.state
            .set(KEY_LAST_DREAM_AT, json!(result.finished_at.clone()))?;
        self.state
            .set(KEY_LAST_DREAM_STATUS, json!(result.status))?;
        self.state
            .set(KEY_LAST_DREAM_SUMMARY, json!(result.summary.clone()))?;
        self.push_trace(
            KEY_DREAM_TRACES,
            json!({
                "trigger": trigger,
                "result": result,
                "at": self.clock.now_iso(),
                "prep_flush": prep,
            }),
        )?;
        self.reset_recall_cache();
        self.reconcile_auto_dream_anchor(None, None)?;
        Ok(result)
    }

    pub fn rollback_last_dream(&self) -> Result<DreamRollbackResult> {
        let current_state = self.dream_runtime_state()?;
        let result = {
            let mut store = self.lock_store()?;
            store.rollback_last_dream(&current_state)?
        };
        for (key, value) in [
            (
                KEY_LAST_DREAM_AT,
                result.last_dream_at.as_ref().map(|value| json!(value)),
            ),
            (
                KEY_LAST_DREAM_STATUS,
                result.last_dream_status.map(|value| json!(value)),
            ),
            (
                KEY_LAST_DREAM_SUMMARY,
                result.last_dream_summary.as_ref().map(|value| json!(value)),
            ),
        ] {
            match value {
                Some(value) => self.state.set(key, value)?,
                None => self.state.remove(key)?,
            }
        }
        self.reset_recall_cache();
        self.reconcile_auto_dream_anchor(Some(&result.rolled_back_at), None)?;
        Ok(result)
    }

    pub fn last_dream_snapshot(&self) -> Result<Option<Value>> {
        self.lock_store()?.last_dream_snapshot()
    }

    // -----------------------------------------------------------------
    // Administration
    // -----------------------------------------------------------------

    pub fn overview(&self) -> Result<MemoryOverview> {
        let last_dream_at = self.state_string(KEY_LAST_DREAM_AT)?;
        let store = self.lock_store()?;
        let mut overview = store.overview(last_dream_at.as_deref())?;
        overview.pending_sessions = self.queue.pending_session_keys()?.len();
        overview.pending_dialogue_turns = self.queue.count_pending_turns()?;
        overview.last_indexed_at = self.state_string(KEY_LAST_INDEXED_AT)?;
        overview.last_dream_at = last_dream_at;
        overview.last_dream_status = self
            .state
            .get(KEY_LAST_DREAM_STATUS)?
            .and_then(|value| serde_json::from_value(value).ok());
        overview.last_dream_summary = self.state_string(KEY_LAST_DREAM_SUMMARY)?;
        Ok(overview)
    }

    pub fn list(&self, options: &MemoryListOptions) -> Result<Vec<MemoryManifestEntry>> {
        self.lock_store()?.list_entries(options)
    }

    pub fn get(&self, ids: &[String], max_lines: usize) -> Result<Vec<MemoryFileRecord>> {
        self.lock_store()?.get_records(ids, max_lines)
    }

    pub fn act(&self, action: MemoryActionRequest) -> Result<MemoryActionResult> {
        let mut store = self.lock_store()?;
        let result = match action {
            MemoryActionRequest::EditProjectMeta {
                project_name,
                description,
                status,
            } => {
                let now = self.clock.now_iso();
                let existing = store.get_project_meta()?;
                let meta = ProjectMetaRecord {
                    project_id: existing
                        .as_ref()
                        .map(|meta| meta.project_id.clone())
                        .unwrap_or_else(|| "current_project".to_string()),
                    project_name,
                    description,
                    status,
                    source_kind: existing.as_ref().and_then(|meta| meta.source_kind.clone()),
                    source_workspace_path: existing
                        .as_ref()
                        .and_then(|meta| meta.source_workspace_path.clone()),
                    source_project_id: existing
                        .as_ref()
                        .and_then(|meta| meta.source_project_id.clone()),
                    created_at: existing
                        .as_ref()
                        .map(|meta| meta.created_at.clone())
                        .unwrap_or_else(|| now.clone()),
                    updated_at: now,
                    dream_updated_at: existing
                        .as_ref()
                        .and_then(|meta| meta.dream_updated_at.clone()),
                    relative_path: "project.meta.md".into(),
                    absolute_path: String::new(),
                };
                store.set_project_meta(&meta)?;
                store.rebuild_manifest()?;
                MemoryActionResult {
                    ok: true,
                    message: "project meta updated".into(),
                    project_meta: Some(meta),
                    ..Default::default()
                }
            }
            MemoryActionRequest::EditEntry {
                id,
                name,
                description,
                fields,
            } => {
                let entry = store.edit_entry(&id, &name, &description, fields.as_ref())?;
                store.rebuild_manifest()?;
                MemoryActionResult {
                    ok: true,
                    message: "entry updated".into(),
                    affected_ids: vec![entry.relative_path],
                    ..Default::default()
                }
            }
            MemoryActionRequest::DeleteEntries { ids } => {
                let deleted = store.delete_entries(&ids)?;
                store.rebuild_manifest()?;
                MemoryActionResult {
                    ok: true,
                    message: format!("{deleted} entries deleted"),
                    affected_ids: ids,
                    ..Default::default()
                }
            }
            MemoryActionRequest::DeprecateEntries { ids } => {
                let changed = store.set_deprecated(&ids, true)?;
                store.rebuild_manifest()?;
                MemoryActionResult {
                    ok: true,
                    message: format!("{changed} entries deprecated"),
                    affected_ids: ids,
                    ..Default::default()
                }
            }
            MemoryActionRequest::RestoreEntries { ids } => {
                let changed = store.set_deprecated(&ids, false)?;
                store.rebuild_manifest()?;
                MemoryActionResult {
                    ok: true,
                    message: format!("{changed} entries restored"),
                    affected_ids: ids,
                    ..Default::default()
                }
            }
        };
        self.reset_recall_cache();
        Ok(result)
    }

    pub fn get_project_meta(&self) -> Result<Option<ProjectMetaRecord>> {
        self.lock_store()?.get_project_meta()
    }

    pub fn update_project_meta(
        &self,
        project_name: &str,
        description: &str,
        status: &str,
    ) -> Result<ProjectMetaRecord> {
        let result = self.act(MemoryActionRequest::EditProjectMeta {
            project_name: project_name.to_string(),
            description: description.to_string(),
            status: status.to_string(),
        })?;
        result
            .project_meta
            .ok_or_else(|| MemoryError::Other("project meta update produced no record".into()))
    }

    pub fn get_user_summary(&self) -> Result<MemoryUserSummary> {
        self.lock_store()?.get_user_summary()
    }

    pub fn settings(&self) -> Result<IndexingSettings> {
        Ok(sanitize_settings(
            &self.state.get(KEY_SETTINGS)?.unwrap_or(Value::Null),
            &self.config.default_settings,
        ))
    }

    pub fn save_settings(&self, settings: IndexingSettings) -> Result<IndexingSettings> {
        let settings = sanitize_settings(&json!(settings), &self.config.default_settings);
        self.state.set(KEY_SETTINGS, json!(settings))?;
        self.reset_recall_cache();
        Ok(settings)
    }

    pub fn snapshot(&self, limit: usize) -> Result<MemoryUiSnapshot> {
        let overview = self.overview()?;
        let store = self.lock_store()?;
        let entries = store.list_entries(&MemoryListOptions {
            limit: Some(limit),
            include_deprecated: true,
            ..Default::default()
        })?;
        let user_summary = store.get_user_summary()?;
        Ok(MemoryUiSnapshot {
            overview,
            entries,
            user_summary,
            retrieval_traces: self.state_array(KEY_RETRIEVAL_TRACES)?,
            index_traces: self.state_array(KEY_INDEX_TRACES)?,
            dream_traces: self.state_array(KEY_DREAM_TRACES)?,
        })
    }

    pub fn export_bundle(&self) -> Result<MemoryExportBundle> {
        let mut bundle = self.lock_store()?.export_bundle()?;
        bundle.metadata.exported_at = self.clock.now_iso();
        bundle.metadata.last_indexed_at = self.state_string(KEY_LAST_INDEXED_AT)?;
        bundle.metadata.last_dream_at = self.state_string(KEY_LAST_DREAM_AT)?;
        bundle.metadata.last_dream_status = self
            .state
            .get(KEY_LAST_DREAM_STATUS)?
            .and_then(|value| serde_json::from_value(value).ok());
        bundle.metadata.last_dream_summary = self.state_string(KEY_LAST_DREAM_SUMMARY)?;
        Ok(bundle)
    }

    pub fn import_bundle(&self, bundle: &MemoryExportBundle) -> Result<MemoryImportResult> {
        let result = {
            let mut store = self.lock_store()?;
            store.import_bundle(bundle)?
        };
        self.queue.clear(None)?;
        for key in [
            KEY_INDEX_TRACES,
            KEY_DREAM_TRACES,
            KEY_RETRIEVAL_TRACES,
            "recentCaseTraces",
            "recentIndexTraces",
            "recentDreamTraces",
            KEY_LAST_INDEXED_AT,
            KEY_LAST_DREAM_AT,
            KEY_LAST_DREAM_STATUS,
            KEY_LAST_DREAM_SUMMARY,
        ] {
            self.state.remove(key)?;
        }
        if let Some(value) = result.metadata.last_indexed_at.clone() {
            self.state.set(KEY_LAST_INDEXED_AT, json!(value))?;
        }
        if let Some(value) = result.metadata.last_dream_at.clone() {
            self.state.set(KEY_LAST_DREAM_AT, json!(value))?;
        }
        if let Some(value) = result.metadata.last_dream_status {
            self.state.set(KEY_LAST_DREAM_STATUS, json!(value))?;
        }
        if let Some(value) = result.metadata.last_dream_summary.clone() {
            self.state.set(KEY_LAST_DREAM_SUMMARY, json!(value))?;
        }
        self.reset_recall_cache();
        Ok(result)
    }

    pub fn clear(&self, scope: ClearMemoryScope) -> Result<MemoryTransferCounts> {
        self.queue.clear(None)?;
        for (key, _) in self.state.entries()? {
            if scope == ClearMemoryScope::CurrentProject
                && [KEY_SETTINGS, KEY_WORKSPACE_DIR].contains(&key.as_str())
            {
                continue;
            }
            self.state.remove(&key)?;
        }
        let counts = {
            let mut store = self.lock_store()?;
            store.clear(scope)?
        };
        self.reset_recall_cache();
        Ok(counts)
    }

    pub fn state_get(&self, key: &str) -> Result<Option<Value>> {
        self.state.get(key)
    }

    // -----------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------

    fn dream_runtime_state(&self) -> Result<DreamRuntimeState> {
        Ok(DreamRuntimeState {
            last_dream_at: self.state_string(KEY_LAST_DREAM_AT)?,
            last_dream_status: self
                .state
                .get(KEY_LAST_DREAM_STATUS)?
                .and_then(|value| serde_json::from_value(value).ok()),
            last_dream_summary: self.state_string(KEY_LAST_DREAM_SUMMARY)?,
        })
    }

    fn reset_recall_cache(&self) {
        if let Ok(mut cache) = self.recall_cache.lock() {
            cache.clear();
        }
    }

    fn state_string(&self, key: &str) -> Result<Option<String>> {
        Ok(self.state.get(key)?.and_then(|value| {
            value
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
        }))
    }

    fn state_array(&self, key: &str) -> Result<Vec<Value>> {
        Ok(self
            .state
            .get(key)?
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default())
    }

    fn push_trace(&self, key: &str, entry: Value) -> Result<()> {
        let mut traces = self.state_array(key)?;
        traces.insert(0, entry);
        traces.truncate(TRACE_RING_LIMIT);
        self.state.set(key, Value::Array(traces))
    }

    fn read_anchor_ms(&self, key: &str) -> Result<Option<i64>> {
        let Some(value) = self.state_string(key)? else {
            return Ok(None);
        };
        Ok(chrono::DateTime::parse_from_rfc3339(&value)
            .ok()
            .map(|value| value.timestamp_millis()))
    }

    fn reconcile_auto_index_anchor(
        &self,
        manual_reset_at: Option<&str>,
        session_keys: Option<&[String]>,
    ) -> Result<()> {
        if let Some(at) = manual_reset_at {
            return self.state.set(KEY_AUTO_INDEX_ANCHOR, json!(at));
        }
        let session_keys = indexing::normalized_session_keys(session_keys);
        let pending = self.queue.pending(session_keys.as_deref(), 1)?;
        let Some(first) = pending.first() else {
            return self.state.remove(KEY_AUTO_INDEX_ANCHOR);
        };
        if self.state_string(KEY_AUTO_INDEX_ANCHOR)?.is_none() {
            self.state
                .set(KEY_AUTO_INDEX_ANCHOR, json!(first.timestamp))?;
        }
        Ok(())
    }

    fn reconcile_auto_dream_anchor(
        &self,
        manual_reset_at: Option<&str>,
        fallback_at: Option<&str>,
    ) -> Result<()> {
        let last_indexed_at = self.state_string(KEY_LAST_INDEXED_AT)?;
        let changed = self
            .lock_store()?
            .overview(self.state_string(KEY_LAST_DREAM_AT)?.as_deref())?
            .changed_files_since_last_dream;
        if last_indexed_at.is_none() || changed == 0 {
            return self.state.remove(KEY_AUTO_DREAM_ANCHOR);
        }
        if let Some(at) = manual_reset_at {
            return self.state.set(KEY_AUTO_DREAM_ANCHOR, json!(at));
        }
        if self.state_string(KEY_AUTO_DREAM_ANCHOR)?.is_none() {
            self.state.set(
                KEY_AUTO_DREAM_ANCHOR,
                json!(fallback_at.or(last_indexed_at.as_deref())),
            )?;
        }
        Ok(())
    }
}

fn sanitize_settings(value: &Value, defaults: &IndexingSettings) -> IndexingSettings {
    fn interval(value: &Value, fallback: u32) -> u32 {
        let numeric = value.as_f64().or_else(|| {
            let text = value.as_str()?.trim_start();
            let sign_len = usize::from(text.starts_with(['+', '-']));
            let digits = text[sign_len..]
                .bytes()
                .take_while(u8::is_ascii_digit)
                .count();
            (digits > 0)
                .then(|| text[..sign_len + digits].parse::<f64>().ok())
                .flatten()
        });
        numeric
            .filter(|value| value.is_finite())
            .map(|value| value.floor().clamp(0.0, 10_080.0) as u32)
            .unwrap_or(fallback)
    }
    IndexingSettings {
        reasoning_mode: if value["reasoning_mode"] == "accuracy_first" {
            ReasoningMode::AccuracyFirst
        } else {
            defaults.reasoning_mode
        },
        auto_index_interval_minutes: interval(
            &value["auto_index_interval_minutes"],
            defaults.auto_index_interval_minutes,
        ),
        auto_dream_interval_minutes: interval(
            &value["auto_dream_interval_minutes"],
            defaults.auto_dream_interval_minutes,
        ),
    }
}
