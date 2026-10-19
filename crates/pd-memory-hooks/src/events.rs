//! Event dispatch and engine port for hook events.
//!
//! Behavior by event (all best effort; a hook must never break the host):
//!
//! | Event | Behavior |
//! |---|---|
//! | `SessionStart` | Render an overview line (file counts + project meta) into `additionalContext`; no model call. |
//! | `UserPromptSubmit` | `retrieve_context(prompt, recent transcript messages)` -> `additionalContext`, truncated to `hooks.max_context_chars`; empty result => `{"continue":true}`. |
//! | `Stop` / `SessionEnd` | Parse the transcript, `capture_turn(session_id, messages)`, then run due scheduled maintenance. |
//! | `PreCompact` | Flush pending indexing batches. |
//! | `PostToolUse` / everything else | No-op (`{"continue":true}`). |

use std::path::Path;

use pd_memory_core::engine::{FlushOptions, MemoryEngine};
use pd_memory_core::error::Result;
use pd_memory_core::recall::RetrieveOptions;
use pd_memory_core::types::{
    CaptureTurnResult, HeartbeatStats, MaintenanceResult, MemoryMessage, MemoryOverview,
};

use crate::hosts::CanonicalEvent;
use crate::transcript::{self, DEFAULT_TRANSCRIPT_MESSAGES};
use crate::{HookInput, HookOutput};

/// Recent messages attached to `UserPromptSubmit` recall (assistant + user).
pub const RECENT_MESSAGES_FOR_RECALL: usize = 8;

/// Engine operations used by hook dispatch.
///
/// Implemented for the real engine through [`EngineHooks`]; tests inject a
/// deterministic fake so hook behavior is verified without storage.
pub trait HookEngine {
    /// Memory overview payload (no model call).
    fn overview(&self) -> Result<MemoryOverview>;
    /// Recall context for a prompt; `Ok(None)` means nothing relevant.
    fn retrieve_context(&self, query: &str, recent: Vec<MemoryMessage>) -> Result<Option<String>>;
    /// Queue a captured turn.
    fn capture_turn(
        &self,
        messages: &[MemoryMessage],
        session_key: &str,
        source: &str,
    ) -> Result<CaptureTurnResult>;
    /// Flush pending indexing work.
    fn flush(&self, reason: &str) -> Result<HeartbeatStats>;
    /// Run whatever scheduled maintenance is due.
    fn run_due_scheduled_maintenance(&self, reason: &str) -> Result<MaintenanceResult>;
}

/// [`HookEngine`] backed by the real [`MemoryEngine`].
pub(crate) struct EngineHooks<'a> {
    engine: &'a MemoryEngine,
}

impl<'a> EngineHooks<'a> {
    pub(crate) fn new(engine: &'a MemoryEngine) -> Self {
        Self { engine }
    }
}

impl HookEngine for EngineHooks<'_> {
    fn overview(&self) -> Result<MemoryOverview> {
        self.engine.overview()
    }

    fn retrieve_context(&self, query: &str, recent: Vec<MemoryMessage>) -> Result<Option<String>> {
        let result = self.engine.retrieve_context(
            query,
            RetrieveOptions {
                recent_messages: recent,
                workspace_hint: None,
                explicit: false,
            },
        )?;
        Ok(result.system_context.or(result.context))
    }

    fn capture_turn(
        &self,
        messages: &[MemoryMessage],
        session_key: &str,
        source: &str,
    ) -> Result<CaptureTurnResult> {
        self.engine
            .capture_turn(messages, session_key, None, source)
    }

    fn flush(&self, reason: &str) -> Result<HeartbeatStats> {
        self.engine.flush(FlushOptions {
            batch_size: None,
            session_keys: None,
            reason: reason.to_string(),
        })
    }

    fn run_due_scheduled_maintenance(&self, reason: &str) -> Result<MaintenanceResult> {
        self.engine.run_due_scheduled_maintenance(reason)
    }
}

/// Reads recent transcript messages: `(transcript_path, max_messages)`.
pub type TranscriptReader<'a> = &'a dyn Fn(&str, usize) -> Result<Vec<MemoryMessage>>;

