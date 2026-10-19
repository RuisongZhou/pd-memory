//! Offline end-to-end tests for the `pd-memory` binary.
//!
//! Every test spawns the real CLI binary (no network) with a temp data root and
//! `PD_MEMORY_PROVIDER=mock`. Tests that need the storage/pipeline crates skip
//! with a `SKIP:` message while those crates are still being implemented, so
//! this file is a reliable readiness gate as well as a regression suite.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_pd-memory")
}

/// Hermetic CLI harness: temp data root, mock provider, clean PD_MEMORY_* env.
struct Harness {
    dir: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn command(&self) -> Command {
        let mut command = Command::new(binary());
        for variable in [
            "PD_MEMORY_CONFIG",
            "PD_MEMORY_DATA_ROOT",
            "PD_MEMORY_WORKSPACE_ROOT",
            "PD_MEMORY_WORKSPACE_ID",
            "PD_MEMORY_MODEL",
            "PD_MEMORY_API_TYPE",
            "PD_MEMORY_BASE_URL",
            "PD_MEMORY_API_KEY_ENV",
            "CODEBUDDY_PROJECT_DIR",
            "CLAUDE_PROJECT_DIR",
        ] {
            command.env_remove(variable);
        }
        command
            .env("PD_MEMORY_PROVIDER", "mock")
            .env("RUST_LOG", "error")
            .arg("--data-root")
            .arg(self.root())
            .arg("--log-level")
            .arg("error");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().expect("spawn pd-memory")
    }

    fn run_with_stdin(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = self
            .command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn pd-memory");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(stdin.as_bytes())
            .expect("write stdin");
        child.wait_with_output().expect("wait pd-memory")
    }

    fn workbuddy_hook(&self, event: &str, session: &str, extra: serde_json::Value) -> Output {
        let mut input = serde_json::json!({"hook_event_name": event, "session_id": session});
        input
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        self.run_with_stdin(
            &["--workspace", "integration", "hook", "--workbuddy"],
            &input.to_string(),
        )
    }

    fn capture(&self, session: &str, fact: &str) {
        let output = self.run_with_stdin(
            &["--workspace", "integration", "add", "--session", session],
            &serde_json::json!([
                {"role": "user", "content": fact},
                {"role": "assistant", "content": "Noted"}
            ])
            .to_string(),
        );
        assert!(output.status.success(), "{}", stderr_text(&output));
    }

    fn pending(&self) -> u64 {
        stdout_json(&self.run(&["--workspace", "integration", "overview"]))
            ["pending_dialogue_turns"]
            .as_u64()
            .unwrap()
    }
}

