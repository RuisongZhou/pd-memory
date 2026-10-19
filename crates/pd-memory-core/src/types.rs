//! Domain types of the memory system.
//!
//! Field names and `snake_case` serialization deliberately mirror the
//! PilotDeck/ClawXMemory frontmatter so that memory directories produced by the
//! TypeScript implementation remain readable by this SDK and vice versa.

use serde::{Deserialize, Serialize};

/// ISO-8601 UTC timestamp string, e.g. `2026-10-18T12:34:56.789Z`.
pub type Timestamp = String;

/// Kind of a memory note. Serialized as `snake_case` in frontmatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MemoryRecordType {
    User,
    Feedback,
    Project,
    GeneralProjectMeta,
}

impl MemoryRecordType {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryRecordType::User => "user",
            MemoryRecordType::Feedback => "feedback",
            MemoryRecordType::Project => "project",
            MemoryRecordType::GeneralProjectMeta => "general_project_meta",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Self::User),
            "feedback" => Some(Self::Feedback),
            "project" => Some(Self::Project),
            "general_project_meta" => Some(Self::GeneralProjectMeta),
            _ => None,
        }
    }

    /// Directory (relative to the memory root) that stores notes of this kind.
    pub fn directory(self) -> &'static str {
        match self {
            MemoryRecordType::User => "global/UserIdentityNotes",
            MemoryRecordType::Project => "Project",
            MemoryRecordType::Feedback => "Feedback",
            MemoryRecordType::GeneralProjectMeta => "general_projects",
        }
    }
}

/// Memory scope. `user` notes are always `global`; project/feedback are
/// `project` scoped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    Global,
    Project,
}

impl MemoryScope {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryScope::Global => "global",
            MemoryScope::Project => "project",
        }
    }
}

/// Retrieval route decided by the memory gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MemoryRoute {
    None,
    User,
    Project,
    Mix,
}

impl Default for MemoryRoute {
    fn default() -> Self {
        MemoryRoute::None
    }
}

impl MemoryRoute {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryRoute::None => "none",
            MemoryRoute::User => "user",
            MemoryRoute::Project => "project",
            MemoryRoute::Mix => "mix",
        }
    }
}

/// A single chat message handed to the memory system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryMessage {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub msg_id: Option<String>,
    pub role: String,
    pub content: String,
}

impl MemoryMessage {
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            msg_id: None,
            role: role.into(),
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::new("user", content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new("assistant", content)
    }
}

/// Frontmatter of a Markdown memory file.
///
/// This is the on-disk contract; add fields only with `#[serde(default)]` so
/// older memory directories keep parsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MemoryFileFrontmatter {
    pub name: String,
    pub description: String,
    #[serde(rename = "type")]
    pub kind: MemoryRecordType,
    pub scope: MemoryScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_workspace_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_project_id: Option<String>,
    pub updated_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dream_updated_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session_key: Option<String>,
    #[serde(default)]
    pub deprecated: bool,
    #[serde(default)]
    pub dream_attempts: u32,
}

impl Default for MemoryFileFrontmatter {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            kind: MemoryRecordType::Project,
            scope: MemoryScope::Project,
            project_id: None,
            source_kind: None,
            source_workspace_path: None,
            source_project_id: None,
            updated_at: String::new(),
            dream_updated_at: None,
            captured_at: None,
            source_session_key: None,
            deprecated: false,
            dream_attempts: 0,
        }
    }
}

/// A manifest entry: frontmatter plus location information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryManifestEntry {
    #[serde(flatten)]
    pub frontmatter: MemoryFileFrontmatter,
    pub file: String,
    /// Path relative to the memory root; this is the public memory `id`.
    pub relative_path: String,
    pub absolute_path: String,
}

impl MemoryManifestEntry {
    pub fn id(&self) -> &str {
        &self.relative_path
    }
}

/// A full memory record (frontmatter + body).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryFileRecord {
    #[serde(flatten)]
    pub entry: MemoryManifestEntry,
    pub content: String,
    #[serde(default)]
    pub preview: String,
}

/// Candidate produced by the LLM extraction pipeline and normalised before it
/// is written to disk.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MemoryCandidate {
    pub kind: MemoryRecordType,
    pub scope: MemoryScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_workspace_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_project_id: Option<String>,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session_key: Option<String>,
    // structured fields, used to rebuild the Markdown body
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profile: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preferences: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relationships: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub how_to_apply: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_steps: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timeline: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl Default for MemoryRecordType {
    fn default() -> Self {
        MemoryRecordType::Project
    }
}

