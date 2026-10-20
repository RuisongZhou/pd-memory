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

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use pd_memory_core::error::Result;
use pd_memory_core::types::MemoryMessage;
use serde_json::Value;

pub(crate) mod claude;
pub(crate) mod codex;
pub(crate) mod workbuddy;

/// Default number of transcript messages considered by a hook.
pub const DEFAULT_TRANSCRIPT_MESSAGES: usize = 200;

/// Transcripts up to this size are read whole.
const FULL_READ_MAX_BYTES: u64 = 1 << 20;
/// Bytes read per step when scanning a larger transcript from its end.
const TAIL_CHUNK_BYTES: usize = 256 << 10;

/// Parse a transcript JSONL file into recent user/assistant messages.
///
/// - missing/unreadable files return an empty vector (logged at debug level);
/// - malformed lines are skipped;
/// - at most `max_messages` of the **most recent** messages are returned, in
///   chronological order.
///
/// Files over 1 MiB (Codex rollouts reach tens of MB) are read backwards in
/// chunks, stopping once the newest `max_messages` messages are known; the
/// result equals a full read. Lines that are not valid UTF-8 are skipped
/// there, while a full read of a small file treats them as unreadable.
pub fn parse_transcript(path: &str, max_messages: usize) -> Result<Vec<MemoryMessage>> {
    parse_transcript_with(path, max_messages, FULL_READ_MAX_BYTES, TAIL_CHUNK_BYTES)
}

fn parse_transcript_with(
    path: &str,
    max_messages: usize,
    full_read_max_bytes: u64,
    chunk_bytes: usize,
) -> Result<Vec<MemoryMessage>> {
    if path.trim().is_empty() {
        return Ok(Vec::new());
    }
    let read = match std::fs::metadata(path) {
        Ok(meta) if max_messages > 0 && meta.len() > full_read_max_bytes => {
            read_tail(path, meta.len(), max_messages, chunk_bytes.max(1))
        }
        _ => read_full(path, max_messages),
    };
    match read {
        Ok((messages, skipped)) => {
            if skipped > 0 {
                log::debug!("transcript {path}: skipped {skipped} malformed line(s)");
            }
            Ok(messages)
        }
        Err(error) => {
            log::debug!("transcript {path} is not readable ({error}); continuing without it");
            Ok(Vec::new())
        }
    }
}

/// Parse the whole file. Returns messages and the malformed line count.
fn read_full(path: &str, max_messages: usize) -> std::io::Result<(Vec<MemoryMessage>, usize)> {
    let raw = std::fs::read_to_string(path)?;
    let mut parsed: Vec<Parsed> = Vec::new();
    let mut skipped = 0usize;
    for line in raw.lines() {
        parse_line(line, &mut parsed, &mut skipped);
    }
    let mut messages = prefer_final_answers(parsed);
    truncate_to_recent(&mut messages, max_messages);
    Ok((messages, skipped))
}

/// Parse backwards from the end in `chunk_bytes` steps until the newest
/// `max_messages` messages are settled.
///
/// [`prefer_final_answers`] looks at whole runs of assistant messages, so the
/// scan may only stop at a user message: everything from there on processes
/// exactly as in a full read.
fn read_tail(
    path: &str,
    len: u64,
    max_messages: usize,
    chunk_bytes: usize,
) -> std::io::Result<(Vec<MemoryMessage>, usize)> {
    let mut file = File::open(path)?;
    let mut pos = len;
    // Start of a line that begins before `pos`, up to and including its '\n'.
    let mut carry: Vec<u8> = Vec::new();
    let mut newest_first: Vec<Parsed> = Vec::new();
    let mut skipped = 0usize;
    while pos > 0 {
        let start = pos.saturating_sub(chunk_bytes as u64);
        let mut data = vec![0u8; (pos - start) as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut data)?;
        data.extend_from_slice(&carry);
        pos = start;
        let complete = if pos == 0 {
            0
        } else {
            match data.iter().position(|&byte| byte == b'\n') {
                Some(newline) => newline + 1,
                None => {
                    carry = data;
                    continue;
                }
            }
        };
        let mut chunk: Vec<Parsed> = Vec::new();
        for line in data[complete..].split(|&byte| byte == b'\n') {
            match std::str::from_utf8(line) {
                Ok(line) => parse_line(line, &mut chunk, &mut skipped),
                Err(_) => skipped += 1,
            }
        }
        newest_first.extend(chunk.into_iter().rev());
        data.truncate(complete);
        carry = data;
        if let Some(messages) = settled_tail(&newest_first, max_messages) {
            return Ok((messages, skipped));
        }
    }
    let mut messages = prefer_final_answers(newest_first.into_iter().rev().collect());
    truncate_to_recent(&mut messages, max_messages);
    Ok((messages, skipped))
}

/// The newest `max_messages` messages if the parsed suffix already determines
/// them: it contains a user message and, from the oldest one on, yields
/// enough messages.
fn settled_tail(newest_first: &[Parsed], max_messages: usize) -> Option<Vec<MemoryMessage>> {
    let oldest_user = newest_first
        .iter()
        .rposition(|entry| entry.message.role != "assistant")?;
    let suffix: Vec<Parsed> = newest_first[..=oldest_user].iter().rev().cloned().collect();
    let mut messages = prefer_final_answers(suffix);
    if messages.len() < max_messages {
        return None;
    }
    truncate_to_recent(&mut messages, max_messages);
    Some(messages)
}

