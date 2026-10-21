//! Agent-platform lifecycle hook adapter for `pd-memory`.
//!
//! Claude-Code-compatible hook contract (also used by PilotDeck,
//! WorkBuddy/CodeBuddy and Codex, all of which inherit it):
//!
//! Input (stdin JSON, snake_case; unknown fields are preserved):
//! `{ "hook_event_name": "UserPromptSubmit", "session_id": "...",
//!    "transcript_path": "...", "cwd": "...", "prompt": "...", ... }`
//!
//! Output (stdout JSON, one line):
//! `{ "continue": true, "systemMessage": "...",
//!    "hookSpecificOutput": { "hookEventName": "...", "additionalContext": "..." } }`
//!
//! Contract guarantees:
//! - a hook **always** prints a valid JSON object with `"continue": true`, even
//!   when recall/capture/flush fail (errors go to stderr via `log`);
//! - unknown events and unknown input fields are tolerated;
//! - no output is written when the payload is empty? Empty stdin still yields
//!   `{"continue":true}` so hosts never see a silent failure.
//!
//! Event behavior lives in [`events`]; transcript parsing in [`transcript`].
//! Per-host wire formats live in [`hosts`] (one adapter per platform), and the
//! host-agnostic lock / first-recall / session-end policy in [`policy`].

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use pd_memory_core::engine::MemoryEngine;
use pd_memory_core::error::{MemoryError, Result};
use serde::{Deserialize, Serialize};

pub mod events;
pub mod hosts;
pub mod policy;
pub mod transcript;

pub use events::{
    capture_only_reading, capture_only_with, derive_session_key, derive_workspace_id,
    dispatch_with, handle_with, render_overview_context, HookEngine, TranscriptReader,
};
pub use hosts::{CanonicalEvent, EventKind, HostAdapter, HostCaps};
pub use transcript::{parse_transcript, DEFAULT_TRANSCRIPT_MESSAGES};

/// Hook input payload. Extra host fields are preserved for fallback capture.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookInput {
    /// Event name, e.g. `SessionStart`, `UserPromptSubmit`, `Stop`.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub hook_event_name: String,
    /// Stable session identifier (host UUID).
    #[serde(default, deserialize_with = "null_as_empty")]
    pub session_id: String,
    /// Absolute path to the host's JSONL transcript for this session.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub transcript_path: String,
    /// Working directory (absent on `SessionStart` in WorkBuddy).
    #[serde(default, deserialize_with = "null_as_empty")]
    pub cwd: String,
    /// User prompt, present on `UserPromptSubmit`.
    #[serde(default)]
    pub prompt: Option<String>,
    /// All other host fields (`last_assistant_message`, `tool_name`, ...).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Codex sends `null` for an absent `transcript_path`; treat `null` as "".
fn null_as_empty<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

impl HookInput {
    /// Event name with surrounding whitespace removed.
    pub fn event(&self) -> &str {
        self.hook_event_name.trim()
    }

    /// Final assistant reply when the host inlines it (`Stop` in WorkBuddy).
    pub fn last_assistant_message(&self) -> Option<&str> {
        self.extra
            .get("last_assistant_message")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }
}

fn default_continue() -> bool {
    true
}

/// Hook output payload.
///
/// Serialized with the exact camelCase keys hosts expect: `continue`,
/// `systemMessage`, `hookSpecificOutput.{hookEventName, additionalContext}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookOutput {
    /// Must stay `true` for a non-blocking hook.
    #[serde(default = "default_continue")]
    pub r#continue: bool,
    /// Optional message surfaced by the host UI (not prompt context).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_message: Option<String>,
    /// Event-specific output, including injected prompt context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_specific_output: Option<HookSpecificOutput>,
}

impl Default for HookOutput {
    fn default() -> Self {
        Self {
            r#continue: true,
            system_message: None,
            hook_specific_output: None,
        }
    }
}

impl HookOutput {
    /// The minimal accepted output: `{"continue":true}`.
    pub fn continue_only() -> Self {
        Self::default()
    }

    /// Output that injects `additional_context` into the host prompt.
    pub fn with_context(event: impl Into<String>, context: impl Into<String>) -> Self {
        Self {
            r#continue: true,
            system_message: None,
            hook_specific_output: Some(HookSpecificOutput {
                hook_event_name: event.into(),
                additional_context: Some(context.into()),
            }),
        }
    }

    /// Context selected for injection, if any.
    pub fn additional_context(&self) -> Option<&str> {
        self.hook_specific_output
            .as_ref()
            .and_then(|specific| specific.additional_context.as_deref())
    }
}

