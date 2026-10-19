//! Capture-time message normalization tests (task-1, dev A).

use pd_memory_core::message::{
    has_new_content, merge_consecutive_same_role, normalize_messages, NormalizeOptions,
};
use pd_memory_core::types::{CaptureStrategy, MemoryMessage};

fn options(
    include_assistant: bool,
    max_message_chars: usize,
    capture_strategy: CaptureStrategy,
) -> NormalizeOptions {
    NormalizeOptions {
        include_assistant,
        max_message_chars,
        capture_strategy,
    }
}

fn default_options(strategy: CaptureStrategy) -> NormalizeOptions {
    options(true, 6000, strategy)
}

#[test]
fn role_mapping_drops_tool_messages() {
    let raw = vec![
        MemoryMessage::new("tool_result", "[tool result]"),
        MemoryMessage::new("toolResult", "again"),
        MemoryMessage::new("system", "system prompt noise"),
        MemoryMessage::user("hello"),
        MemoryMessage::assistant("hi"),
    ];
    let normalized = normalize_messages(&raw, &default_options(CaptureStrategy::FullSession));
    assert_eq!(normalized.len(), 2);
    assert_eq!(normalized[0].role, "user");
    assert_eq!(normalized[1].role, "assistant");
}

#[test]
fn truncates_with_exact_dot_suffix() {
    let raw = vec![MemoryMessage::user("一".repeat(20))];
    let normalized = normalize_messages(&raw, &options(false, 10, CaptureStrategy::LastTurn));
    let content = &normalized[0].content;
    assert_eq!(content.chars().count(), 13);
    assert!(content.ends_with("..."));
    assert!(content.starts_with("一一一一一一一一一一"));
}

#[test]
fn include_assistant_filters_and_consecutive_roles_merge() {
    let raw = vec![
        MemoryMessage::user("a"),
        MemoryMessage::user("b"),
        MemoryMessage::assistant("c"),
    ];
    let normalized = normalize_messages(&raw, &options(false, 6000, CaptureStrategy::FullSession));
    assert_eq!(normalized.len(), 1);
    assert_eq!(normalized[0].role, "user");
    assert_eq!(normalized[0].content, "a\nb");

    let merged = merge_consecutive_same_role(&[
        MemoryMessage::user("one"),
        MemoryMessage::user("two"),
        MemoryMessage::assistant("three"),
    ]);
    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0].content, "one\ntwo");
}

#[test]
fn capture_strategy_last_turn_vs_full_session() {
    let raw = vec![
        MemoryMessage::user("first question"),
        MemoryMessage::assistant("first answer"),
        MemoryMessage::user("second question"),
        MemoryMessage::assistant("second answer"),
    ];
    let last_turn = normalize_messages(&raw, &default_options(CaptureStrategy::LastTurn));
    assert_eq!(last_turn.len(), 2);
    assert_eq!(last_turn[0].content, "second question");

    let full = normalize_messages(&raw, &default_options(CaptureStrategy::FullSession));
    assert_eq!(full.len(), 4);
}

#[test]
fn strips_injected_memory_context_and_envelopes() {
    let injected = "You are using retrieved ClawXMemory file memories for this turn.\n\n## old memory\n- stale fact\n\nTreat the above as authoritative prior memory when it is relevant. Prioritize the user's latest request, and do not claim memory is missing or that this is a fresh conversation if the answer is already shown above.\n\n[Current message - respond to this]\n@alice 我的真实问题";
    let raw = vec![MemoryMessage::user(injected)];
    let normalized = normalize_messages(&raw, &default_options(CaptureStrategy::LastTurn));
    assert_eq!(normalized.len(), 1);
    assert_eq!(normalized[0].content, "我的真实问题");
}

#[test]
fn strips_sender_metadata_and_think_blocks() {
    let metadata = "Conversation info (untrusted metadata):\n```json\n{\"sender\": \"alice\"}\n```\n\n真实问题";
    let normalized = normalize_messages(
        &[MemoryMessage::user(metadata)],
        &default_options(CaptureStrategy::LastTurn),
    );
    assert_eq!(normalized[0].content, "真实问题");

    let thinking = "<thinking>internal reasoning</thinking>\n\n最终答复";
    let normalized = normalize_messages(
        &[MemoryMessage::assistant(thinking)],
        &default_options(CaptureStrategy::LastTurn),
    );
    assert_eq!(normalized[0].content, "最终答复");
}

#[test]
fn skips_commands_and_session_startup_markers() {
    let raw = vec![
        MemoryMessage::user("/new"),
        MemoryMessage::user("A new session was started via /new or /reset."),
        MemoryMessage::user("Based on this conversation, generate a short 1-2 word filename slug"),
        MemoryMessage::user("keep me"),
    ];
    let normalized = normalize_messages(&raw, &options(false, 6000, CaptureStrategy::LastTurn));
    assert_eq!(normalized.len(), 1);
    assert_eq!(normalized[0].content, "keep me");
}

#[test]
fn has_new_content_prefix_diff() {
    let previous = vec![MemoryMessage::user("a"), MemoryMessage::assistant("b")];
    let same = previous.clone();
    assert!(!has_new_content(&previous, &same));
    let appended = vec![
        MemoryMessage::user("a"),
        MemoryMessage::assistant("b"),
        MemoryMessage::user("c"),
    ];
    assert!(has_new_content(&previous, &appended));
    assert!(has_new_content(&[], &previous));
    assert!(!has_new_content(&previous, &[]));
    let changed = vec![
        MemoryMessage::user("a"),
        MemoryMessage::assistant("different"),
    ];
    assert!(has_new_content(&previous, &changed));
    // msg_id is ignored in the comparison.
    let mut with_id = previous.clone();
    with_id[0].msg_id = Some("message-1".into());
    assert!(!has_new_content(&previous, &with_id));
}
