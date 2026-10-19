//! Per-host adapters: one file per agent platform.
//!
//! An adapter translates between one host's hook wire format and the
//! host-agnostic pipeline:
//!
//! ```text
//! stdin JSON --parse--> HookInput --normalize--> CanonicalEvent
//!     --policy::Session (lock, first-recall barrier, caps)--> HookOutput
//!     --render--> stdout JSON
//! ```
//!
//! [`crate::policy`] reads [`HostCaps`] instead of branching on the host, so a
//! new platform is added by writing one adapter, listing it in [`all`], and
//! adding fixtures under `tests/fixtures/<id>/`. Adapters stay out of
//! `pd-memory-core`: they never read the environment or the home directory.

use pd_memory_core::error::Result;
use pd_memory_core::types::MemoryMessage;

use crate::{HookInput, HookOutput};

pub mod codex;
pub mod workbuddy;

/// Host-agnostic lifecycle event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalEvent {
    /// Session started: inject the memory overview (no model call).
    Start,
    /// User submitted a prompt: recall context for it.
    Recall {
        /// Prompt text as sent by the host (may be empty).
        prompt: String,
    },
    /// A turn finished: capture the transcript, then run due maintenance.
    Capture,
    /// The session ended: capture, and flush when the host allows the time.
    End,
    /// The host is about to compact its context: flush pending indexing.
    Compact,
    /// No pd-memory behavior.
    Ignore,
}

impl CanonicalEvent {
    /// The event without its payload.
    pub fn kind(&self) -> EventKind {
        match self {
            Self::Start => EventKind::Start,
            Self::Recall { .. } => EventKind::Recall,
            Self::Capture => EventKind::Capture,
            Self::End => EventKind::End,
            Self::Compact => EventKind::Compact,
            Self::Ignore => EventKind::Ignore,
        }
    }
}

/// [`CanonicalEvent`] discriminant, used by [`HostCaps::inject_on`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    Start,
    Recall,
    Capture,
    End,
    Compact,
    Ignore,
}

/// What a host allows. Policy reads these instead of matching on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostCaps {
    /// How long the host lets the session-end hook run before killing it.
    /// Below [`HostCaps::MIN_END_FLUSH_MS`] the hook only captures (no
    /// maintenance, no backlog flush) and waits at most half the budget for
    /// the host lock.
    pub end_budget_ms: u64,
    /// Whether hook payloads carry a usable `transcript_path`.
    pub has_transcript: bool,
    /// Events whose output may carry `hookSpecificOutput` (injected context).
    /// Every other event renders without it.
    pub inject_on: &'static [EventKind],
}

impl HostCaps {
    /// Smallest session-end budget that still leaves time for model calls.
    pub const MIN_END_FLUSH_MS: u64 = 30_000;
    /// Lock wait for every event that is not budget-limited.
    pub const DEFAULT_LOCK_WAIT_MS: u64 = 120_000;

    /// Whether session end has time to flush the backlog with model calls.
    pub fn end_can_flush(&self) -> bool {
        self.end_budget_ms >= Self::MIN_END_FLUSH_MS
    }

    /// Longest time to wait for the host lock before giving up on `event`.
    pub fn lock_wait_ms(&self, event: EventKind) -> u64 {
        match event {
            EventKind::End => (self.end_budget_ms / 2).min(Self::DEFAULT_LOCK_WAIT_MS),
            _ => Self::DEFAULT_LOCK_WAIT_MS,
        }
    }

    /// Whether `event`'s output may inject context.
    pub fn injects_on(&self, event: EventKind) -> bool {
        self.inject_on.contains(&event)
    }
}

/// One agent platform's hook contract.
pub trait HostAdapter: Sync {
    /// Stable id: the `hook --host <id>` value and the `control/<id>/`
    /// directory holding this host's lock and session markers.
    fn id(&self) -> &'static str;

    /// Human-readable name used in logs.
    fn label(&self) -> &'static str;

    /// Host capabilities read by policy.
    fn caps(&self) -> HostCaps;

    /// Map the host payload to a canonical event. [`HookInput`] keeps every
    /// unknown field in `extra`, so adapters can read host-specific fields.
    fn normalize(&self, input: &HookInput) -> CanonicalEvent;

