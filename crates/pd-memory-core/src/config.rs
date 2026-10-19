//! Configuration for the memory SDK.
//!
//! Design rule: **no implicit host coupling**. Every path is explicit, and
//! environment variables are only read through [`MemoryConfig::apply_env`],
//! which is called by host adapters (CLI / MCP / FFI) and never by the engine
//! itself.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::error::{MemoryError, Result};
use crate::ports::StoreLayout;
use crate::types::{
    CaptureStrategy, IndexingSettings, WorkspaceMode, MEMORY_EXPORT_FORMAT_VERSION,
};

/// Wire format used to talk to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ApiType {
    #[default]
    OpenaiChat,
    OpenaiResponses,
    Anthropic,
    Google,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(default)]
    pub api_type: ApiType,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub base_url: Option<String>,
    /// Literal key. Prefer `api_key_env` so keys never live on disk.
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "default_model_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Max completion tokens hint; provider-specific.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
}

fn default_model_timeout_ms() -> u64 {
    60_000
}
fn default_max_retries() -> u32 {
    2
}

impl ModelConfig {
    /// Resolve the API key from config or its named environment variable.
    pub fn resolve_api_key(&self) -> Option<String> {
        if let Some(key) = self.api_key.clone() {
            if !key.trim().is_empty() {
                return Some(key);
            }
        }
        self.api_key_env
            .as_ref()
            .and_then(|name| std::env::var(name).ok())
            .filter(|value| !value.trim().is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetrievalConfig {
    /// Maximum number of manifest entries considered for recall.
    #[serde(default = "default_manifest_limit")]
    pub manifest_limit: usize,
    /// Maximum number of files the model may select.
    #[serde(default = "default_selection_limit")]
    pub selection_limit: usize,
    /// Lines read per file when rendering recall context.
    #[serde(default = "default_recall_file_max_lines")]
    pub recall_file_max_lines: usize,
    /// Recall cache TTL in milliseconds.
    #[serde(default = "default_recall_cache_ttl_ms")]
    pub cache_ttl_ms: u64,
    /// Outer timeout applied by host adapters to a whole recall.
    #[serde(default = "default_retrieval_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_manifest_limit() -> usize {
    200
}
fn default_selection_limit() -> usize {
    5
}
fn default_recall_file_max_lines() -> usize {
    200
}
fn default_recall_cache_ttl_ms() -> u64 {
    30_000
}
fn default_retrieval_timeout_ms() -> u64 {
    30_000
}
fn default_true() -> bool {
    true
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            manifest_limit: default_manifest_limit(),
            selection_limit: default_selection_limit(),
            recall_file_max_lines: default_recall_file_max_lines(),
            cache_ttl_ms: default_recall_cache_ttl_ms(),
            timeout_ms: default_retrieval_timeout_ms(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionConfig {
    /// Redact secrets / emails / phone numbers from captured content.
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub extra_patterns: Vec<String>,
}

impl Default for RedactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            extra_patterns: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ControlPlaneKind {
    /// Pure-Rust JSONL + JSON files. Default on Android; no native deps.
    #[default]
    Json,
    /// SQLite via `rusqlite` (desktop parity with PilotDeck).
    Sqlite,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HooksConfig {
    /// Capture turns on these hook events (PilotDeck / WorkBuddy names).
    #[serde(default = "default_capture_events")]
    pub capture_events: Vec<String>,
    /// Emit recall context on these hook events.
    #[serde(default = "default_recall_events")]
    pub recall_events: Vec<String>,
    /// Flush the index when this many turns are pending.
    #[serde(default = "default_pending_threshold")]
    pub pending_turn_threshold: usize,
    /// Minimum interval between automatic flushes, in minutes.
    #[serde(default = "default_auto_index_minutes")]
    pub auto_index_interval_minutes: u32,
    /// Minimum interval between automatic dreams, in minutes.
    #[serde(default = "default_auto_dream_minutes")]
    pub auto_dream_interval_minutes: u32,
    /// Maximum characters injected into the prompt by a hook.
    #[serde(default = "default_max_context_chars")]
    pub max_context_chars: usize,
}

fn default_capture_events() -> Vec<String> {
    vec!["Stop".into(), "SessionEnd".into(), "PostToolUse".into()]
}
fn default_recall_events() -> Vec<String> {
    vec!["UserPromptSubmit".into(), "SessionStart".into()]
}
fn default_pending_threshold() -> usize {
    20
}
fn default_auto_index_minutes() -> u32 {
    30
}
fn default_auto_dream_minutes() -> u32 {
    60
}
fn default_max_context_chars() -> usize {
    8_000
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            capture_events: default_capture_events(),
            recall_events: default_recall_events(),
            pending_turn_threshold: default_pending_threshold(),
            auto_index_interval_minutes: default_auto_index_minutes(),
            auto_dream_interval_minutes: default_auto_dream_minutes(),
            max_context_chars: default_max_context_chars(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default = "default_mcp_server_name")]
    pub server_name: String,
    /// Tools exposed over MCP. Empty means all implemented tools.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Optional HTTP transport (streamable HTTP / SSE) bind address.
    #[serde(default)]
    pub http_bind: Option<String>,
}

fn default_mcp_server_name() -> String {
    "pd-memory".into()
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            server_name: default_mcp_server_name(),
            tools: Vec::new(),
            http_bind: None,
        }
    }
}

/// Top-level memory configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// Root of all memory data (global profile + one directory per workspace).
    pub data_root: PathBuf,
    /// Logical project root. In `single` mode a workspace id is derived from it
    /// unless `workspace_id` is set. On Android callers usually set
    /// `workspace_id` explicitly instead of pointing at a filesystem path.
    #[serde(default)]
    pub workspace_root: Option<PathBuf>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub workspace_name: Option<String>,
    #[serde(default)]
    pub workspace_mode: WorkspaceMode,
    #[serde(default)]
    pub capture_strategy: CaptureStrategy,
    #[serde(default = "default_true")]
    pub include_assistant: bool,
    #[serde(default = "default_max_message_chars")]
    pub max_message_chars: usize,
    #[serde(default = "default_heartbeat_batch_size")]
    pub heartbeat_batch_size: usize,
    #[serde(default)]
    pub default_settings: IndexingSettings,
    #[serde(default)]
    pub control_plane: ControlPlaneKind,
    #[serde(default)]
    pub model: ModelConfig,
    #[serde(default)]
    pub retrieval: RetrievalConfig,
    #[serde(default)]
    pub redaction: RedactionConfig,
    #[serde(default)]
    pub hooks: HooksConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    /// Tenant id reserved for future multi-tenant hosting.
    #[serde(default)]
    pub tenant_id: Option<String>,
    #[serde(default = "default_export_format")]
    pub export_format_version: String,
}

fn default_max_message_chars() -> usize {
    6000
}
fn default_heartbeat_batch_size() -> usize {
    30
}
fn default_export_format() -> String {
    MEMORY_EXPORT_FORMAT_VERSION.to_string()
}

impl MemoryConfig {
    /// Minimal configuration for a data root; model must be filled in by caller.
    pub fn for_data_root(root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: root.into(),
            workspace_root: None,
            workspace_id: None,
            workspace_name: None,
            workspace_mode: WorkspaceMode::Single,
            capture_strategy: CaptureStrategy::LastTurn,
            include_assistant: true,
            max_message_chars: default_max_message_chars(),
            heartbeat_batch_size: default_heartbeat_batch_size(),
            default_settings: IndexingSettings::default(),
            control_plane: ControlPlaneKind::Json,
            model: ModelConfig::default(),
            retrieval: RetrievalConfig::default(),
            redaction: RedactionConfig::default(),
            hooks: HooksConfig::default(),
            mcp: McpConfig::default(),
            tenant_id: None,
            export_format_version: default_export_format(),
        }
    }

    pub fn from_json_str(raw: &str) -> Result<Self> {
        let config: MemoryConfig = serde_json::from_str(raw)
            .map_err(|error| MemoryError::InvalidConfig(error.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(|error| MemoryError::io(path, error))?;
        Self::from_json_str(&raw)
    }

    pub fn to_json_pretty(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(MemoryError::from)
    }

    pub fn validate(&self) -> Result<()> {
        if self.data_root.as_os_str().is_empty() {
            return Err(MemoryError::InvalidConfig(
                "data_root must not be empty".into(),
            ));
        }
        if self.max_message_chars == 0 {
            return Err(MemoryError::InvalidConfig(
                "max_message_chars must be > 0".into(),
            ));
        }
        if self.heartbeat_batch_size == 0 {
            return Err(MemoryError::InvalidConfig(
                "heartbeat_batch_size must be > 0".into(),
            ));
        }
        if self.workspace_mode == WorkspaceMode::General {
            return Err(MemoryError::InvalidConfig(
                "workspace_mode=general is not supported by this SDK build yet; use single".into(),
            ));
        }
        if self.model.api_type != ApiType::OpenaiChat
            && self.model.api_type != ApiType::Anthropic
            && !self.model.model.is_empty()
        {
            // Adapter coverage is validated by pd-memory-llm; core accepts all.
            log::debug!("model api type {:?} accepted by core", self.model.api_type);
        }
        Ok(())
    }

    /// Stable workspace key used as the on-disk directory name.
    pub fn workspace_key(&self) -> String {
        if let Some(id) = self.workspace_id.as_deref() {
            let trimmed = id.trim();
            if !trimmed.is_empty() {
                return sanitize_workspace_key(trimmed);
            }
        }
        if let Some(root) = self.workspace_root.as_deref() {
            return workspace_key_for_path(root);
        }
        "default".to_string()
    }

    pub fn store_layout(&self) -> StoreLayout {
        StoreLayout::new(self.data_root.clone(), &self.workspace_key())
    }

    /// Apply environment variable overrides used by CLI / hook / MCP entry
    /// points. Precedence: explicit JSON < env.
    ///
    /// Recognised variables:
    /// - `PD_MEMORY_DATA_ROOT`
    /// - `PD_MEMORY_WORKSPACE_ROOT`
    /// - `PD_MEMORY_WORKSPACE_ID`
    /// - `PD_MEMORY_MODEL`, `PD_MEMORY_MODEL_API_TYPE`, `PD_MEMORY_BASE_URL`
    /// - `PD_MEMORY_API_KEY_ENV` (name of env var holding the key)
    /// - `PD_MEMORY_API_TYPE`
    pub fn apply_env(&mut self) {
        if let Ok(value) = std::env::var("PD_MEMORY_DATA_ROOT") {
            if !value.trim().is_empty() {
                self.data_root = PathBuf::from(value);
            }
        }
        if let Ok(value) = std::env::var("PD_MEMORY_WORKSPACE_ROOT") {
            if !value.trim().is_empty() {
                self.workspace_root = Some(PathBuf::from(value));
            }
        }
        if let Ok(value) = std::env::var("PD_MEMORY_WORKSPACE_ID") {
            if !value.trim().is_empty() {
                self.workspace_id = Some(value);
            }
        }
        if let Ok(value) = std::env::var("PD_MEMORY_MODEL") {
            if !value.trim().is_empty() {
                self.model.model = value;
            }
        }
        if let Ok(value) = std::env::var("PD_MEMORY_API_TYPE") {
            if let Ok(api_type) = serde_json::from_value(serde_json::Value::String(value)) {
                self.model.api_type = api_type;
            }
        }
        if let Ok(value) = std::env::var("PD_MEMORY_BASE_URL") {
            if !value.trim().is_empty() {
                self.model.base_url = Some(value);
            }
        }
        if let Ok(value) = std::env::var("PD_MEMORY_API_KEY_ENV") {
            if !value.trim().is_empty() {
                self.model.api_key_env = Some(value);
            }
        }
    }

    /// Merge host-specific overrides (e.g. hook `transcript_path`/`cwd`) into a
    /// copy; used by the hook adapter to derive a workspace from the session.
    pub fn with_workspace_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.workspace_root = Some(root.into());
        self
    }
}

/// `sha256(path)` first 10 hex chars, matching the TypeScript implementation's
/// stability requirement (content of the path only, not the filesystem).
pub fn workspace_key_for_path(path: &Path) -> String {
    // Parity: hashText(x) = sha1(UTF-8(x)).hex[:10] (core/utils/id.ts).
    let mut hasher = Sha1::new();
    hasher.update(normalize_path_string(path).as_bytes());
    format_hash10(&hex::encode(hasher.finalize()))
}

/// First 10 lowercase hex chars — the PilotDeck `hashText` contract.
pub fn format_hash10(hex_digest: &str) -> String {
    hex_digest.chars().take(10).collect()
}

/// Normalise the path representation used for hashing so that the same logical
/// directory maps to the same workspace even with trailing separators.
pub fn normalize_path_string(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

fn sanitize_workspace_key(raw: &str) -> String {
    let sanitized: String = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches(['-', '.'].as_ref());
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_key_is_stable() {
        let config =
            MemoryConfig::for_data_root("/tmp/pd-memory").with_workspace_root("/Users/me/project");
        let first = config.workspace_key();
        let second = config.workspace_key();
        assert_eq!(first, second);
        assert_eq!(first.len(), 10);
    }

    #[test]
    fn workspace_key_matches_pilotdeck_golden_value() {
        // Spec §8.9: sha1("/Users/ruisong/code/PilotDeck")[:10] == 92e5f837f4
        assert_eq!(
            workspace_key_for_path(Path::new("/Users/ruisong/code/PilotDeck")),
            "92e5f837f4"
        );
    }

    #[test]
    fn explicit_workspace_id_wins() {
        let mut config = MemoryConfig::for_data_root("/tmp/pd-memory");
        config.workspace_id = Some("my_phone/app".into());
        config.workspace_root = Some(PathBuf::from("/Users/me/project"));
        assert_eq!(config.workspace_key(), "my_phone-app");
    }

    #[test]
    fn config_roundtrip() {
        let config = MemoryConfig::for_data_root("/tmp/pd-memory");
        let json = config.to_json_pretty().unwrap();
        let parsed = MemoryConfig::from_json_str(&json).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn layout_matches_pilotdeck_shape() {
        let mut config = MemoryConfig::for_data_root("/tmp/pd-memory");
        config.workspace_id = Some("ws".into());
        let layout = config.store_layout();
        assert_eq!(
            layout.roots.memory_dir,
            PathBuf::from("/tmp/pd-memory/workspaces/ws/memory")
        );
        assert_eq!(
            layout.roots.global_dir,
            PathBuf::from("/tmp/pd-memory/global")
        );
    }
}
