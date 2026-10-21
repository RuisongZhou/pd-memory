//! Claude-Code-style records, the base shape WorkBuddy/CodeBuddy inherit.
//!
//! Nested `{"type":"user","message":{"role","content"}}`, flat
//! `{"type":"message","role","content"|"text"}` and bare `{"role","content"}`.

use pd_memory_core::types::MemoryMessage;
use serde_json::Value;

use super::workbuddy::conversation_text;

pub(crate) fn record_to_message(value: &Value) -> Option<MemoryMessage> {
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
        .or(match record_type {
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

#[cfg(test)]
mod tests {
    use crate::transcript::test_support::parse_lines;

    #[test]
    fn parses_claude_code_nested_transcript() {
        let lines = [
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hello memory"}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"hi there"}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":"plain string content"}}"#,
        ];
        let messages = parse_lines(&lines, 10);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].content, "hello memory");
        assert_eq!(messages[1].content, "hi there");
        assert_eq!(messages[2].content, "plain string content");
    }

    #[test]
    fn unknown_roles_are_skipped() {
        let lines = [
            r#"{"type":"message","role":"system","text":"system prompt"}"#,
            r#"{"type":"message","role":"user","text":"real user"}"#,
        ];
        let messages = parse_lines(&lines, 10);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "real user");
    }
}