    /// Serialize `outcome` for `event` as the single JSON object the host
    /// reads from stdout, enforcing the host's output schema.
    fn render(&self, event: &CanonicalEvent, outcome: HookOutput) -> String {
        render_claude_code(self.caps(), event, outcome)
    }

    /// Recent user/assistant messages from the host transcript (oldest first,
    /// at most `max_messages`). Missing or unreadable files yield nothing.
    fn read_transcript(&self, path: &str, max_messages: usize) -> Result<Vec<MemoryMessage>> {
        crate::transcript::parse_transcript(path, max_messages)
    }
}

/// Every registered host, in `--host` help order.
pub fn all() -> &'static [&'static dyn HostAdapter] {
    &[&workbuddy::WorkBuddy, &codex::Codex]
}

/// The adapter registered under `id`.
pub fn by_id(id: &str) -> Option<&'static dyn HostAdapter> {
    all().iter().copied().find(|host| host.id() == id)
}

/// Registered host ids.
pub fn ids() -> Vec<&'static str> {
    all().iter().map(|host| host.id()).collect()
}

/// Claude Code event names, which WorkBuddy/CodeBuddy, Codex and the generic
/// `pd-memory hook` share.
pub fn claude_code_event(input: &HookInput) -> CanonicalEvent {
    match input.event() {
        "SessionStart" => CanonicalEvent::Start,
        "UserPromptSubmit" => CanonicalEvent::Recall {
            prompt: input.prompt.clone().unwrap_or_default(),
        },
        "Stop" => CanonicalEvent::Capture,
        "SessionEnd" => CanonicalEvent::End,
        "PreCompact" => CanonicalEvent::Compact,
        _ => CanonicalEvent::Ignore,
    }
}

/// Claude-Code-shaped output: `{"continue":true}` plus `hookSpecificOutput`
/// only on events listed in `caps.inject_on`.
pub fn render_claude_code(
    caps: HostCaps,
    event: &CanonicalEvent,
    mut outcome: HookOutput,
) -> String {
    if !caps.injects_on(event.kind()) {
        outcome.hook_specific_output = None;
    }
    let mut line = crate::render_output(&outcome);
    line.pop();
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_ids_are_unique_and_resolvable() {
        let ids = ids();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len());
        for id in ids {
            assert_eq!(by_id(id).map(|host| host.id()), Some(id));
            assert!(id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        }
        assert!(by_id("unknown").is_none());
    }

    #[test]
    fn end_budget_drives_lock_wait_and_flush() {
        let short = HostCaps {
            end_budget_ms: 3_000,
            has_transcript: true,
            inject_on: &[],
        };
        assert_eq!(short.lock_wait_ms(EventKind::End), 1_500);
        assert_eq!(short.lock_wait_ms(EventKind::Recall), 120_000);
        assert!(!short.end_can_flush());
        let long = HostCaps {
            end_budget_ms: 300_000,
            ..short
        };
        assert_eq!(long.lock_wait_ms(EventKind::End), 120_000);
        assert!(long.end_can_flush());
    }

    #[test]
    fn claude_code_names_map_to_canonical_events() {
        let input = |event: &str| HookInput {
            hook_event_name: format!(" {event} "),
            prompt: Some("hi".into()),
            ..Default::default()
        };
        assert_eq!(
            claude_code_event(&input("SessionStart")),
            CanonicalEvent::Start
        );
        assert_eq!(
            claude_code_event(&input("UserPromptSubmit")),
            CanonicalEvent::Recall {
                prompt: "hi".into()
            }
        );
        assert_eq!(claude_code_event(&input("Stop")), CanonicalEvent::Capture);
        assert_eq!(claude_code_event(&input("SessionEnd")), CanonicalEvent::End);
        assert_eq!(
            claude_code_event(&input("PreCompact")),
            CanonicalEvent::Compact
        );
        for other in ["", "PostToolUse", "sessionstart", "Notification"] {
            assert_eq!(claude_code_event(&input(other)), CanonicalEvent::Ignore);
        }
    }
}
