//! Table-driven contract test over every registered host adapter.
//!
//! Each host needs `tests/fixtures/<id>/`:
//! - `caps.json`: the declared [`HostCaps`](pd_memory_hooks::HostCaps), so a
//!   capability change is a reviewed fixture change;
//! - `transcript.jsonl` + `transcript.expected.json`: a real-shaped transcript
//!   and the user/assistant messages its reader must return;
//! - `cases.json`: ordered hook invocations run through the full policy (lock,
//!   session marker, caps) against a recording fake engine. Each case gives
//!   the stdin payload (`"$TRANSCRIPT"` is replaced with the fixture path), the
//!   canonical event, the backlog before the call (`pending`), the fake recall
//!   result (`recall`), the exact stdout JSON and the engine calls in order.
//!
//! Adding a host without fixtures fails this test.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};

use pd_memory_core::error::Result;
use pd_memory_core::types::{
    CaptureTurnResult, HeartbeatStats, MaintenanceResult, MemoryMessage, MemoryOverview,
};
use pd_memory_hooks::hosts::{self, CanonicalEvent, EventKind, HostAdapter};
use pd_memory_hooks::policy::Session;
use pd_memory_hooks::{HookEngine, HookInput, HookOutput};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Case {
    name: String,
    input: Value,
    event: String,
    #[serde(default)]
    pending: Option<usize>,
    #[serde(default)]
    recall: Option<String>,
    output: Value,
    calls: Vec<String>,
}

#[derive(Deserialize, PartialEq, Debug)]
struct Message {
    role: String,
    content: String,
}

/// Records every engine call; `flush` clears the backlog.
#[derive(Default)]
struct Recorder {
    calls: RefCell<Vec<String>>,
    pending: Cell<usize>,
    recall: RefCell<Option<String>>,
}

impl HookEngine for Recorder {
    fn overview(&self) -> Result<MemoryOverview> {
        self.calls.borrow_mut().push("overview".into());
        Ok(MemoryOverview {
            total_files: 2,
            project_memories: 1,
            feedback_memories: 1,
            pending_dialogue_turns: self.pending.get(),
            ..Default::default()
        })
    }

    fn retrieve_context(&self, query: &str, recent: Vec<MemoryMessage>) -> Result<Option<String>> {
        self.calls
            .borrow_mut()
            .push(format!("retrieve:{query}:{}", recent.len()));
        Ok(self.recall.borrow().clone())
    }

    fn capture_turn(
        &self,
        messages: &[MemoryMessage],
        session_key: &str,
        source: &str,
    ) -> Result<CaptureTurnResult> {
        self.calls
            .borrow_mut()
            .push(format!("capture:{}:{session_key}:{source}", messages.len()));
        Ok(CaptureTurnResult {
            captured: true,
            normalized_messages: messages.to_vec(),
            session_key: session_key.to_string(),
        })
    }

    fn flush(&self, reason: &str) -> Result<HeartbeatStats> {
        self.calls.borrow_mut().push(format!("flush:{reason}"));
        self.pending.set(0);
        Ok(HeartbeatStats::default())
    }

    fn run_due_scheduled_maintenance(&self, reason: &str) -> Result<MaintenanceResult> {
        self.calls
            .borrow_mut()
            .push(format!("maintenance:{reason}"));
        Ok(MaintenanceResult::default())
    }
}