#[test]
fn workbuddy_first_recall_flushes_but_generic_hook_does_not() {
    let harness = Harness::new();
    harness.capture(
        "old",
        "Always remember: the Boreal project code is WB-5813.",
    );
    let prompt = serde_json::json!({"prompt": "What is the Boreal project code?"});
    let generic = harness.run_with_stdin(
        &["--workspace", "integration", "hook"],
        &serde_json::json!({"hook_event_name":"UserPromptSubmit", "session_id":"new", "prompt":prompt["prompt"]}).to_string(),
    );
    assert!(generic.status.success());
    assert_eq!(harness.pending(), 1);
    let output = harness.workbuddy_hook("UserPromptSubmit", "new", prompt);
    assert!(output.status.success());
    let response = stdout_json(&output);
    assert!(response["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .contains("WB-5813"));
    assert_eq!(harness.pending(), 0);
}

#[test]
fn workbuddy_checks_once_per_session_regardless_of_start_order() {
    let harness = Harness::new();
    let prompt = serde_json::json!({"prompt":"What are the project rules?"});
    harness.workbuddy_hook("UserPromptSubmit", "a", prompt.clone()); // Empty queue: no flush.
    harness.capture("old", "Always run offline tests before Boreal releases.");
    harness.workbuddy_hook("SessionStart", "a", serde_json::json!({"source":"startup"}));
    harness.workbuddy_hook("UserPromptSubmit", "a", prompt.clone());
    assert_eq!(
        harness.pending(),
        1,
        "late SessionStart must not reset the barrier"
    );
    harness.workbuddy_hook("SessionStart", "b", serde_json::json!({"source":"resume"}));
    assert_eq!(harness.pending(), 1, "SessionStart alone must not flush");
    harness.workbuddy_hook("UserPromptSubmit", "b", prompt.clone());
    assert_eq!(harness.pending(), 0);
    harness.workbuddy_hook("SessionEnd", "b", serde_json::json!({"reason":"clear"}));
    harness.capture("other", "Always use Chinese for Boreal delivery notes.");
    harness.workbuddy_hook("UserPromptSubmit", "b", prompt);
    assert_eq!(harness.pending(), 0);
}

#[test]
fn workbuddy_session_end_captures_and_flushes_without_stop() {
    let harness = Harness::new();
    let transcript = harness.root().join("end.jsonl");
    std::fs::write(&transcript, concat!(
        r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"<system-reminder>host</system-reminder><user_query>Always remember Boreal release code WB-END-7921.</user_query>"}]}"#,
        "\n",
        r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Noted"}]}"#,
        "\n"
    )).unwrap();
    let output = harness.workbuddy_hook(
        "SessionEnd",
        "old",
        serde_json::json!({"transcript_path":transcript,"reason":"clear"}),
    );
    assert!(output.status.success());
    assert_eq!(stdout_json(&output), serde_json::json!({"continue":true}));
    assert_eq!(harness.pending(), 0);
    let result = harness.run(&[
        "--workspace",
        "integration",
        "search",
        "What is the Boreal release code?",
    ]);
    assert!(String::from_utf8_lossy(&result.stdout).contains("WB-END-7921"));
    let queue = std::fs::read_to_string(
        harness
            .root()
            .join("workspaces/integration/control/l0_sessions.jsonl"),
    )
    .unwrap();
    assert!(queue.contains("hook:SessionEnd"));
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            harness
                .root()
                .join("workspaces/integration/control/pipeline_state.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(state["indexTraces"][0]["reason"], "workbuddy:session-end");
}

