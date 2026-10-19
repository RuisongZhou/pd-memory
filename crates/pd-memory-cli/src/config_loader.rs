//! Configuration resolution, provider selection and engine assembly for the
//! `pd-memory` CLI.
//!
//! Precedence (highest first):
//! 1. CLI flags (`--data-root`, `--workspace`);
//! 2. environment variables understood by [`MemoryConfig::apply_env`]
//!    (`PD_MEMORY_DATA_ROOT`, `PD_MEMORY_WORKSPACE_ROOT/ID`,
//!    `PD_MEMORY_MODEL`, `PD_MEMORY_API_TYPE`, `PD_MEMORY_BASE_URL`,
//!    `PD_MEMORY_API_KEY_ENV`);
//! 3. `--config` JSON file, when given;
//! 4. defaults: [`MemoryConfig::for_data_root`] with the data root taken from
//!    `PD_MEMORY_DATA_ROOT`, else `$XDG_DATA_HOME/pd-memory`, else
//!    `~/.pd-memory`.
//!
//! `PD_MEMORY_PROVIDER=mock` forces the deterministic [`MockProvider`] for
//! every command (offline tests / WorkBuddy mock E2E).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pd_memory_core::config::{ApiType, MemoryConfig};
use pd_memory_core::engine::MemoryEngine;
use pd_memory_core::error::Result;
use pd_memory_core::ports::ModelProvider;
use pd_memory_hooks::HookInput;
use pd_memory_llm::{mock_selected_from_env, provider_from_config, MockProvider};

/// Global CLI options shared by every subcommand.
#[derive(Debug, Clone, Default)]
pub struct GlobalOptions {
    /// `--config <path>` JSON configuration file.
    pub config: Option<PathBuf>,
    /// `--data-root <dir>` memory data root override.
    pub data_root: Option<PathBuf>,
    /// `--workspace <id-or-path>` workspace override.
    pub workspace: Option<String>,
}

/// Default data root: `PD_MEMORY_DATA_ROOT`, else `$XDG_DATA_HOME/pd-memory`,
/// else `~/.pd-memory`.
pub fn default_data_root() -> PathBuf {
    if let Some(value) = non_empty_env("PD_MEMORY_DATA_ROOT") {
        return PathBuf::from(value);
    }
    if let Some(value) = non_empty_env("XDG_DATA_HOME") {
        return PathBuf::from(value).join("pd-memory");
    }
    home_directory()
        .map(|home| home.join(".pd-memory"))
        .unwrap_or_else(|| PathBuf::from(".pd-memory"))
}

fn home_directory() -> Option<PathBuf> {
    non_empty_env("HOME")
        .or_else(|| non_empty_env("USERPROFILE"))
        .map(PathBuf::from)
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Workspace from the environment, used by hooks when the payload has no
/// `cwd` (WorkBuddy/CodeBuddy set these for hook subprocesses).
pub fn workspace_from_env() -> Option<PathBuf> {
    non_empty_env("CODEBUDDY_PROJECT_DIR")
        .or_else(|| non_empty_env("CLAUDE_PROJECT_DIR"))
        .map(PathBuf::from)
}

/// Resolve the effective [`MemoryConfig`] for one CLI invocation.
pub fn resolve_config(options: &GlobalOptions) -> Result<MemoryConfig> {
    let mut config = match options.config.as_deref() {
        Some(path) => {
            let mut config = MemoryConfig::from_file(path)?;
            // Relative paths inside the config file resolve against the config
            // file's directory, not the process cwd (hook processes and MCP
            // servers may run with different working directories).
            let base = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf();
            if config.data_root.is_relative() {
                config.data_root = base.join(&config.data_root);
            }
            if let Some(workspace_root) = config.workspace_root.as_mut() {
                if workspace_root.is_relative() {
                    *workspace_root = base.join(&*workspace_root);
                }
            }
            config
        }
        None => MemoryConfig::for_data_root(default_data_root()),
    };
    config.apply_env();
    if let Some(data_root) = &options.data_root {
        config.data_root = data_root.clone();
    }
    if let Some(workspace) = options.workspace.as_deref() {
        apply_workspace_override(&mut config, workspace);
    }
    config.validate()?;
    Ok(config)
}

/// Interpret `--workspace` as an explicit id unless it looks like a path.
pub fn apply_workspace_override(config: &mut MemoryConfig, value: &str) {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return;
    }
    let looks_like_path = trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.starts_with('.')
        || Path::new(trimmed).is_dir();
    if looks_like_path {
        config.workspace_root = Some(PathBuf::from(trimmed));
        config.workspace_id = None;
    } else {
        config.workspace_id = Some(trimmed.to_string());
        config.workspace_root = None;
    }
    if config.workspace_name.is_none() {
        config.workspace_name = Some(trimmed.to_string());
    }
}

