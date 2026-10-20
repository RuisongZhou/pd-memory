//! CLI command implementations.
//!
//! Every command resolves configuration, builds the engine once, calls the
//! frozen engine facade and prints pretty JSON to stdout. Diagnostics go to
//! stderr through `log`/`env_logger`.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

use pd_memory_core::config::MemoryConfig;
use pd_memory_core::engine::{FlushOptions, MemoryEngine};
use pd_memory_core::error::{MemoryError, Result};
use pd_memory_core::types::{
    DreamTrigger, MemoryExportBundle, MemoryListOptions, MemoryMessage, MemoryRecordType,
};
use pd_memory_hooks::{HookOutput, HostAdapter};
use serde_json::{json, Value};

use crate::config_loader::{self, GlobalOptions};

/// Resolve config + build the engine for commands that need a model provider.
fn open_engine(options: &GlobalOptions) -> Result<Arc<MemoryEngine>> {
    let config = config_loader::resolve_config(options)?;
    config_loader::build_engine(&config)
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    println!("{text}");
    Ok(())
}

/// `pd-memory init` — write an example JSON config file.
pub fn init(options: &GlobalOptions, force: bool, path: Option<&Path>) -> Result<()> {
    let target = path
        .map(Path::to_path_buf)
        .or_else(|| options.config.clone())
        .unwrap_or_else(|| config_loader::default_data_root().join("config.json"));
    if target.exists() && !force {
        return Err(MemoryError::InvalidConfig(format!(
            "{} already exists; pass --force to overwrite",
            target.display()
        )));
    }
    let root = options
        .data_root
        .clone()
        .unwrap_or_else(config_loader::default_data_root);
    let mut config = MemoryConfig::for_data_root(root);
    config.model.model = "gpt-4o-mini".to_string();
    config.model.api_key_env = Some("OPENAI_API_KEY".to_string());
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|error| MemoryError::io(parent, error))?;
        }
    }
    write_atomic(&target, config.to_json_pretty()?.as_bytes())?;
    println!("wrote example config to {}", target.display());
    Ok(())
}

/// `pd-memory serve mcp` — run the MCP stdio server until stdin closes.
pub fn serve_mcp(options: &GlobalOptions) -> Result<()> {
    let config = config_loader::resolve_config(options)?;
    let engine = config_loader::build_engine_lenient(&config, "serve mcp")?;
    let server_config = pd_memory_mcp::McpServerConfig {
        server_name: config.mcp.server_name.clone(),
        server_version: pd_memory_core::VERSION.to_string(),
    };
    log::info!(
        "serving MCP over stdio: server={} workspace={} data_root={}",
        server_config.server_name,
        config.workspace_key(),
        config.data_root.display()
    );
    pd_memory_mcp::serve_stdio(engine, server_config)
}

/// `pd-memory hook` — read one hook payload from stdin, print one JSON line.
///
/// This command is intentionally infallible from the host's point of view: any
/// internal failure is logged to stderr and downgraded to `{"continue":true}`.
pub fn hook(options: &GlobalOptions, host: Option<&dyn HostAdapter>) {
    println!("{}", hook_output_from_stdin(options, host));
    let _ = std::io::stdout().flush();
}

/// Rendered stdout JSON (without newline) for one hook invocation.
fn hook_output_from_stdin(options: &GlobalOptions, host: Option<&dyn HostAdapter>) -> String {
    let fallback = || pd_memory_hooks::render_json(&HookOutput::continue_only());
    let mut raw = String::new();
    if let Err(error) = std::io::stdin().read_to_string(&mut raw) {
        log::error!("could not read hook stdin ({error}); continuing");
        return fallback();
    }
    let input = pd_memory_hooks::parse_input(&raw);
    let mut config = match config_loader::resolve_config(options) {
        Ok(config) => config,
        Err(error) => {
            log::error!("hook config resolution failed ({error}); continuing");
            return fallback();
        }
    };
    config_loader::apply_hook_workspace(&mut config, &input);
    // Acquire the host lock before opening the engine, so each subprocess sees
    // state committed by the preceding capture / flush / recall.
    let session = match host {
        Some(host) => match pd_memory_hooks::policy::Session::lock(host, &config, &input) {
            Ok(session) => Some(session),
            Err(error) => {
                log::warn!("{} hook lock failed ({error}); continuing", host.label());
                return fallback();
            }
        },
        None => None,
    };
    let event = input.event().to_string();
    let engine = match config_loader::build_engine_lenient(&config, &format!("hook:{event}")) {
        Ok(engine) => engine,
        Err(error) => {
            log::error!("hook engine could not be opened ({error}); continuing");
            return fallback();
        }
    };
    if let Some(session) = session {
        return session.dispatch(&engine, &input);
    }
    match pd_memory_hooks::dispatch(&engine, &input) {
        Ok(output) => pd_memory_hooks::render_json(&output),
        Err(error) => {
            log::error!("hook dispatch failed ({error}); continuing");
            fallback()
        }
    }
}