impl Default for MemoryScope {
    fn default() -> Self {
        MemoryScope::Project
    }
}

/// Editable structured fields of an existing memory entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MemoryEntryEditFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_steps: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timeline: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub how_to_apply: Option<String>,
}

/// Project metadata stored in `project.meta.md`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ProjectMetaRecord {
    pub project_id: String,
    pub project_name: String,
    pub description: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_workspace_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_project_id: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dream_updated_at: Option<Timestamp>,
    #[serde(default)]
    pub relative_path: String,
    #[serde(default)]
    pub absolute_path: String,
}

/// One captured raw conversation batch (the L0 layer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct L0SessionRecord {
    pub l0_index_id: String,
    pub session_key: String,
    pub timestamp: Timestamp,
    pub messages: Vec<MemoryMessage>,
    pub source: String,
    #[serde(default)]
    pub indexed: bool,
    pub created_at: Timestamp,
}

/// Tunable indexing / consolidation settings (persisted in the state store).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct IndexingSettings {
    pub reasoning_mode: ReasoningMode,
    pub auto_index_interval_minutes: u32,
    pub auto_dream_interval_minutes: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningMode {
    AnswerFirst,
    AccuracyFirst,
}

impl Default for IndexingSettings {
    fn default() -> Self {
        Self {
            reasoning_mode: ReasoningMode::AnswerFirst,
            auto_index_interval_minutes: 30,
            auto_dream_interval_minutes: 60,
        }
    }
}

/// How much of a session is captured on every turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureStrategy {
    LastTurn,
    FullSession,
}