/// Hook-specific output matching the Claude-Code/PilotDeck contract.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookSpecificOutput {
    /// Event this output belongs to (`hookEventName`).
    #[serde(default)]
    pub hook_event_name: String,
    /// Prompt context to inject (`additionalContext`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

/// Parse hook stdin tolerantly. Invalid JSON never fails the hook: a warning is
/// logged and a default (no-op) input is returned.
pub fn parse_input(raw: &str) -> HookInput {
    match serde_json::from_str(raw) {
        Ok(input) => input,
        Err(error) => {
            log::warn!("hook stdin is not valid JSON ({error}); treating as no-op");
            HookInput::default()
        }
    }
}

/// Serialize one hook output as a single JSON object (no newline).
pub fn render_json(output: &HookOutput) -> String {
    serde_json::to_string(output).unwrap_or_else(|_| "{\"continue\":true}".to_string())
}

/// Serialize one hook output as a single stdout line (with newline).
pub fn render_output(output: &HookOutput) -> String {
    let mut line = render_json(output);
    line.push('\n');
    line
}

/// Run one hook invocation: read stdin JSON, dispatch by event, write stdout.
///
/// Reading/parsing failures are logged and downgraded to `{"continue":true}` so
/// the host turn is never blocked.
pub fn run_hook_stdio(
    engine: Arc<MemoryEngine>,
    reader: &mut dyn Read,
    writer: &mut dyn Write,
) -> Result<()> {
    let mut raw = String::new();
    let output = match reader.read_to_string(&mut raw) {
        Ok(_) => hook_output_for_raw(&engine, &raw),
        Err(error) => {
            log::warn!("could not read hook stdin ({error}); printing continue=true");
            HookOutput::continue_only()
        }
    };
    write_output(writer, &output)
}

/// [`run_hook_stdio`] against an injectable [`HookEngine`] (tests, embedding).
pub fn run_hook_stdio_with_backend(
    backend: &dyn HookEngine,
    max_context_chars: usize,
    reader: &mut dyn Read,
    writer: &mut dyn Write,
) -> Result<()> {
    let mut raw = String::new();
    let output = match reader.read_to_string(&mut raw) {
        Ok(_) => {
            let input = parse_input(&raw);
            match dispatch_with(backend, max_context_chars, &input) {
                Ok(output) => output,
                Err(error) => {
                    log::warn!("hook dispatch failed ({error}); printing continue=true");
                    HookOutput::continue_only()
                }
            }
        }
        Err(error) => {
            log::warn!("could not read hook stdin ({error}); printing continue=true");
            HookOutput::continue_only()
        }
    };
    write_output(writer, &output)
}

/// Dispatch a raw hook payload against the engine, downgrading all errors.
pub fn hook_output_for_raw(engine: &MemoryEngine, raw: &str) -> HookOutput {
    let input = parse_input(raw);
    match dispatch(engine, &input) {
        Ok(output) => output,
        Err(error) => {
            log::warn!("hook dispatch failed ({error}); printing continue=true");
            HookOutput::continue_only()
        }
    }
}

/// Dispatch one already-parsed hook input against the engine.
pub fn dispatch(engine: &MemoryEngine, input: &HookInput) -> Result<HookOutput> {
    let backend = events::EngineHooks::new(engine);
    dispatch_with(&backend, engine.config().hooks.max_context_chars, input)
}

