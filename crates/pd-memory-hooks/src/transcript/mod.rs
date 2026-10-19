//! Tolerant JSONL transcript parsing.
//!
//! Hook payloads point at the host's transcript file (`transcript_path`). One
//! reader auto-detects every supported shape line by line, so a transcript
//! that mixes shapes (or a host that changes format) still parses:
//!
//! 1. **Claude Code style** (`claude.rs`):
//!    `{"type":"user","message":{"role":"user","content":[{"type":"text","text":"..."}]}}`
//!    and `{"type":"message","role":"user","text":"..."}`.
//! 2. **WorkBuddy / CodeBuddy** (`workbuddy.rs`, desktop + CLI): the flat
//!    Claude-style shape
//!    `{"type":"message","role":"user","content":[{"type":"input_text","text":"..."}]}`
//!    (`output_text` for assistant messages; `function_call`, `reasoning`,
//!    `session-meta` and similar records are ignored) plus the GUI's
//!    `<system-reminder>…<user_query>` envelope.
//! 3. **Codex rollout** (`codex.rs`, `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`):
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

pub(crate) mod claude;
pub(crate) mod codex;
pub(crate) mod workbuddy;

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
pub(crate) struct Parsed {
    pub(crate) message: MemoryMessage,
    pub(crate) commentary: bool,
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
    if let Some((record_type, payload)) = codex::codex_record(value) {
        if record_type != "response_item" {
            // session_meta, turn_context, event_msg (duplicates response
            // items), compacted, token counts...
            return None;
        }
        return codex::codex_message(payload);
    }
    claude::record_to_message(value).map(|message| Parsed {
        message,
        commentary: false,
    })
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::io::Write;

    use pd_memory_core::types::MemoryMessage;

    pub(crate) fn write_transcript(lines: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session.jsonl");
        let mut file = std::fs::File::create(&path).expect("create transcript");
        for line in lines {
            writeln!(file, "{line}").expect("write transcript line");
        }
        dir
    }

    /// Write `lines` to a temporary transcript and parse it.
    pub(crate) fn parse_lines(lines: &[&str], max_messages: usize) -> Vec<MemoryMessage> {
        let dir = write_transcript(lines);
        let path = dir.path().join("session.jsonl");
        super::parse_transcript(path.to_str().unwrap(), max_messages).expect("parses")
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::parse_lines;
    use super::*;

    #[test]
    fn malformed_lines_are_skipped() {
        let lines = [
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"first"}]}"#,
            "{ this is not json",
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"second"}]}"#,
        ];
        assert_eq!(parse_lines(&lines, 10).len(), 2);
    }

    #[test]
    fn keeps_last_max_messages() {
        let lines: Vec<String> = (0..5)
            .map(|index| format!(r#"{{"type":"message","role":"user","text":"m{index}"}}"#))
            .collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let messages = parse_lines(&refs, 2);
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