impl Default for CaptureStrategy {
    fn default() -> Self {
        CaptureStrategy::LastTurn
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode {
    Single,
    General,
}

impl Default for WorkspaceMode {
    fn default() -> Self {
        WorkspaceMode::Single
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DreamTrigger {
    #[default]
    Manual,
    Scheduled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DreamPipelineStatus {
    Running,
    Success,
    #[default]
    Skipped,
    Failed,
}

// ---------------------------------------------------------------------------
// Pipeline results
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureTurnResult {
    /// Whether nonempty normalized input was accepted, including a queue duplicate.
    pub captured: bool,
    pub normalized_messages: Vec<MemoryMessage>,
    pub session_key: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatStats {
    pub processed_sessions: usize,
    pub processed_turns: usize,
    pub written_files: usize,
    pub skipped_turns: usize,
    #[serde(default)]
    pub errors: Vec<String>,
    pub started_at: Timestamp,
    pub finished_at: Timestamp,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetrievalResult {
    pub route: MemoryRoute,
    #[serde(default)]
    pub context: Option<String>,
    /// PilotDeck four-section system context ready for prompt injection.
    #[serde(default)]
    pub system_context: Option<String>,
    #[serde(default)]
    pub selected_ids: Vec<String>,
    #[serde(default)]
    pub intent: Option<String>,
    #[serde(default)]
    pub cache_hit: bool,
    #[serde(default)]
    pub trace: serde_json::Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DreamRunResult {
    /// Internal commit decision; successful no-op runs leave the snapshot intact.
    #[serde(skip)]
    pub is_no_op: bool,
    pub reviewed_files: usize,
    pub rewritten_projects: usize,
    pub deleted_projects: usize,
    pub deleted_files: usize,
    pub profile_updated: bool,
    pub duplicate_topic_count: usize,
    pub conflict_topic_count: usize,
    pub summary: String,
    pub trigger: DreamTrigger,
    pub status: DreamPipelineStatus,
    #[serde(default)]
    pub started_at: Timestamp,
    #[serde(default)]
    pub finished_at: Timestamp,
}

/// Dream state stored with each snapshot so rollback and redo restore metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DreamRuntimeState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_dream_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_dream_status: Option<DreamPipelineStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_dream_summary: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DreamRollbackResult {
    pub rolled_back_at: Timestamp,
    pub snapshot_captured_at: Timestamp,
    #[serde(default)]
    pub last_dream_at: Option<Timestamp>,
    #[serde(default)]
    pub last_dream_status: Option<DreamPipelineStatus>,
    #[serde(default)]
    pub last_dream_summary: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaintenanceResult {
    pub index_ran: bool,
    pub dream_ran: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_stats: Option<HeartbeatStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dream_result: Option<DreamRunResult>,
}

/// Overview / diagnostics payload (also used by MCP `memory_overview`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MemoryOverview {
    pub total_files: usize,
    pub project_memories: usize,
    pub feedback_memories: usize,
    pub user_profiles: usize,
    pub pending_sessions: usize,
    pub pending_dialogue_turns: usize,
    #[serde(default)]
    pub last_indexed_at: Option<Timestamp>,
    #[serde(default)]
    pub last_dream_at: Option<Timestamp>,
    #[serde(default)]
    pub last_dream_status: Option<DreamPipelineStatus>,
    #[serde(default)]
    pub last_dream_summary: Option<String>,
    pub changed_files_since_last_dream: usize,
    pub workspace_mode: WorkspaceMode,
    pub workspace_name: String,
    #[serde(default)]
    pub project_meta: Option<ProjectMetaRecord>,
    pub deprecated_files: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryUserSummary {
    pub identity_background: Vec<String>,
    pub files: Vec<MemoryFileRecord>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MemoryListOptions {
    #[serde(default)]
    pub kinds: Vec<MemoryRecordType>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub include_deprecated: bool,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: usize,
}

// ---------------------------------------------------------------------------
// Import / export / clear
// ---------------------------------------------------------------------------

pub const MEMORY_EXPORT_FORMAT_VERSION: &str = "clawxmemory-memory-snapshot.v4";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySnapshotFileRecord {
    pub relative_path: String,
    pub content: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MemoryTransferCounts {
    pub managed_files: usize,
    pub memory_files: usize,
    pub project: usize,
    pub feedback: usize,
    pub user: usize,
    pub project_metas: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MemoryBundleMetadata {
    pub exported_at: Timestamp,
    #[serde(default)]
    pub last_indexed_at: Option<Timestamp>,
    #[serde(default)]
    pub last_dream_at: Option<Timestamp>,
    #[serde(default)]
    pub last_dream_status: Option<DreamPipelineStatus>,
    #[serde(default)]
    pub last_dream_summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MemoryExportBundle {
    pub format_version: String,
    #[serde(flatten)]
    pub metadata: MemoryBundleMetadata,
    pub files: Vec<MemorySnapshotFileRecord>,
}

impl Default for MemoryExportBundle {
    fn default() -> Self {
        Self {
            format_version: MEMORY_EXPORT_FORMAT_VERSION.to_string(),
            metadata: MemoryBundleMetadata::default(),
            files: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MemoryImportResult {
    pub format_version: String,
    pub imported: MemoryTransferCounts,
    pub imported_at: Timestamp,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(flatten)]
    pub metadata: MemoryBundleMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClearMemoryScope {
    CurrentProject,
    AllMemory,
}

// ---------------------------------------------------------------------------
// Actions (UI / MCP administration surface)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum MemoryActionRequest {
    EditProjectMeta {
        project_name: String,
        description: String,
        status: String,
    },
    EditEntry {
        id: String,
        name: String,
        description: String,
        #[serde(default)]
        fields: Option<MemoryEntryEditFields>,
    },
    DeleteEntries {
        ids: Vec<String>,
    },
    DeprecateEntries {
        ids: Vec<String>,
    },
    RestoreEntries {
        ids: Vec<String>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MemoryActionResult {
    pub ok: bool,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub affected_ids: Vec<String>,
    #[serde(default)]
    pub project_meta: Option<ProjectMetaRecord>,
}

/// A single retrieval step for observability.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RetrievalTrace {
    pub trace_id: String,
    pub started_at: Timestamp,
    pub finished_at: Timestamp,
    pub route: MemoryRoute,
    pub cache_hit: bool,
    pub steps: Vec<serde_json::Value>,
    pub selected_ids: Vec<String>,
    pub candidate_count: usize,
}

/// Snapshot shown by `overview`-style UI endpoints.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MemoryUiSnapshot {
    pub overview: MemoryOverview,
    pub entries: Vec<MemoryManifestEntry>,
    pub user_summary: MemoryUserSummary,
    #[serde(default)]
    pub retrieval_traces: Vec<serde_json::Value>,
    #[serde(default)]
    pub index_traces: Vec<serde_json::Value>,
    #[serde(default)]
    pub dream_traces: Vec<serde_json::Value>,
}
