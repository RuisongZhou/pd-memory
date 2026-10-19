//! Tolerant JSONL transcript parsing.
//!
//! Hook payloads point at the host's transcript file (`transcript_path`). Two
//! real-world shapes are supported:
//!
//! 1. **WorkBuddy / CodeBuddy** (desktop + CLI):
//!    `{"type":"message","role":"user","content":[{"type":"input_text","text":"..."}]}`
//!    (`output_text` for assistant messages; other record types such as
//!    `function_call`, `reasoning`, `session-meta` are ignored).
//! 2. **Claude Code style**:
//!    `{"type":"user","message":{"role":"user","content":[{"type":"text","text":"..."}]}}`
//!    and `{"type":"message","role":"user","text":"..."}`.
//! 3. **Codex rollout** (`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`):
//!    `{"timestamp":"...","type":"response_item","payload":{"type":"message",
//!    "role":"user","content":[{"type":"input_text","text":"..."}]}}`.
//!    Only `response_item` messages count; `developer` messages and host
//!    context injected as user messages (AGENTS.md, `<environment_context>`,
//!    ...) are dropped. Assistant `commentary` is kept for a turn only when it
//!    has no `final_answer`, and then only the last one.
//!
//! Parsing is best-effort by design: a hook must never fail because a
//! transcript line is malformed, truncated or from another agent version. Only
//! `user` / `assistant` text is returned; tool calls, tool results and
//! reasoning are dropped (they are noise for memory extraction).

use pd_memory_core::error::Result;
use pd_memory_core::types::MemoryMessage;
use serde_json::Value;

/// Default number of transcript messages considered by a hook.
pub const DEFAULT_TRANSCRIPT_MESSAGES: usize = 200;

/// Parse a transcript JSONL file into recent user/assistant messages.
///
/// - missing/unreadable files return an empty vector (logged at debug level);
/// - malformed lines are skipped;
/// - at most `max_messages` of the **most recent** messages are returned, in
///   chronological order.
pub fn parse_transcript(path: &str, max_messages: usize) -> Result<Vec<MemoryMessage>> {
    if path.trim().is_empty() {
        return Ok(Vec::new());
    }
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) => {
            log::debug!("transcript {path} is not readable ({error}); continuing without it");
            return Ok(Vec::new());
        }
    };
    let mut parsed: Vec<Parsed> = Vec::new();
    let mut skipped = 0usize;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if let Some(entry) = record_to_parsed(&value) {
            parsed.push(entry);
        }
    }
    if skipped > 0 {
        log::debug!("transcript {path}: skipped {skipped} malformed line(s)");
    }
    let mut messages = prefer_final_answers(parsed);
    truncate_to_recent(&mut messages, max_messages);
    Ok(messages)
}

/// One transcript message plus whether it is Codex interim `commentary`.
struct Parsed {
    message: MemoryMessage,
    commentary: bool,
}

/// Within each run of assistant messages, drop Codex `commentary` when the
/// run has a final answer; otherwise keep only the last commentary.
fn prefer_final_answers(parsed: Vec<Parsed>) -> Vec<MemoryMessage> {
    fn flush(run: &mut Vec<Parsed>, messages: &mut Vec<MemoryMessage>) {
        if run.iter().any(|entry| !entry.commentary) {
            messages.extend(
                run.drain(..)
                    .filter(|entry| !entry.commentary)
                    .map(|entry| entry.message),
            );
        } else if let Some(last) = run.pop() {
            messages.push(last.message);
        }
        run.clear();
    }
    let mut messages = Vec::with_capacity(parsed.len());
    let mut run: Vec<Parsed> = Vec::new();
    for entry in parsed {
        if entry.message.role == "assistant" {
            run.push(entry);
        } else {
            flush(&mut run, &mut messages);
            messages.push(entry.message);
        }
    }
    flush(&mut run, &mut messages);
    messages
}

/// Keep only the last `max_messages` entries (0 means no limit).
pub fn truncate_to_recent(messages: &mut Vec<MemoryMessage>, max_messages: usize) {
    if max_messages > 0 && messages.len() > max_messages {
        let drain = messages.len() - max_messages;
        messages.drain(0..drain);
    }
}

fn record_to_parsed(value: &Value) -> Option<Parsed> {
    if let Some((record_type, payload)) = codex_record(value) {
        if record_type != "response_item" {
            // session_meta, turn_context, event_msg (duplicates response
            // items), compacted, token counts...
            return None;
        }
        return codex_message(payload);
    }
    record_to_message(value).map(|message| Parsed {
        message,
        commentary: false,
    })
}