#[test]
fn workbuddy_first_recall_drains_more_than_one_index_batch() {
    let harness = Harness::new();
    let config = harness.root().join("config.json");
    std::fs::write(
        &config,
        serde_json::json!({"data_root":harness.root(), "heartbeat_batch_size":1}).to_string(),
    )
    .unwrap();
    for id in ["one", "two", "three"] {
        harness.capture(
            id,
            &format!("Always remember the Boreal {id} project rule."),
        );
    }
    let output = harness.run_with_stdin(
        &["--config", config.to_str().unwrap(), "--workspace", "integration", "hook", "--workbuddy"],
        &serde_json::json!({"hook_event_name":"UserPromptSubmit","session_id":"new", "prompt":"What are the Boreal rules?"}).to_string(),
    );
    assert!(output.status.success());
    assert_eq!(harness.pending(), 0);
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            harness
                .root()
                .join("workspaces/integration/control/pipeline_state.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(state["indexTraces"].as_array().unwrap().len(), 3);
}

#[test]
fn workbuddy_failed_hook_keeps_backlog_and_retries_next_prompt() {
    let harness = Harness::new();
    let fact = "Always remember the Boreal release code WB-RETRY-5891.";
    harness.capture("old", fact);
    let path = harness
        .root()
        .join("workspaces/integration/control/workbuddy/hooks.lock");
    std::fs::create_dir_all(&path).unwrap(); // Deterministic lock failure, even as root.
    let prompt = serde_json::json!({"prompt":"What is the Boreal release code?"});
    let failed = harness.workbuddy_hook("UserPromptSubmit", "new", prompt.clone());
    assert!(failed.status.success());
    assert_eq!(stdout_json(&failed)["continue"], true);
    assert!(harness.pending() > 0);
    std::fs::remove_dir(&path).unwrap();
    let retried = harness.workbuddy_hook("UserPromptSubmit", "new", prompt);
    assert!(retried.status.success());
    assert_eq!(harness.pending(), 0);
    assert!(
        stdout_json(&retried)["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("WB-RETRY-5891")
    );
}

#[test]
fn workbuddy_concurrent_first_prompts_do_not_duplicate_flush() {
    let harness = Harness::new();
    harness.capture(
        "old",
        "Always remember the Boreal release code WB-RACE-4231.",
    );
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    let output = harness.workbuddy_hook(
                        "UserPromptSubmit",
                        "new",
                        serde_json::json!({"prompt":"What is the Boreal release code?"}),
                    );
                    assert!(output.status.success());
                    assert!(
                        stdout_json(&output)["hookSpecificOutput"]["additionalContext"]
                            .as_str()
                            .unwrap()
                            .contains("WB-RACE-4231")
                    );
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
    });
    assert_eq!(harness.pending(), 0);
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            harness
                .root()
                .join("workspaces/integration/control/pipeline_state.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(state["indexTraces"].as_array().unwrap().len(), 1);
}

fn stdout_json(output: &Output) -> serde_json::Value {
    let text = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(text.trim()).unwrap_or_else(|error| {
        panic!(
            "stdout is not JSON ({error}); status={:?} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// The engine can be opened and `overview` works (core storage landed).
fn core_ready() -> bool {
    let harness = Harness::new();
    let output = harness.run(&["overview"]);
    output.status.success()
}

/// Indexing + recall pipelines work (core-index / core-recall-dream landed).
fn pipeline_ready() -> bool {
    if !core_ready() {
        return false;
    }
    let harness = Harness::new();
    let flush = harness.run(&["flush"]);
    let search = harness.run(&["search", "anything"]);
    flush.status.success() && search.status.success()
}

#[test]
fn help_init_and_doctor_work_without_engine() {
    let harness = Harness::new();
    let help = harness.run(&["--help"]);
    assert!(help.status.success(), "stderr={}", stderr_text(&help));
    assert!(String::from_utf8_lossy(&help.stdout).contains("pd-memory"));

    let config_path = harness.root().join("config.json");
    let init = harness.run(&["init", "--path", config_path.to_str().unwrap()]);
    assert!(init.status.success(), "stderr={}", stderr_text(&init));
    let raw = std::fs::read_to_string(&config_path).expect("config written");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("config is JSON");
    assert!(parsed.get("data_root").is_some());
    assert!(parsed["model"]["api_key_env"].as_str().is_some());

    // `init` refuses to overwrite without --force.
    let second = harness.run(&["init", "--path", config_path.to_str().unwrap()]);
    assert!(!second.status.success());
    let forced = harness.run(&["init", "--force", "--path", config_path.to_str().unwrap()]);
    assert!(forced.status.success());

    let doctor = harness.run(&["--config", config_path.to_str().unwrap(), "doctor"]);
    assert!(doctor.status.success(), "stderr={}", stderr_text(&doctor));
    let stdout = String::from_utf8_lossy(&doctor.stdout).to_string();
    assert!(stdout.contains("data root:"), "stdout={stdout}");
    assert!(stdout.contains("provider:"), "stdout={stdout}");
    assert!(stdout.contains("workspace key:"), "stdout={stdout}");
    assert!(stdout.contains("mcp tools: 7"), "stdout={stdout}");
}

#[test]
fn mcp_initialize_tools_list_and_overview_roundtrip() {
    if !core_ready() {
        eprintln!("SKIP: core storage not implemented yet (engine cannot open)");
        return;
    }
    let harness = Harness::new();
    let requests = [
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "workbuddy", "version": "2.161.0"}
            }
        }),
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}),
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "memory_overview", "arguments": {}}
        }),
    ]
    .iter()
    .map(serde_json::Value::to_string)
    .collect::<Vec<_>>()
    .join("\n")
        + "\n";
    let mut child = harness
        .command()
        .args(["--workspace", "integration", "serve", "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve mcp");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(requests.as_bytes())
        .expect("write requests");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait MCP server");
    assert!(
        output.status.success(),
        "MCP server failed: stderr={}",
        stderr_text(&output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let responses: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("stdout line is not JSON-RPC ({error}): {line}"))
        })
        .collect();
    assert_eq!(
        responses.len(),
        3,
        "notification must not produce a response: {stdout}"
    );

    let initialize = &responses[0]["result"];
    // WorkBuddy sends a newer protocol date; it must be echoed back.
    assert_eq!(initialize["protocolVersion"], "2025-11-25");
    assert_eq!(initialize["capabilities"]["tools"]["listChanged"], false);
    assert_eq!(initialize["serverInfo"]["name"], "pd-memory");
    assert!(initialize["serverInfo"]["version"].is_string());

    let tools = responses[1]["result"]["tools"]
        .as_array()
        .expect("tools array");
    assert_eq!(tools.len(), 7);
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted);

    let overview = &responses[2]["result"];
    assert_eq!(overview["isError"], false);
    assert_eq!(overview["content"][0]["type"], "text");
    let structured = &overview["structuredContent"];
    assert_eq!(structured["total_files"], 0);
    assert_eq!(structured["workspace_name"], "integration");

    // Server must exit cleanly at EOF; stdout contains JSON-RPC only.
    assert!(!stdout.contains("pd-memory MCP stdio server started"));
}

