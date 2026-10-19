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
    let mut messages: Vec<MemoryMessage> = Vec::new();
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
        if let Some(message) = record_to_message(&value) {
            messages.push(message);
        }
    }
    if skipped > 0 {
        log::debug!("transcript {path}: skipped {skipped} malformed line(s)");
    }
    truncate_to_recent(&mut messages, max_messages);
    Ok(messages)
}

/// Keep only the last `max_messages` entries (0 means no limit).
pub fn truncate_to_recent(messages: &mut Vec<MemoryMessage>, max_messages: usize) {
    if max_messages > 0 && messages.len() > max_messages {
        let drain = messages.len() - max_messages;
        messages.drain(0..drain);
    }
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
