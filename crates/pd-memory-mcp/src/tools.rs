//! MCP tool catalogue and handlers.
//!
//! The `pd-memory` server exposes seven tools. Every tool returns pretty JSON
//! as `content[0].text` plus the same object as `structuredContent`.
//!
//! Tool execution is written against [`MemoryBackend`] rather than
//! [`pd_memory_core::engine::MemoryEngine`] directly so JSON-RPC dispatch can be
//! unit-tested without a live engine.

use serde_json::{json, Map, Value};

use pd_memory_core::engine::MemoryEngine;
use pd_memory_core::error::{MemoryError, Result};
use pd_memory_core::types::*;

/// Maximum ids accepted by `memory_get` in one call.
pub const MAX_GET_IDS: usize = 100;

/// Engine operations required by the MCP tools.
///
/// Implemented for the real engine through [`EngineBackend`]; tests provide a
/// deterministic fake.
pub trait MemoryBackend: Send + Sync {
    /// Model-guided (explicit) memory search.
    fn search(&self, query: &str, recent: Vec<MemoryMessage>) -> Result<RetrievalResult>;
    /// Browse the memory manifest.
    fn list(&self, options: &MemoryListOptions) -> Result<Vec<MemoryManifestEntry>>;
    /// Read memory files by id.
    fn get(&self, ids: &[String], max_lines: usize) -> Result<Vec<MemoryFileRecord>>;
    /// Overview / health payload.
    fn overview(&self) -> Result<MemoryOverview>;
    /// Run one indexing batch.
    fn flush(&self, batch_size: Option<usize>, reason: &str) -> Result<HeartbeatStats>;
    /// Run dream consolidation.
    fn dream(&self, trigger: DreamTrigger) -> Result<DreamRunResult>;
    /// Capture one conversation turn into the L0 queue.
    fn add(
        &self,
        messages: &[MemoryMessage],
        session_id: &str,
        source: &str,
    ) -> Result<CaptureTurnResult>;
}

/// [`MemoryBackend`] backed by the real [`MemoryEngine`].
pub(crate) struct EngineBackend<'a> {
    engine: &'a MemoryEngine,
}

impl<'a> EngineBackend<'a> {
    pub(crate) fn new(engine: &'a MemoryEngine) -> Self {
        Self { engine }
    }
}

impl MemoryBackend for EngineBackend<'_> {
    fn search(&self, query: &str, recent: Vec<MemoryMessage>) -> Result<RetrievalResult> {
        self.engine.search(query, &recent)
    }

    fn list(&self, options: &MemoryListOptions) -> Result<Vec<MemoryManifestEntry>> {
        self.engine.list(options)
    }

    fn get(&self, ids: &[String], max_lines: usize) -> Result<Vec<MemoryFileRecord>> {
        self.engine.get(ids, max_lines)
    }

    fn overview(&self) -> Result<MemoryOverview> {
        self.engine.overview()
    }

    fn flush(&self, batch_size: Option<usize>, reason: &str) -> Result<HeartbeatStats> {
        self.engine.flush(pd_memory_core::engine::FlushOptions {
            batch_size,
            session_keys: None,
            reason: reason.to_string(),
        })
    }

    fn dream(&self, trigger: DreamTrigger) -> Result<DreamRunResult> {
        self.engine.dream(trigger)
    }

    fn add(
        &self,
        messages: &[MemoryMessage],
        session_id: &str,
        source: &str,
    ) -> Result<CaptureTurnResult> {
        self.engine.capture_turn(messages, session_id, None, source)
    }
}

/// Tool-level failure rendered as an MCP `isError: true` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    /// Stable machine-readable code (`invalid_arguments`, `model`, `io`, ...).
    pub code: &'static str,
    /// Human-readable message.
    pub message: String,
}

impl ToolError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: "invalid_arguments",
            message: message.into(),
        }
    }

    fn from_memory(error: MemoryError) -> Self {
        Self {
            code: error.code(),
            message: error.to_string(),
        }
    }
}

fn to_value<T: serde::Serialize>(value: &T) -> std::result::Result<Value, ToolError> {
    serde_json::to_value(value).map_err(|error| ToolError {
        code: "internal",
        message: format!("could not serialize tool result: {error}"),
    })
}