/// Derive a workspace for a hook invocation when the user did not pin one.
///
/// Priority: `CODEBUDDY_PROJECT_DIR`/`CLAUDE_PROJECT_DIR` (always set for hook
/// subprocesses), then the payload `cwd` (absent on `SessionStart`), then the
/// transcript's parent directory name (``projects/<encoded-cwd>``), which is a
/// stable per-workspace key.
pub fn apply_hook_workspace(config: &mut MemoryConfig, input: &HookInput) {
    if config.workspace_root.is_some() || config.workspace_id.is_some() {
        // Explicit `--config` / `--workspace` wins.
        return;
    }
    if let Some(directory) = workspace_from_env() {
        config.workspace_root = Some(directory);
        return;
    }
    let cwd = input.cwd.trim();
    if !cwd.is_empty() {
        config.workspace_root = Some(PathBuf::from(cwd));
        return;
    }
    if let Some(workspace_id) = pd_memory_hooks::derive_workspace_id(input) {
        config.workspace_id = Some(workspace_id);
    }
}

/// Provider label for `doctor` / logs.
pub fn provider_label(config: &MemoryConfig) -> &'static str {
    match config.model.api_type {
        ApiType::OpenaiChat => "openai-chat",
        ApiType::Anthropic => "anthropic",
        ApiType::OpenaiResponses => "openai-responses",
        ApiType::Google => "google",
    }
}

/// Build the model provider selected by config + environment.
///
/// Returns a human-readable status label alongside the provider for logging.
pub fn build_provider(config: &MemoryConfig) -> Result<(Arc<dyn ModelProvider>, String)> {
    if mock_selected_from_env() {
        log::warn!("PD_MEMORY_PROVIDER=mock: using deterministic MockProvider (no network)");
        return Ok((Arc::new(MockProvider::new()), "mock".to_string()));
    }
    let provider = provider_from_config(&config.model)?;
    Ok((provider, provider_label(config).to_string()))
}

/// Build the engine once per process.
pub fn build_engine(config: &MemoryConfig) -> Result<Arc<MemoryEngine>> {
    let (provider, label) = build_provider(config)?;
    log::info!(
        "opening memory engine: data_root={} workspace={} provider={label}",
        config.data_root.display(),
        config.workspace_key()
    );
    Ok(Arc::new(MemoryEngine::open(config.clone(), provider)?))
}

/// Build the engine, falling back to [`MockProvider`] when no model is
/// configured. Used by `hook` and `serve mcp`, which must stay functional
/// (capture-only / protocol-only) even without model credentials.
pub fn build_engine_lenient(config: &MemoryConfig, context: &str) -> Result<Arc<MemoryEngine>> {
    match build_provider(config) {
        Ok((provider, label)) => {
            log::info!("{context}: using provider {label}");
            Ok(Arc::new(MemoryEngine::open(config.clone(), provider)?))
        }
        Err(error) => {
            log::warn!(
                "{context}: model provider unavailable ({error}); falling back to deterministic \
                 MockProvider (capture-only mode, recall/flush will be mocked)"
            );
            Ok(Arc::new(MemoryEngine::open(
                config.clone(),
                Arc::new(MockProvider::new()),
            )?))
        }
    }
}

/// Human-readable API key state for `doctor` (never prints the key itself).
pub fn api_key_state(config: &MemoryConfig) -> String {
    if let Some(key) = config.model.resolve_api_key() {
        let source = if config
            .model
            .api_key
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        {
            "inline".to_string()
        } else {
            config
                .model
                .api_key_env
                .clone()
                .unwrap_or_else(|| "env".to_string())
        };
        format!("present (source: {source}, {} chars)", key.chars().count())
    } else {
        "missing".to_string()
    }
}