/// `(type, payload)` for a Codex rollout line: `{timestamp, type, payload}`.
fn codex_record(value: &Value) -> Option<(&str, &serde_json::Map<String, Value>)> {
    let object = value.as_object()?;
    if object.contains_key("role") || object.contains_key("message") {
        return None;
    }
    let payload = object.get("payload")?.as_object()?;
    let record_type = object.get("type")?.as_str()?;
    Some((record_type, payload))
}

fn codex_message(payload: &serde_json::Map<String, Value>) -> Option<Parsed> {
    if payload.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    // `developer` carries host instructions; it is never memory material.
    let role = match payload.get("role").and_then(Value::as_str)? {
        "user" => "user",
        "assistant" => "assistant",
        _ => return None,
    };
    let blocks = match payload.get("content")? {
        Value::Array(blocks) => blocks.iter().collect::<Vec<_>>(),
        other => vec![other],
    };
    let mut collected = String::new();
    for block in blocks {
        let text = match block {
            Value::String(text) => Some(text.as_str()),
            Value::Object(block) => {
                let block_type = block.get("type").and_then(Value::as_str).unwrap_or("text");
                if !matches!(block_type, "text" | "input_text" | "output_text") {
                    continue;
                }
                block.get("text").and_then(Value::as_str)
            }
            _ => None,
        };
        let text = match text {
            Some(text) if role == "user" => codex_user_text(text),
            Some(text) => Some(text.trim()).filter(|text| !text.is_empty()),
            None => None,
        };
        if let Some(text) = text {
            if !collected.is_empty() {
                collected.push('\n');
            }
            collected.push_str(text);
        }
    }
    if collected.is_empty() {
        return None;
    }
    let commentary =
        role == "assistant" && payload.get("phase").and_then(Value::as_str) == Some("commentary");
    Some(Parsed {
        message: MemoryMessage::new(role, collected),
        commentary,
    })
}

/// User-typed text of one Codex user content block, or `None` for host context
/// that Codex injects as a user message.
fn codex_user_text(text: &str) -> Option<&str> {
    let text = text.trim();
    if text.is_empty() || text.starts_with("# AGENTS.md instructions for ") {
        return None;
    }
    // Attachments/annotations preamble: "# Files mentioned by the user: ...
    // ## My request:\n<text>". Keep only the request.
    if text.starts_with("# ") {
        if let Some((_, request)) = text.split_once("\n## My request:") {
            let request = request.trim();
            return (!request.is_empty()).then_some(request);
        }
    }
    if is_codex_context_envelope(text) {
        return None;
    }
    Some(text)
}

/// `<environment_context>...</environment_context>`, `<skill>...</skill>`, a lone
/// `<image ...>` / `</image>` marker, and similar host-generated tag blocks.
fn is_codex_context_envelope(text: &str) -> bool {
    let Some(rest) = text.strip_prefix('<') else {
        return false;
    };
    let closing = rest.starts_with('/');
    let rest = rest.trim_start_matches('/');
    let name_len = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':')))
        .unwrap_or(rest.len());
    let name = &rest[..name_len];
    let host_tag = name.contains(['_', '-']) || matches!(name, "skill" | "image" | "INSTRUCTIONS");
    if name.is_empty() || !host_tag {
        return false;
    }
    let after = &rest[name_len..];
    if !(after.starts_with('>') || after.starts_with(char::is_whitespace)) {
        return false;
    }
    // A single tag on its own (attachment markers around input_image blocks).
    if text.find('>') == Some(text.len() - 1) {
        return true;
    }
    !closing && text.ends_with(&format!("</{name}>"))
}

fn record_to_message(value: &Value) -> Option<MemoryMessage> {
    let object = value.as_object()?;
    let record_type = object.get("type").and_then(Value::as_str).unwrap_or("");
    // WorkBuddy/CodeBuddy stream events and session metadata are not messages.
    if matches!(
        record_type,
        "function_call"
            | "function_call_result"
            | "reasoning"
            | "session-meta"
            | "ai-title"
            | "file-history-snapshot"
    ) {
        return None;
    }

    // Claude Code nests the message: {"type":"user","message":{"role","content"}}.
    let inner = object.get("message").and_then(Value::as_object);
    let role = inner
        .and_then(|inner| inner.get("role"))
        .or_else(|| object.get("role"))
        .and_then(Value::as_str)
        .map(normalize_role)
        .or_else(|| match record_type {
            "user" | "human" => Some("user"),
            "assistant" | "ai" => Some("assistant"),
            _ => None,
        })?;
    // Only conversation text is memory material; system/meta/tool roles are
    // dropped (normalize_role uses "unknown" as its sentinel).
    if !matches!(role, "user" | "assistant") {
        return None;
    }
    let content = inner
        .and_then(|inner| inner.get("content"))
        .or_else(|| object.get("content"))
        .or_else(|| object.get("text"))
        .and_then(content_to_text)?;
    let content = content.trim();
    if content.is_empty() {
        return None;
    }
    Some(MemoryMessage::new(role, content))
}