/// Dispatch one hook input against an injectable [`HookEngine`].
///
/// Uses Claude Code event names and the auto-detecting transcript reader.
/// Individual event failures are logged and downgraded to `{"continue":true}`;
/// the `Result` exists only so future versions can report fatal protocol errors.
pub fn dispatch_with(
    hooks: &dyn HookEngine,
    max_context_chars: usize,
    input: &HookInput,
) -> Result<HookOutput> {
    let event = crate::hosts::claude_code_event(input);
    Ok(handle_with(
        hooks,
        max_context_chars,
        input,
        &event,
        &transcript::parse_transcript,
    ))
}

/// Run the generic behavior for one canonical event.
///
/// Context is labeled with the host's own event name (`hookEventName`), and
/// capture sources / maintenance reasons use it too (`hook:<event>`).
pub fn handle_with(
    hooks: &dyn HookEngine,
    max_context_chars: usize,
    input: &HookInput,
    event: &CanonicalEvent,
    read: TranscriptReader<'_>,
) -> HookOutput {
    let name = input.event();
    match event {
        CanonicalEvent::Start => session_start(hooks, max_context_chars, name),
        CanonicalEvent::Recall { prompt } => {
            user_prompt_submit(hooks, max_context_chars, input, name, prompt, read)
        }
        CanonicalEvent::Capture | CanonicalEvent::End => {
            capture_and_maintain(hooks, input, name, read)
        }
        CanonicalEvent::Compact => pre_compact(hooks, name),
        CanonicalEvent::Ignore => {
            match name {
                // Tool results are noisy for memory extraction; capture from
                // `Stop`/`SessionEnd` transcript parsing instead.
                "PostToolUse" => log::debug!("PostToolUse: no pd-memory behavior"),
                "" => log::debug!("hook input has no hook_event_name; no-op"),
                other => log::debug!("hook event {other} has no pd-memory behavior; no-op"),
            }
            HookOutput::continue_only()
        }
    }
}

fn session_start(hooks: &dyn HookEngine, max_context_chars: usize, event: &str) -> HookOutput {
    match hooks.overview() {
        Ok(overview) => {
            let rendered = render_overview_context(&overview);
            if rendered.trim().is_empty() {
                return HookOutput::continue_only();
            }
            let context = truncate_chars(&rendered, max_context_chars);
            if context.is_empty() {
                HookOutput::continue_only()
            } else {
                HookOutput::with_context(event, context)
            }
        }
        Err(error) => {
            log::warn!("{event} overview failed (ignored): {error}");
            HookOutput::continue_only()
        }
    }
}

fn user_prompt_submit(
    hooks: &dyn HookEngine,
    max_context_chars: usize,
    input: &HookInput,
    event: &str,
    prompt: &str,
    read: TranscriptReader<'_>,
) -> HookOutput {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        log::debug!("{event} without a prompt; skipping recall");
        return HookOutput::continue_only();
    }
    let recent = read(&input.transcript_path, RECENT_MESSAGES_FOR_RECALL).unwrap_or_default();
    match hooks.retrieve_context(prompt, recent) {
        Ok(Some(context)) => {
            let context = truncate_chars(&context, max_context_chars);
            if context.trim().is_empty() {
                HookOutput::continue_only()
            } else {
                HookOutput::with_context(event, context)
            }
        }
        Ok(None) => HookOutput::continue_only(),
        Err(error) => {
            log::warn!("{event} recall failed (ignored): {error}");
            HookOutput::continue_only()
        }
    }
}

fn capture_and_maintain(
    hooks: &dyn HookEngine,
    input: &HookInput,
    event: &str,
    read: TranscriptReader<'_>,
) -> HookOutput {
    capture_transcript(hooks, input, event, read);
    // Heartbeat / dream scheduling is best effort: a failure here must not fail
    // the host's turn.
    if let Err(error) = hooks.run_due_scheduled_maintenance(&format!("hook:{event}")) {
        log::warn!("{event}: scheduled maintenance failed (ignored): {error}");
    }
    HookOutput::continue_only()
}

/// Capture the transcript (or inline final reply) without scheduled
/// maintenance, for hosts that give an event too little time for model calls
/// (Codex caps `SessionEnd` at 3 s).
pub fn capture_only_with(hooks: &dyn HookEngine, input: &HookInput) -> HookOutput {
    capture_only_reading(hooks, input, &transcript::parse_transcript)
}

