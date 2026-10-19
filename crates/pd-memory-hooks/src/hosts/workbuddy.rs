//! WorkBuddy / CodeBuddy (desktop GUI and CLI), `hook --host workbuddy`.
//!
//! Claude Code event names and output. Transcripts are
//! `<configDir>/projects/<encoded-cwd>/<sessionId>.jsonl`
//! (`transcript/workbuddy.rs`). The installer gives `SessionEnd` 300 s,
//! so session end captures and then flushes the backlog.

use super::{claude_code_event, CanonicalEvent, EventKind, HostAdapter, HostCaps};
use crate::HookInput;

/// WorkBuddy / CodeBuddy adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct WorkBuddy;

impl HostAdapter for WorkBuddy {
    fn id(&self) -> &'static str {
        "workbuddy"
    }

    fn label(&self) -> &'static str {
        "WorkBuddy"
    }

    fn caps(&self) -> HostCaps {
        HostCaps {
            end_budget_ms: 300_000,
            has_transcript: true,
            inject_on: &[EventKind::Start, EventKind::Recall],
        }
    }

    fn normalize(&self, input: &HookInput) -> CanonicalEvent {
        claude_code_event(input)
    }
}