/// All tool definitions, in catalogue order.
pub fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "memory_search",
            "description": "Search long-term project and user memory. Use before answering questions that depend on durable preferences, collaboration rules, project facts, decisions, or progress.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "What to recall, in natural language."
                    },
                    "recent_messages": {
                        "type": "array",
                        "description": "Optional recent conversation messages used to disambiguate the query.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "role": {"type": "string", "description": "user or assistant"},
                                "content": {"type": "string"}
                            },
                            "required": ["role", "content"],
                            "additionalProperties": false
                        }
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 20,
                        "description": "Maximum number of selected memory files to keep in the result."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "memory_list",
            "description": "Browse file-based memory indexes (project, feedback, user profiles). Returns manifest entries without file bodies.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kinds": {
                        "type": "array",
                        "description": "Restrict results to these memory kinds.",
                        "items": {
                            "type": "string",
                            "enum": ["user", "feedback", "project", "general_project_meta"]
                        }
                    },
                    "query": {
                        "type": "string",
                        "description": "Optional case-insensitive filter over names and descriptions."
                    },
                    "limit": {"type": "integer", "minimum": 1, "maximum": 500},
                    "offset": {"type": "integer", "minimum": 0},
                    "include_deprecated": {
                        "type": "boolean",
                        "description": "Include deprecated entries (default false)."
                    }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "memory_get",
            "description": "Read full memory files by id. Use only for exact ids returned by memory_search or memory_list; ids are relative paths such as Project/example-1234567890.md.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ids": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 100,
                        "items": {"type": "string"}
                    },
                    "max_lines": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 2000,
                        "description": "Maximum body lines per file (default 200)."
                    }
                },
                "required": ["ids"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "memory_overview",
            "description": "Memory status: file counts, freshness, indexing backlog, dream status, and current project metadata.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }),
        json!({
            "name": "memory_flush",
            "description": "Index recently captured conversation turns into memory files now. Use when the user asks why a just-finished conversation is not visible yet.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "batch_size": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 500,
                        "description": "Maximum number of pending L0 session batches to process."
                    },
                    "reason": {"type": "string", "description": "Free-form reason recorded in pipeline traces."}
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "memory_dream",
            "description": "Run memory consolidation (deduplicate, merge, rewrite project memory and user profile). Reversible with the CLI rollback command.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "trigger": {
                        "type": "string",
                        "enum": ["manual", "scheduled"],
                        "description": "Dream trigger recorded in traces (default manual)."
                    }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "memory_add",
            "description": "Capture one conversation turn into the raw L0 queue. Indexing into memory files happens on the next flush.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "messages": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "role": {"type": "string", "description": "user or assistant"},
                                "content": {"type": "string"}
                            },
                            "required": ["role", "content"],
                            "additionalProperties": false
                        }
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Stable session identifier used to group captured turns."
                    },
                    "source": {
                        "type": "string",
                        "description": "Capture source label (default mcp)."
                    }
                },
                "required": ["messages", "session_id"],
                "additionalProperties": false
            }
        }),
    ]
}

fn object_arg(args: &Value) -> std::result::Result<&Map<String, Value>, ToolError> {
    args.as_object()
        .ok_or_else(|| ToolError::invalid("tool arguments must be a JSON object"))
}

fn required_string(args: &Map<String, Value>, key: &str) -> std::result::Result<String, ToolError> {
    match args.get(key) {
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(value.clone()),
        Some(Value::String(_)) => Err(ToolError::invalid(format!(
            "argument `{key}` must not be empty"
        ))),
        Some(_) => Err(ToolError::invalid(format!(
            "argument `{key}` must be a string"
        ))),
        None => Err(ToolError::invalid(format!(
            "missing required argument `{key}`"
        ))),
    }
}

fn optional_string(
    args: &Map<String, Value>,
    key: &str,
) -> std::result::Result<Option<String>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ToolError::invalid(format!(
            "argument `{key}` must be a string"
        ))),
    }
}

fn optional_usize(
    args: &Map<String, Value>,
    key: &str,
) -> std::result::Result<Option<usize>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .map(|value| Some(value as usize))
            .ok_or_else(|| {
                ToolError::invalid(format!("argument `{key}` must be a non-negative integer"))
            }),
        Some(_) => Err(ToolError::invalid(format!(
            "argument `{key}` must be an integer"
        ))),
    }
}

fn optional_bool(
    args: &Map<String, Value>,
    key: &str,
) -> std::result::Result<Option<bool>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(ToolError::invalid(format!(
            "argument `{key}` must be a boolean"
        ))),
    }
}

fn string_array(
    args: &Map<String, Value>,
    key: &str,
) -> std::result::Result<Vec<String>, ToolError> {
    match args.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_string).ok_or_else(|| {
                    ToolError::invalid(format!("argument `{key}` must be an array of strings"))
                })
            })
            .collect(),
        Some(_) => Err(ToolError::invalid(format!(
            "argument `{key}` must be an array of strings"
        ))),
        None => Err(ToolError::invalid(format!(
            "missing required argument `{key}`"
        ))),
    }
}

