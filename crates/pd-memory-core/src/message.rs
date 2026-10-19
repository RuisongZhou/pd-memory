//! Message normalization for capture (dev A).
//!
//! Port of `message-utils.ts`: cleans, denoises, truncates and merges messages
//! before they enter the L0 queue. All behaviour must stay deterministic.
//!
//! Input here is already flattened (`MemoryMessage { role, content }`), so the
//! block-level canonicalization that the TypeScript host does in
//! `MemoryResolver.canonicalMessagesToMemoryMessages` is expressed as: role
//! aliasing (`tool_result` -> `tool`, ...), filtering to `user`/`assistant`,
//! and merging consecutive messages of the same role with `\n`.
//!
//! The cleaning pipeline mirrors the reference step for step (`stripUserNoise`
//! has 15 ordered steps; `stripAssistantThinking` has 5) because step order
//! changes the resulting stored text.

use serde_json::Value;

use crate::types::{CaptureStrategy, MemoryMessage};

// ---------------------------------------------------------------------------
// Reference constants (message-utils.ts)
// ---------------------------------------------------------------------------

const MEMORY_CONTEXT_HEADER: &str =
    "You are using retrieved ClawXMemory file memories for this turn.";
const LEGACY_MEMORY_CONTEXT_HEADER: &str =
    "You are using multi-level memory indexes for this turn.";
const MEMORY_CONTEXT_FOOTER: &str = "Treat the above as authoritative prior memory when it is relevant. Prioritize the user's latest request, and do not claim memory is missing or that this is a fresh conversation if the answer is already shown above.";
const RECALL_CONTEXT_HEADER: &str = "## ClawXMemory Recall";
const RECALL_CONTEXT_INSTRUCTION: &str =
    "Use the following retrieved ClawXMemory evidence for this turn.";
const RECALL_CONTEXT_FOOTER: &str = "Treat the selected evidence above as authoritative historical memory for this turn when it is relevant.";
const RECALL_CONTEXT_FOOTER_FINAL: &str = "If the needed answer is already shown above, do not claim that memory is missing or that this is a fresh conversation.";
const SESSION_START_PREFIX: &str = "A new session was started via /new or /reset.";
const SLUG_PROMPT_PREFIX: &str =
    "Based on this conversation, generate a short 1-2 word filename slug";
const CURRENT_MESSAGE_MARKER: &str = "[Current message - respond to this]";
const QUOTED_HISTORY_MARKER: &str = "[Chat messages since your last reply - for context]";

const KNOWN_SLASH_COMMANDS: &[&str] = &[
    "help",
    "commands",
    "skill",
    "status",
    "allowlist",
    "approve",
    "context",
    "whoami",
    "id",
    "subagents",
    "config",
    "debug",
    "usage",
    "tts",
    "voice",
    "stop",
    "restart",
    "dock-telegram",
    "dock_telegram",
    "dock-discord",
    "dock_discord",
    "dock-slack",
    "dock_slack",
    "activation",
    "send",
    "reset",
    "new",
    "think",
    "thinking",
    "t",
    "verbose",
    "v",
    "reasoning",
    "reason",
    "elevated",
    "elev",
    "exec",
    "model",
    "models",
    "queue",
    "bash",
    "compact",
];

const KNOWN_BANG_COMMANDS: &[&str] = &["poll", "stop"];

/// Options controlling capture normalization.
#[derive(Debug, Clone)]
pub struct NormalizeOptions {
    pub include_assistant: bool,
    pub max_message_chars: usize,
    pub capture_strategy: CaptureStrategy,
}

/// Normalize raw messages into the L0 representation.
///
/// Implements role aliasing (`tool_result` -> `tool`), user/assistant filtering,
/// injection-memory and transport-envelope stripping, whitespace normalization,
/// `max_message_chars` truncation (with the reference `"..."` suffix), merging
/// of consecutive same-role messages, and `capture_strategy` slicing.
pub fn normalize_messages(raw: &[MemoryMessage], options: &NormalizeOptions) -> Vec<MemoryMessage> {
    let mut normalized: Vec<MemoryMessage> = Vec::new();
    for message in raw {
        if let Some(message) = normalize_single_message(message, options) {
            normalized.push(message);
        }
    }
    let normalized = merge_consecutive_same_role(&normalized);
    if options.capture_strategy == CaptureStrategy::FullSession {
        return normalized;
    }
    // last_turn: keep the last user message and everything after it.
    if let Some(last_user) = normalized.iter().rposition(|item| item.role == "user") {
        return normalized[last_user..].to_vec();
    }
    normalized.iter().rev().take(2).rev().cloned().collect()
}

/// Merge adjacent messages that share the same role (`content` joined by `\n`).
///
/// The TypeScript host performs this per canonical message when flattening
/// content blocks; because this crate receives flat messages the equivalent
/// normalization is applied across adjacent entries. The first `msg_id` wins.
pub fn merge_consecutive_same_role(messages: &[MemoryMessage]) -> Vec<MemoryMessage> {
    let mut merged: Vec<MemoryMessage> = Vec::with_capacity(messages.len());
    for message in messages {
        if let Some(last) = merged.last_mut() {
            if last.role == message.role {
                last.content.push('\n');
                last.content.push_str(&message.content);
                continue;
            }
        }
        merged.push(message.clone());
    }
    merged
}