fn parse_line(line: &str, parsed: &mut Vec<Parsed>, skipped: &mut usize) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    let value: Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(_) => {
            *skipped += 1;
            return;
        }
    };
    if let Some(entry) = record_to_parsed(&value) {
        parsed.push(entry);
    }
}

/// One transcript message plus whether it is Codex interim `commentary`.
#[derive(Clone)]
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

    /// Deterministic mix of every record shape, with commentary runs, noise,
    /// CRLF, multibyte text and no trailing newline.
    fn mixed_transcript(lines: usize, seed: u64) -> String {
        let mut state = seed;
        let mut next = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as usize
        };
        let codex = |role: &str, phase: Option<&str>, text: &str| {
            let block = if role == "user" {
                "input_text"
            } else {
                "output_text"
            };
            let mut payload = serde_json::json!({
                "type": "message", "role": role, "content": [{"type": block, "text": text}]
            });
            if let Some(phase) = phase {
                payload["phase"] = serde_json::json!(phase);
            }
            serde_json::json!({"timestamp": "t", "type": "response_item", "payload": payload})
                .to_string()
        };
        let mut out = String::new();
        for index in 0..lines {
            let text = format!("m{index} Boreal 记忆 ✓ {}", "x".repeat(next() % 40));
            let line = match next() % 12 {
                0 | 1 => codex("user", None, &text),
                2 | 3 => codex("assistant", Some("commentary"), &text),
                4 => codex("assistant", Some("final_answer"), &text),
                5 => codex("assistant", None, &text),
                6 => codex("user", None, "<environment_context>\n</environment_context>"),
                7 => serde_json::json!({"type": "message", "role": "user", "text": text}).to_string(),
                8 => serde_json::json!({"type": "assistant", "message": {"role": "assistant", "content": text}})
                    .to_string(),
                9 => "{ not json".to_string(),
                10 => String::new(),
                _ => serde_json::json!({"timestamp": "t", "type": "event_msg", "payload": {"type": "x"}})
                    .to_string(),
            };
            out.push_str(&line);
            out.push_str(if next() % 5 == 0 { "\r\n" } else { "\n" });
        }
        out.pop();
        out
    }

    fn write(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> String {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        path.to_str().unwrap().to_string()
    }

    #[test]
    fn tail_read_matches_full_read() {
        let dir = tempfile::tempdir().unwrap();
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let mut paths = vec![
            fixtures
                .join("codex/transcript.jsonl")
                .to_str()
                .unwrap()
                .to_string(),
            fixtures
                .join("workbuddy/transcript.jsonl")
                .to_str()
                .unwrap()
                .to_string(),
        ];
        for seed in 0..6 {
            let text = mixed_transcript(300, seed);
            paths.push(write(&dir, &format!("mixed-{seed}.jsonl"), text.as_bytes()));
        }
        // A file that is one long assistant run (no user boundary at all).
        let run: String = (0..50)
            .map(|i| {
                let phase = if i == 7 { "final_answer" } else { "commentary" };
                format!(
                    "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"{phase}\",\"content\":\"a{i}\"}}}}\n"
                )
            })
            .collect();
        paths.push(write(&dir, "run.jsonl", run.as_bytes()));
        let mut compared = 0;
        for path in &paths {
            for max in [1, 2, 3, 4, 7, 8, 20, 200] {
                let full = parse_transcript_with(path, max, u64::MAX, 1).unwrap();
                // 3-byte chunks split multibyte characters and CRLF pairs.
                for chunk in [3, 17, 64, 1_000, 1 << 20] {
                    let tail = parse_transcript_with(path, max, 0, chunk).unwrap();
                    assert_eq!(
                        tail.iter()
                            .map(|m| (&m.role, &m.content))
                            .collect::<Vec<_>>(),
                        full.iter()
                            .map(|m| (&m.role, &m.content))
                            .collect::<Vec<_>>(),
                        "{path} max={max} chunk={chunk}"
                    );
                    compared += 1;
                }
            }
        }
        assert_eq!(compared, paths.len() * 8 * 5);
    }

    #[test]
    fn tail_read_stops_before_the_start_of_a_large_file() {
        // Invalid UTF-8 at the start makes a whole-file read fail; the tail
        // read never reaches it once the newest messages are settled.
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = b"\xff\xfe not utf-8\n".to_vec();
        bytes.extend_from_slice(mixed_transcript(300, 9).as_bytes());
        let path = write(&dir, "large.jsonl", &bytes);
        assert!(parse_transcript_with(&path, 8, u64::MAX, 64)
            .unwrap()
            .is_empty());
        let tail = parse_transcript_with(&path, 8, 0, 64).unwrap();
        assert_eq!(tail.len(), 8);
        // max_messages = 0 means "everything" and always reads the whole file.
        assert!(parse_transcript_with(&path, 0, 0, 64).unwrap().is_empty());
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