/// `pd-memory add` — capture one turn (positional text or JSON on stdin).
pub fn add(
    options: &GlobalOptions,
    session: Option<String>,
    role: String,
    source: String,
    timestamp: Option<String>,
    content: Vec<String>,
) -> Result<()> {
    let messages = if content.is_empty() {
        read_messages_from_stdin(&role)?
    } else {
        vec![MemoryMessage::new(role.clone(), content.join(" "))]
    };
    if messages.is_empty() {
        return Err(MemoryError::InvalidInput(
            "no content provided; pass positional text or JSON messages on stdin".into(),
        ));
    }
    let engine = open_engine(options)?;
    let session_key = session
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "cli".to_string());
    let result = engine.capture_turn(&messages, &session_key, timestamp, &source)?;
    print_json(&result)
}

fn read_messages_from_stdin(default_role: &str) -> Result<Vec<MemoryMessage>> {
    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .map_err(|error| MemoryError::io("<stdin>", error))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
        return Ok(vec![MemoryMessage::new(default_role, trimmed)]);
    };
    parse_messages_value(&value, default_role)
}

fn parse_messages_value(value: &Value, default_role: &str) -> Result<Vec<MemoryMessage>> {
    match value {
        Value::Array(items) => {
            let mut messages = Vec::with_capacity(items.len());
            for item in items {
                messages.extend(parse_messages_value(item, default_role)?);
            }
            Ok(messages)
        }
        Value::Object(map) => {
            if let Some(Value::Array(items)) = map.get("messages") {
                return parse_messages_value(&Value::Array(items.clone()), default_role);
            }
            let content = map.get("content").and_then(Value::as_str).ok_or_else(|| {
                MemoryError::InvalidInput(
                    "stdin JSON message objects need a string `content` field".into(),
                )
            })?;
            let role = map
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or(default_role);
            Ok(vec![MemoryMessage::new(role, content)])
        }
        Value::String(text) => Ok(vec![MemoryMessage::new(default_role, text.clone())]),
        _ => Err(MemoryError::InvalidInput(
            "stdin JSON must be a string, a message object, or an array of messages".into(),
        )),
    }
}

/// `pd-memory search <query>` — explicit model-guided memory search.
pub fn search(options: &GlobalOptions, query: String, limit: Option<usize>) -> Result<()> {
    let engine = open_engine(options)?;
    let mut result = engine.search(&query, &[])?;
    if let Some(limit) = limit {
        result.selected_ids.truncate(limit.max(1));
    }
    print_json(&result)
}

/// `pd-memory list` — browse the memory manifest.
pub fn list(
    options: &GlobalOptions,
    kinds: Vec<String>,
    query: Option<String>,
    limit: Option<usize>,
    include_deprecated: bool,
) -> Result<()> {
    let mut parsed_kinds = Vec::with_capacity(kinds.len());
    for kind in kinds {
        let parsed = MemoryRecordType::parse(kind.trim()).ok_or_else(|| {
            MemoryError::InvalidInput(format!(
                "unknown memory kind `{kind}`; expected user, feedback, project, general_project_meta"
            ))
        })?;
        parsed_kinds.push(parsed);
    }
    let engine = open_engine(options)?;
    let entries = engine.list(&MemoryListOptions {
        kinds: parsed_kinds,
        query: query.filter(|value| !value.trim().is_empty()),
        include_deprecated,
        limit,
        offset: 0,
    })?;
    print_json(&json!({"count": entries.len(), "entries": entries}))
}

/// `pd-memory get <id...>` — read memory files by id.
pub fn get(options: &GlobalOptions, ids: Vec<String>, max_lines: usize) -> Result<()> {
    if ids.is_empty() {
        return Err(MemoryError::InvalidInput(
            "provide at least one memory id".into(),
        ));
    }
    let engine = open_engine(options)?;
    let files = engine.get(&ids, max_lines.max(1))?;
    print_json(&json!({
        "requested": ids,
        "count": files.len(),
        "files": files,
    }))
}

/// `pd-memory overview` — file counts / freshness / backlog.
pub fn overview(options: &GlobalOptions) -> Result<()> {
    let engine = open_engine(options)?;
    print_json(&engine.overview()?)
}

/// `pd-memory flush` — run one indexing batch now.
pub fn flush(options: &GlobalOptions, batch_size: Option<usize>, reason: String) -> Result<()> {
    let engine = open_engine(options)?;
    let stats = engine.flush(FlushOptions {
        batch_size,
        session_keys: None,
        reason,
    })?;
    print_json(&stats)
}

