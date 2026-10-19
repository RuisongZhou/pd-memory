//! MCP (Model Context Protocol) stdio server for `pd-memory`.
//!
//! Transport: newline-delimited JSON-RPC 2.0 on stdin/stdout, per the MCP
//! stdio transport. Methods: `initialize`, `notifications/initialized`, `ping`,
//! `tools/list`, `tools/call` (plus tolerant handling of unknown
//! `notifications/*`). Tools: `memory_search`, `memory_list`, `memory_get`,
//! `memory_overview`, `memory_flush`, `memory_dream`, `memory_add`.
//!
//! Invariants:
//! - stdout carries **only** JSON-RPC responses (one per line); logs go to
//!   stderr via the `log` crate;
//! - notifications (messages without `id`) never produce a response;
//! - the tool catalogue is stable and sorted so clients and tests can assert it;
//! - `tools/list` and `initialize` need no model or credentials.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use pd_memory_core::engine::MemoryEngine;
use pd_memory_core::error::{MemoryError, Result};
use serde_json::{json, Map, Value};

pub mod protocol;
pub mod tools;

use protocol::{
    error, negotiate_protocol_version, success, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND,
    PARSE_ERROR,
};
use tools::{call_tool, EngineBackend, MemoryBackend};

/// Minimal MCP server configuration.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    /// Name reported in `serverInfo.name` (and used by clients to identify us).
    pub server_name: String,
    /// Version reported in `serverInfo.version`.
    pub server_version: String,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            server_name: "pd-memory".into(),
            server_version: pd_memory_core::VERSION.into(),
        }
    }
}

/// Run the stdio server until stdin closes.
pub fn serve_stdio(engine: Arc<MemoryEngine>, config: McpServerConfig) -> Result<()> {
    let backend = EngineBackend::new(&engine);
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve_stdio_with_backend(&backend, &config, stdin.lock(), stdout.lock())
}

/// Run the JSON-RPC line loop over arbitrary streams (exposed for tests and
/// embedded transports). Returns [`MemoryError::Io`] if the streams fail.
pub fn serve_stdio_with_backend(
    backend: &dyn MemoryBackend,
    config: &McpServerConfig,
    reader: impl BufRead,
    mut writer: impl Write,
) -> Result<()> {
    log::info!(
        "pd-memory MCP stdio server started (server={}, version={})",
        config.server_name,
        config.server_version
    );
    for line in reader.lines() {
        let line = line.map_err(|source| MemoryError::io(PathBuf::from("<stdin>"), source))?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_line_with_backend(backend, config, &line) {
            let encoded = serde_json::to_string(&response)?;
            writer
                .write_all(encoded.as_bytes())
                .and_then(|_| writer.write_all(b"\n"))
                .and_then(|_| writer.flush())
                .map_err(|source| MemoryError::io(PathBuf::from("<stdout>"), source))?;
        }
    }
    log::info!("pd-memory MCP stdio server stopped (stdin closed)");
    Ok(())
}

/// Handle one JSON-RPC line (exposed for tests and non-stdio transports).
///
/// Returns `None` for notifications and empty lines, otherwise the JSON-RPC
/// response object.
pub fn handle_line(engine: &MemoryEngine, config: &McpServerConfig, line: &str) -> Option<Value> {
    let backend = EngineBackend::new(engine);
    handle_line_with_backend(&backend, config, line)
}

/// [`handle_line`] against a [`MemoryBackend`] (unit-test seam).
pub(crate) fn handle_line_with_backend(
    backend: &dyn MemoryBackend,
    config: &McpServerConfig,
    line: &str,
) -> Option<Value> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value: Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(parse_error) => {
            return Some(error(
                Value::Null,
                PARSE_ERROR,
                format!("parse error: {parse_error}"),
            ))
        }
    };
    let Some(object) = value.as_object() else {
        return Some(error(
            Value::Null,
            INVALID_REQUEST,
            "invalid request: expected a JSON object",
        ));
    };
    let is_notification = !object.contains_key("id");
    let id = object.get("id").cloned().unwrap_or(Value::Null);
    if is_notification {
        // MCP notifications (initialized/cancelled/...) never get a response.
        log::debug!("ignoring MCP notification");
        return None;
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return Some(error(
            id,
            INVALID_REQUEST,
            "invalid request: missing method",
        ));
    };
    let params = object.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "initialize" => Some(success(id, initialize_result(config, &params))),
        "ping" => Some(success(id, json!({}))),
        "tools/list" => Some(success(id, json!({ "tools": sorted_tool_definitions() }))),
        "tools/call" => Some(tools_call(backend, id, &params)),
        other if other.starts_with("notifications/") => {
            log::debug!("ignoring MCP notification method {other}");
            None
        }
        other => Some(error(
            id,
            METHOD_NOT_FOUND,
            format!("method not found: {other}"),
        )),
    }
}