#[test]
fn hook_stdin_stdout_contract() {
    if !core_ready() {
        eprintln!("SKIP: core storage not implemented yet (hook engine cannot open)");
        return;
    }
    let harness = Harness::new();

    // UserPromptSubmit with no stored memory: exactly the minimal continue line.
    let payload = serde_json::json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": "hook-itest",
        "transcript_path": "",
        "cwd": harness.root().to_str().unwrap(),
        "prompt": "what do I remember about this project?"
    })
    .to_string();
    let output = harness.run_with_stdin(&["hook", "--workspace", "integration"], &payload);
    assert!(
        output.status.success(),
        "hook must exit 0: stderr={}",
        stderr_text(&output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert_eq!(stdout.lines().count(), 1, "hook prints one line: {stdout}");
    let response: serde_json::Value = serde_json::from_str(stdout.trim()).expect("hook JSON");
    assert_eq!(response["continue"], true);

    // SessionStart carries a transcript that is readable; hook still succeeds.
    let transcript = harness.root().join("session.jsonl");
    std::fs::write(
        &transcript,
        concat!(
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"<system-reminder data-role=\"user-context\">host instructions</system-reminder>\n<user_query>remember: always answer in Chinese</user_query>"}]}"#,
            "\n",
            r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"noted"}]}"#,
            "\n",
        ),
    )
    .expect("write transcript");
    let start_payload = serde_json::json!({
        "hook_event_name": "SessionStart",
        "session_id": "hook-itest",
        "transcript_path": transcript.to_str().unwrap(),
        "source": "startup"
    })
    .to_string();
    let output = harness.run_with_stdin(&["hook", "--workspace", "integration"], &start_payload);
    assert!(output.status.success());
    let response: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).expect("hook JSON");
    assert_eq!(response["continue"], true);

    // Stop captures the turn and runs best-effort maintenance.
    let stop_payload = serde_json::json!({
        "hook_event_name": "Stop",
        "session_id": "hook-itest",
        "transcript_path": transcript.to_str().unwrap(),
        "cwd": harness.root().to_str().unwrap(),
        "last_assistant_message": "noted",
        "stop_hook_active": false
    })
    .to_string();
    let output = harness.run_with_stdin(&["hook", "--workspace", "integration"], &stop_payload);
    assert!(output.status.success());
    let response: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).expect("hook JSON");
    assert_eq!(response["continue"], true);

    let queue = std::fs::read_to_string(
        harness
            .root()
            .join("workspaces/integration/control/l0_sessions.jsonl"),
    )
    .expect("Stop captured the GUI turn");
    let captured: serde_json::Value = serde_json::from_str(queue.lines().last().unwrap()).unwrap();
    assert_eq!(captured["source"], "hook:Stop");
    assert_eq!(
        captured["messages"][0]["content"],
        "always answer in Chinese"
    );
    assert_eq!(captured["messages"][1]["content"], "noted");
    assert!(!queue.contains("host instructions"));

    // Malformed stdin must never break the hook.
    let output = harness.run_with_stdin(&["hook", "--workspace", "integration"], "{not json");
    assert!(output.status.success());
    let response: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).expect("hook JSON");
    assert_eq!(response, serde_json::json!({"continue": true}));
}