fn messages_arg(
    args: &Map<String, Value>,
    key: &str,
) -> std::result::Result<Vec<MemoryMessage>, ToolError> {
    let items = match args.get(key) {
        Some(Value::Array(items)) => items,
        Some(_) => {
            return Err(ToolError::invalid(format!(
                "argument `{key}` must be an array of messages"
            )))
        }
        None => {
            return Err(ToolError::invalid(format!(
                "missing required argument `{key}`"
            )))
        }
    };
    let mut messages = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        match item {
            Value::String(content) => messages.push(MemoryMessage::user(content.clone())),
            Value::Object(map) => {
                let content = match map.get("content") {
                    Some(Value::String(content)) => content.clone(),
                    _ => {
                        return Err(ToolError::invalid(format!(
                            "argument `{key}[{index}].content` must be a string"
                        )))
                    }
                };
                let role = match map.get("role") {
                    None | Some(Value::Null) => "user".to_string(),
                    Some(Value::String(role)) if !role.trim().is_empty() => role.clone(),
                    Some(_) => {
                        return Err(ToolError::invalid(format!(
                            "argument `{key}[{index}].role` must be a string"
                        )))
                    }
                };
                messages.push(MemoryMessage::new(role, content));
            }
            _ => {
                return Err(ToolError::invalid(format!(
                    "argument `{key}[{index}]` must be an object or string"
                )))
            }
        }
    }
    if messages.is_empty() {
        return Err(ToolError::invalid(format!(
            "argument `{key}` must contain at least one message"
        )));
    }
    Ok(messages)
}

fn kinds_arg(args: &Map<String, Value>) -> std::result::Result<Vec<MemoryRecordType>, ToolError> {
    let raw = match args.get("kinds") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => {
            return Err(ToolError::invalid(
                "argument `kinds` must be an array of strings",
            ))
        }
    };
    let mut kinds = Vec::with_capacity(raw.len());
    for item in raw {
        let Some(raw_kind) = item.as_str() else {
            return Err(ToolError::invalid(
                "argument `kinds` must be an array of strings",
            ));
        };
        let kind = MemoryRecordType::parse(raw_kind).ok_or_else(|| {
            ToolError::invalid(format!(
                "unknown memory kind `{raw_kind}`; expected one of user, feedback, project, general_project_meta"
            ))
        })?;
        kinds.push(kind);
    }
    Ok(kinds)
}

fn trigger_arg(args: &Map<String, Value>) -> std::result::Result<DreamTrigger, ToolError> {
    match optional_string(args, "trigger")?.as_deref() {
        None => Ok(DreamTrigger::Manual),
        Some(value) => match value.trim().to_ascii_lowercase().as_str() {
            "manual" => Ok(DreamTrigger::Manual),
            "scheduled" => Ok(DreamTrigger::Scheduled),
            other => Err(ToolError::invalid(format!(
                "unknown dream trigger `{other}`; expected manual or scheduled"
            ))),
        },
    }
}

fn bounded(value: Option<usize>, default: usize, max: usize) -> usize {
    value.unwrap_or(default).clamp(1, max)
}

