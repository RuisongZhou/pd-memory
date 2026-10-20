//! OpenAI Codex CLI / App (0.154 hooks contract), `hook --host codex`.
//!
//! Claude Code event names. Differences that matter to policy:
//! - every input field is sent, and `transcript_path` may be JSON `null`;
//! - output schemas use `additionalProperties: false` and only `SessionStart`
//!   / `UserPromptSubmit` define `hookSpecificOutput`, so every other event
//!   renders exactly `{"continue":true}`;
//! - `SessionEnd` is killed after 1 s by default and 3 s at most, so it only
//!   captures; the next session's first recall flushes the backlog.
//!
//! Transcripts are rollout files (`transcript/codex.rs`).

use super::{claude_code_event, CanonicalEvent, EventKind, HostAdapter, HostCaps};
use crate::HookInput;

/// Codex adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct Codex;

impl HostAdapter for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn label(&self) -> &'static str {
        "Codex"
    }

    fn caps(&self) -> HostCaps {
        HostCaps {
            end_budget_ms: 3_000,
            has_transcript: true,
            inject_on: &[EventKind::Start, EventKind::Recall],
        }
    }

    fn normalize(&self, input: &HookInput) -> CanonicalEvent {
        claude_code_event(input)
    }
}
