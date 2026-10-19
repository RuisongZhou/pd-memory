//! Codex rollout records: `{"timestamp", "type", "payload"}` per line.

use pd_memory_core::types::MemoryMessage;
use serde_json::Value;

use super::Parsed;

/// `(type, payload)` for a Codex rollout line: `{timestamp, type, payload}`.
pub(crate) fn codex_record(value: &Value) -> Option<(&str, &serde_json::Map<String, Value>)> {
    let object = value.as_object()?;
    if object.contains_key("role") || object.contains_key("message") {
        return None;
    }
    let payload = object.get("payload")?.as_object()?;
    let record_type = object.get("type")?.as_str()?;
    Some((record_type, payload))
}

pub(crate) fn codex_message(payload: &serde_json::Map<String, Value>) -> Option<Parsed> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::test_support;

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
        test_support::parse_lines(&refs, 50)
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
}
