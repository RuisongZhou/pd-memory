//! JSON-in / JSON-out operation dispatch shared by the C ABI and JNI surfaces.
//!
//! Every operation accepts an optional JSON object (`args_json`, `""` and
//! `"null"` are treated as `{}`) and returns the plain operation result. The
//! outer envelope (`{"ok":true,"result":...}` / `{"ok":false,...}`) is added by
//! [`crate::call_envelope`] so both surfaces stay byte-identical.
//!
//! Operation names are matched case- and separator-insensitively, so `add`,
//! `capture_turn` and `captureTurn` are the same op. This keeps the C header,
//! the Kotlin wrapper and the MCP tool names loosely coupled.

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use pd_memory_core::engine::{FlushOptions, MemoryEngine};
use pd_memory_core::recall::RetrieveOptions;
use pd_memory_core::types::{
    DreamTrigger, IndexingSettings, MemoryActionRequest, MemoryExportBundle, MemoryListOptions,
    MemoryMessage,
};

use crate::FfiError;

/// Dispatch one operation against an engine.
pub(crate) fn dispatch(
    engine: &MemoryEngine,
    op: &str,
    args_json: &str,
) -> Result<Value, FfiError> {
    match normalize_op(op).as_str() {
        "add" | "capture" | "captureturn" => capture(engine, args_json),
        "retrieve" | "retrievecontext" => retrieve(engine, args_json, false),
        "search" => retrieve(engine, args_json, true),
        "list" => list(engine, args_json),
        "get" => get(engine, args_json),
        "overview" => to_value(engine.overview()?),
        "flush" => flush(engine, args_json),
        "dream" => dream(engine, args_json),
        "rollback" | "rollbacklastdream" => to_value(engine.rollback_last_dream()?),
        "export" | "exportbundle" => to_value(engine.export_bundle()?),
        "import" | "importbundle" => import(engine, args_json),
        "settingsget" | "getsettings" => to_value(engine.settings()?),
        "settingssave" | "savesettings" => settings_save(engine, args_json),
        "act" | "action" => act(engine, args_json),
        "stateget" | "getstate" => state_get(engine, args_json),
        "memorycontext" | "context" => memory_context(engine, args_json),
        other => Err(FfiError::unknown_op(other)),
    }
}

/// Normalise an op name: trim, drop `_`/`-`/spaces, lower-case.
pub(crate) fn normalize_op(op: &str) -> String {
    op.trim()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

// ---------------------------------------------------------------------------
// Ops
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CaptureArgs {
    messages: Vec<MemoryMessage>,
    /// Session correlation key; `session_key` and `sessionKey` are accepted too.
    #[serde(default, alias = "session_key", alias = "sessionKey")]
    session_id: Option<String>,
    /// Free-form origin recorded in L0 (`android`, `hook:Stop`, `test`, ...).
    #[serde(default)]
    source: Option<String>,
    /// Optional ISO-8601 timestamp; defaults to the engine clock.
    #[serde(default)]
    timestamp: Option<String>,
}

fn capture(engine: &MemoryEngine, args_json: &str) -> Result<Value, FfiError> {
    let args: CaptureArgs = parse_args(args_json)?;
    let session_key = args
        .session_id
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "default".to_string());
    let source = args
        .source
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "ffi".to_string());
    let result = engine.capture_turn(&args.messages, &session_key, args.timestamp, &source)?;
    to_value(result)
}

#[derive(Debug, Default, Deserialize)]
struct RetrieveArgs {
    #[serde(default)]
    query: Option<String>,
    #[serde(default, alias = "recent", alias = "recentMessages")]
    recent_messages: Vec<MemoryMessage>,
    #[serde(default, alias = "workspaceHint")]
    workspace_hint: Option<String>,
    /// Only honoured by `retrieve`; `search` is always explicit. Defaults to
    /// the config setting when absent (model-gated recall).
    #[serde(default)]
    explicit: Option<bool>,
}