fn normalize_role(role: &str) -> &'static str {
    match role.to_ascii_lowercase().as_str() {
        "user" | "human" => "user",
        "assistant" | "ai" | "model" => "assistant",
        _ => "unknown",
    }
}

/// Convert a transcript `content` value into plain text.
fn content_to_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => conversation_text(text).map(str::to_string),
        Value::Array(blocks) => {
            let mut collected = String::new();
            for block in blocks {
                let text = match block {
                    Value::String(text) => Some(text.clone()),
                    Value::Object(block_object) => {
                        let block_type = block_object
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("text");
                        if !matches!(block_type, "text" | "input_text" | "output_text") {
                            continue;
                        }
                        block_object
                            .get("text")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    }
                    _ => None,
                };
                if let Some(text) = text.as_deref().and_then(conversation_text) {
                    if !collected.is_empty() {
                        collected.push('\n');
                    }
                    collected.push_str(text);
                }
            }
            if collected.is_empty() {
                None
            } else {
                Some(collected)
            }
        }
        Value::Object(object) => object
            .get("text")
            .and_then(Value::as_str)
            .and_then(conversation_text)
            .map(str::to_string),
        _ => None,
    }
}

fn conversation_text(text: &str) -> Option<&str> {
    let mut rest = text.trim_start();
    if !rest.starts_with("<system-reminder") {
        return Some(text);
    }
    // WorkBuddy GUI combines host reminders and <user_query> in one block.
    // Keep the user query, while excluding host instructions from memory.
    while rest.starts_with("<system-reminder") {
        rest = rest.split_once("</system-reminder>")?.1.trim_start();
    }
    rest.strip_prefix("<user_query>")?
        .trim_end()
        .strip_suffix("</user_query>")
        .map(str::trim)
        .filter(|query| !query.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_transcript(lines: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session.jsonl");
        let mut file = std::fs::File::create(&path).expect("create transcript");
        for line in lines {
            writeln!(file, "{line}").expect("write transcript line");
        }
        dir
    }

    #[test]
    fn parses_workbuddy_desktop_transcript() {
        let lines = [
            r#"{"id":"1","type":"session-meta","sessionId":"s"}"#,
            r#"{"id":"2","type":"message","role":"user","content":[{"type":"input_text","text":"<system-reminder data-role=\"user-context\">ctx</system-reminder>"}]}"#,
            r#"{"id":"3","type":"message","role":"user","content":[{"type":"input_text","text":"这个项目叫 Boreal"}]}"#,
            r#"{"id":"4","type":"function_call","name":"Bash","arguments":"{}"}"#,
            r#"{"id":"5","type":"function_call_result","name":"Bash","output":{"type":"text","text":"ok"}}"#,
            r#"{"id":"6","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"收到，记住了"}]}"#,
        ];
        let dir = write_transcript(&lines);
        let path = dir.path().join("session.jsonl");
        let messages = parse_transcript(path.to_str().unwrap(), 10).expect("parses");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "这个项目叫 Boreal");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].content, "收到，记住了");
    }

    #[test]
    fn extracts_workbuddy_gui_user_query_from_combined_context_block() {
        let query = "Azure Heron release code is Kestrel-9462; literal </user_query> stays.";
        let envelope = format!(
            "<system-reminder data-role=\"user-context\">host instructions</system-reminder>\n\
             <system-reminder data-role=\"additional-data\">host memory</system-reminder>\n\
             <user_query>{query}</user_query>"
        );
        for content in [
            serde_json::json!([{ "type": "input_text", "text": envelope }]),
            serde_json::json!(envelope),
            serde_json::json!({ "text": envelope }),
        ] {
            let record = serde_json::json!({
                "type": "message", "role": "user", "content": content
            });
            let message = record_to_message(&record).expect("GUI user query");
            assert_eq!(message.content, query);
        }
    }

    #[test]
    fn skips_workbuddy_context_without_a_complete_user_query() {
        for text in [
            "<system-reminder>host instructions</system-reminder>",
            "<system-reminder>host instructions</system-reminder><user_query>incomplete",
            "<system-reminder>incomplete<user_query>not user text</user_query>",
            "<system-reminder>host instructions</system-reminder><user_query> </user_query>",
        ] {
            let record = serde_json::json!({
                "type": "message", "role": "user",
                "content": [{ "type": "input_text", "text": text }]
            });
            assert!(record_to_message(&record).is_none(), "{text}");
        }
    }

    #[test]
    fn parses_claude_code_nested_transcript() {
        let lines = [
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hello memory"}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"hi there"}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":"plain string content"}}"#,
        ];
        let dir = write_transcript(&lines);
        let path = dir.path().join("session.jsonl");
        let messages = parse_transcript(path.to_str().unwrap(), 10).expect("parses");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].content, "hello memory");
        assert_eq!(messages[1].content, "hi there");
        assert_eq!(messages[2].content, "plain string content");
    }

    #[test]
    fn parses_flat_workbuddy_cli_message_shape() {
        let lines = [
            r#"{"type":"message","role":"user","text":"flat text"}"#,
            r#"{"role":"assistant","content":"bare role content"}"#,
        ];
        let dir = write_transcript(&lines);
        let path = dir.path().join("session.jsonl");
        let messages = parse_transcript(path.to_str().unwrap(), 10).expect("parses");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "flat text");
        assert_eq!(messages[1].content, "bare role content");
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let lines = [
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"first"}]}"#,
            "{ this is not json",
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"second"}]}"#,
        ];
        let dir = write_transcript(&lines);
        let path = dir.path().join("session.jsonl");
        let messages = parse_transcript(path.to_str().unwrap(), 10).expect("parses");
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn keeps_last_max_messages() {
        let lines: Vec<String> = (0..5)
            .map(|index| format!(r#"{{"type":"message","role":"user","text":"m{index}"}}"#))
            .collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let dir = write_transcript(&refs);
        let path = dir.path().join("session.jsonl");
        let messages = parse_transcript(path.to_str().unwrap(), 2).expect("parses");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "m3");
        assert_eq!(messages[1].content, "m4");
    }

    #[test]
    fn missing_or_empty_transcript_is_not_an_error() {
        assert!(parse_transcript("", 10).unwrap().is_empty());
        assert!(parse_transcript("/definitely/not/here.jsonl", 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn unknown_roles_are_skipped() {
        let lines = [
            r#"{"type":"message","role":"system","text":"system prompt"}"#,
            r#"{"type":"message","role":"user","text":"real user"}"#,
        ];
        let dir = write_transcript(&lines);
        let path = dir.path().join("session.jsonl");
        let messages = parse_transcript(path.to_str().unwrap(), 10).expect("parses");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "real user");
    }

    fn codex_line(record_type: &str, payload: serde_json::Value) -> String {
        serde_json::json!({
            "timestamp": "2026-10-19T09:15:23.000Z",
            "type": record_type,
            "payload": payload,
        })
        .to_string()
    }

    fn codex_message(role: &str, phase: Option<&str>, blocks: &[(&str, &str)]) -> String {
        let content: Vec<_> = blocks
            .iter()
            .map(|(kind, text)| serde_json::json!({"type": kind, "text": text}))
            .collect();
        let mut payload = serde_json::json!({
            "type": "message", "role": role, "content": content, "id": "msg_1"
        });
        if let Some(phase) = phase {
            payload["phase"] = serde_json::json!(phase);
        }
        codex_line("response_item", payload)
    }

    fn parse_lines(lines: &[String]) -> Vec<MemoryMessage> {
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let dir = write_transcript(&refs);
        let path = dir.path().join("session.jsonl");
        parse_transcript(path.to_str().unwrap(), 50).expect("parses")
    }

    #[test]
    fn parses_codex_rollout_and_drops_host_context() {
        let lines = vec![
            codex_line(
                "session_meta",
                serde_json::json!({"id": "t1", "cwd": "/repo", "originator": "codex_cli_rs"}),
            ),
            codex_message("developer", None, &[("input_text", "<permissions instructions>")]),
            codex_message(
                "user",
                None,
                &[(
                    "input_text",
                    "# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>\nbe terse\n</INSTRUCTIONS>",
                )],
            ),
            codex_message(
                "user",
                None,
                &[
                    ("input_text", "<recommended_plugins>\n- A\n</recommended_plugins>"),
                    (
                        "input_text",
                        "<environment_context>\n  <cwd>/repo</cwd>\n</environment_context>",
                    ),
                ],
            ),
            codex_line(
                "turn_context",
                serde_json::json!({"cwd": "/repo", "model": "gpt-x"}),
            ),
            codex_message(
                "user",
                None,
                &[("input_text", "This project is called Boreal.\n")],
            ),
            codex_line(
                "event_msg",
                serde_json::json!({"type": "user_message", "message": "This project is called Boreal."}),
            ),
            codex_line(
                "response_item",
                serde_json::json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": "thinking"}]}),
            ),
            codex_message(
                "assistant",
                Some("commentary"),
                &[("output_text", "Let me look around.")],
            ),
            codex_line(
                "response_item",
                serde_json::json!({"type": "function_call", "name": "shell", "arguments": "{}", "call_id": "c1"}),
            ),
            codex_line(
                "response_item",
                serde_json::json!({"type": "function_call_output", "call_id": "c1", "output": "ok"}),
            ),
            codex_message(
                "assistant",
                Some("final_answer"),
                &[("output_text", "Noted: the project is Boreal.")],
            ),
            codex_line(
                "event_msg",
                serde_json::json!({"type": "agent_message", "message": "Noted: the project is Boreal."}),
            ),
        ];
        let messages = parse_lines(&lines);
        let pairs: Vec<(&str, &str)> = messages
            .iter()
            .map(|m| (m.role.as_str(), m.content.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("user", "This project is called Boreal."),
                ("assistant", "Noted: the project is Boreal."),
            ]
        );
    }

    #[test]
    fn codex_commentary_is_a_fallback_only_without_final_answer() {
        let lines = vec![
            codex_message("user", None, &[("input_text", "first question")]),
            codex_message(
                "assistant",
                Some("commentary"),
                &[("output_text", "step 1")],
            ),
            codex_message(
                "assistant",
                Some("commentary"),
                &[("output_text", "step 2")],
            ),
            // Interrupted turn: no final answer, keep only the last commentary.
            codex_message("user", None, &[("input_text", "second question")]),
            codex_message(
                "assistant",
                Some("commentary"),
                &[("output_text", "checking")],
            ),
            codex_message(
                "assistant",
                Some("final_answer"),
                &[("output_text", "answer two")],
            ),
            // Older rollouts have no phase at all.
            codex_message("user", None, &[("input_text", "third question")]),
            codex_message("assistant", None, &[("output_text", "answer three")]),
        ];
        let contents: Vec<String> = parse_lines(&lines)
            .into_iter()
            .map(|m| format!("{}:{}", m.role, m.content))
            .collect();
        assert_eq!(
            contents,
            vec![
                "user:first question",
                "assistant:step 2",
                "user:second question",
                "assistant:answer two",
                "user:third question",
                "assistant:answer three",
            ]
        );
    }

    #[test]
    fn codex_attachment_preambles_keep_only_the_request() {
        let lines = vec![codex_message(
            "user",
            None,
            &[
                (
                    "input_text",
                    "\n# Files mentioned by the user:\n\n## shot.png: /tmp/shot.png\n\n## My request:\nRename the Boreal module\n",
                ),
                ("input_text", "<image name=[Image #1] path=\"/tmp/shot.png\">"),
                ("input_image", ""),
                ("input_text", "</image>"),
            ],
        )];
        let messages = parse_lines(&lines);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "Rename the Boreal module");
    }

    #[test]
    fn codex_user_text_keeps_ordinary_markup() {
        for text in [
            "# Plan\nship the Boreal release",
            "<b>bold</b> is fine",
            "<div>user html</div>",
            "<not closed",
        ] {
            assert_eq!(codex_user_text(text), Some(text), "{text}");
        }
        for text in [
            "<environment_context>\n</environment_context>",
            "<skill>\n<name>x</name>\n</skill>",
            "<turn_aborted>\nThe user interrupted.\n</turn_aborted>",
            "</image>",
            "# AGENTS.md instructions for /repo\n\nbody",
            "# Files mentioned by the user:\n\n## My request:\n  ",
        ] {
            assert_eq!(codex_user_text(text), None, "{text}");
        }
    }

    #[test]
    fn codex_shaped_lines_never_fall_through_to_generic_parsing() {
        // A payload with a role-like shape outside response_item is not a message.
        let lines = vec![
            codex_line(
                "compacted",
                serde_json::json!({"message": "summary", "role": "user", "content": "x"}),
            ),
            codex_line(
                "event_msg",
                serde_json::json!({"type": "message", "role": "user", "content": "dup"}),
            ),
            codex_line("response_item", serde_json::json!("not an object")),
        ];
        assert!(parse_lines(&lines).is_empty());
    }

    #[test]
    fn truncate_to_recent_is_bounded() {
        let mut messages = vec![
            MemoryMessage::user("a"),
            MemoryMessage::user("b"),
            MemoryMessage::user("c"),
        ];
        truncate_to_recent(&mut messages, 2);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "b");
        truncate_to_recent(&mut messages, 0);
        assert_eq!(messages.len(), 2);
    }
}