/// Build the `initialize` result.
fn initialize_result(config: &McpServerConfig, params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let protocol_version = negotiate_protocol_version(requested);
    let client = params.get("clientInfo");
    if let Some(client) = client {
        log::info!(
            "MCP client connected: {} {}",
            client
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>"),
            client
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>")
        );
    }
    json!({
        "protocolVersion": protocol_version,
        "capabilities": {
            "tools": {
                "listChanged": false
            }
        },
        "serverInfo": {
            "name": config.server_name,
            "version": config.server_version,
        },
    })
}

/// Tool definitions sorted by name (stable catalogue for clients and tests).
pub fn sorted_tool_definitions() -> Vec<Value> {
    let mut definitions = tools::tool_definitions();
    definitions.sort_by(|left, right| {
        left.get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .cmp(
                right
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
    });
    definitions
}

/// All tool definitions as a JSON array (used by the CLI `doctor` command).
pub fn tools_json() -> Value {
    Value::Array(sorted_tool_definitions())
}

/// Handle a `tools/call` request.
fn tools_call(backend: &dyn MemoryBackend, id: Value, params: &Value) -> Value {
    let Some(params) = params.as_object() else {
        return error(id, INVALID_PARAMS, "invalid params: expected an object");
    };
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return error(
            id,
            INVALID_PARAMS,
            "invalid params: missing tool name `<name>`",
        );
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !arguments.is_object() {
        return error(
            id,
            INVALID_PARAMS,
            "invalid params: `arguments` must be an object",
        );
    }
    if !tools::tool_definitions()
        .iter()
        .any(|definition| definition.get("name").and_then(Value::as_str) == Some(name))
    {
        return error(id, INVALID_PARAMS, format!("unknown tool `{name}`"));
    }
    match call_tool(backend, name, &arguments) {
        Ok(structured) => success(id, tool_success_result(&structured)),
        Err(tool_error) => success(id, tool_error_result(&tool_error)),
    }
}

/// MCP tool success payload: text content + structured content.
fn tool_success_result(structured: &Value) -> Value {
    let text = serde_json::to_string_pretty(structured).unwrap_or_else(|_| structured.to_string());
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": structured,
        "isError": false,
    })
}

/// MCP tool error payload (`isError: true`, never a transport failure).
fn tool_error_result(tool_error: &tools::ToolError) -> Value {
    let structured = json!({
        "error": {
            "code": tool_error.code,
            "message": tool_error.message,
        }
    });
    json!({
        "content": [{"type": "text", "text": tool_error.message}],
        "structuredContent": structured,
        "isError": true,
    })
}