fn retrieve(engine: &MemoryEngine, args_json: &str, explicit: bool) -> Result<Value, FfiError> {
    let args: RetrieveArgs = parse_args(args_json)?;
    let query = args
        .query
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| FfiError::invalid_input("`query` is required and must not be empty"))?;
    let result = if explicit {
        engine.search(&query, &args.recent_messages)?
    } else {
        engine.retrieve_context(
            &query,
            RetrieveOptions {
                recent_messages: args.recent_messages,
                workspace_hint: args.workspace_hint,
                explicit: args.explicit.unwrap_or(false),
            },
        )?
    };
    to_value(result)
}

fn list(engine: &MemoryEngine, args_json: &str) -> Result<Value, FfiError> {
    // Accept either the options object directly or `{"options": {...}}`.
    let args: MemoryListOptions = parse_args_or_key(args_json, "options")?;
    to_value(engine.list(&args)?)
}

#[derive(Debug, Deserialize)]
struct GetArgs {
    #[serde(default)]
    ids: Vec<String>,
    #[serde(default, alias = "maxLines")]
    max_lines: Option<usize>,
}

fn get(engine: &MemoryEngine, args_json: &str) -> Result<Value, FfiError> {
    let args: GetArgs = parse_args(args_json)?;
    let max_lines = args
        .max_lines
        .unwrap_or_else(|| engine.config().retrieval.recall_file_max_lines);
    to_value(engine.get(&args.ids, max_lines)?)
}

#[derive(Debug, Default, Deserialize)]
struct FlushArgs {
    #[serde(default, alias = "batchSize")]
    batch_size: Option<usize>,
    #[serde(default, alias = "sessionKeys")]
    session_keys: Option<Vec<String>>,
    #[serde(default)]
    reason: Option<String>,
}

fn flush(engine: &MemoryEngine, args_json: &str) -> Result<Value, FfiError> {
    let args: FlushArgs = parse_args(args_json)?;
    let options = FlushOptions {
        batch_size: args.batch_size,
        session_keys: args.session_keys,
        reason: args.reason.unwrap_or_else(|| "ffi".to_string()),
    };
    to_value(engine.flush(options)?)
}

fn dream(engine: &MemoryEngine, args_json: &str) -> Result<Value, FfiError> {
    let trigger = parse_dream_trigger(args_json)?;
    to_value(engine.dream(trigger)?)
}

fn parse_dream_trigger(args_json: &str) -> Result<DreamTrigger, FfiError> {
    let value = parse_value(args_json)?;
    let raw = match value {
        Value::Null => return Ok(DreamTrigger::Manual),
        Value::String(text) => text,
        Value::Object(mut map) => match map.remove("trigger") {
            None | Some(Value::Null) => return Ok(DreamTrigger::Manual),
            Some(Value::String(text)) => text,
            Some(other) => {
                return Err(FfiError::invalid_input(format!(
                    "`trigger` must be a string (manual|scheduled), got {other}"
                )))
            }
        },
        other => {
            return Err(FfiError::invalid_input(format!(
                "`dream` expects an object or string trigger, got {other}"
            )))
        }
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "manual" | "user" => Ok(DreamTrigger::Manual),
        "scheduled" | "auto" => Ok(DreamTrigger::Scheduled),
        other => Err(FfiError::invalid_input(format!(
            "unknown dream trigger `{other}` (expected `manual` or `scheduled`)"
        ))),
    }
}

fn import(engine: &MemoryEngine, args_json: &str) -> Result<Value, FfiError> {
    let bundle: MemoryExportBundle = parse_args_or_self(args_json, "bundle")?;
    to_value(engine.import_bundle(&bundle)?)
}

fn settings_save(engine: &MemoryEngine, args_json: &str) -> Result<Value, FfiError> {
    let settings: IndexingSettings = parse_args_or_self(args_json, "settings")?;
    to_value(engine.save_settings(settings)?)
}