#[test]
fn add_flush_list_search_export_snapshot_with_mock_provider() {
    if !pipeline_ready() {
        eprintln!("SKIP: indexing/recall pipeline not implemented yet");
        return;
    }
    let harness = Harness::new();
    let workspace = ["--workspace", "integration"];

    let fact = "这个项目叫 Boreal，是一个本地知识库整理工具";
    let turn = serde_json::json!([
        {"role": "user", "content": fact},
        {"role": "assistant", "content": "记住了，Boreal 是一个本地知识库整理工具。"}
    ])
    .to_string();
    let mut args = workspace.to_vec();
    args.extend(["add", "--session", "itest-1"]);
    let add = harness.run_with_stdin(&args, &turn);
    assert!(
        add.status.success(),
        "add failed: stderr={}",
        stderr_text(&add)
    );
    let added = stdout_json(&add);
    assert_eq!(added["captured"], true, "add output: {added}");
    assert_eq!(added["session_key"], "itest-1");

    // Identical turn again => de-duplicated, still exit code 0.
    let duplicate = harness.run_with_stdin(&args, &turn);
    assert!(duplicate.status.success());
    let duplicate = stdout_json(&duplicate);
    assert_eq!(duplicate["captured"], true);
    assert!(duplicate.get("duplicate").is_none());
    assert!(duplicate.get("message").is_none());

    let mut args = workspace.to_vec();
    args.extend(["flush", "--batch-size", "10"]);
    let flush = harness.run(&args);
    assert!(
        flush.status.success(),
        "flush failed: stderr={}",
        stderr_text(&flush)
    );
    let stats = stdout_json(&flush);
    assert!(
        stats["processed_turns"].as_u64().unwrap_or(0) >= 1,
        "stats={stats}"
    );

    let mut args = workspace.to_vec();
    args.extend(["list"]);
    let list = harness.run(&args);
    assert!(list.status.success(), "stderr={}", stderr_text(&list));
    let entries = stdout_json(&list);
    assert!(
        entries["count"].as_u64().unwrap_or(0) >= 1,
        "expected memory entries, got {entries}"
    );

    let mut args = workspace.to_vec();
    args.extend(["search", "Boreal"]);
    let search = harness.run(&args);
    assert!(
        search.status.success(),
        "search failed: stderr={}",
        stderr_text(&search)
    );
    let result = stdout_json(&search);
    let selected = result["selected_ids"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !selected.is_empty()
            || result["context"]
                .as_str()
                .is_some_and(|context| !context.is_empty()),
        "mock search should recall the stored fact: {result}"
    );

    let mut args = workspace.to_vec();
    let export_path = harness.root().join("export.json");
    args.extend(["export", export_path.to_str().unwrap()]);
    let export = harness.run(&args);
    assert!(export.status.success(), "stderr={}", stderr_text(&export));
    let bundle: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&export_path).expect("bundle file"))
            .expect("bundle JSON");
    assert_eq!(bundle["format_version"], "clawxmemory-memory-snapshot.v4");
    assert!(!bundle["files"].as_array().expect("files").is_empty());

    let mut args = workspace.to_vec();
    args.extend(["snapshot", "--limit", "10"]);
    let snapshot = harness.run(&args);
    assert!(
        snapshot.status.success(),
        "stderr={}",
        stderr_text(&snapshot)
    );
    let snapshot = stdout_json(&snapshot);
    assert!(snapshot["overview"]["total_files"].as_u64().unwrap_or(0) >= 1);
}
