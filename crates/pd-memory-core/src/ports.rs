//! Ports: every host-facing capability the engine depends on is injected
//! through one of these traits. This is what makes the engine portable to
//! Android (or any other host) without pulling in a runtime, a database driver,
//! or an HTTP stack.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::error::{MemoryError, Result};
use crate::types::*;

// ---------------------------------------------------------------------------
// Model provider
// ---------------------------------------------------------------------------

/// A single JSON-mode completion request.
///
/// `schema_name` is the logical schema the response must satisfy
/// (`memory_classify`, `memory_create_project`, `memory_create_user`,
/// `memory_create_feedback`, `memory_route_project`, `memory_select_files`,
/// `dream_cluster`, `dream_merge`, `dream_user_profile`, ...). Adapters may use
/// it for provider-side response_format hints; the engine always validates and
/// repairs the returned text itself.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub system: String,
    pub user: String,
    pub schema_name: String,
    pub timeout: Duration,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompletionResponse {
    pub text: String,
    pub usage: Option<TokenUsage>,
}

/// The only LLM dependency of the engine.
///
/// Implementations must be blocking and thread-safe: the engine is synchronous
/// by design so it can run behind JNI on Android, inside a hook process, or in
/// a CLI without an async runtime.
pub trait ModelProvider: Send + Sync {
    fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse>;
}

// ---------------------------------------------------------------------------
// Storage ports
// ---------------------------------------------------------------------------

/// Resolved physical locations of a memory workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreRoots {
    /// Directory that holds `MEMORY.md`, `Project/`, `Feedback/`, `project.meta.md`.
    pub memory_dir: PathBuf,
    /// Directory that holds `UserIdentity/` and `UserIdentityNotes/`.
    pub global_dir: PathBuf,
    /// Parent workspace directory (holds control plane + `last_dream/`).
    pub workspace_dir: PathBuf,
}

/// Deterministic directory layout derived from [`crate::config::MemoryConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreLayout {
    pub roots: StoreRoots,
}

impl StoreLayout {
    /// `root` is the data root (e.g. `~/.pd-memory`); `workspace_key` is a
    /// stable id for the logical project (path hash or explicit id).
    pub fn new(root: impl Into<PathBuf>, workspace_key: &str) -> Self {
        let root = root.into();
        let workspace_dir = root.join("workspaces").join(workspace_key);
        let memory_dir = workspace_dir.join("memory");
        let global_dir = root.join("global");
        Self {
            roots: StoreRoots {
                memory_dir,
                global_dir,
                workspace_dir,
            },
        }
    }
}

/// File-native memory records. The default implementation is
/// [`crate::files::FileMemoryStore`] (Markdown + `MEMORY.md` manifest).
///
/// Implementations must be single-writer safe: the engine serialises writes
/// with its own lock, but methods may be called from multiple threads.
pub trait MemoryStore: Send + Sync {
    fn roots(&self) -> &StoreRoots;

    /// Create the directory skeleton and repair the manifest if needed.
    fn ensure_layout(&mut self) -> Result<()>;

    /// List manifest entries, newest first. `include_deprecated` must be
    /// honoured; the recall pipeline filters deprecated entries itself.
    fn list_entries(&self, options: &MemoryListOptions) -> Result<Vec<MemoryManifestEntry>>;

    /// Read full records by relative path id. Missing ids are skipped.
    fn get_records(&self, ids: &[String], max_lines: usize) -> Result<Vec<MemoryFileRecord>>;

    /// Insert or merge a candidate. Returns the written record id.
    fn upsert_candidate(&mut self, candidate: &MemoryCandidate) -> Result<MemoryManifestEntry>;

    /// Write the single global user profile (`global/UserIdentity/user-profile.md`).
    ///
    /// Used by the Dream user-profile rewrite (behavior spec §5.7). Unlike a
    /// note, the profile has a fixed path and is replaced wholesale. Must write
    /// atomically and rebuild the workspace manifest.
    fn upsert_user_profile(&mut self, candidate: &MemoryCandidate) -> Result<MemoryManifestEntry>;

    /// Rewrite `MEMORY.md` from the current directory contents.
    fn rebuild_manifest(&mut self) -> Result<bool>;

    /// Current `MEMORY.md` text, if present.
    fn manifest_text(&self) -> Result<Option<String>>;

    fn get_project_meta(&self) -> Result<Option<ProjectMetaRecord>>;
    fn set_project_meta(&mut self, meta: &ProjectMetaRecord) -> Result<()>;
    fn list_project_metas(&self) -> Result<Vec<ProjectMetaRecord>>;