fn write_output(writer: &mut dyn Write, output: &HookOutput) -> Result<()> {
    writer
        .write_all(render_output(output).as_bytes())
        .and_then(|_| writer.flush())
        .map_err(|error| MemoryError::io(PathBuf::from("<stdout>"), error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::Mutex;

    use pd_memory_core::error::Result;
    use pd_memory_core::types::{
        CaptureTurnResult, HeartbeatStats, MaintenanceResult, MemoryMessage, MemoryOverview,
    };

    #[derive(Default)]
    struct FakeHooks {
        context: Mutex<Option<String>>,
    }

    impl HookEngine for FakeHooks {
        fn overview(&self) -> Result<MemoryOverview> {
            Ok(MemoryOverview::default())
        }

        fn retrieve_context(
            &self,
            _query: &str,
            _recent: Vec<MemoryMessage>,
        ) -> Result<Option<String>> {
            Ok(self.context.lock().unwrap().clone())
        }

        fn capture_turn(
            &self,
            messages: &[MemoryMessage],
            session_key: &str,
            _source: &str,
        ) -> Result<CaptureTurnResult> {
            Ok(CaptureTurnResult {
                captured: true,
                normalized_messages: messages.to_vec(),
                session_key: session_key.to_string(),
            })
        }

        fn flush(&self, _reason: &str) -> Result<HeartbeatStats> {
            Ok(HeartbeatStats::default())
        }

        fn run_due_scheduled_maintenance(&self, _reason: &str) -> Result<MaintenanceResult> {
            Ok(MaintenanceResult::default())
        }
    }

    #[test]
    fn input_deserializes_snake_case_and_preserves_extras() {
        let raw = r#"{
            "hook_event_name": "Stop",
            "session_id": "s1",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/tmp/project",
            "last_assistant_message": "final reply",
            "unknown_future_field": {"nested": true}
        }"#;
        let input = parse_input(raw);
        assert_eq!(input.event(), "Stop");
        assert_eq!(input.session_id, "s1");
        assert_eq!(input.last_assistant_message(), Some("final reply"));
        assert!(input.extra.contains_key("unknown_future_field"));
    }

    #[test]
    fn null_string_fields_are_empty() {
        let input = parse_input(
            r#"{"hook_event_name":"SessionEnd","session_id":"s1","transcript_path":null,"cwd":"/p","reason":"other"}"#,
        );
        assert_eq!(input.event(), "SessionEnd");
        assert_eq!(input.session_id, "s1");
        assert!(input.transcript_path.is_empty());
        assert_eq!(input.cwd, "/p");
    }

    #[test]
    fn invalid_json_is_tolerated() {
        let input = parse_input("{not json");
        assert_eq!(input.event(), "");
        assert!(input.session_id.is_empty());
    }

    #[test]
    fn output_uses_exact_camel_case_contract() {
        let output = HookOutput::with_context("UserPromptSubmit", "remembered facts");
        let json: serde_json::Value =
            serde_json::from_str(&render_output(&output)).expect("output is JSON");
        assert_eq!(json["continue"], true);
        assert_eq!(
            json["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
        assert_eq!(
            json["hookSpecificOutput"]["additionalContext"],
            "remembered facts"
        );
        assert!(json.get("systemMessage").is_none());
        assert!(json.get("system_message").is_none());
    }

    #[test]
    fn continue_only_output_is_minimal() {
        let json: serde_json::Value =
            serde_json::from_str(&render_output(&HookOutput::continue_only())).unwrap();
        assert_eq!(json, serde_json::json!({"continue": true}));
    }

    #[test]
    fn output_roundtrips_through_serde() {
        let output = HookOutput {
            r#continue: true,
            system_message: Some("note".into()),
            hook_specific_output: Some(HookSpecificOutput {
                hook_event_name: "SessionStart".into(),
                additional_context: Some("<pd-memory>context</pd-memory>".into()),
            }),
        };
        let raw = render_output(&output);
        let parsed: HookOutput = serde_json::from_str(raw.trim()).expect("roundtrip");
        assert!(parsed.r#continue);
        assert_eq!(parsed.system_message.as_deref(), Some("note"));
        assert_eq!(
            parsed.additional_context(),
            Some("<pd-memory>context</pd-memory>")
        );
    }

    #[test]
    fn stdio_hook_emits_one_json_line() {
        let backend = FakeHooks {
            context: Mutex::new(Some("memory context".into())),
        };
        let mut reader = Cursor::new(
            br#"{"hook_event_name":"UserPromptSubmit","session_id":"s1","prompt":"hello"}"#
                .to_vec(),
        );
        let mut writer: Vec<u8> = Vec::new();
        run_hook_stdio_with_backend(&backend, 8_000, &mut reader, &mut writer).expect("hook runs");
        let text = String::from_utf8(writer).expect("utf8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1);
        let json: serde_json::Value = serde_json::from_str(lines[0]).expect("valid JSON");
        assert_eq!(json["continue"], true);
        assert_eq!(
            json["hookSpecificOutput"]["additionalContext"],
            "memory context"
        );
    }

    #[test]
    fn stdio_hook_survives_garbage_stdin() {
        let backend = FakeHooks::default();
        let mut reader = Cursor::new(b"garbage".to_vec());
        let mut writer: Vec<u8> = Vec::new();
        run_hook_stdio_with_backend(&backend, 8_000, &mut reader, &mut writer).expect("hook runs");
        let json: serde_json::Value =
            serde_json::from_str(String::from_utf8(writer).unwrap().trim()).unwrap();
        assert_eq!(json, serde_json::json!({"continue": true}));
    }

    #[test]
    fn stdio_hook_survives_empty_stdin() {
        let backend = FakeHooks::default();
        let mut reader = Cursor::new(Vec::new());
        let mut writer: Vec<u8> = Vec::new();
        run_hook_stdio_with_backend(&backend, 8_000, &mut reader, &mut writer).expect("hook runs");
        let json: serde_json::Value =
            serde_json::from_str(String::from_utf8(writer).unwrap().trim()).unwrap();
        assert_eq!(json, serde_json::json!({"continue": true}));
    }
}