fn fixtures(host: &dyn HostAdapter) -> PathBuf {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(host.id());
    assert!(
        directory.is_dir(),
        "host {} has no fixtures at {}",
        host.id(),
        directory.display()
    );
    directory
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> T {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn substitute(value: &mut Value, transcript: &str) {
    match value {
        Value::String(text) if text == "$TRANSCRIPT" => *text = transcript.to_string(),
        Value::Object(map) => map.values_mut().for_each(|v| substitute(v, transcript)),
        Value::Array(items) => items.iter_mut().for_each(|v| substitute(v, transcript)),
        _ => {}
    }
}

#[test]
fn every_host_reads_its_transcript_fixture() {
    for host in hosts::all() {
        let directory = fixtures(*host);
        let transcript = directory.join("transcript.jsonl");
        let expected: Vec<Message> = read_json(&directory.join("transcript.expected.json"));
        let messages: Vec<Message> = host
            .read_transcript(transcript.to_str().unwrap(), 200)
            .unwrap()
            .into_iter()
            .map(|m| Message {
                role: m.role,
                content: m.content,
            })
            .collect();
        assert_eq!(messages, expected, "{}", host.id());
        if host.caps().has_transcript {
            assert!(
                !expected.is_empty(),
                "{}: empty transcript fixture",
                host.id()
            );
        }
    }
}

#[test]
fn every_host_passes_its_lifecycle_cases() {
    for host in hosts::all() {
        let directory = fixtures(*host);
        let transcript = directory.join("transcript.jsonl");
        let cases: Vec<Case> = read_json(&directory.join("cases.json"));
        assert!(!cases.is_empty(), "{}: no cases", host.id());
        let control = tempfile::tempdir().unwrap();
        let engine = Recorder::default();
        for case in cases {
            let label = format!("{} / {}", host.id(), case.name);
            let mut payload = case.input.clone();
            substitute(&mut payload, transcript.to_str().unwrap());
            let input: HookInput = pd_memory_hooks::parse_input(&payload.to_string());
            assert_eq!(
                format!("{:?}", host.normalize(&input).kind()),
                case.event,
                "{label}"
            );
            if let Some(pending) = case.pending {
                engine.pending.set(pending);
            }
            *engine.recall.borrow_mut() = case.recall.clone();
            engine.calls.borrow_mut().clear();

            let session = Session::lock_in(*host, control.path(), &input).expect(&label);
            let rendered = session.dispatch_with(&engine, 8_000, &input);
            drop(session);

            assert!(!rendered.contains('\n'), "{label}: one line");
            let output: Value = serde_json::from_str(&rendered).expect(&label);
            assert_eq!(output, case.output, "{label}");
            assert_eq!(*engine.calls.borrow(), case.calls, "{label}");
        }
    }
}

#[test]
fn every_host_render_enforces_inject_on() {
    for host in hosts::all() {
        let caps = host.caps();
        assert!(
            caps.injects_on(EventKind::Recall),
            "{}: recall must inject",
            host.id()
        );
        let events = [
            CanonicalEvent::Start,
            CanonicalEvent::Recall { prompt: "p".into() },
            CanonicalEvent::Capture,
            CanonicalEvent::End,
            CanonicalEvent::Compact,
            CanonicalEvent::Ignore,
        ];
        for event in events {
            let kind = event.kind();
            let output = HookOutput::with_context("AnyEvent", "context");
            let rendered: Value = serde_json::from_str(&host.render(&event, output)).unwrap();
            assert_eq!(rendered["continue"], true, "{} {kind:?}", host.id());
            assert_eq!(
                rendered.get("hookSpecificOutput").is_some(),
                caps.injects_on(kind),
                "{} {kind:?}",
                host.id()
            );
            if !caps.injects_on(kind) {
                assert_eq!(rendered, serde_json::json!({"continue": true}));
            }
        }
        let continue_only = host.render(&CanonicalEvent::Ignore, HookOutput::default());
        assert_eq!(continue_only, r#"{"continue":true}"#, "{}", host.id());
    }
}

#[derive(Deserialize, PartialEq, Debug)]
struct Caps {
    end_budget_ms: u64,
    has_transcript: bool,
    inject_on: Vec<String>,
}

#[test]
fn every_host_declares_its_caps_fixture() {
    for host in hosts::all() {
        let caps = host.caps();
        let declared = Caps {
            end_budget_ms: caps.end_budget_ms,
            has_transcript: caps.has_transcript,
            inject_on: caps.inject_on.iter().map(|k| format!("{k:?}")).collect(),
        };
        let expected: Caps = read_json(&fixtures(*host).join("caps.json"));
        assert_eq!(declared, expected, "{}", host.id());
    }
}

#[test]
fn every_host_lock_fits_its_session_end_budget() {
    for host in hosts::all() {
        let caps = host.caps();
        assert!(caps.end_budget_ms > 0, "{}", host.id());
        assert!(
            caps.lock_wait_ms(EventKind::End) <= caps.end_budget_ms / 2,
            "{}",
            host.id()
        );
    }
}