/// [`capture_only_with`] with an explicit transcript reader.
pub fn capture_only_reading(
    hooks: &dyn HookEngine,
    input: &HookInput,
    read: TranscriptReader<'_>,
) -> HookOutput {
    capture_transcript(hooks, input, input.event(), read);
    HookOutput::continue_only()
}

fn capture_transcript(
    hooks: &dyn HookEngine,
    input: &HookInput,
    event: &str,
    read: TranscriptReader<'_>,
) -> bool {
    let mut captured = false;
    if !input.transcript_path.trim().is_empty() {
        let mut messages =
            read(&input.transcript_path, DEFAULT_TRANSCRIPT_MESSAGES).unwrap_or_default();
        // WorkBuddy/CodeBuddy flush the final assistant transcript line *after*
        // the Stop hook fires, so the transcript seen by the hook may end with
        // a user turn. The host inlines the final reply as
        // `last_assistant_message`; append it as a synthetic assistant message
        // so indexing does not see an unanswered user turn (which it skips).
        if let Some(last_assistant) = input.last_assistant_message() {
            if needs_synthetic_assistant_reply(&messages, last_assistant) {
                log::debug!(
                    "{event}: transcript ends with an unanswered user turn; appending \
                     last_assistant_message from the hook payload"
                );
                messages.push(MemoryMessage::assistant(last_assistant));
            }
        }
        captured = capture_messages(hooks, input, event, messages);
    }
    if !captured {
        // Fallback for hosts that include the final reply inline but no usable
        // transcript path.
        if let Some(last) = input.last_assistant_message() {
            let message = MemoryMessage::assistant(last);
            captured = capture_messages(hooks, input, event, vec![message]);
        }
    }
    if !captured {
        log::debug!("{event}: no transcript messages available; nothing captured");
    }
    captured
}

fn capture_messages(
    hooks: &dyn HookEngine,
    input: &HookInput,
    event: &str,
    messages: Vec<MemoryMessage>,
) -> bool {
    if messages.is_empty() {
        return false;
    }
    let session_key = derive_session_key(input);
    let source = format!("hook:{event}");
    match hooks.capture_turn(&messages, &session_key, &source) {
        Ok(result) => {
            log::debug!(
                "{event}: captured={} normalized_messages={} session={}",
                result.captured,
                result.normalized_messages.len(),
                result.session_key
            );
            true
        }
        Err(error) => {
            log::warn!("{event}: capture failed (ignored): {error}");
            false
        }
    }
}

fn pre_compact(hooks: &dyn HookEngine, event: &str) -> HookOutput {
    if let Err(error) = hooks.flush(&format!("hook:{event}")) {
        log::warn!("{event} flush failed (ignored): {error}");
    }
    HookOutput::continue_only()
}

/// Whether `last_assistant_message` should be appended to the transcript.
///
/// True when the transcript's final turn is a user message with no assistant
/// reply after it (the host flushed the transcript late), unless that exact
/// assistant reply is already present.
fn needs_synthetic_assistant_reply(
    messages: &[MemoryMessage],
    last_assistant_message: &str,
) -> bool {
    let last = last_assistant_message.trim();
    if last.is_empty() {
        return false;
    }
    if messages
        .iter()
        .any(|message| message.role == "assistant" && message.content.trim() == last)
    {
        // The real assistant line is already in the transcript.
        return false;
    }
    let Some(last_user_index) = messages.iter().rposition(|message| message.role == "user") else {
        // No user turn: the plain inline fallback handles this case.
        return false;
    };
    !messages[last_user_index + 1..]
        .iter()
        .any(|message| message.role == "assistant")
}

/// Stable session key for capture: `session_id`, else the transcript file stem.
pub fn derive_session_key(input: &HookInput) -> String {
    let session_id = input.session_id.trim();
    if !session_id.is_empty() {
        return session_id.to_string();
    }
    let from_transcript = Path::new(input.transcript_path.trim())
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::trim)
        .filter(|stem| !stem.is_empty());
    match from_transcript {
        Some(stem) => stem.to_string(),
        None => "hook-session".to_string(),
    }
}