fn act(engine: &MemoryEngine, args_json: &str) -> Result<Value, FfiError> {
    // Accept both the internally-tagged action object itself
    // (`{"action":"delete_entries","ids":[...]}`) and a wrapper
    // (`{"action":{"action":"delete_entries", ...}}`).
    let mut value = parse_value(args_json)?;
    if let Value::Object(map) = &mut value {
        let is_wrapped = map
            .get("action")
            .map(|action| action.is_object())
            .unwrap_or(false);
        if is_wrapped {
            value = map.remove("action").unwrap_or(Value::Null);
        }
    }
    let action: MemoryActionRequest = serde_json::from_value(value)
        .map_err(|error| FfiError::invalid_input(format!("invalid action: {error}")))?;
    to_value(engine.act(action)?)
}

#[derive(Debug, Deserialize)]
struct StateGetArgs {
    key: String,
}

fn state_get(engine: &MemoryEngine, args_json: &str) -> Result<Value, FfiError> {
    let args: StateGetArgs = parse_args(args_json)?;
    if args.key.trim().is_empty() {
        return Err(FfiError::invalid_input("`key` must not be empty"));
    }
    to_value(engine.state_get(&args.key)?)
}

fn memory_context(engine: &MemoryEngine, args_json: &str) -> Result<Value, FfiError> {
    let args: RetrieveArgs = parse_args(args_json)?;
    let query = args
        .query
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| FfiError::invalid_input("`query` is required and must not be empty"))?;
    let result = engine.retrieve_context(
        &query,
        RetrieveOptions {
            recent_messages: args.recent_messages,
            workspace_hint: args.workspace_hint,
            explicit: args.explicit.unwrap_or(false),
        },
    )?;
    // Compact shape for prompt-injection use sites: just the rendered
    // four-section system context (null when nothing matched).
    Ok(json!({ "systemContext": result.system_context }))
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

/// Parse `args_json` as a JSON value; empty or whitespace means `null`.
fn parse_value(args_json: &str) -> Result<Value, FfiError> {
    let trimmed = args_json.trim();
    if trimmed.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(trimmed)
        .map_err(|error| FfiError::invalid_input(format!("args_json is not valid JSON: {error}")))
}

/// Parse an operation's argument object; `null`/empty is treated as `{}`.
fn parse_args<T: DeserializeOwned>(args_json: &str) -> Result<T, FfiError> {
    let value = parse_value(args_json)?;
    let value = if value.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        value
    };
    serde_json::from_value(value)
        .map_err(|error| FfiError::invalid_input(format!("invalid arguments: {error}")))
}

/// Parse `{"<key>": {...}}` or the bare `{...}` document into `T`.
fn parse_args_or_key<T: DeserializeOwned>(args_json: &str, key: &str) -> Result<T, FfiError> {
    let value = parse_value(args_json)?;
    let value = unwrap_key(value, key);
    let value = if value.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        value
    };
    serde_json::from_value(value)
        .map_err(|error| FfiError::invalid_input(format!("invalid `{key}` arguments: {error}")))
}

/// Parse `{"<key>": {...}}` or the bare document itself into `T` (not `{}`).
fn parse_args_or_self<T: DeserializeOwned>(args_json: &str, key: &str) -> Result<T, FfiError> {
    let value = unwrap_key(parse_value(args_json)?, key);
    if value.is_null() {
        return Err(FfiError::invalid_input(format!(
            "a `{key}` object is required"
        )));
    }
    serde_json::from_value(value)
        .map_err(|error| FfiError::invalid_input(format!("invalid `{key}` object: {error}")))
}

fn unwrap_key(value: Value, key: &str) -> Value {
    match value {
        Value::Object(mut map) => map.remove(key).unwrap_or(Value::Object(map)),
        other => other,
    }
}

fn to_value<T: serde::Serialize>(value: T) -> Result<Value, FfiError> {
    serde_json::to_value(value)
        .map_err(|error| FfiError::internal(format!("could not serialize result: {error}")))
}