/// Execute one tool. `Ok(value)` is the `structuredContent` payload; `Err` is
/// rendered as an MCP tool error result (`isError: true`).
pub fn call_tool(
    backend: &dyn MemoryBackend,
    name: &str,
    args: &Value,
) -> std::result::Result<Value, ToolError> {
    let args = object_arg(args)?;
    match name {
        "memory_search" => {
            let query = required_string(args, "query")?;
            let recent = match args.get("recent_messages") {
                None | Some(Value::Null) => Vec::new(),
                Some(_) => messages_arg(args, "recent_messages")?,
            };
            let limit = optional_usize(args, "limit")?;
            let mut result = backend
                .search(&query, recent)
                .map_err(ToolError::from_memory)?;
            if let Some(limit) = limit {
                result.selected_ids.truncate(limit.max(1));
            }
            to_value(&result)
        }
        "memory_list" => {
            let options = MemoryListOptions {
                kinds: kinds_arg(args)?,
                query: optional_string(args, "query")?.filter(|value| !value.trim().is_empty()),
                include_deprecated: optional_bool(args, "include_deprecated")?.unwrap_or(false),
                limit: optional_usize(args, "limit")?,
                offset: optional_usize(args, "offset")?.unwrap_or(0),
            };
            let entries = backend.list(&options).map_err(ToolError::from_memory)?;
            Ok(json!({
                "count": entries.len(),
                "entries": to_value(&entries)?,
            }))
        }
        "memory_get" => {
            let ids = string_array(args, "ids")?;
            if ids.len() > MAX_GET_IDS {
                return Err(ToolError::invalid(format!(
                    "argument `ids` accepts at most {MAX_GET_IDS} entries"
                )));
            }
            let max_lines = bounded(optional_usize(args, "max_lines")?, 200, 2_000);
            let files = backend
                .get(&ids, max_lines)
                .map_err(ToolError::from_memory)?;
            Ok(json!({
                "count": files.len(),
                "files": to_value(&files)?,
            }))
        }
        "memory_overview" => {
            let overview = backend.overview().map_err(ToolError::from_memory)?;
            to_value(&overview)
        }
        "memory_flush" => {
            let batch_size = optional_usize(args, "batch_size")?;
            let reason = optional_string(args, "reason")?.unwrap_or_else(|| "mcp".to_string());
            let stats = backend
                .flush(batch_size, &reason)
                .map_err(ToolError::from_memory)?;
            to_value(&stats)
        }
        "memory_dream" => {
            let trigger = trigger_arg(args)?;
            let result = backend.dream(trigger).map_err(ToolError::from_memory)?;
            to_value(&result)
        }
        "memory_add" => {
            let messages = messages_arg(args, "messages")?;
            let session_id = required_string(args, "session_id")?;
            let source = optional_string(args, "source")?.unwrap_or_else(|| "mcp".to_string());
            let mut result = backend
                .add(&messages, &session_id, &source)
                .map_err(ToolError::from_memory)?;
            result.session_key = if result.session_key.is_empty() {
                session_id
            } else {
                result.session_key
            };
            to_value(&result)
        }
        other => Err(ToolError {
            code: "unknown_tool",
            message: format!("unknown tool `{other}`"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_has_seven_unique_tools_with_schemas() {
        let definitions = tool_definitions();
        assert_eq!(definitions.len(), 7);
        let mut names: Vec<&str> = definitions
            .iter()
            .map(|definition| definition["name"].as_str().expect("tool name"))
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "memory_add",
                "memory_dream",
                "memory_flush",
                "memory_get",
                "memory_list",
                "memory_overview",
                "memory_search",
            ]
        );
        for definition in &definitions {
            assert!(definition["description"]
                .as_str()
                .is_some_and(|value| !value.is_empty()));
            assert_eq!(definition["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn argument_helpers_validate_types() {
        let args = json!({"query": "hello", "limit": 3, "flag": true});
        let map = args.as_object().unwrap();
        assert_eq!(required_string(map, "query").unwrap(), "hello");
        assert_eq!(optional_usize(map, "limit").unwrap(), Some(3));
        assert_eq!(optional_bool(map, "flag").unwrap(), Some(true));
        assert!(required_string(map, "missing").is_err());
        assert!(required_string(map, "limit").is_err());
        assert!(optional_usize(map, "query").is_err());
    }

    #[test]
    fn kinds_parse_and_reject_unknown() {
        let args = json!({"kinds": ["project", "user"]});
        let kinds = kinds_arg(args.as_object().unwrap()).unwrap();
        assert_eq!(
            kinds,
            vec![MemoryRecordType::Project, MemoryRecordType::User]
        );
        let bad = json!({"kinds": ["nope"]});
        assert!(kinds_arg(bad.as_object().unwrap()).is_err());
        let empty = json!({});
        assert!(kinds_arg(empty.as_object().unwrap()).unwrap().is_empty());
    }

    #[test]
    fn trigger_parses_and_defaults() {
        let empty = json!({});
        assert_eq!(
            trigger_arg(empty.as_object().unwrap()).unwrap(),
            DreamTrigger::Manual
        );
        let scheduled = json!({"trigger": "Scheduled"});
        assert_eq!(
            trigger_arg(scheduled.as_object().unwrap()).unwrap(),
            DreamTrigger::Scheduled
        );
        let bad = json!({"trigger": "later"});
        assert!(trigger_arg(bad.as_object().unwrap()).is_err());
    }

    #[test]
    fn messages_accept_objects_and_strings() {
        let args = json!({"messages": [
            {"role": "user", "content": "hi"},
            "plain text"
        ]});
        let messages = messages_arg(args.as_object().unwrap(), "messages").unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].content, "plain text");
    }
}