    fn get_user_summary(&self) -> Result<MemoryUserSummary>;

    fn overview(&self, last_dream_at: Option<&str>) -> Result<MemoryOverview>;

    fn edit_entry(
        &mut self,
        id: &str,
        name: &str,
        description: &str,
        fields: Option<&MemoryEntryEditFields>,
    ) -> Result<MemoryManifestEntry>;

    /// Delete records by id; returns number of removed files.
    fn delete_entries(&mut self, ids: &[String]) -> Result<usize>;

    /// Soft-delete / restore records.
    fn set_deprecated(&mut self, ids: &[String], deprecated: bool) -> Result<usize>;

    fn clear(&mut self, scope: ClearMemoryScope) -> Result<MemoryTransferCounts>;

    fn export_bundle(&self) -> Result<MemoryExportBundle>;
    fn import_bundle(&mut self, bundle: &MemoryExportBundle) -> Result<MemoryImportResult>;

    /// Clone the live store into a staging directory for consolidation.
    /// The returned store must expose the same roots layout so the dream
    /// pipeline can operate on it transparently.
    fn create_stage(&self, tag: &str) -> Result<Box<dyn MemoryStore>>;

    /// Install a pre-commit snapshot of the live store (for rollback), then
    /// replace the live roots with the staged contents atomically.
    fn commit_stage(
        &mut self,
        stage: &dyn MemoryStore,
        before_state: &DreamRuntimeState,
        after_state: &DreamRuntimeState,
    ) -> Result<()>;

    /// Discard a staged store (best effort).
    fn dispose_stage(&self, stage: Box<dyn MemoryStore>);

    /// Restore the saved tree and install the current tree as a reverse snapshot.
    fn rollback_last_dream(
        &mut self,
        current_state: &DreamRuntimeState,
    ) -> Result<DreamRollbackResult>;
    fn last_dream_snapshot(&self) -> Result<Option<serde_json::Value>>;
}

/// L0 raw-capture queue.
pub trait QueueStore: Send + Sync {
    fn enqueue(&self, record: &L0SessionRecord) -> Result<()>;
    fn pending(
        &self,
        session_keys: Option<&[String]>,
        limit: usize,
    ) -> Result<Vec<L0SessionRecord>>;
    fn pending_session_keys(&self) -> Result<Vec<String>>;
    fn mark_indexed(&self, ids: &[String]) -> Result<()>;
    fn count_pending_turns(&self) -> Result<usize>;
    /// Most recent L0 records across all sessions (timestamp desc, created_at desc).
    /// Used by capture-time de-duplication, mirroring `listRecentL0(1)`.
    fn recent(&self, limit: usize) -> Result<Vec<L0SessionRecord>>;
    fn recent_for_session(&self, session_key: &str, limit: usize) -> Result<Vec<L0SessionRecord>>;
    fn clear(&self, session_keys: Option<&[String]>) -> Result<usize>;
}

/// Small persistent KV store for pipeline state / traces / settings.
pub trait StateStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<Value>>;
    fn set(&self, key: &str, value: Value) -> Result<()>;
    fn remove(&self, key: &str) -> Result<()>;
    /// All keys with their values, ordered by key.
    fn entries(&self) -> Result<Vec<(String, Value)>>;
}

// ---------------------------------------------------------------------------
// Clock / telemetry / redaction
// ---------------------------------------------------------------------------

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
    /// ISO-8601 string with millisecond precision (`...Z`).
    fn now_iso(&self) -> String {
        format_iso(self.now())
    }
}

/// Format a UTC timestamp the way the TypeScript implementation does.
pub fn format_iso(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

pub trait Telemetry: Send + Sync {
    /// Record a structured event. Implementations must never panic or block on
    /// I/O; the default is a no-op.
    fn event(&self, _name: &str, _attributes: Value) {}
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoopTelemetry;

impl Telemetry for NoopTelemetry {}

/// PII / secret scrubber applied to captured messages and observable traces.
pub trait Redactor: Send + Sync {
    fn redact(&self, text: &str) -> String;
}

// ---------------------------------------------------------------------------
// Convenience
// ---------------------------------------------------------------------------

/// Handle helpers used by callers that construct engines manually.
pub type SharedStore = Arc<dyn MemoryStore>;

/// Read a text file, mapping IO errors into [`MemoryError::Io`].
pub fn read_to_string(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|error| MemoryError::io(path, error))
}