/// Whether a provider is constructible for the current config (for `doctor`).
pub fn provider_availability(config: &MemoryConfig) -> String {
    if mock_selected_from_env() {
        return "ok (mock, PD_MEMORY_PROVIDER=mock)".to_string();
    }
    match build_provider(config) {
        Ok((_, label)) => format!("ok ({label})"),
        Err(error) => format!("unavailable: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(
        config: Option<PathBuf>,
        data_root: Option<PathBuf>,
        workspace: Option<&str>,
    ) -> GlobalOptions {
        GlobalOptions {
            config,
            data_root,
            workspace: workspace.map(str::to_string),
        }
    }

    #[test]
    fn data_root_flag_beats_env_and_json() {
        let json = r#"{"data_root": "/from/json"}"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.json");
        std::fs::write(&path, json).expect("write config");
        let resolved = resolve_config(&options(
            Some(path),
            Some(PathBuf::from("/from/flag")),
            None,
        ))
        .expect("resolves");
        assert_eq!(resolved.data_root, PathBuf::from("/from/flag"));
    }

    #[test]
    fn relative_paths_in_config_resolve_against_config_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_dir = dir.path().join("conf");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        let path = config_dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"data_root": "memory-data", "workspace_root": "project"}"#,
        )
        .expect("write config");
        let resolved = resolve_config(&options(Some(path), None, None)).expect("resolves");
        assert_eq!(resolved.data_root, config_dir.join("memory-data"));
        assert_eq!(resolved.workspace_root, Some(config_dir.join("project")));
    }

    #[test]
    fn workspace_path_is_detected() {
        let mut config = MemoryConfig::for_data_root("/tmp/pd-memory");
        apply_workspace_override(&mut config, "/Users/me/project");
        assert_eq!(
            config.workspace_root,
            Some(PathBuf::from("/Users/me/project"))
        );
        assert_eq!(config.workspace_id, None);
        apply_workspace_override(&mut config, "phone-workspace");
        assert_eq!(config.workspace_id.as_deref(), Some("phone-workspace"));
        assert_eq!(config.workspace_root, None);
    }

    #[test]
    fn workspace_key_is_stable_for_explicit_id() {
        let mut config = MemoryConfig::for_data_root("/tmp/pd-memory");
        apply_workspace_override(&mut config, "my_workspace");
        assert_eq!(config.workspace_key(), "my_workspace");
    }

    #[test]
    fn hook_workspace_priority() {
        let mut config = MemoryConfig::for_data_root("/tmp/pd-memory");
        let input: HookInput = serde_json::from_value(serde_json::json!({
            "hook_event_name": "Stop",
            "cwd": "/tmp/cwd-project",
            "transcript_path": "/tmp/config/projects/encoded-ws/s1.jsonl",
        }))
        .expect("input");
        // `cwd` wins over transcript derivation when no env override is set.
        std::env::remove_var("CODEBUDDY_PROJECT_DIR");
        std::env::remove_var("CLAUDE_PROJECT_DIR");
        apply_hook_workspace(&mut config, &input);
        assert_eq!(
            config.workspace_root,
            Some(PathBuf::from("/tmp/cwd-project"))
        );

        let mut config = MemoryConfig::for_data_root("/tmp/pd-memory");
        let no_cwd: HookInput = serde_json::from_value(serde_json::json!({
            "hook_event_name": "SessionStart",
            "transcript_path": "/tmp/config/projects/encoded-ws/s1.jsonl",
        }))
        .expect("input");
        apply_hook_workspace(&mut config, &no_cwd);
        assert_eq!(config.workspace_id.as_deref(), Some("encoded-ws"));
    }

    #[test]
    fn explicit_workspace_is_not_overridden_by_hook_derivation() {
        let mut config = MemoryConfig::for_data_root("/tmp/pd-memory");
        config.workspace_root = Some(PathBuf::from("/explicit"));
        let input: HookInput = serde_json::from_value(serde_json::json!({
            "hook_event_name": "Stop",
            "cwd": "/tmp/cwd-project",
        }))
        .expect("input");
        apply_hook_workspace(&mut config, &input);
        assert_eq!(config.workspace_root, Some(PathBuf::from("/explicit")));
    }

    #[test]
    fn api_key_state_never_leaks_secret() {
        let mut config = MemoryConfig::for_data_root("/tmp/pd-memory");
        config.model.api_key = Some("sk-super-secret-value".into());
        let state = api_key_state(&config);
        assert!(state.contains("present"));
        assert!(!state.contains("sk-super-secret-value"));
    }

    #[test]
    fn provider_label_matches_api_type() {
        let mut config = MemoryConfig::for_data_root("/tmp/pd-memory");
        config.model.api_type = ApiType::Anthropic;
        assert_eq!(provider_label(&config), "anthropic");
        config.model.api_type = ApiType::Google;
        assert_eq!(provider_label(&config), "google");
    }
}