/// Whether `next` contains anything newer than `previous` (prefix diff used by
/// the heartbeat indexer to avoid re-indexing).
///
/// Mirrors `hasNewContent` (`heartbeat.ts:59-67`): `msg_id` is ignored.
pub fn has_new_content(previous: &[MemoryMessage], next: &[MemoryMessage]) -> bool {
    if next.is_empty() {
        return false;
    }
    if previous.is_empty() {
        return true;
    }
    if next.len() > previous.len() {
        return true;
    }
    for (left, right) in previous.iter().zip(next.iter()) {
        if left.role != right.role || left.content != right.content {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Per-message normalization
// ---------------------------------------------------------------------------

fn normalize_single_message(
    message: &MemoryMessage,
    options: &NormalizeOptions,
) -> Option<MemoryMessage> {
    let role = map_role(&message.role)?;
    if role != "user" && role != "assistant" {
        // `tool` entries never enter L0 (the reference drops them after the
        // canonical -> memory flattening step).
        return None;
    }
    if role == "assistant" && !options.include_assistant {
        return None;
    }
    let stripped = match role {
        "user" => strip_user_noise(&message.content),
        _ => strip_assistant_thinking(&message.content),
    };
    if role == "user" && should_skip_user_message(&stripped) {
        return None;
    }
    if role == "assistant" && should_skip_assistant_message(&stripped) {
        return None;
    }
    if stripped.is_empty() {
        return None;
    }
    Some(MemoryMessage {
        msg_id: message
            .msg_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .map(str::to_string),
        role: role.to_string(),
        content: truncate(&stripped, options.max_message_chars),
    })
}

/// Map raw transcript roles onto the L0 vocabulary.
///
/// `tool_result` (and its canonical aliases) become `tool`; everything outside
/// `user`/`assistant`/`tool` is rejected (`None`), matching `inspectTranscriptMessage`.
fn map_role(role: &str) -> Option<&'static str> {
    match role {
        "user" => Some("user"),
        "assistant" => Some("assistant"),
        "tool" | "tool_result" | "toolResult" | "tool_use" | "toolUse" | "toolCall" => Some("tool"),
        _ => None,
    }
}

/// `truncate` from `message-utils.ts:85-88`: no-op when short, otherwise
/// `slice(0, max) + "..."` (the result may be `max + 3` chars long).
pub(crate) fn truncate(text: &str, max_length: usize) -> String {
    if max_length == 0 {
        return text.to_string();
    }
    let mut chars = text.chars();
    let mut prefix = String::new();
    for _ in 0..max_length {
        match chars.next() {
            Some(character) => prefix.push(character),
            None => return text.to_string(),
        }
    }
    if chars.next().is_none() {
        return text.to_string();
    }
    prefix.push_str("...");
    prefix
}

// ---------------------------------------------------------------------------
// User noise stripping (stripUserNoise, 15 ordered steps)
// ---------------------------------------------------------------------------

fn strip_user_noise(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let sender_hints = collect_sender_hints(text);
    let mut cleaned = strip_injected_memory_context(text);
    cleaned = extract_current_message_segment(&cleaned);
    cleaned = strip_quoted_context_blocks(&cleaned);
    cleaned = strip_attachment_scaffolding(&cleaned);
    cleaned = strip_untrusted_sender_metadata(&cleaned);
    cleaned = strip_leading_current_message_envelope(&cleaned);
    cleaned = strip_leading_timestamp_prefix(&cleaned);
    cleaned = strip_leading_message_envelope_lines(&cleaned);
    cleaned = strip_leading_sender_prefix(&cleaned, &sender_hints);
    cleaned = strip_leading_mentions(&cleaned);
    cleaned = strip_plugin_state_scaffolding(&cleaned);
    cleaned = strip_leading_message_envelope_lines(&cleaned);
    cleaned = strip_leading_timestamp_prefix(&cleaned);
    compact_whitespace(&cleaned)
}

fn strip_injected_memory_context(text: &str) -> String {
    let mut cleaned = text.trim().to_string();
    for header in [MEMORY_CONTEXT_HEADER, LEGACY_MEMORY_CONTEXT_HEADER] {
        cleaned = remove_between_spans(&cleaned, header, MEMORY_CONTEXT_FOOTER);
    }
    if cleaned.contains(RECALL_CONTEXT_HEADER) && cleaned.contains(RECALL_CONTEXT_INSTRUCTION) {
        cleaned = remove_recall_context_block(&cleaned);
        let mut lines: Vec<String> = cleaned
            .replace('\r', "")
            .split('\n')
            .map(str::to_string)
            .collect();
        while let Some(first) = lines.first() {
            let value = first.trim();
            if value.is_empty() || value.to_ascii_lowercase().starts_with("system:") {
                lines.remove(0);
            } else {
                break;
            }
        }
        cleaned = lines.join("\n").trim().to_string();
    }
    cleaned
}

/// Remove `header ... footer` spans, consuming trailing whitespace after the
/// footer (the reference uses the `g` flag; we loop for parity).
fn remove_between_spans(text: &str, header: &str, footer: &str) -> String {
    let mut result = text.to_string();
    loop {
        let Some(start) = result.find(header) else {
            break;
        };
        let search_from = start + header.len();
        let Some(footer_offset) = result[search_from..].find(footer) else {
            break;
        };
        let mut end = search_from + footer_offset + footer.len();
        end += trailing_whitespace_len(&result[end..]);
        result.replace_range(start..end, "");
        result = result.trim().to_string();
    }
    result
}

fn remove_recall_context_block(text: &str) -> String {
    let mut result = text.to_string();
    loop {
        let Some(start) = result.find(RECALL_CONTEXT_HEADER) else {
            break;
        };
        let Some(instruction_offset) = result[start..].find(RECALL_CONTEXT_INSTRUCTION) else {
            break;
        };
        let after_instruction = start + instruction_offset + RECALL_CONTEXT_INSTRUCTION.len();
        let Some(footer_offset) = result[after_instruction..].find(RECALL_CONTEXT_FOOTER) else {
            break;
        };
        let after_footer = after_instruction + footer_offset + RECALL_CONTEXT_FOOTER.len();
        let Some(final_offset) = result[after_footer..].find(RECALL_CONTEXT_FOOTER_FINAL) else {
            break;
        };
        let mut end = after_footer + final_offset + RECALL_CONTEXT_FOOTER_FINAL.len();
        end += trailing_whitespace_len(&result[end..]);
        result.replace_range(start..end, "");
        result = result.trim().to_string();
    }
    result
}

fn extract_current_message_segment(text: &str) -> String {
    match text.rfind(CURRENT_MESSAGE_MARKER) {
        Some(index) => text[index + CURRENT_MESSAGE_MARKER.len()..]
            .trim()
            .to_string(),
        None => text.to_string(),
    }
}

fn strip_quoted_context_blocks(text: &str) -> String {
    let mut cleaned = text.to_string();
    for marker in ["Chat history since last reply", "Replied message"] {
        loop {
            let Some(start) = find_ci(&cleaned, marker, 0) else {
                break;
            };
            let Some(removed) = quoted_context_span(&cleaned, start, marker.len()) else {
                break;
            };
            cleaned.replace_range(start..removed, "");
        }
    }
    // `[Chat messages since your last reply - for context] ... [Current message...]`
    loop {
        let Some(start) = cleaned.find(QUOTED_HISTORY_MARKER) else {
            break;
        };
        let after_marker = start + QUOTED_HISTORY_MARKER.len();
        let end = match cleaned[after_marker..].find(CURRENT_MESSAGE_MARKER) {
            Some(offset) => after_marker + offset,
            None => cleaned.len(),
        };
        cleaned.replace_range(start..end, "");
    }
    cleaned
}

/// Span end for `Label (untrusted...): ```json ... ``` ` starting at `start`.
fn quoted_context_span(text: &str, start: usize, label_len: usize) -> Option<usize> {
    let mut index = start + label_len;
    index += leading_whitespace_len(&text[index..]);
    if !text[index..].starts_with('(') {
        return None;
    }
    index += 1;
    if !find_ci(&text[index..], "untrusted", 0)
        .map(|offset| offset == 0)
        .unwrap_or(false)
    {
        return None;
    }
    let close = text[index..].find(')')?;
    index += close + 1;
    index += leading_whitespace_len(&text[index..]);
    if !text[index..].starts_with(':') {
        return None;
    }
    index += 1;
    index += leading_whitespace_len(&text[index..]);
    if !text[index..].starts_with("```") {
        return None;
    }
    index += 3;
    if text[index..].starts_with("json") {
        index += 4;
    }
    let close = text[index..].find("```")?;
    let mut end = index + close + 3;
    end += trailing_whitespace_len(&text[end..]);
    Some(end)
}

fn strip_attachment_scaffolding(text: &str) -> String {
    let mut cleaned = remove_whole_lines(text, |line| {
        lower_starts_with(line, "[media attached:") && line.rfind(']').is_some()
    });
    cleaned = remove_whole_lines(&cleaned, |line| {
        lower_starts_with(line, "to send an image back,")
    });
    if let Some(index) = find_ci(&cleaned, "\n<file", 0) {
        cleaned.truncate(index);
    }
    // Remove `<file ...>` tags (case-insensitive, tag ends at the first `>`).
    let mut search_from = 0;
    loop {
        let Some(start) = find_ci(&cleaned, "<file", search_from) else {
            break;
        };
        let after = start + "<file".len();
        let is_word_boundary = cleaned[after..]
            .chars()
            .next()
            .map(|character| !(character.is_ascii_alphanumeric() || character == '_'))
            .unwrap_or(true);
        if !is_word_boundary {
            search_from = after;
            continue;
        }
        let Some(close) = cleaned[after..].find('>') else {
            break;
        };
        cleaned.replace_range(start..after + close + 1, "");
    }
    cleaned
}

/// Strip `Conversation info|Sender (untrusted metadata):` blocks.
fn strip_untrusted_sender_metadata(text: &str) -> String {
    let mut cleaned = remove_sender_metadata_blocks(text);
    cleaned = remove_inline_sender_metadata(&cleaned);
    cleaned
}

fn remove_sender_metadata_blocks(text: &str) -> String {
    let mut result = text.to_string();
    let mut search_from = 0;
    while let Some((header_start, after_colon)) = find_sender_metadata_header(&result, search_from)
    {
        if !result[after_colon..].starts_with("```") {
            search_from = after_colon.max(header_start + 1);
            continue;
        }
        let fence_body = after_colon + 3;
        let Some(close) = result[fence_body..].find("```") else {
            break;
        };
        let mut end = fence_body + close + 3;
        end += trailing_whitespace_len(&result[end..]);
        result.replace_range(header_start..end, "");
        search_from = header_start;
    }
    result
}

fn remove_inline_sender_metadata(text: &str) -> String {
    let mut result = text.to_string();
    let mut search_from = 0;
    while let Some((header_start, after_colon)) = find_sender_metadata_header(&result, search_from)
    {
        if !result[after_colon..].starts_with('{') {
            search_from = after_colon.max(header_start + 1);
            continue;
        }
        let Some(close) = result[after_colon..].find('}') else {
            break;
        };
        let mut end = after_colon + close + 1;
        end += trailing_whitespace_len(&result[end..]);
        result.replace_range(header_start..end, "");
        search_from = header_start;
    }
    result
}

/// Find `Conversation info|Sender ... (untrusted metadata) :` and return
/// `(header_start, index_after_colon)`.
fn find_sender_metadata_header(text: &str, from: usize) -> Option<(usize, usize)> {
    let mut candidates: Vec<(usize, usize)> = Vec::new();
    for marker in ["conversation info", "sender"] {
        if let Some(start) = find_ci(text, marker, from) {
            let mut index = start + marker.len();
            index += leading_whitespace_len(&text[index..]);
            let rest = &text[index..];
            if rest
                .to_ascii_lowercase()
                .starts_with("(untrusted metadata)")
            {
                index += "(untrusted metadata)".len();
                index += leading_whitespace_len(&text[index..]);
                if text[index..].starts_with(':') {
                    index += 1;
                    index += leading_whitespace_len(&text[index..]);
                    candidates.push((start, index));
                }
            }
        }
    }
    candidates.into_iter().min_by_key(|(start, _)| *start)
}

fn collect_sender_hints(text: &str) -> Vec<String> {
    let mut hints: Vec<String> = Vec::new();
    let mut from = 0;
    while let Some((header_start, after_colon)) = find_sender_metadata_header(text, from) {
        let mut json_start = after_colon;
        if text[json_start..].starts_with("```") {
            json_start += 3;
            if text[json_start..].to_ascii_lowercase().starts_with("json") {
                json_start += 4;
            }
        }
        if let Some((value, end)) = parse_json_object_at(text, json_start) {
            if let Some(object) = value.as_object() {
                for key in ["sender", "sender_id", "sender_label"] {
                    if let Some(Value::String(raw)) = object.get(key) {
                        let normalized = normalize_sender_hint(raw);
                        if !normalized.is_empty() && !hints.contains(&normalized) {
                            hints.push(normalized);
                        }
                    }
                }
            }
            from = end;
        } else {
            from = header_start + 1;
            if from >= text.len() {
                break;
            }
        }
    }
    hints
}

fn strip_leading_current_message_envelope(text: &str) -> String {
    let mut index = 0;
    let mut matched_groups = 0;
    loop {
        let rest = &text[index..];
        if !rest.starts_with('[') {
            break;
        }
        let mut cursor = index + 1;
        let mut count = 0;
        let mut closed = false;
        while cursor < text.len() && count <= 320 {
            let character = text[cursor..].chars().next().unwrap_or('\0');
            if character == ']' {
                closed = true;
                break;
            }
            if character == '\n' {
                break;
            }
            count += 1;
            cursor += character.len_utf8();
        }
        if !closed || count == 0 {
            break;
        }
        cursor += 1; // ']'
        cursor += leading_whitespace_len(&text[cursor..]);
        index = cursor;
        matched_groups += 1;
    }
    if matched_groups == 0 {
        return text.to_string();
    }
    let rest = &text[index..];
    let colon = match rest.find(':') {
        Some(offset) if offset <= 120 => offset,
        _ => return text.to_string(),
    };
    let label = &rest[..colon];
    if label.is_empty() || label.contains('\n') {
        return text.to_string();
    }
    rest[colon + 1..].trim().to_string()
}

fn strip_leading_timestamp_prefix(text: &str) -> String {
    let after_whitespace = leading_whitespace_len(text);
    let rest = &text[after_whitespace..];
    if !rest.starts_with('[') {
        return text.to_string();
    }
    let Some(close) = rest.find(']') else {
        return text.to_string();
    };
    let contents = &rest[1..close];
    if contents.contains('\n') {
        return text.to_string();
    }
    if !(contains_date_like(contents)
        || contains_ci(contents, "GMT")
        || contains_ci(contents, "UTC"))
    {
        return text.to_string();
    }
    let mut index = after_whitespace + close + 1;
    index += leading_whitespace_len(&text[index..]);
    text[index..].to_string()
}

fn strip_leading_message_envelope_lines(text: &str) -> String {
    let mut lines: Vec<&str> = text.split('\n').collect();
    while let Some(first) = lines.first() {
        if is_envelope_noise_line(first) {
            lines.remove(0);
        } else {
            break;
        }
    }
    lines.join("\n").trim().to_string()
}

fn is_envelope_noise_line(line: &str) -> bool {
    let value = line.trim();
    if value.is_empty() {
        return true;
    }
    let lowered = value.to_ascii_lowercase();
    if value == "```" || lowered == "```json" || value == "{" || value == "}" {
        return true;
    }
    for marker in ["conversation info", "sender"] {
        if starts_with_ci(value, marker) {
            let after_label = value[marker.len()..].trim_start();
            if after_label
                .to_ascii_lowercase()
                .starts_with("(untrusted metadata)")
            {
                return true;
            }
        }
    }
    if (starts_with_ci(value, "chat history since last reply")
        || starts_with_ci(value, "replied message"))
        && contains_ci(value, "(untrusted")
    {
        return true;
    }
    if lowered.starts_with("system:") && value.contains('[') {
        if let Some(close) = value.find(']') {
            let contents = &value[value.find('[').unwrap_or(0)..=close];
            if contains_date_like(contents)
                || contains_ci(contents, "GMT")
                || contains_ci(contents, "UTC")
            {
                return true;
            }
        }
    }
    if lowered.starts_with("[message_id:") && value.ends_with(']') {
        return true;
    }
    if value.starts_with('[') && value.ends_with(']') {
        let inner = &value[1..value.len() - 1];
        if contains_ci(inner, "for context") || inner.contains(CURRENT_MESSAGE_MARKER) {
            return true;
        }
    }
    if value.starts_with('[') && !value.contains('\n') {
        if let Some(close) = value.find(']') {
            let contents = &value[1..close];
            let rest = value[close + 1..].trim_start();
            let has_timestamp = contains_date_like(contents)
                || contains_ci(contents, "GMT")
                || contains_ci(contents, "UTC")
                || contains_ci(contents, "channel:");
            if has_timestamp {
                if let Some(colon) = rest.find(':') {
                    let label = &rest[..colon];
                    if !label.is_empty() && label.len() <= 120 && !label.contains('\n') {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn strip_leading_sender_prefix(text: &str, sender_hints: &[String]) -> String {
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    let first_line = lines.first().cloned().unwrap_or_default();
    let trimmed_first = first_line.trim();
    let Some(colon) = trimmed_first.find(':') else {
        return text.to_string();
    };
    let label = &trimmed_first[..colon];
    if label.is_empty() || label.len() > 120 {
        return text.to_string();
    }
    let normalized_label = normalize_sender_hint(label);
    let should_strip = sender_hints.iter().any(|hint| hint == &normalized_label)
        || looks_like_opaque_sender_label(label);
    if !should_strip {
        return text.to_string();
    }
    lines[0] = trimmed_first[colon + 1..].trim_start().to_string();
    lines.join("\n").trim().to_string()
}

fn strip_leading_mentions(text: &str) -> String {
    let mut cleaned = text.trim_start().to_string();
    loop {
        let trimmed = cleaned.trim_start();
        if !trimmed.starts_with('@') {
            break;
        }
        let mut end = 1;
        for character in trimmed[1..].chars() {
            if character.is_ascii_alphanumeric()
                || character == '_'
                || character == '.'
                || character == '-'
            {
                end += character.len_utf8();
            } else {
                break;
            }
        }
        if end == 1 {
            break;
        }
        let after = end + leading_whitespace_len(&trimmed[end..]);
        cleaned = trimmed[after..].to_string();
    }
    cleaned
}

fn normalize_sender_hint(value: &str) -> String {
    value
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn looks_like_opaque_sender_label(label: &str) -> bool {
    let trimmed = label.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 120 {
        return false;
    }
    let ascii_label = trimmed
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "_.@()- ".contains(character));
    if !ascii_label {
        return false;
    }
    let has_digit_or_underscore = trimmed
        .chars()
        .any(|character| character.is_ascii_digit() || character == '_');
    let looks_like_name = trimmed
        .chars()
        .next()
        .map(|first| first.is_ascii_alphabetic())
        .unwrap_or(false)
        && trimmed.chars().count() >= 5
        && trimmed
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_.@()- ".contains(character));
    has_digit_or_underscore || looks_like_name
}

// ---------------------------------------------------------------------------
// Plugin-state scaffolding
// ---------------------------------------------------------------------------

const PLUGIN_STATE_METADATA_WORDS: &[&str] = &["agent", "host", "workspace", "os", "node", "model"];

fn strip_plugin_state_scaffolding(text: &str) -> String {
    let normalized = compact_whitespace(text);
    if normalized.is_empty() {
        return String::new();
    }

    let mut paragraphs: Vec<String> = split_paragraphs(&normalized)
        .into_iter()
        .map(|paragraph| paragraph.trim().to_string())
        .filter(|paragraph| !paragraph.is_empty())
        .collect();
    while paragraphs
        .first()
        .map(|paragraph| paragraph_looks_like_plugin_noise(paragraph))
        .unwrap_or(false)
    {
        paragraphs.remove(0);
    }
    let mut cleaned = paragraphs.join("\n\n").trim().to_string();
    if cleaned.is_empty() {
        return String::new();
    }

    let mut lines: Vec<&str> = cleaned.split('\n').collect();
    while let Some(first) = lines.first() {
        if line_looks_like_plugin_noise(first) {
            lines.remove(0);
        } else {
            break;
        }
    }
    cleaned = compact_whitespace(&lines.join("\n"));
    if cleaned.is_empty() {
        return String::new();
    }
    if paragraph_looks_like_plugin_noise(&cleaned) {
        String::new()
    } else {
        cleaned
    }
}

fn normalize_plugin_noise_probe(text: &str) -> String {
    let without_emphasis = text.replace('\r', "").replace("**", "").replace('`', "");
    let stripped_lines: Vec<String> = without_emphasis
        .split('\n')
        .map(|line| {
            line.trim_start_matches(|character: char| {
                character == '>'
                    || character == '-'
                    || character == '*'
                    || character.is_whitespace()
            })
            .to_string()
        })
        .collect();
    stripped_lines
        .join("\n")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn starts_with_plugin_state_line(normalized: &str) -> bool {
    if starts_with_ci(normalized, "project state maintained by")
        || starts_with_ci(normalized, "session status")
    {
        return true;
    }
    if starts_with_ci(normalized, "current git branch") {
        let rest = &normalized["current git branch".len()..];
        return rest.trim_start().starts_with(':');
    }
    if starts_with_ci(normalized, "git status") {
        let rest = &normalized["git status".len()..];
        let rest = if starts_with_ci(rest.trim_start(), "summary") {
            &rest.trim_start()["summary".len()..]
        } else {
            rest
        };
        return rest.trim_start().starts_with(':');
    }
    false
}

fn count_plugin_state_markers(normalized: &str) -> usize {
    let mut count = 0;
    if contains_ci(normalized, "project state maintained by") {
        count += 1;
    }
    if contains_ci(normalized, "session status") {
        count += 1;
    }
    for word in PLUGIN_STATE_METADATA_WORDS {
        if contains_word_then_colon(normalized, word) {
            count += 1;
        }
    }
    if contains_word_then_colon(normalized, "current git branch") {
        count += 1;
    }
    if contains_word_then_colon(normalized, "git status") {
        count += 1;
    }
    count
}

fn line_looks_like_plugin_noise(text: &str) -> bool {
    let normalized = normalize_plugin_noise_probe(text);
    if normalized.is_empty() {
        return false;
    }
    starts_with_plugin_state_line(&normalized) || count_plugin_state_markers(&normalized) >= 4
}

fn paragraph_looks_like_plugin_noise(text: &str) -> bool {
    let normalized = normalize_plugin_noise_probe(text);
    if normalized.is_empty() {
        return false;
    }
    if starts_with_plugin_state_line(&normalized) {
        return true;
    }
    let lines: Vec<String> = text
        .replace('\r', "")
        .split('\n')
        .map(normalize_plugin_noise_probe)
        .filter(|line| !line.is_empty())
        .collect();
    let noisy_lines = lines
        .iter()
        .filter(|line| starts_with_plugin_state_line(line) || count_plugin_state_markers(line) >= 3)
        .count();
    let marker_count = count_plugin_state_markers(&normalized);
    if contains_ci(&normalized, "session status") && marker_count >= 2 {
        return true;
    }
    let half = if lines.is_empty() {
        1
    } else {
        ((lines.len() as f64) / 2.0).ceil() as usize
    };
    marker_count >= 4 && noisy_lines >= half.max(1)
}

// ---------------------------------------------------------------------------
// Assistant cleaning
// ---------------------------------------------------------------------------

fn strip_assistant_thinking(text: &str) -> String {
    let mut without_leading = text.to_string();
    for close_tag in ["</think>", "</thinking>"] {
        if let Some(index) = find_ci(&without_leading, close_tag, 0) {
            without_leading = without_leading[index + close_tag.len()..].to_string();
        }
    }

    let mut without_think = remove_tag_pair(&without_leading, "think");
    without_think = remove_tag_pair(&without_think, "thinking");
    without_think = remove_simple_tag(&without_think, "think");
    without_think = remove_simple_tag(&without_think, "thinking");

    let trimmed_lines: Vec<String> = without_think
        .split('\n')
        .map(|line| line.trim_end().to_string())
        .collect();
    let mut compact: Vec<String> = Vec::with_capacity(trimmed_lines.len());
    let mut previous_empty = false;
    for line in &trimmed_lines {
        let empty = line.trim().is_empty();
        if empty && previous_empty {
            continue;
        }
        compact.push(line.clone());
        previous_empty = empty;
    }
    let normalized = compact.join("\n").trim().to_string();
    let paragraphs: Vec<String> = split_paragraphs(&normalized)
        .into_iter()
        .map(|paragraph| paragraph.trim().to_string())
        .filter(|paragraph| !paragraph.is_empty())
        .collect();
    if paragraphs.len() >= 2 {
        let first = paragraphs[0].as_str();
        let meta_reasoning = (first.starts_with("用户")
            && ["需要", "应该", "这是", "表达", "分享", "说明", "记录"]
                .iter()
                .any(|marker| first.contains(marker)))
            || (first.starts_with("搜索结果")
                && ["需要", "无法", "不太理想"]
                    .iter()
                    .any(|marker| first.contains(marker)))
            || (first.starts_with("这搜索")
                && ["不太全", "看不到", "找不到"]
                    .iter()
                    .any(|marker| first.contains(marker)));
        if meta_reasoning {
            return strip_plugin_state_scaffolding(
                &paragraphs[1..].join("\n\n").trim().to_string(),
            );
        }
    }
    strip_plugin_state_scaffolding(&normalized)
}

/// Remove `<name ...> ... </name>` pairs (case-insensitive).
fn remove_tag_pair(text: &str, name: &str) -> String {
    let mut result = text.to_string();
    loop {
        let Some(open_start) = find_tag_start(&result, name) else {
            break;
        };
        // Closing tags are handled by `remove_simple_tag`.
        if result[open_start..].starts_with("</") {
            break;
        }
        let open_after_name = open_start + tag_prefix_len(&result[open_start..], name) + name.len();
        let Some(open_close) = result[open_after_name..].find('>') else {
            break;
        };
        let body_start = open_after_name + open_close + 1;
        let closing = format!("</{name}");
        let Some(close_offset) = find_ci(&result[body_start..], &closing, 0) else {
            break;
        };
        let close_start = body_start + close_offset;
        let Some(close_end) = result[close_start..].find('>') else {
            break;
        };
        result.replace_range(open_start..close_start + close_end + 1, "");
    }
    result
}

fn remove_simple_tag(text: &str, name: &str) -> String {
    let mut result = text.to_string();
    loop {
        let Some(start) = find_tag_start(&result, name) else {
            break;
        };
        let after_name = start + tag_prefix_len(&result[start..], name) + name.len();
        let Some(close) = result[after_name..].find('>') else {
            break;
        };
        result.replace_range(start..after_name + close + 1, "");
    }
    result
}

/// Find `<name` / `</name` (case-insensitive) with a `<` or `</` prefix.
fn find_tag_start(text: &str, name: &str) -> Option<usize> {
    let mut from = 0;
    loop {
        let index = find_ci(text, name, from)?;
        let before = text[..index].chars().next_back();
        if before == Some('<') {
            return Some(index - 1);
        }
        if before == Some('/') {
            let mut prefix = index - 1;
            let before_slash = text[..prefix].chars().next_back();
            if before_slash == Some('<') {
                return Some(prefix - 1);
            }
            // Guard against underflow while scanning further.
            prefix = index;
            from = prefix;
        } else {
            from = index + name.len();
        }
    }
}

fn tag_prefix_len(text: &str, _name: &str) -> usize {
    if text.starts_with("</") {
        2
    } else {
        1
    }
}

// ---------------------------------------------------------------------------
// Skip predicates
// ---------------------------------------------------------------------------

fn should_skip_user_message(content: &str) -> bool {
    if content.is_empty() {
        return true;
    }
    is_session_startup_marker_text(content)
        || is_command_only_user_text(content)
        || content.starts_with(SLUG_PROMPT_PREFIX)
}

fn should_skip_assistant_message(content: &str) -> bool {
    if content.is_empty() {
        return true;
    }
    if !content.contains('\n') {
        let compact = content.trim();
        let meta_reasoning = (compact.starts_with("用户")
            && ["需要", "应该", "这是", "表达", "分享", "记录"]
                .iter()
                .any(|marker| compact.contains(marker)))
            || (compact.starts_with("搜索结果")
                && ["需要", "无法", "不太理想"]
                    .iter()
                    .any(|marker| compact.contains(marker)));
        return meta_reasoning;
    }
    false
}

/// `isSessionStartupMarkerText` (`message-utils.ts:509-517`).
pub fn is_session_startup_marker_text(text: &str) -> bool {
    let normalized = compact_whitespace(&strip_user_noise(text));
    if normalized.is_empty() {
        return false;
    }
    normalized.contains(SESSION_START_PREFIX)
        || (normalized.contains("A new session was started via /new or /reset.")
            && (contains_ci(&normalized, "Run your Session Startup sequence")
                || contains_ci(&normalized, "Execute your Session Startup sequence")))
}

/// `isCommandOnlyUserText` (`message-utils.ts:519-523`).
pub fn is_command_only_user_text(text: &str) -> bool {
    let normalized = compact_whitespace(&strip_user_noise(text));
    if normalized.is_empty() || is_session_startup_marker_text(&normalized) {
        return false;
    }
    parse_leading_slash_command(&normalized).is_some() || parse_bang_command(&normalized).is_some()
}

fn parse_leading_slash_command(text: &str) -> Option<String> {
    let trimmed = compact_whitespace(text);
    let rest = trimmed.strip_prefix('/')?;
    let mut characters = rest.chars();
    let first = characters.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    let mut end = first.len_utf8();
    for character in rest[end..].chars() {
        if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
            end += character.len_utf8();
        } else {
            break;
        }
    }
    let command = rest[..end].to_ascii_lowercase();
    let mut tail = &rest[end..];
    if let Some(stripped) = tail.strip_prefix(':') {
        tail = stripped;
    }
    let tail_ok = tail.is_empty() || tail.starts_with(char::is_whitespace);
    if !tail_ok {
        return None;
    }
    KNOWN_SLASH_COMMANDS
        .contains(&command.as_str())
        .then_some(command)
}

fn parse_bang_command(text: &str) -> Option<String> {
    let trimmed = compact_whitespace(text);
    let rest = trimmed.strip_prefix('!')?;
    if rest.starts_with(char::is_whitespace) {
        let mut characters = rest.chars();
        characters.next();
        return characters.next().is_some().then(|| "bash".to_string());
    }
    let mut characters = rest.chars();
    let first = characters.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    let mut end = first.len_utf8();
    for character in rest[end..].chars() {
        if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
            end += character.len_utf8();
        } else {
            break;
        }
    }
    let command = rest[..end].to_ascii_lowercase();
    let tail = &rest[end..];
    let tail_ok = tail.is_empty() || tail.starts_with(char::is_whitespace);
    if !tail_ok {
        return None;
    }
    KNOWN_BANG_COMMANDS
        .contains(&command.as_str())
        .then_some(command)
}

// ---------------------------------------------------------------------------
// Small text utilities
// ---------------------------------------------------------------------------

/// `compactWhitespace` (`message-utils.ts:187-202`): strip `\r`, trim line
/// ends, collapse runs of blank lines, join with `\n`, trim the result.
pub(crate) fn compact_whitespace(text: &str) -> String {
    let lines: Vec<String> = text
        .replace('\r', "")
        .split('\n')
        .map(|line| line.trim_end().to_string())
        .collect();
    let mut compact: Vec<String> = Vec::with_capacity(lines.len());
    let mut previous_empty = false;
    for line in lines {
        let empty = line.trim().is_empty();
        if empty && previous_empty {
            continue;
        }
        compact.push(if empty { String::new() } else { line });
        previous_empty = empty;
    }
    compact.join("\n").trim().to_string()
}

fn split_paragraphs(text: &str) -> Vec<String> {
    let mut paragraphs: Vec<String> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in text.split('\n') {
        if line.trim().is_empty() {
            if !current.is_empty() {
                paragraphs.push(current.join("\n"));
                current.clear();
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        paragraphs.push(current.join("\n"));
    }
    paragraphs
}

/// Search for an ASCII needle case-insensitively, returning the byte offset.
fn find_ci(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    if from > haystack.len() || needle.is_empty() {
        return None;
    }
    let haystack_bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    if needle_bytes.len() > haystack_bytes.len() {
        return None;
    }
    let mut index = from;
    while index + needle_bytes.len() <= haystack_bytes.len() {
        let matches = haystack_bytes[index].eq_ignore_ascii_case(&needle_bytes[0])
            && haystack_bytes[index..index + needle_bytes.len()]
                .iter()
                .zip(needle_bytes.iter())
                .all(|(left, right)| left.eq_ignore_ascii_case(right));
        if matches {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    find_ci(haystack, needle, 0).is_some()
}

fn starts_with_ci(haystack: &str, needle: &str) -> bool {
    find_ci(haystack, needle, 0) == Some(0)
}

fn lower_starts_with(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .starts_with(&needle.to_ascii_lowercase())
}

/// `\b<word>\s*:` (ASCII word boundary, case-insensitive).
fn contains_word_then_colon(text: &str, word: &str) -> bool {
    let mut from = 0;
    while let Some(index) = find_ci(text, word, from) {
        let before = text[..index].chars().next_back();
        let boundary_before = before
            .map(|character| !(character.is_ascii_alphanumeric() || character == '_'))
            .unwrap_or(true);
        if boundary_before {
            let after = &text[index + word.len()..];
            let after_trimmed = after.trim_start_matches(char::is_whitespace);
            if after_trimmed.starts_with(':') {
                return true;
            }
        }
        from = index + word.len();
    }
    false
}

fn contains_date_like(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_digit() {
            let mut cursor = index;
            let mut digits = 0;
            while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
                cursor += 1;
                digits += 1;
            }
            if digits == 4
                && cursor < bytes.len()
                && bytes[cursor] == b'-'
                && month_day_match(&text[cursor + 1..])
            {
                return true;
            }
            index = cursor;
        } else {
            index += 1;
        }
    }
    false
}

fn month_day_match(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut index = 0;
    let mut digits = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
        digits += 1;
    }
    if !(1..=2).contains(&digits) || index >= bytes.len() || bytes[index] != b'-' {
        return false;
    }
    index += 1;
    let mut day_digits = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
        day_digits += 1;
    }
    (1..=2).contains(&day_digits)
}

fn leading_whitespace_len(text: &str) -> usize {
    text.len() - text.trim_start_matches(char::is_whitespace).len()
}

fn trailing_whitespace_len(text: &str) -> usize {
    text.len() - text.trim_end_matches(char::is_whitespace).len()
}

/// Parse a JSON object starting exactly at `start` (leading whitespace allowed)
/// and return the value plus the byte offset just past the closing brace.
fn parse_json_object_at(text: &str, start: usize) -> Option<(serde_json::Value, usize)> {
    let bytes = text.as_bytes();
    let mut index = start;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    if index >= bytes.len() || bytes[index] != b'{' {
        return None;
    }
    let object_start = index;
    let mut depth = 0_i64;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else {
            match byte {
                b'"' => in_string = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        let end = index + 1;
                        let value = serde_json::from_str(&text[object_start..end]).ok()?;
                        return Some((value, end));
                    }
                }
                _ => {}
            }
        }
        index += 1;
    }
    None
}

/// Remove whole lines whose content satisfies `predicate` (line endings dropped).
fn remove_whole_lines(text: &str, predicate: impl Fn(&str) -> bool) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    lines
        .into_iter()
        .filter(|line| !predicate(line))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(include_assistant: bool, max: usize, strategy: CaptureStrategy) -> NormalizeOptions {
        NormalizeOptions {
            include_assistant,
            max_message_chars: max,
            capture_strategy: strategy,
        }
    }

    #[test]
    fn truncates_with_exact_suffix() {
        assert_eq!(truncate("abcdef", 6), "abcdef");
        assert_eq!(truncate("abcdef", 5), "abcde...");
        assert_eq!(truncate("abcdef", 0), "abcdef");
    }

    #[test]
    fn maps_roles_and_filters() {
        let raw = vec![
            MemoryMessage::new("tool_result", "[tool result]"),
            MemoryMessage::new("system", "ignore me"),
            MemoryMessage::user("hello"),
            MemoryMessage::assistant("hi"),
            MemoryMessage::new("tool", "output"),
        ];
        let out = normalize_messages(&raw, &options(true, 100, CaptureStrategy::FullSession));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, "user");
        assert_eq!(out[1].role, "assistant");
    }

    #[test]
    fn last_turn_strategy_keeps_last_user_turn() {
        let raw = vec![
            MemoryMessage::user("first"),
            MemoryMessage::assistant("answer 1"),
            MemoryMessage::user("second"),
            MemoryMessage::assistant("answer 2"),
        ];
        let out = normalize_messages(&raw, &options(true, 100, CaptureStrategy::LastTurn));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].content, "second");
        assert_eq!(out[1].content, "answer 2");
    }

    #[test]
    fn merges_consecutive_same_role() {
        let raw = vec![
            MemoryMessage::user("a"),
            MemoryMessage::user("b"),
            MemoryMessage::assistant("c"),
        ];
        let out = normalize_messages(&raw, &options(true, 100, CaptureStrategy::FullSession));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].content, "a\nb");
    }

    #[test]
    fn strips_injected_memory_context() {
        let text = format!(
            "{MEMORY_CONTEXT_HEADER}\n\n## old memory\nstuff\n\n{MEMORY_CONTEXT_FOOTER}\n\nreal question"
        );
        assert_eq!(strip_user_noise(&text), "real question");
    }

    #[test]
    fn strips_sender_metadata_and_envelopes() {
        let text = "Conversation info (untrusted metadata):\n```json\n{\"sender\": \"alice\"}\n```\n\n[Current message - respond to this]\n@alice 你好";
        assert_eq!(strip_user_noise(text), "你好");
    }

    #[test]
    fn strips_think_blocks() {
        let text = "<thinking>internal</thinking>\n\n最终答复";
        assert_eq!(strip_assistant_thinking(text), "最终答复");
    }

    #[test]
    fn has_new_content_semantics() {
        let a = vec![MemoryMessage::user("a")];
        let b = vec![MemoryMessage::user("a"), MemoryMessage::assistant("b")];
        assert!(!has_new_content(&a, &a.clone()));
        assert!(has_new_content(&a, &b));
        assert!(has_new_content(&[], &a));
        assert!(!has_new_content(&a, &[]));
        let changed = vec![MemoryMessage::user("changed")];
        assert!(has_new_content(&a, &changed));
    }
}