/// Derive a stable workspace id from the hook payload.
///
/// WorkBuddy/CodeBuddy store transcripts at
/// `<configDir>/projects/<encoded-cwd>/<sessionId>.jsonl`, so the transcript's
/// parent directory name is a stable per-workspace key even when `cwd` is
/// absent (e.g. `SessionStart`). Returns `None` when no transcript path is set.
pub fn derive_workspace_id(input: &HookInput) -> Option<String> {
    let path = Path::new(input.transcript_path.trim());
    let directory = path.parent()?;
    if directory.as_os_str().is_empty() {
        return None;
    }
    let name = directory.file_name()?.to_str()?.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Render the `SessionStart` overview block. Empty when there is no memory yet.
pub fn render_overview_context(overview: &MemoryOverview) -> String {
    let has_memory = overview.total_files > 0
        || overview.pending_sessions > 0
        || overview.pending_dialogue_turns > 0
        || overview.project_meta.is_some()
        || overview.last_indexed_at.is_some();
    if !has_memory {
        return String::new();
    }
    let mut lines = vec!["<pd-memory>".to_string()];
    lines.push(format!(
        "Long-term memory: {} file(s) — {} project, {} feedback, {} user profile(s).",
        overview.total_files,
        overview.project_memories,
        overview.feedback_memories,
        overview.user_profiles
    ));
    if overview.pending_dialogue_turns > 0 || overview.pending_sessions > 0 {
        lines.push(format!(
            "Pending indexing backlog: {} turn(s) across {} session(s).",
            overview.pending_dialogue_turns, overview.pending_sessions
        ));
    }
    if let Some(meta) = &overview.project_meta {
        let description = meta.description.trim();
        if description.is_empty() {
            lines.push(format!(
                "Project: {} (status: {}).",
                meta.project_name.trim(),
                meta.status.trim()
            ));
        } else {
            lines.push(format!(
                "Project: {} — {} (status: {}).",
                meta.project_name.trim(),
                description,
                meta.status.trim()
            ));
        }
    }
    if let Some(last_dream) = &overview.last_dream_at {
        lines.push(format!("Last consolidation: {last_dream}."));
    }
    lines.push(
        "Use memory_search when durable preferences, rules, or project facts are relevant."
            .to_string(),
    );
    lines.push("</pd-memory>".to_string());
    lines.join("\n")
}

/// Truncate by characters without splitting UTF-8; never exceeds `max_chars`.
pub(crate) fn truncate_chars(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    text.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use pd_memory_core::types::*;

    #[derive(Default)]
    struct FakeHooks {
        calls: Mutex<Vec<String>>,
        overview: Mutex<Option<MemoryOverview>>,
        overview_fails: bool,
        context: Mutex<Option<String>>,
        retrieve_fails: bool,
        flush_fails: bool,
        capture_fails: bool,
        maintenance_fails: bool,
    }

    impl FakeHooks {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn record(&self, call: impl Into<String>) {
            self.calls.lock().unwrap().push(call.into());
        }
    }

    impl HookEngine for FakeHooks {
        fn overview(&self) -> Result<MemoryOverview> {
            self.record("overview");
            if self.overview_fails {
                return Err(pd_memory_core::error::MemoryError::Other(
                    "overview exploded".into(),
                ));
            }
            Ok(self.overview.lock().unwrap().clone().unwrap_or_default())
        }

        fn retrieve_context(
            &self,
            query: &str,
            recent: Vec<MemoryMessage>,
        ) -> Result<Option<String>> {
            self.record(format!("retrieve:{query}:{}", recent.len()));
            if self.retrieve_fails {
                return Err(pd_memory_core::error::MemoryError::Other(
                    "retrieve exploded".into(),
                ));
            }
            Ok(self.context.lock().unwrap().clone())
        }

        fn capture_turn(
            &self,
            messages: &[MemoryMessage],
            session_key: &str,
            source: &str,
        ) -> Result<CaptureTurnResult> {
            self.record(format!("capture:{}:{session_key}:{source}", messages.len()));
            if self.capture_fails {
                return Err(pd_memory_core::error::MemoryError::Other(
                    "capture exploded".into(),
                ));
            }
            Ok(CaptureTurnResult {
                captured: true,
                normalized_messages: messages.to_vec(),
                session_key: session_key.to_string(),
            })
        }

        fn flush(&self, reason: &str) -> Result<HeartbeatStats> {
            self.record(format!("flush:{reason}"));
            if self.flush_fails {
                return Err(pd_memory_core::error::MemoryError::Other(
                    "flush exploded".into(),
                ));
            }
            Ok(HeartbeatStats::default())
        }

        fn run_due_scheduled_maintenance(&self, reason: &str) -> Result<MaintenanceResult> {
            self.record(format!("maintenance:{reason}"));
            if self.maintenance_fails {
                return Err(pd_memory_core::error::MemoryError::Other(
                    "maintenance exploded".into(),
                ));
            }
            Ok(MaintenanceResult::default())
        }
    }

    fn input(event: &str, extra: serde_json::Value) -> HookInput {
        let mut base = serde_json::json!({
            "hook_event_name": event,
            "session_id": "session-1",
            "transcript_path": "",
            "cwd": "/tmp/project",
        });
        if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                base.insert(key.clone(), value.clone());
            }
        }
        serde_json::from_value(base).expect("hook input")
    }

    fn overview_with_memory() -> MemoryOverview {
        MemoryOverview {
            total_files: 3,
            project_memories: 2,
            feedback_memories: 1,
            user_profiles: 0,
            pending_dialogue_turns: 4,
            pending_sessions: 1,
            project_meta: Some(ProjectMetaRecord {
                project_name: "Boreal".into(),
                description: "local knowledge base".into(),
                status: "in_progress".into(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn session_start_emits_overview_without_model_call() {
        let hooks = FakeHooks {
            overview: Mutex::new(Some(overview_with_memory())),
            ..Default::default()
        };
        let output = dispatch_with(&hooks, 8_000, &input("SessionStart", serde_json::json!({})))
            .expect("dispatch never fails");
        assert!(output.r#continue);
        let context = output
            .hook_specific_output
            .as_ref()
            .and_then(|specific| specific.additional_context.as_deref())
            .expect("additional context");
        assert!(context.contains("3 file(s)"));
        assert!(context.contains("Boreal"));
        assert!(context.contains("<pd-memory>"));
        // No model-backed operation may run for SessionStart.
        assert_eq!(hooks.calls(), vec!["overview".to_string()]);
    }

    #[test]
    fn session_start_without_memory_is_a_noop() {
        let hooks = FakeHooks::default();
        let output =
            dispatch_with(&hooks, 8_000, &input("SessionStart", serde_json::json!({}))).unwrap();
        assert!(output.r#continue);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn session_start_errors_are_downgraded() {
        let hooks = FakeHooks {
            overview_fails: true,
            ..Default::default()
        };
        let output =
            dispatch_with(&hooks, 8_000, &input("SessionStart", serde_json::json!({}))).unwrap();
        assert!(output.r#continue);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn user_prompt_submit_injects_recall_context_truncated() {
        let hooks = FakeHooks {
            context: Mutex::new(Some("x".repeat(100))),
            ..Default::default()
        };
        let output = dispatch_with(
            &hooks,
            40,
            &input(
                "UserPromptSubmit",
                serde_json::json!({"prompt": "what are my deployment rules?"}),
            ),
        )
        .unwrap();
        let context = output
            .hook_specific_output
            .as_ref()
            .and_then(|specific| specific.additional_context.clone())
            .expect("context");
        assert_eq!(context.chars().count(), 40);
        assert_eq!(
            hooks.calls(),
            vec!["retrieve:what are my deployment rules?:0".to_string()]
        );
    }

    #[test]
    fn user_prompt_submit_without_recall_is_continue_only() {
        let hooks = FakeHooks::default();
        let output = dispatch_with(
            &hooks,
            8_000,
            &input("UserPromptSubmit", serde_json::json!({"prompt": "hello"})),
        )
        .unwrap();
        assert!(output.r#continue);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn user_prompt_submit_without_prompt_is_a_noop() {
        let hooks = FakeHooks::default();
        let output = dispatch_with(
            &hooks,
            8_000,
            &input("UserPromptSubmit", serde_json::json!({})),
        )
        .unwrap();
        assert!(output.hook_specific_output.is_none());
        assert!(hooks.calls().is_empty());
    }

    #[test]
    fn user_prompt_submit_recall_errors_are_downgraded() {
        let hooks = FakeHooks {
            retrieve_fails: true,
            ..Default::default()
        };
        let output = dispatch_with(
            &hooks,
            8_000,
            &input("UserPromptSubmit", serde_json::json!({"prompt": "hello"})),
        )
        .unwrap();
        assert!(output.r#continue);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn stop_captures_transcript_then_runs_maintenance() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = dir.path().join("session-1.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"this project is called Boreal"}]}"#,
                "\n",
                r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"noted"}]}"#,
                "\n",
            ),
        )
        .expect("transcript");
        let hooks = FakeHooks::default();
        let output = dispatch_with(
            &hooks,
            8_000,
            &input(
                "Stop",
                serde_json::json!({"transcript_path": transcript.to_str().unwrap()}),
            ),
        )
        .unwrap();
        assert!(output.r#continue);
        assert!(output.hook_specific_output.is_none());
        let calls = hooks.calls();
        assert_eq!(calls[0], "capture:2:session-1:hook:Stop");
        assert_eq!(calls[1], "maintenance:hook:Stop");
    }

    #[test]
    fn stop_falls_back_to_last_assistant_message() {
        let hooks = FakeHooks::default();
        let output = dispatch_with(
            &hooks,
            8_000,
            &input(
                "Stop",
                serde_json::json!({"last_assistant_message": "final reply text"}),
            ),
        )
        .unwrap();
        assert!(output.r#continue);
        let calls = hooks.calls();
        assert_eq!(calls[0], "capture:1:session-1:hook:Stop");
        assert_eq!(calls[1], "maintenance:hook:Stop");
    }

    #[test]
    fn stop_errors_never_fail_the_hook() {
        let hooks = FakeHooks {
            capture_fails: true,
            maintenance_fails: true,
            ..Default::default()
        };
        let output = dispatch_with(
            &hooks,
            8_000,
            &input(
                "Stop",
                serde_json::json!({"last_assistant_message": "reply"}),
            ),
        )
        .unwrap();
        assert!(output.r#continue);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn capture_only_skips_maintenance() {
        let hooks = FakeHooks::default();
        let output = capture_only_with(
            &hooks,
            &input(
                "SessionEnd",
                serde_json::json!({"last_assistant_message": "final reply"}),
            ),
        );
        assert!(output.r#continue);
        assert!(output.hook_specific_output.is_none());
        assert_eq!(
            hooks.calls(),
            vec!["capture:1:session-1:hook:SessionEnd".to_string()]
        );
    }

    #[test]
    fn pre_compact_flushes() {
        let hooks = FakeHooks::default();
        let output =
            dispatch_with(&hooks, 8_000, &input("PreCompact", serde_json::json!({}))).unwrap();
        assert!(output.r#continue);
        assert_eq!(hooks.calls(), vec!["flush:hook:PreCompact".to_string()]);
    }

    #[test]
    fn pre_compact_errors_are_downgraded() {
        let hooks = FakeHooks {
            flush_fails: true,
            ..Default::default()
        };
        let output =
            dispatch_with(&hooks, 8_000, &input("PreCompact", serde_json::json!({}))).unwrap();
        assert!(output.r#continue);
    }

    #[test]
    fn other_events_are_noops() {
        for event in [
            "PostToolUse",
            "PreToolUse",
            "Notification",
            "SubagentStart",
            "",
            "FutureEvent",
        ] {
            let hooks = FakeHooks::default();
            let output =
                dispatch_with(&hooks, 8_000, &input(event, serde_json::json!({}))).unwrap();
            assert!(output.r#continue, "event {event} must continue");
            assert!(output.hook_specific_output.is_none());
            assert!(hooks.calls().is_empty(), "event {event} must be a no-op");
        }
    }

    #[test]
    fn session_key_prefers_session_id_then_transcript_stem() {
        let with_session = input("Stop", serde_json::json!({}));
        assert_eq!(derive_session_key(&with_session), "session-1");

        let mut from_transcript = input(
            "Stop",
            serde_json::json!({"transcript_path": "/tmp/x/abc-def.jsonl"}),
        );
        from_transcript.session_id.clear();
        assert_eq!(derive_session_key(&from_transcript), "abc-def");

        let mut fallback = input("Stop", serde_json::json!({}));
        fallback.session_id.clear();
        assert_eq!(derive_session_key(&fallback), "hook-session");
    }

    #[test]
    fn workspace_id_comes_from_transcript_parent() {
        let payload = input(
            "SessionStart",
            serde_json::json!({
                "transcript_path": "/home/me/.workbuddy/projects/private-tmp-x/session.jsonl"
            }),
        );
        assert_eq!(
            derive_workspace_id(&payload).as_deref(),
            Some("private-tmp-x")
        );
        let no_transcript = input("SessionStart", serde_json::json!({}));
        assert_eq!(derive_workspace_id(&no_transcript), None);
    }

    #[test]
    fn stop_appends_last_assistant_message_when_transcript_lags() {
        // Real WorkBuddy race: the transcript contained only the user message
        // when the Stop hook fired; the final reply is only in the payload.
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = dir.path().join("session-1.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"MOCK_FACT: project is Boreal"}]}"#,
                "\n",
            ),
        )
        .expect("transcript");
        let hooks = FakeHooks::default();
        let output = dispatch_with(
            &hooks,
            8_000,
            &input(
                "Stop",
                serde_json::json!({
                    "transcript_path": transcript.to_str().unwrap(),
                    "last_assistant_message": "MOCK_REPLY: noted, project is Boreal"
                }),
            ),
        )
        .unwrap();
        assert!(output.r#continue);
        let calls = hooks.calls();
        // One real user message + one synthetic assistant reply.
        assert_eq!(calls[0], "capture:2:session-1:hook:Stop");
        assert_eq!(calls[1], "maintenance:hook:Stop");
    }

    #[test]
    fn stop_does_not_duplicate_assistant_already_in_transcript() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = dir.path().join("session-1.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}"#,
                "\n",
                r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"final reply"}]}"#,
                "\n",
            ),
        )
        .expect("transcript");
        let hooks = FakeHooks::default();
        dispatch_with(
            &hooks,
            8_000,
            &input(
                "Stop",
                serde_json::json!({
                    "transcript_path": transcript.to_str().unwrap(),
                    "last_assistant_message": "final reply"
                }),
            ),
        )
        .unwrap();
        assert_eq!(hooks.calls()[0], "capture:2:session-1:hook:Stop");
    }

    #[test]
    fn stop_appends_reply_only_when_final_turn_is_unanswered() {
        // Full transcript already has user + assistant: no synthetic message.
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = dir.path().join("session-1.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"first"}]}"#,
                "\n",
                r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"first reply"}]}"#,
                "\n",
            ),
        )
        .expect("transcript");
        let hooks = FakeHooks::default();
        dispatch_with(
            &hooks,
            8_000,
            &input(
                "Stop",
                serde_json::json!({
                    "transcript_path": transcript.to_str().unwrap(),
                    "last_assistant_message": "first reply"
                }),
            ),
        )
        .unwrap();
        assert_eq!(hooks.calls()[0], "capture:2:session-1:hook:Stop");
    }

    #[test]
    fn dedupe_helper_logic() {
        let user = MemoryMessage::user("hi");
        assert!(!needs_synthetic_assistant_reply(&[], "reply"));
        assert!(needs_synthetic_assistant_reply(
            std::slice::from_ref(&user),
            "reply"
        ));
        let assistant = MemoryMessage::assistant("reply");
        assert!(!needs_synthetic_assistant_reply(
            &[user.clone(), assistant.clone()],
            "reply"
        ));
        assert!(!needs_synthetic_assistant_reply(&[user], ""));
    }

    #[test]
    fn truncate_chars_never_exceeds_limit() {
        assert_eq!(truncate_chars("abcdef", 3), "abc");
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert_eq!(truncate_chars("abc", 0), "");
        assert_eq!(truncate_chars("你好世界", 2), "你好");
    }
}