/// `pd-memory dream` — run consolidation.
pub fn dream(options: &GlobalOptions, trigger: String) -> Result<()> {
    let trigger = match trigger.trim().to_ascii_lowercase().as_str() {
        "manual" => DreamTrigger::Manual,
        "scheduled" => DreamTrigger::Scheduled,
        other => {
            return Err(MemoryError::InvalidInput(format!(
                "unknown dream trigger `{other}`; expected manual or scheduled"
            )))
        }
    };
    let engine = open_engine(options)?;
    print_json(&engine.dream(trigger)?)
}

/// `pd-memory rollback` — roll back the last dream commit.
pub fn rollback(options: &GlobalOptions) -> Result<()> {
    let engine = open_engine(options)?;
    print_json(&engine.rollback_last_dream()?)
}

/// `pd-memory export <file>` — write an export bundle (atomic write).
pub fn export(options: &GlobalOptions, file: &Path) -> Result<()> {
    let engine = open_engine(options)?;
    let bundle = engine.export_bundle()?;
    let payload = serde_json::to_string_pretty(&bundle)?;
    write_atomic(file, payload.as_bytes())?;
    println!(
        "exported {} memory file(s) to {}",
        bundle.files.len(),
        file.display()
    );
    Ok(())
}

/// `pd-memory import <file>` — import an export bundle.
pub fn import(options: &GlobalOptions, file: &Path) -> Result<()> {
    let raw = std::fs::read_to_string(file).map_err(|error| MemoryError::io(file, error))?;
    let bundle: MemoryExportBundle = serde_json::from_str(&raw)?;
    let engine = open_engine(options)?;
    let result = engine.import_bundle(&bundle)?;
    print_json(&result)
}

/// `pd-memory snapshot` — UI/administration snapshot.
pub fn snapshot(options: &GlobalOptions, limit: usize) -> Result<()> {
    let engine = open_engine(options)?;
    print_json(&engine.snapshot(limit.max(1))?)
}

/// `pd-memory doctor` — print effective config, directories and provider state.
pub fn doctor(options: &GlobalOptions) -> Result<()> {
    let config = config_loader::resolve_config(options)?;
    let layout = config.store_layout();
    println!("pd-memory {}", pd_memory_core::VERSION);
    println!(
        "config file: {}",
        options
            .config
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(none; defaults + environment)".to_string())
    );
    println!("data root: {}", config.data_root.display());
    println!("workspace key: {}", config.workspace_key());
    println!(
        "workspace root: {}",
        config
            .workspace_root
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(not set)".to_string())
    );
    println!(
        "workspace id: {}",
        config.workspace_id.as_deref().unwrap_or("(not set)")
    );
    println!("memory dir: {}", layout.roots.memory_dir.display());
    println!("global dir: {}", layout.roots.global_dir.display());
    println!("workspace dir: {}", layout.roots.workspace_dir.display());
    println!("control plane: {:?}", config.control_plane);
    println!(
        "model: {}",
        if config.model.model.is_empty() {
            "(not set)"
        } else {
            &config.model.model
        }
    );
    println!("api type: {}", config_loader::provider_label(&config));
    println!(
        "base url: {}",
        config
            .model
            .base_url
            .as_deref()
            .unwrap_or("(provider default)")
    );
    println!("api key: {}", config_loader::api_key_state(&config));
    println!(
        "provider: {}",
        config_loader::provider_availability(&config)
    );
    println!("max retries: {}", config.model.max_retries);
    println!("model timeout: {} ms", config.model.timeout_ms);
    println!(
        "hooks: capture_events={:?} recall_events={:?} max_context_chars={}",
        config.hooks.capture_events, config.hooks.recall_events, config.hooks.max_context_chars
    );
    println!(
        "mcp tools: {}",
        pd_memory_mcp::tools_json()
            .as_array()
            .map(|tools| tools.len())
            .unwrap_or(0)
    );
    Ok(())
}

/// Write a file atomically: temp file in the same directory + rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|error| MemoryError::io(parent, error))?;
        }
    }
    let tmp = path.with_extension(format!(
        "{}.{}.tmp",
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("out"),
        std::process::id()
    ));
    std::fs::write(&tmp, bytes).map_err(|error| MemoryError::io(&tmp, error))?;
    std::fs::rename(&tmp, path).map_err(|error| {
        let _ = std::fs::remove_file(&tmp);
        MemoryError::io(path, error)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_messages_from_json_shapes() {
        let array = json!([
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "hi"}
        ]);
        let messages = parse_messages_value(&array, "user").unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].content, "hi");

        let object = json!({"messages": [{"content": "nested"}]});
        let messages = parse_messages_value(&object, "assistant").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "assistant");

        let string = json!("plain");
        let messages = parse_messages_value(&string, "user").unwrap();
        assert_eq!(messages[0].content, "plain");

        assert!(parse_messages_value(&json!(42), "user").is_err());
    }

    #[test]
    fn atomic_write_replaces_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/out.json");
        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
    }
}