/// Parse an MCP `params` map (used by non-stdio transports).
pub fn params_object(params: &Value) -> Option<&Map<String, Value>> {
    params.as_object()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::Mutex;

    use pd_memory_core::types::*;

    #[derive(Default)]
    struct FakeBackend {
        calls: Mutex<Vec<String>>,
        fail_overview: bool,
        skip_add: bool,
    }

    impl FakeBackend {
        fn record(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl MemoryBackend for FakeBackend {
        fn search(&self, query: &str, recent: Vec<MemoryMessage>) -> Result<RetrievalResult> {
            self.record(format!("search:{query}:{}", recent.len()));
            Ok(RetrievalResult {
                route: MemoryRoute::Mix,
                context: Some("remembered context".into()),
                system_context: Some("<memory-context>remembered context</memory-context>".into()),
                selected_ids: vec!["Project/a.md".into(), "Project/b.md".into()],
                ..Default::default()
            })
        }

        fn list(&self, options: &MemoryListOptions) -> Result<Vec<MemoryManifestEntry>> {
            self.record(format!("list:{}", options.kinds.len()));
            Ok(vec![MemoryManifestEntry {
                frontmatter: MemoryFileFrontmatter {
                    name: "Project memory".into(),
                    description: "desc".into(),
                    ..Default::default()
                },
                file: "Project/a.md".into(),
                relative_path: "Project/a.md".into(),
                absolute_path: "/tmp/Project/a.md".into(),
            }])
        }

        fn get(&self, ids: &[String], max_lines: usize) -> Result<Vec<MemoryFileRecord>> {
            self.record(format!("get:{}:{max_lines}", ids.len()));
            Ok(Vec::new())
        }

        fn overview(&self) -> Result<MemoryOverview> {
            self.record("overview".into());
            if self.fail_overview {
                return Err(MemoryError::Corrupt {
                    path: PathBuf::from("/tmp/memory"),
                    message: "manifest is broken".into(),
                });
            }
            Ok(MemoryOverview {
                total_files: 3,
                project_memories: 2,
                feedback_memories: 1,
                workspace_name: "demo".into(),
                ..Default::default()
            })
        }

        fn flush(&self, batch_size: Option<usize>, reason: &str) -> Result<HeartbeatStats> {
            self.record(format!("flush:{batch_size:?}:{reason}"));
            Ok(HeartbeatStats {
                processed_turns: 1,
                written_files: 1,
                ..Default::default()
            })
        }

        fn dream(&self, trigger: DreamTrigger) -> Result<DreamRunResult> {
            self.record(format!("dream:{trigger:?}"));
            Ok(DreamRunResult {
                trigger,
                status: DreamPipelineStatus::Success,
                summary: "dream ok".into(),
                ..Default::default()
            })
        }

        fn add(
            &self,
            messages: &[MemoryMessage],
            session_id: &str,
            source: &str,
        ) -> Result<CaptureTurnResult> {
            self.record(format!("add:{}:{session_id}:{source}", messages.len()));
            Ok(CaptureTurnResult {
                captured: !self.skip_add,
                normalized_messages: if self.skip_add {
                    Vec::new()
                } else {
                    messages.to_vec()
                },
                session_key: session_id.to_string(),
            })
        }
    }

    fn config() -> McpServerConfig {
        McpServerConfig {
            server_name: "pd-memory-test".into(),
            server_version: "9.9.9".into(),
        }
    }

    fn request(id: u64, method: &str, params: Value) -> String {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
        .to_string()
    }

    fn parse(response: &Value) -> &Value {
        assert_eq!(response["jsonrpc"], "2.0");
        response
    }

    #[test]
    fn initialize_handshake_and_negotiation() {
        let backend = FakeBackend::default();
        let line = request(
            1,
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "workbuddy", "version": "1.0.0"}
            }),
        );
        let response = handle_line_with_backend(&backend, &config(), &line).expect("response");
        let response = parse(&response);
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(
            response["result"]["capabilities"]["tools"]["listChanged"],
            false
        );
        assert_eq!(response["result"]["serverInfo"]["name"], "pd-memory-test");
        assert_eq!(response["result"]["serverInfo"]["version"], "9.9.9");
        // The handshake must not touch the engine/backend.
        assert!(backend.calls().is_empty());
    }

    #[test]
    fn initialize_echoes_unknown_but_well_formed_versions() {
        let backend = FakeBackend::default();
        let line = request(2, "initialize", json!({"protocolVersion": "2025-11-25"}));
        let response = handle_line_with_backend(&backend, &config(), &line).unwrap();
        assert_eq!(response["result"]["protocolVersion"], "2025-11-25");
        let weird = request(3, "initialize", json!({"protocolVersion": "v2"}));
        let response = handle_line_with_backend(&backend, &config(), &weird).unwrap();
        assert_eq!(
            response["result"]["protocolVersion"],
            protocol::DEFAULT_PROTOCOL_VERSION
        );
    }

    #[test]
    fn notifications_produce_no_response() {
        let backend = FakeBackend::default();
        for line in [
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(),
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {}})
                .to_string(),
            json!({"jsonrpc": "2.0", "method": "ping"}).to_string(),
        ] {
            assert!(handle_line_with_backend(&backend, &config(), &line).is_none());
        }
    }

    #[test]
    fn ping_returns_empty_result() {
        let backend = FakeBackend::default();
        let line = request(4, "ping", json!({}));
        let response = handle_line_with_backend(&backend, &config(), &line).unwrap();
        assert_eq!(response["result"], json!({}));
    }

    #[test]
    fn tools_list_is_stable_sorted_and_complete() {
        let backend = FakeBackend::default();
        let line = request(5, "tools/list", json!({}));
        let response = handle_line_with_backend(&backend, &config(), &line).unwrap();
        let tools = response["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 7);
        let names: Vec<&str> = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "tools must be sorted by name");
        for tool in tools {
            assert!(tool["description"]
                .as_str()
                .is_some_and(|value| !value.is_empty()));
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
        assert!(tools_json()
            .as_array()
            .is_some_and(|array| array.len() == 7));
        assert!(backend.calls().is_empty());
    }

    #[test]
    fn unknown_method_returns_minus_32601() {
        let backend = FakeBackend::default();
        let line = request(6, "memory/magic", json!({}));
        let response = handle_line_with_backend(&backend, &config(), &line).unwrap();
        assert_eq!(response["error"]["code"], protocol::METHOD_NOT_FOUND);
        assert_eq!(response["id"], 6);
    }

    #[test]
    fn parse_error_returns_minus_32700_with_null_id() {
        let backend = FakeBackend::default();
        let response = handle_line_with_backend(&backend, &config(), "{not json").unwrap();
        assert_eq!(response["error"]["code"], protocol::PARSE_ERROR);
        assert_eq!(response["id"], Value::Null);
    }

    #[test]
    fn non_object_json_is_invalid_request() {
        let backend = FakeBackend::default();
        let response = handle_line_with_backend(&backend, &config(), "[1,2,3]").unwrap();
        assert_eq!(response["error"]["code"], protocol::INVALID_REQUEST);
    }

    #[test]
    fn missing_method_is_invalid_request() {
        let backend = FakeBackend::default();
        let line = json!({"jsonrpc": "2.0", "id": 7}).to_string();
        let response = handle_line_with_backend(&backend, &config(), &line).unwrap();
        assert_eq!(response["error"]["code"], protocol::INVALID_REQUEST);
    }

    #[test]
    fn tools_call_missing_name_is_invalid_params() {
        let backend = FakeBackend::default();
        let line = request(8, "tools/call", json!({"arguments": {}}));
        let response = handle_line_with_backend(&backend, &config(), &line).unwrap();
        assert_eq!(response["error"]["code"], protocol::INVALID_PARAMS);
    }

    #[test]
    fn tools_call_unknown_tool_is_invalid_params() {
        let backend = FakeBackend::default();
        let line = request(
            9,
            "tools/call",
            json!({"name": "memory_unknown", "arguments": {}}),
        );
        let response = handle_line_with_backend(&backend, &config(), &line).unwrap();
        assert_eq!(response["error"]["code"], protocol::INVALID_PARAMS);
    }

    #[test]
    fn tools_call_memory_overview_returns_content_and_structured_content() {
        let backend = FakeBackend::default();
        let line = request(10, "tools/call", json!({"name": "memory_overview"}));
        let response = handle_line_with_backend(&backend, &config(), &line).unwrap();
        let result = &response["result"];
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"].as_str().expect("text content");
        let parsed: Value = serde_json::from_str(text).expect("text is JSON");
        assert_eq!(parsed, result["structuredContent"]);
        assert_eq!(result["structuredContent"]["total_files"], 3);
        assert_eq!(result["content"][0]["type"], "text");
        assert_eq!(backend.calls(), vec!["overview".to_string()]);
    }

    #[test]
    fn tools_call_memory_search_passes_arguments_and_limit() {
        let backend = FakeBackend::default();
        let line = request(
            11,
            "tools/call",
            json!({
                "name": "memory_search",
                "arguments": {
                    "query": "deployment rules",
                    "recent_messages": [{"role": "user", "content": "hi"}],
                    "limit": 1
                }
            }),
        );
        let response = handle_line_with_backend(&backend, &config(), &line).unwrap();
        let result = &response["result"];
        assert_eq!(result["isError"], false);
        assert_eq!(
            result["structuredContent"]["selected_ids"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            backend.calls(),
            vec!["search:deployment rules:1".to_string()]
        );
    }

    #[test]
    fn tools_call_invalid_arguments_returns_is_error_not_transport_error() {
        let backend = FakeBackend::default();
        let line = request(
            12,
            "tools/call",
            json!({"name": "memory_get", "arguments": {"ids": "not-an-array"}}),
        );
        let response = handle_line_with_backend(&backend, &config(), &line).unwrap();
        assert!(response.get("error").is_none());
        let result = &response["result"];
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("ids"));
        assert_eq!(
            result["structuredContent"]["error"]["code"],
            "invalid_arguments"
        );
    }

    #[test]
    fn backend_errors_are_tool_errors_with_stable_codes() {
        let backend = FakeBackend {
            fail_overview: true,
            ..Default::default()
        };
        let line = request(
            13,
            "tools/call",
            json!({"name": "memory_overview", "arguments": {}}),
        );
        let response = handle_line_with_backend(&backend, &config(), &line).unwrap();
        let result = &response["result"];
        assert_eq!(result["isError"], true);
        assert_eq!(result["structuredContent"]["error"]["code"], "corrupt_data");
    }

    #[test]
    fn memory_add_skipped_input_is_not_reported_as_duplicate() {
        let backend = FakeBackend {
            skip_add: true,
            ..Default::default()
        };
        let line = request(
            14,
            "tools/call",
            json!({
                "name": "memory_add",
                "arguments": {
                    "session_id": "s1",
                    "messages": [{"role": "user", "content": "hello"}]
                }
            }),
        );
        let response = handle_line_with_backend(&backend, &config(), &line).unwrap();
        let result = &response["result"];
        assert_eq!(result["isError"], false);
        assert_eq!(result["structuredContent"]["captured"], false);
        assert!(result["structuredContent"].get("duplicate").is_none());
        assert!(result["structuredContent"].get("message").is_none());
        assert!(backend.calls()[0].starts_with("add:1:s1:mcp"));
    }

    #[test]
    fn tools_call_memory_flush_and_list_and_dream() {
        let backend = FakeBackend::default();
        let flush = request(
            15,
            "tools/call",
            json!({"name": "memory_flush", "arguments": {"batch_size": 5, "reason": "test"}}),
        );
        let response = handle_line_with_backend(&backend, &config(), &flush).unwrap();
        assert_eq!(
            response["result"]["structuredContent"]["processed_turns"],
            1
        );

        let list = request(
            16,
            "tools/call",
            json!({"name": "memory_list", "arguments": {"kinds": ["project"], "include_deprecated": true}}),
        );
        let response = handle_line_with_backend(&backend, &config(), &list).unwrap();
        assert_eq!(response["result"]["structuredContent"]["count"], 1);

        let dream = request(
            17,
            "tools/call",
            json!({"name": "memory_dream", "arguments": {"trigger": "scheduled"}}),
        );
        let response = handle_line_with_backend(&backend, &config(), &dream).unwrap();
        assert_eq!(response["result"]["isError"], false);
        let calls = backend.calls();
        assert!(calls.iter().any(|call| call == "flush:Some(5):test"));
        assert!(calls.iter().any(|call| call == "list:1"));
        assert!(calls.iter().any(|call| call == "dream:Scheduled"));
    }

    #[test]
    fn serve_loop_writes_only_json_rpc_lines() {
        let backend = FakeBackend::default();
        let input = format!(
            "{}\n{}\n\n{}\n",
            request(20, "initialize", json!({"protocolVersion": "2025-06-18"})),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(),
            request(21, "tools/list", json!({})),
        );
        let reader = Cursor::new(input.into_bytes());
        let mut output: Vec<u8> = Vec::new();
        serve_stdio_with_backend(&backend, &config(), reader, &mut output).expect("loop succeeds");
        let text = String::from_utf8(output).expect("utf8");
        let lines: Vec<&str> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        assert_eq!(
            lines.len(),
            2,
            "only responses for the two requests: {text}"
        );
        for line in lines {
            let value: Value = serde_json::from_str(line).expect("each stdout line is JSON");
            assert_eq!(value["jsonrpc"], "2.0");
        }
    }

    #[test]
    fn serve_loop_stops_at_eof_with_no_requests() {
        let backend = FakeBackend::default();
        let reader = Cursor::new(Vec::new());
        let mut output: Vec<u8> = Vec::new();
        serve_stdio_with_backend(&backend, &config(), reader, &mut output).expect("loop succeeds");
        assert!(output.is_empty());
    }
}
