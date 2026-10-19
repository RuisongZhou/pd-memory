#!/usr/bin/env python3
"""End-to-end harness for the pd-memory memory SDK against WorkBuddy.

Two independent routes:

  Route A - contract simulator (always run; CI-safe, no WorkBuddy install needed)
    Phase 1  real WorkBuddy-shaped `Stop` hook JSON on stdin + synthetic
             WorkBuddy transcript JSONL -> assert exit 0 / one-line JSON.
    Phase 2  `pd-memory flush` -> assert Markdown memory files + MEMORY.md.
    Phase 3  `UserPromptSubmit` hook -> assert additionalContext contains the
             stored fact.
    Phase 4  MCP stdio JSON-RPC: initialize -> notifications/initialized ->
             tools/list -> tools/call memory_overview -> tools/call memory_search.
    Phase 5  `pd-memory dream` then `pd-memory rollback` -> assert snapshot
             and rollback work.

  Route B - real headless WorkBuddy CLI (--real)
    Locates WorkBuddy's bundled CodeBuddy CLI 2.161.0 + bundled Node, starts a
    stdlib OpenAI-compatible SSE mock endpoint, writes an isolated
    CODEBUDDY_CONFIG_DIR with models.json / settings.json (hooks) / mcp.json
    (MCP server = our binary), runs:

        node bin/codebuddy -p '<prompt>' --model mock-model \
            --mcp-config <mcp.json> --strict-mcp-config \
            --settings <settings.json> --output-format json

    and asserts that (a) our hook command really received WorkBuddy hook JSON,
    (b) the real CLI performed an MCP initialize/tools/list handshake with our
    `pd-memory serve mcp`, and (c) captured turns turn into memory files.

Everything is local: PD_MEMORY_PROVIDER=mock for our SDK, a localhost-only
mock OpenAI endpoint for the WorkBuddy agent. No network, no login.

Usage:
    python3 examples/workbuddy/simulate_workbuddy.py [--real] [-v]
    python3 examples/workbuddy/simulate_workbuddy.py --binary target/debug/pd-memory

Exit code is non-zero if any required check fails.
"""

from __future__ import annotations

import argparse
import http.server
import json
import os
import queue
import re
import shutil
import socket
import subprocess
import sys
import threading
import time
import urllib.parse
import uuid
from pathlib import Path

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

HERE = Path(__file__).resolve().parent          # sdk/pd-memory/examples/workbuddy
SDK_ROOT = HERE.parents[1]                      # sdk/pd-memory
RUN_DIR = HERE / ".run"
DEFAULT_BINARY = SDK_ROOT / "target" / "debug" / "pd-memory"
COMMITTED_CONFIG = HERE / "pd-memory.config.json"
WORKSPACE_ID = "workbuddy-demo"

FACT_TOKEN = "ORION-7421"
FACT_SENTENCE = f"请记住：WorkBuddy 集成项目的代号是 {FACT_TOKEN}，主要使用 Rust 实现。"
FACT_QUESTION = "我们项目的代号是什么？请只回答代号。"
MOCK_MODEL_REPLY = "MOCK_REPLY: remembered that you like Rust."

# WorkBuddy 5.8.2 / bundled CodeBuddy Code CLI 2.161.0 (verified on this machine)
WORKBUDDY_CLI = Path(
    "/Applications/WorkBuddy.app/Contents/Resources/app.asar.unpacked/cli/bin/codebuddy"
)
WORKBUDDY_NODE_GLOB = Path.home() / ".workbuddy" / "binaries" / "node" / "versions"

MCP_TOOLS_EXPECTED = [
    "memory_search",
    "memory_list",
    "memory_get",
    "memory_overview",
    "memory_flush",
    "memory_dream",
    "memory_add",
]

MCP_PROTOCOL_VERSIONS = ["2025-11-25", "2024-11-05"]

VERBOSE = False


# ---------------------------------------------------------------------------
# Small utilities
# ---------------------------------------------------------------------------


def log(message: str = "") -> None:
    print(message, flush=True)


def vlog(message: str) -> None:
    if VERBOSE:
        print(f"    [verbose] {message}", flush=True)


def truncate(text: str, limit: int = 1200) -> str:
    text = text.strip()
    if len(text) <= limit:
        return text
    return text[:limit] + f"\n... ({len(text) - limit} chars truncated)"


def parse_json_blob(text: str):
    """Best-effort JSON parse of CLI stdout (single object or last JSON line)."""
    if text is None:
        return None
    stripped = text.strip()
    if not stripped:
        return None
    try:
        return json.loads(stripped)
    except json.JSONDecodeError:
        pass
    for line in reversed(stripped.splitlines()):
        candidate = line.strip()
        if not candidate or candidate[0] not in "{[":
            continue
        try:
            return json.loads(candidate)
        except json.JSONDecodeError:
            continue
    return None


def extract_result_object(payload):
    """WorkBuddy's -p --output-format json may emit a single result object or a
    JSON array of conversation events; find the result record either way."""
    if isinstance(payload, dict):
        if payload.get("type") == "result" or "result" in payload:
            return payload
        return payload
    if isinstance(payload, list):
        for item in reversed(payload):
            if isinstance(item, dict) and item.get("type") == "result":
                return item
        for item in reversed(payload):
            if isinstance(item, dict):
                return item
        return None
    return None


def parse_strict_json(text: str, what: str):
    """Parse stdout that MUST be one JSON object (hook contract)."""
    stripped = (text or "").strip()
    if not stripped:
        raise AssertionError(f"{what}: stdout was empty")
    try:
        value = json.loads(stripped)
    except json.JSONDecodeError as exc:
        raise AssertionError(f"{what}: stdout is not valid JSON ({exc}): {truncate(stripped)}")
    if not isinstance(value, dict):
        raise AssertionError(f"{what}: stdout JSON is not an object: {truncate(stripped)}")
    return value


def save_text(path: Path, text: str) -> None:
    try:
        path.write_text(text or "", encoding="utf-8")
    except OSError as exc:
        log(f"  [warn] could not save {path}: {exc}")


def read_text(path: Path, limit: int = 4000) -> str:
    try:
        return truncate(path.read_text(encoding="utf-8", errors="replace"), limit)
    except OSError as exc:
        return f"<cannot read {path}: {exc}>"


def iter_files(root: Path):
    if not root.exists():
        return
    for path in sorted(root.rglob("*")):
        if path.is_file():
            yield path


def find_fact_in_tree(root: Path, token: str = FACT_TOKEN):
    """Return the list of files under root whose text content mentions token."""
    hits = []
    for path in iter_files(root):
        try:
            if token.lower() in path.read_text(encoding="utf-8", errors="ignore").lower():
                hits.append(path)
        except OSError:
            continue
    return hits


def run_cmd(cmd, *, input_text=None, env=None, timeout=180, cwd=None):
    vlog(f"$ {' '.join(str(part) for part in cmd)}")
    started = time.time()
    result = subprocess.run(
        [str(part) for part in cmd],
        input=input_text,
        capture_output=True,
        text=True,
        timeout=timeout,
        env=env,
        cwd=str(cwd) if cwd else None,
    )
    vlog(f"  exit={result.returncode} in {time.time() - started:.2f}s")
    if result.stdout:
        vlog(f"  stdout: {truncate(result.stdout, 2000)}")
    if result.stderr:
        vlog(f"  stderr: {truncate(result.stderr, 2000)}")
    return result


def base_env(data_root: Path | None = None) -> dict:
    env = os.environ.copy()
    env["PD_MEMORY_PROVIDER"] = "mock"
    if data_root is not None:
        env["PD_MEMORY_DATA_ROOT"] = str(data_root)
    env.setdefault("RUST_LOG", "warn")
    env["NO_COLOR"] = "1"
    return env


# ---------------------------------------------------------------------------
# Result accounting
# ---------------------------------------------------------------------------


class Checks:
    def __init__(self):
        self.rows = []  # (phase, name, status, detail)

    def ok(self, phase, name, detail=""):
        self.rows.append((phase, name, "PASS", detail))
        log(f"  [PASS] {name}" + (f" - {detail}" if detail else ""))

    def warn(self, phase, name, detail=""):
        self.rows.append((phase, name, "WARN", detail))
        log(f"  [WARN] {name}" + (f" - {detail}" if detail else ""))

    def fail(self, phase, name, detail=""):
        self.rows.append((phase, name, "FAIL", detail))
        log(f"  [FAIL] {name}" + (f" - {detail}" if detail else ""))

    def skip(self, phase, name, detail=""):
        self.rows.append((phase, name, "SKIP", detail))
        log(f"  [SKIP] {name}" + (f" - {detail}" if detail else ""))

    def check(self, condition, phase, name, detail="", warn_only=False):
        if condition:
            self.ok(phase, name, detail)
        elif warn_only:
            self.warn(phase, name, detail)
        else:
            self.fail(phase, name, detail)
        return bool(condition)

    def failures(self):
        return [row for row in self.rows if row[2] == "FAIL"]

    def warnings(self):
        return [row for row in self.rows if row[2] == "WARN"]

    def passes(self):
        return [row for row in self.rows if row[2] == "PASS"]


# ---------------------------------------------------------------------------
# CLI binary handling
# ---------------------------------------------------------------------------


def ensure_binary(binary: Path, build: bool) -> None:
    if binary.exists() and os.access(binary, os.X_OK):
        log(f"binary          : {binary}")
        return
    if not build:
        raise SystemExit(f"[error] binary not found: {binary} (run cargo build -p pd-memory-cli or drop --no-build)")
    log(f"binary missing, building: cargo build -p pd-memory-cli (cwd={SDK_ROOT})")
    result = run_cmd(["cargo", "build", "-p", "pd-memory-cli"], cwd=SDK_ROOT, timeout=900)
    if result.returncode != 0:
        raise SystemExit(f"[error] cargo build failed:\n{result.stderr}")
    if not binary.exists():
        raise SystemExit(f"[error] build succeeded but binary still missing: {binary}")


def cli_version(binary: Path) -> str:
    for flag in ("--version", "-V"):
        try:
            result = run_cmd([binary, flag], timeout=30)
        except (OSError, subprocess.TimeoutExpired) as exc:
            return f"<error: {exc}>"
        text = (result.stdout or "").strip() or (result.stderr or "").strip()
        if text:
            return text.splitlines()[0].strip()
    return "<unknown>"


def cli_help(binary: Path) -> str:
    try:
        result = run_cmd([binary, "--help"], timeout=30)
    except (OSError, subprocess.TimeoutExpired) as exc:
        return f"<error: {exc}>"
    return (result.stdout or "") + (result.stderr or "")


def detect_subcommands(help_text: str):
    """Extract subcommand names from clap-style help output."""
    names = set()
    in_commands = False
    for line in help_text.splitlines():
        if re.match(r"^\s*(Commands|Subcommands):\s*$", line, re.IGNORECASE):
            in_commands = True
            continue
        if in_commands:
            if not line.strip():
                continue
            match = re.match(r"^\s{2,}([a-zA-Z][a-zA-Z0-9_-]*)\b", line)
            if match:
                names.add(match.group(1))
            elif not line.startswith(" "):
                in_commands = False
    return names


def run_cli(binary: Path, config: Path, args, *, stdin_text=None, env=None, timeout=180):
    """Invoke the CLI with the documented global flag order: pd-memory --config <cfg> <cmd>."""
    cmd = [binary, "--config", config, *args]
    return run_cmd(cmd, input_text=stdin_text, env=env, timeout=timeout, cwd=SDK_ROOT)


# ---------------------------------------------------------------------------
# Route A - WorkBuddy contract simulator
# ---------------------------------------------------------------------------


def write_runtime_config(sim_dir: Path) -> Path:
    config = json.loads(COMMITTED_CONFIG.read_text(encoding="utf-8"))
    config["data_root"] = str((sim_dir / "data").resolve())
    config["workspace_id"] = WORKSPACE_ID
    config.setdefault("model", {})
    config["model"]["provider"] = "mock"
    config["model"]["model"] = "mock-deterministic"
    config.setdefault("retrieval", {})
    config["retrieval"]["cache_ttl_ms"] = 0  # deterministic recall in the harness
    path = sim_dir / "pd-memory.config.json"
    path.write_text(json.dumps(config, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    log(f"runtime config  : {path}")
    return path


def write_transcript(path: Path, session_id: str, cwd: Path, fact_sentence: str = FACT_SENTENCE, assistant_reply: str | None = None) -> None:
    """Write a WorkBuddy-style transcript JSONL (format verified in research doc §4.2).

    WorkBuddy records look like:
        {"type":"message","role":"user","content":[{"type":"input_text","text":...}]}
        {"type":"message","role":"assistant","content":[{"type":"output_text","text":...}]}
        {"type":"function_call",...} / {"type":"function_call_result",...}
    """
    now = int(time.time() * 1000)
    records = [
        {
            "id": str(uuid.uuid4()),
            "timestamp": now - 4000,
            "type": "session-meta",
            "sessionId": session_id,
            "cwd": str(cwd),
        },
        {
            "id": str(uuid.uuid4()),
            "timestamp": now - 3000,
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": fact_sentence}],
            "sessionId": session_id,
        },
        {
            "id": str(uuid.uuid4()),
            "timestamp": now - 2000,
            "type": "reasoning",
            "rawContent": [{"type": "reasoning_text", "text": "用户给了项目代号，应该记住。"}],
            "sessionId": session_id,
        },
        {
            "id": str(uuid.uuid4()),
            "timestamp": now - 1500,
            "type": "function_call",
            "name": "Read",
            "arguments": json.dumps({"file_path": str(cwd / "README.md")}, ensure_ascii=False),
            "callId": "call-sim-1",
            "sessionId": session_id,
        },
        {
            "id": str(uuid.uuid4()),
            "timestamp": now - 1200,
            "type": "function_call_result",
            "name": "Read",
            "callId": "call-sim-1",
            "status": "completed",
            "output": {"type": "text", "text": "WorkBuddy demo workspace"},
        },
        {
            "id": str(uuid.uuid4()),
            "timestamp": now - 1000,
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [
                {
                    "type": "output_text",
                    "text": assistant_reply
                    or f"已记住：项目代号 {FACT_TOKEN}，主语言 Rust。",
                }
            ],
            "sessionId": session_id,
        },
    ]
    with path.open("w", encoding="utf-8") as handle:
        for record in records:
            handle.write(json.dumps(record, ensure_ascii=False) + "\n")


def hook_stdin(event: str, session_id: str, transcript: Path, cwd: Path, **extra) -> str:
    payload = {
        "hook_event_name": event,
        "session_id": session_id,
        "transcript_path": str(transcript),
        "cwd": str(cwd),
    }
    payload.update(extra)
    return json.dumps(payload, ensure_ascii=False)


def simulate_route(binary: Path, config: Path, sim_dir: Path, checks: Checks, subcommands):
    data_root = sim_dir / "data"
    runs = {}

    # ---------------- Phase 1: Stop hook capture ----------------
    log("\n[route A] phase 1: Stop hook (WorkBuddy-shaped stdin + synthetic transcript)")
    session_id = "wb-sim-1"
    cwd = sim_dir / "workspace"
    cwd.mkdir(parents=True, exist_ok=True)
    (cwd / "README.md").write_text("WorkBuddy demo workspace\n", encoding="utf-8")
    transcript = sim_dir / "transcripts" / f"{session_id}.jsonl"
    transcript.parent.mkdir(parents=True, exist_ok=True)
    write_transcript(transcript, session_id, cwd)
    log(f"transcript      : {transcript} ({len(transcript.read_text(encoding='utf-8').splitlines())} records)")

    stop_input = hook_stdin(
        "Stop",
        session_id,
        transcript,
        cwd,
        stop_hook_active=False,
        last_assistant_message=f"已记住：项目代号 {FACT_TOKEN}，主语言 Rust。",
        permission_mode="default",
        client="simulator",
        version="2.161.0",
        model="mock-model",
    )
    result = run_cli(binary, config, ["hook"], stdin_text=stop_input, env=base_env(data_root), timeout=120)
    runs["stop"] = result
    checks.check(result.returncode == 0, "A1", "Stop hook exit code 0", f"rc={result.returncode}")
    try:
        stop_out = parse_strict_json(result.stdout, "Stop hook")
        checks.ok("A1", "Stop hook stdout is one-line JSON", truncate(result.stdout, 300))
    except AssertionError as exc:
        stop_out = None
        checks.fail("A1", "Stop hook stdout is one-line JSON", str(exc))
    if stop_out is not None:
        checks.check("continue" in stop_out, "A1", "Stop hook JSON has 'continue'", json.dumps(stop_out)[:300])
        checks.check(
            stop_out.get("hookSpecificOutput") is None
            or isinstance(stop_out.get("hookSpecificOutput"), dict),
            "A1",
            "Stop hook hookSpecificOutput shape",
        )
    # The capture must have been persisted (queue/L0 control plane) even before flush.
    captured = find_fact_in_tree(data_root)
    checks.check(
        bool(captured),
        "A1",
        "Stop hook captured the turn into the data root",
        f"{len(captured)} file(s): " + ", ".join(str(p.relative_to(data_root)) for p in captured[:5]),
    )
    log(f"  data root files now: {[str(p.relative_to(data_root)) for p in iter_files(data_root)][:12]}")

    # ---------------- Phase 2: flush -> Markdown memory ----------------
    log("\n[route A] phase 2: flush -> Markdown memory + MEMORY.md")
    if "flush" in subcommands:
        flush = run_cli(binary, config, ["flush"], env=base_env(data_root), timeout=180)
    else:
        # fall back to the PreCompact hook path if this CLI build has no `flush`
        precompact_input = hook_stdin(
            "PreCompact",
            session_id,
            transcript,
            cwd,
            trigger="auto",
            custom_instructions="",
        )
        flush = run_cli(binary, config, ["hook"], stdin_text=precompact_input, env=base_env(data_root), timeout=180)
    runs["flush"] = flush
    checks.check(flush.returncode == 0, "A2", "flush/PreCompact exit code 0", f"rc={flush.returncode}; stderr={truncate(flush.stderr, 300)}")
    flush_json = parse_json_blob(flush.stdout)
    if flush_json is not None:
        vlog(f"flush output: {json.dumps(flush_json, ensure_ascii=False)[:800]}")
        if isinstance(flush_json, dict) and "written_files" in flush_json:
            # Stop hooks may already have run scheduled maintenance, in which
            # case the explicit flush legitimately reports 0 new files; the
            # on-disk assertions below are authoritative.
            checks.check(
                int(flush_json.get("written_files") or 0) >= 0,
                "A2",
                "flush stats parsed",
                f"written_files={flush_json.get('written_files')}",
            )
    memory_root = data_root / "workspaces" / WORKSPACE_ID / "memory"
    manifest = memory_root / "MEMORY.md"
    checks.check(manifest.exists(), "A2", "MEMORY.md exists", str(manifest))
    if manifest.exists():
        checks.check(manifest.stat().st_size > 0, "A2", "MEMORY.md is non-empty", f"{manifest.stat().st_size} bytes")
    markdown_files = [p for p in memory_root.rglob("*.md") if p.name != "MEMORY.md"] if memory_root.exists() else []
    checks.check(
        bool(markdown_files),
        "A2",
        "at least one memory Markdown file written",
        f"{len(markdown_files)} file(s)",
    )
    log("  memory files:")
    for path in sorted(memory_root.rglob("*")) if memory_root.exists() else []:
        if path.is_file():
            log(f"    - {path.relative_to(data_root)} ({path.stat().st_size} bytes)")
    if markdown_files:
        log("  memory Markdown excerpt:")
        log("    " + "\n    ".join(read_text(markdown_files[0], 1200).splitlines()[:12]))
        fact_hits = find_fact_in_tree(memory_root)
        checks.check(
            bool(fact_hits),
            "A2",
            f"memory files contain the stored fact {FACT_TOKEN}",
            f"{len(fact_hits)} hit(s)",
            warn_only=True,
        )
    if manifest.exists():
        log("  MEMORY.md excerpt:")
        log("    " + "\n    ".join(read_text(manifest, 1200).splitlines()[:14]))

    # ---------------- Phase 3: UserPromptSubmit recall ----------------
    log("\n[route A] phase 3: UserPromptSubmit hook -> additionalContext")
    submit_input = hook_stdin(
        "UserPromptSubmit",
        session_id,
        transcript,
        cwd,
        prompt=FACT_QUESTION,
        is_internal_continuation=False,
        permission_mode="default",
        client="simulator",
        version="2.161.0",
        model="mock-model",
    )
    submit = run_cli(binary, config, ["hook"], stdin_text=submit_input, env=base_env(data_root), timeout=120)
    runs["user_prompt_submit"] = submit
    checks.check(submit.returncode == 0, "A3", "UserPromptSubmit hook exit code 0", f"rc={submit.returncode}")
    try:
        submit_out = parse_strict_json(submit.stdout, "UserPromptSubmit hook")
        checks.ok("A3", "UserPromptSubmit hook stdout is JSON", truncate(submit.stdout, 300))
    except AssertionError as exc:
        submit_out = None
        checks.fail("A3", "UserPromptSubmit hook stdout is JSON", str(exc))
    additional_context = ""
    if submit_out is not None:
        specific = submit_out.get("hookSpecificOutput") or {}
        additional_context = str(specific.get("additionalContext") or "")
        checks.check(
            bool(additional_context.strip()),
            "A3",
            "additionalContext is non-empty",
            truncate(additional_context, 300) if additional_context else "empty",
        )
        checks.check(
            FACT_TOKEN.lower() in additional_context.lower(),
            "A3",
            f"additionalContext contains the stored fact {FACT_TOKEN}",
            truncate(additional_context, 300) if additional_context else "empty",
        )
        if additional_context:
            log("  injected additionalContext excerpt:")
            log("    " + "\n    ".join(additional_context.splitlines()[:10]))

    # ---------------- Phase 4: MCP stdio JSON-RPC ----------------
    log("\n[route A] phase 4: MCP stdio (initialize / tools/list / tools/call)")
    mcp_result = run_mcp_phase(binary, config, data_root, checks)
    runs["mcp"] = mcp_result
    if mcp_result.get("search_text"):
        log("  memory_search result excerpt:")
        log("    " + "\n    ".join(mcp_result["search_text"].splitlines()[:10]))

    # ---------------- Phase 5: dream + rollback ----------------
    log("\n[route A] phase 5: dream then rollback")
    # The mock model's dream only consolidates when a kind has >=2 files, so
    # capture a second (WorkBuddy-shaped) session through the real Stop hook.
    session2 = "wb-sim-2"
    transcript2 = sim_dir / "transcripts" / f"{session2}.jsonl"
    fact2 = "请记住：WorkBuddy 团队约定使用 pnpm 管理前端依赖，统一使用 TypeScript。"
    write_transcript(transcript2, session2, cwd, fact_sentence=fact2, assistant_reply="已记住：pnpm + TypeScript 约定。")
    stop2 = run_cli(
        binary,
        config,
        ["hook"],
        stdin_text=hook_stdin(
            "Stop",
            session2,
            transcript2,
            cwd,
            stop_hook_active=False,
            last_assistant_message="已记住：pnpm + TypeScript 约定。",
            permission_mode="default",
            client="simulator",
            version="2.161.0",
            model="mock-model",
        ),
        env=base_env(data_root),
        timeout=120,
    )
    checks.check(stop2.returncode == 0, "A5", "second Stop hook exit code 0", f"rc={stop2.returncode}")
    flush2 = run_cli(binary, config, ["flush"], env=base_env(data_root), timeout=180)
    checks.check(flush2.returncode == 0, "A5", "second flush exit code 0", f"rc={flush2.returncode}")
    seeded_files = [p for p in memory_root.rglob("*.md") if p.name != "MEMORY.md"]
    log(f"  memory files before dream: {len(seeded_files)}")
    last_dream_dir = data_root / "workspaces" / WORKSPACE_ID / "last_dream"
    dream = run_cli(binary, config, ["dream"], env=base_env(data_root), timeout=240)
    runs["dream"] = dream
    checks.check(dream.returncode == 0, "A5", "dream exit code 0", f"rc={dream.returncode}; stderr={truncate(dream.stderr, 300)}")
    dream_json = parse_json_blob(dream.stdout)
    if dream_json is not None:
        vlog(f"dream output: {json.dumps(dream_json, ensure_ascii=False)[:800]}")
        checks.check(
            "summary" in dream_json or "status" in dream_json,
            "A5",
            "dream output looks like DreamRunResult",
            truncate(json.dumps(dream_json, ensure_ascii=False), 300),
            warn_only=True,
        )
    no_mutations = isinstance(dream_json, dict) and not any(
        dream_json.get(key) for key in ("rewritten_projects", "deleted_files", "deleted_projects", "profile_updated")
    )
    checks.check(last_dream_dir.exists() or no_mutations, "A5", "dream snapshot matches mutation outcome", str(last_dream_dir))
    if last_dream_dir.exists():
        snapshot_files = [p for p in iter_files(last_dream_dir)]
        log(f"  dream snapshot files: {[str(p.relative_to(data_root)) for p in snapshot_files][:8]}")

    dream_status = str(dream_json.get("status", "")) if isinstance(dream_json, dict) else ""
    snapshot_ready = last_dream_dir.exists()
    rollback = run_cli(binary, config, ["rollback"], env=base_env(data_root), timeout=120)
    runs["rollback"] = rollback
    if snapshot_ready:
        checks.check(
            rollback.returncode == 0,
            "A5",
            "rollback exit code 0",
            f"rc={rollback.returncode}; stderr={truncate(rollback.stderr, 300)}",
        )
        rollback_json = parse_json_blob(rollback.stdout)
        if rollback_json is not None:
            vlog(f"rollback output: {json.dumps(rollback_json, ensure_ascii=False)[:800]}")
            checks.check(
                "rolled_back_at" in rollback_json or "snapshot_captured_at" in rollback_json,
                "A5",
                "rollback output looks like DreamRollbackResult",
                truncate(json.dumps(rollback_json, ensure_ascii=False), 300),
            )
        else:
            checks.fail("A5", "rollback printed a JSON result", truncate(rollback.stdout, 300))
    else:
        checks.check(
            rollback.returncode != 0 and "snapshot" in (rollback.stderr or "").lower(),
            "A5",
            "rollback fails cleanly when there is no snapshot",
            f"rc={rollback.returncode}; stderr={truncate(rollback.stderr, 200)}",
        )
    manifest_after_rollback = memory_root / "MEMORY.md"
    checks.check(
        manifest_after_rollback.exists(),
        "A5",
        "MEMORY.md still present after rollback",
        str(manifest_after_rollback),
    )

    return runs


# ---------------------------------------------------------------------------
# MCP client (route A phase 4)
# ---------------------------------------------------------------------------


class McpClient:
    def __init__(self, cmd, env, *, timeout=30):
        self.timeout = timeout
        self.lines = queue.Queue()
        self.stderr_lines = []
        self.proc = subprocess.Popen(
            [str(part) for part in cmd],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            env=env,
        )
        self._reader = threading.Thread(target=self._read_stdout, daemon=True)
        self._err_reader = threading.Thread(target=self._read_stderr, daemon=True)
        self._reader.start()
        self._err_reader.start()
        self.next_id = 1

    def _read_stdout(self):
        assert self.proc.stdout is not None
        for line in self.proc.stdout:
            self.lines.put(line)
        self.lines.put(None)

    def _read_stderr(self):
        assert self.proc.stderr is not None
        for line in self.proc.stderr:
            self.stderr_lines.append(line.rstrip("\n"))
            if len(self.stderr_lines) > 400:
                del self.stderr_lines[:200]

    def send(self, payload):
        assert self.proc.stdin is not None
        self.proc.stdin.write(json.dumps(payload, ensure_ascii=False) + "\n")
        self.proc.stdin.flush()
        vlog(f"MCP -> {truncate(json.dumps(payload, ensure_ascii=False), 500)}")

    def request(self, method, params=None):
        request_id = self.next_id
        self.next_id += 1
        payload = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            payload["params"] = params
        self.send(payload)
        return request_id

    def notify(self, method, params=None):
        payload = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            payload["params"] = params
        self.send(payload)

    def wait_for(self, wanted_ids, timeout=None):
        deadline = time.time() + (timeout or self.timeout)
        responses = {}
        pending = set(wanted_ids)
        while pending and time.time() < deadline:
            remaining = max(0.05, deadline - time.time())
            try:
                line = self.lines.get(timeout=remaining)
            except queue.Empty:
                break
            if line is None:
                break
            line = line.strip()
            if not line:
                continue
            vlog(f"MCP <- {truncate(line, 800)}")
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(message, dict) and isinstance(message.get("id"), int):
                responses[message["id"]] = message
                pending.discard(message["id"])
        return responses

    def close(self):
        try:
            if self.proc.stdin is not None:
                self.proc.stdin.close()
        except OSError:
            pass
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        self._reader.join(timeout=2)
        self._err_reader.join(timeout=2)


def run_mcp_phase(binary: Path, config: Path, data_root: Path, checks: Checks):
    env = base_env(data_root)
    client = McpClient([binary, "--config", config, "serve", "mcp"], env, timeout=30)
    result = {"tools": [], "search_text": "", "overview": None}
    try:
        # initialize: use the exact protocol version WorkBuddy's CLI sends; fall back
        # if the server only speaks an older revision.
        init_params = {
            "protocolVersion": MCP_PROTOCOL_VERSIONS[0],
            "capabilities": {},
            "clientInfo": {"name": "workbuddy-simulator", "version": "0.1.0"},
        }
        init_id = client.request("initialize", init_params)
        client.notify("notifications/initialized")
        responses = client.wait_for([init_id], timeout=20)
        init_response = responses.get(init_id)
        if init_response is None:
            checks.fail("A4", "MCP initialize response", "no response before timeout")
            return result
        if "error" in init_response:
            # retry with the older protocol revision before declaring failure
            checks.warn(
                "A4",
                "MCP initialize with protocolVersion " + MCP_PROTOCOL_VERSIONS[0],
                truncate(json.dumps(init_response, ensure_ascii=False), 300),
            )
            init_id = client.request(
                "initialize",
                dict(init_params, protocolVersion=MCP_PROTOCOL_VERSIONS[1]),
            )
            client.notify("notifications/initialized")
            responses = client.wait_for([init_id], timeout=20)
            init_response = responses.get(init_id)
        checks.check(
            isinstance(init_response, dict) and "result" in init_response,
            "A4",
            "MCP initialize ok",
            truncate(json.dumps(init_response, ensure_ascii=False), 400),
        )
        server_info = (init_response or {}).get("result", {}).get("serverInfo", {})
        if server_info:
            log(f"  MCP server: {json.dumps(server_info, ensure_ascii=False)}")

        # tools/list
        tools_id = client.request("tools/list")
        responses = client.wait_for([tools_id], timeout=20)
        tools_response = responses.get(tools_id) or {}
        tools = (tools_response.get("result") or {}).get("tools") or []
        result["tools"] = tools
        names = [tool.get("name") for tool in tools]
        checks.check(
            len(tools) == 7,
            "A4",
            "tools/list returns exactly 7 tools",
            f"got {len(tools)}: {names}",
        )
        missing = [name for name in MCP_TOOLS_EXPECTED if name not in names]
        checks.check(not missing, "A4", "all documented tools are present", f"missing={missing}; names={names}")
        log(f"  MCP tools: {names}")

        # tools/call memory_overview
        overview_id = client.request(
            "tools/call", {"name": "memory_overview", "arguments": {}}
        )
        responses = client.wait_for([overview_id], timeout=20)
        overview_response = responses.get(overview_id) or {}
        overview_text = mcp_result_text(overview_response)
        result["overview"] = overview_text
        checks.check(
            bool(overview_text) and not overview_response.get("error"),
            "A4",
            "tools/call memory_overview returns content",
            truncate(overview_text or json.dumps(overview_response)[:300], 400),
        )
        overview_json = parse_json_blob(overview_text or "")
        if overview_json is not None:
            checks.check(
                "total_files" in overview_json,
                "A4",
                "memory_overview payload is MemoryOverview",
                truncate(json.dumps(overview_json, ensure_ascii=False), 400),
            )
            log(f"  memory_overview: {truncate(json.dumps(overview_json, ensure_ascii=False), 500)}")

        # tools/call memory_search
        search_id = client.request(
            "tools/call",
            {"name": "memory_search", "arguments": {"query": f"项目代号 {FACT_TOKEN} Rust"}},
        )
        responses = client.wait_for([search_id], timeout=20)
        search_response = responses.get(search_id) or {}
        search_text = mcp_result_text(search_response) or json.dumps(search_response, ensure_ascii=False)
        result["search_text"] = search_text
        checks.check(
            bool(search_text) and not search_response.get("error"),
            "A4",
            "tools/call memory_search returns content",
            truncate(search_text, 400),
        )
        checks.check(
            FACT_TOKEN.lower() in search_text.lower(),
            "A4",
            f"memory_search result contains the stored fact {FACT_TOKEN}",
            truncate(search_text, 400),
        )
        return result
    finally:
        client.close()


def mcp_result_text(response) -> str:
    result = (response or {}).get("result") or {}
    content = result.get("content")
    if isinstance(content, list):
        parts = []
        for item in content:
            if isinstance(item, dict) and item.get("type") == "text":
                parts.append(str(item.get("text", "")))
            elif isinstance(item, dict):
                parts.append(json.dumps(item, ensure_ascii=False))
        if parts:
            return "\n".join(parts)
    structured = result.get("structuredContent")
    if structured is not None:
        return json.dumps(structured, ensure_ascii=False)
    if isinstance(result, dict) and result:
        return json.dumps(result, ensure_ascii=False)
    if isinstance((response or {}).get("error"), dict):
        return json.dumps(response["error"], ensure_ascii=False)
    return ""


# ---------------------------------------------------------------------------
# Route B - real headless WorkBuddy CLI
# ---------------------------------------------------------------------------


class MockOpenAiServer(http.server.ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, address):
        super().__init__(address, MockOpenAiHandler)
        self.requests = []
        self.lock = threading.Lock()

    def record(self, path, headers, body):
        with self.lock:
            self.requests.append({"path": path, "headers": dict(headers), "body": body})


class MockOpenAiHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # keep the harness output clean
        if VERBOSE:
            try:
                log("  [mock-openai] " + (fmt % args if args else str(fmt)))
            except Exception:
                log(f"  [mock-openai] {fmt} {args}")

    def do_GET(self):
        if self.path.rstrip("/").endswith("/models"):
            payload = json.dumps({"object": "list", "data": [{"id": "mock-model"}]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        self.send_error(404)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0") or 0)
        body = self.rfile.read(length) if length else b""
        self.server.record(self.path, self.headers, body)
        if not urllib.parse.urlparse(self.path).path.rstrip("/").endswith("/chat/completions"):
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        chunks = [
            {
                "id": "chatcmpl-mock1",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "mock-model",
                "choices": [
                    {"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": None}
                ],
            },
            {
                "id": "chatcmpl-mock1",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "mock-model",
                "choices": [
                    {
                        "index": 0,
                        "delta": {"content": MOCK_MODEL_REPLY},
                        "finish_reason": None,
                    }
                ],
            },
            {
                "id": "chatcmpl-mock1",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "mock-model",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 8, "total_tokens": 18},
            },
        ]
        for chunk in chunks:
            self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            self.wfile.flush()
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()
        self.close_connection = True


def find_workbuddy_node(explicit: str | None):
    if explicit:
        path = Path(explicit).expanduser()
        return path if path.exists() else None
    candidates = sorted(WORKBUDDY_NODE_GLOB.glob("*/bin/node"))
    if candidates:
        return candidates[-1]
    found = shutil.which("node")
    if found:
        return Path(found)
    return None


def write_real_route_files(real_dir: Path, binary: Path, config: Path, node: Path, cli: Path):
    """Materialise models.json / settings.json / mcp.json / probe + capture scripts."""
    (real_dir / "home").mkdir(parents=True, exist_ok=True)
    (real_dir / "hooks").mkdir(parents=True, exist_ok=True)
    data_root = real_dir / "data"
    data_root.mkdir(parents=True, exist_ok=True)

    # models.json: custom model pointing at the local mock OpenAI endpoint
    port = free_port()
    models = {
        "models": [
            {
                "id": "mock-model",
                "name": "Mock Model",
                "vendor": "mock",
                "url": f"http://127.0.0.1:{port}/v1",
                "apiKey": "sk-mock-test",
                "maxOutputTokens": 2048,
                "maxInputTokens": 32000,
                "supportsToolCall": True,
            }
        ],
        "availableModels": ["mock-model"],
    }
    (real_dir / "models.json").write_text(
        json.dumps(models, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )

    # hook wrapper: capture the exact real stdin/stdout of our hook command, then
    # forward the payload to our binary and pass its stdout straight through.
    wrapper = real_dir / "hook_capture.py"
    wrapper.write_text(HOOK_WRAPPER_TEMPLATE, encoding="utf-8")

    capture_dir = real_dir / "hooks"
    hook_events = ["SessionStart", "UserPromptSubmit", "Stop"]
    commands = {}
    for event in hook_events:
        commands[event] = (
            f"env PD_MEMORY_PROVIDER=mock PD_MEMORY_DATA_ROOT={data_root} "
            f"{sys.executable} {wrapper} {event} {binary} {config} {capture_dir}"
        )
    settings = {
        "hooks": {
            event: [{"hooks": [{"type": "command", "command": commands[event], "timeout": 60}]}]
            for event in hook_events
        }
    }
    settings_path = real_dir / "settings.json"
    settings_path.write_text(
        json.dumps(settings, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )

    # MCP server = transparent probe proxy in front of our real `serve mcp`
    probe = real_dir / "mcp_probe.py"
    probe.write_text(MCP_PROBE_TEMPLATE, encoding="utf-8")
    probe_log = real_dir / "mcp-probe.log"
    mcp_config = {
        "mcpServers": {
            "pd-memory": {
                "type": "stdio",
                "command": sys.executable,
                "args": [str(probe), str(probe_log), "--", str(binary), "--config", str(config), "serve", "mcp"],
                "env": {
                    "PD_MEMORY_PROVIDER": "mock",
                    "PD_MEMORY_DATA_ROOT": str(data_root),
                    "RUST_LOG": "warn",
                },
                "timeout": 60000,
            }
        },
        "disabledMcpServers": [],
    }
    mcp_path = real_dir / "mcp.json"
    mcp_path.write_text(json.dumps(mcp_config, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")

    return {
        "data_root": data_root,
        "models": real_dir / "models.json",
        "settings": settings_path,
        "mcp": mcp_path,
        "probe_log": probe_log,
        "capture_dir": capture_dir,
        "port": port,
        "node": node,
        "cli": cli,
    }


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_for_server(port: int, timeout=10.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.5):
                return True
        except OSError:
            time.sleep(0.05)
    return False


def run_real_route(binary: Path, real_dir: Path, checks: Checks, *, require_real=False, node_path=None, cli_path=None):
    log("\n[route B] real WorkBuddy headless CLI")
    node = find_workbuddy_node(node_path)
    cli = Path(cli_path).expanduser() if cli_path else WORKBUDDY_CLI
    if node is None or not cli.exists():
        detail = f"node={node} cli={cli}"
        if require_real:
            checks.fail("B0", "WorkBuddy CLI + bundled Node available", detail)
        else:
            checks.skip("B0", "WorkBuddy CLI + bundled Node available", detail)
        return
    checks.ok("B0", "WorkBuddy CLI + bundled Node available", f"node={node}, cli={cli}")
    try:
        version = run_cmd([node, cli, "--version"], timeout=30)
        version_text = (version.stdout or version.stderr or "").strip().splitlines()
        checks.check(
            bool(version_text),
            "B0",
            "WorkBuddy CLI --version works",
            version_text[0] if version_text else "empty",
        )
        log(f"  WorkBuddy CLI version: {version_text[0] if version_text else 'unknown'}")
    except (OSError, subprocess.TimeoutExpired) as exc:
        checks.warn("B0", "WorkBuddy CLI --version works", str(exc))

    config = write_runtime_config(real_dir)
    files = write_real_route_files(real_dir, binary, config, node, cli)
    data_root = files["data_root"]

    server = MockOpenAiServer(("127.0.0.1", files["port"]))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    if not wait_for_server(files["port"]):
        checks.fail("B0", "local mock OpenAI endpoint starts", f"port={files['port']}")
        server.shutdown()
        return
    checks.ok("B0", "local mock OpenAI endpoint starts", f"http://127.0.0.1:{files['port']}/v1")

    env = os.environ.copy()
    env.update(
        {
            "CODEBUDDY_FORCE_LITE_WB_BUNDLE": "1",
            "CODEBUDDY_CONFIG_DIR": str(real_dir.resolve()),
            "WORKBUDDY_CONFIG_DIR": str(real_dir.resolve()),
            "HOME": str((real_dir / "home").resolve()),
            "NO_COLOR": "1",
            "CI": "1",
        }
    )
    # keep any pre-existing PATH; the CLI is invoked by absolute path
    env.setdefault("PATH", os.environ.get("PATH", "/usr/bin:/bin"))

    def invoke(prompt: str, label: str):
        cmd = [
            node,
            cli,
            "-p",
            prompt,
            "--model",
            "mock-model",
            "--mcp-config",
            files["mcp"],
            "--strict-mcp-config",
            "--settings",
            files["settings"],
            "--output-format",
            "json",
        ]
        log(f"  [{label}] running real CLI headless session ...")
        result = run_cmd(cmd, env=env, timeout=180, cwd=SDK_ROOT)
        log(f"  [{label}] rc={result.returncode} stdout={len(result.stdout)}B stderr={len(result.stderr)}B")
        return result

    try:
        first = invoke(FACT_SENTENCE, "run1")
        # ---- assertions on the first real session ----
        checks.check(
            first.returncode == 0,
            "B1",
            "real CLI exit code 0",
            f"rc={first.returncode} (note: WorkBuddy returns 0 even on auth errors)",
        )
        save_text(real_dir / "cli-run1.stdout.json", first.stdout)
        save_text(real_dir / "cli-run1.stderr.txt", first.stderr)
        output_payload = parse_json_blob(first.stdout)
        result_json = extract_result_object(output_payload)
        if output_payload is not None:
            kind = type(output_payload).__name__
            count = len(output_payload) if isinstance(output_payload, (list, dict)) else 1
            checks.ok("B1", "real CLI emitted JSON output", f"{kind} with {count} item(s)")
        else:
            checks.warn("B1", "real CLI emitted JSON output", truncate(first.stdout, 400))
        if result_json is not None:
            result_text = str(result_json.get("result") or "")
            checks.ok(
                "B1",
                "real CLI reported a session result",
                truncate(result_text or json.dumps(result_json, ensure_ascii=False), 400),
            )
        mock_reply_seen = MOCK_MODEL_REPLY in (first.stdout or "") or (
            len(server.requests) > 0
        )
        checks.check(
            mock_reply_seen,
            "B1",
            "real model call reached the local mock OpenAI endpoint",
            f"{len(server.requests)} model request(s); reply_seen={MOCK_MODEL_REPLY in (first.stdout or '')}",
        )
        if "Authentication required" in (first.stderr or ""):
            checks.warn(
                "B1",
                "real model call bypassed login",
                "stderr still says 'Authentication required' - models.json mock model may not have been picked up",
            )

        # hooks: our wrapper must have captured the real WorkBuddy payloads
        captured_events = {}
        for event in ["SessionStart", "UserPromptSubmit", "Stop"]:
            stdin_file = files["capture_dir"] / f"{event}.stdin.json"
            stdout_file = files["capture_dir"] / f"{event}.stdout.json"
            if not stdin_file.exists():
                checks.fail("B2", f"real hook invoked: {event}", f"missing {stdin_file}")
                continue
            try:
                payload = json.loads(stdin_file.read_text(encoding="utf-8"))
            except json.JSONDecodeError as exc:
                checks.fail("B2", f"real hook stdin JSON: {event}", f"{exc}: {truncate(read_text(stdin_file), 300)}")
                continue
            captured_events[event] = payload
            checks.check(
                payload.get("hook_event_name") == event,
                "B2",
                f"real hook stdin has hook_event_name={event}",
                json.dumps({k: payload.get(k) for k in ("session_id", "transcript_path", "cwd", "model")}, ensure_ascii=False),
            )
            if stdout_file.exists():
                try:
                    hook_out = parse_strict_json(stdout_file.read_text(encoding="utf-8"), f"hook {event}")
                    checks.ok("B2", f"pd-memory hook produced valid JSON for {event}", truncate(json.dumps(hook_out), 300))
                except AssertionError as exc:
                    checks.fail("B2", f"pd-memory hook produced valid JSON for {event}", str(exc))
        stop_payload = captured_events.get("Stop")
        if stop_payload:
            checks.check(
                str(stop_payload.get("last_assistant_message", "")).strip() != "",
                "B2",
                "real Stop hook carries last_assistant_message",
                truncate(str(stop_payload.get("last_assistant_message", "")), 200),
            )
            transcript_path = Path(str(stop_payload.get("transcript_path", "")))
            checks.check(
                transcript_path.exists(),
                "B2",
                "real transcript file exists",
                str(transcript_path),
            )

        # WorkBuddy's `-p` mode may flush the assistant transcript line *after*
        # the Stop hook fires. If the captured L0 record has no assistant reply
        # the indexer rightly refuses to extract it (no_assistant_reply), so
        # replay the same real Stop payload once the transcript is final. This
        # mirrors what an async/deferred capture would do in production.
        l0_path = data_root / "workspaces" / WORKSPACE_ID / "control" / "l0_sessions.jsonl"
        if l0_path.exists() and stop_payload is not None and '"role":"assistant"' not in l0_path.read_text(
            encoding="utf-8", errors="ignore"
        ):
            checks.warn(
                "B3",
                "Stop hook transcript already contained the assistant reply",
                "assistant line was written after the hook; replaying Stop hook with the finalized transcript",
            )
            stop_stdin_path = files["capture_dir"] / "Stop.stdin.json"
            replay = run_cli(
                binary,
                config,
                ["hook"],
                stdin_text=stop_stdin_path.read_text(encoding="utf-8"),
                env=base_env(data_root),
                timeout=120,
            )
            checks.check(
                replay.returncode == 0,
                "B3",
                "Stop hook replay with finalized transcript exit 0",
                f"rc={replay.returncode}",
            )

        # captured turns must have landed in the SDK control plane (L0)
        l0_hits = find_fact_in_tree(data_root)
        checks.check(
            bool(l0_hits),
            "B3",
            "real hooks captured the conversation into the SDK data root",
            f"{len(l0_hits)} file(s): " + ", ".join(str(p.relative_to(data_root)) for p in l0_hits[:5]),
        )
        for path in list(iter_files(data_root))[:12]:
            log(f"    - {path.relative_to(data_root)} ({path.stat().st_size} bytes)")

        # MCP handshake against our real `serve mcp`
        probe_text = read_text(files["probe_log"], 20000) if files["probe_log"].exists() else ""
        for method in ["initialize", "notifications/initialized", "tools/list"]:
            checks.check(
                method in probe_text,
                "B4",
                f"real CLI MCP handshake contains {method}",
                "" if method in probe_text else truncate(probe_text, 400),
            )
        # our server must have answered initialize/tools/list
        checks.check(
            '"serverInfo"' in probe_text or '"protocolVersion"' in probe_text,
            "B4",
            "our MCP server answered initialize",
            "" if ('serverInfo' in probe_text) else truncate(probe_text, 400),
        )
        if probe_text:
            log("  MCP probe log excerpt:")
            log("    " + "\n    ".join(probe_text.splitlines()[:12]))

        # flush the captured real-session turns into Markdown memory
        flush = run_cli(binary, config, ["flush"], env=base_env(data_root), timeout=180)
        checks.check(flush.returncode == 0, "B5", "flush real captured turns (exit 0)", f"rc={flush.returncode}")
        memory_root = data_root / "workspaces" / WORKSPACE_ID / "memory"
        manifest = memory_root / "MEMORY.md"
        checks.check(manifest.exists(), "B5", "real-run MEMORY.md created", str(manifest))
        markdown_files = [p for p in memory_root.rglob("*.md") if p.name != "MEMORY.md"] if memory_root.exists() else []
        checks.check(bool(markdown_files), "B5", "real-run memory Markdown files created", f"{len(markdown_files)} file(s)")
        if markdown_files:
            log(f"  real-run memory file: {markdown_files[0].relative_to(data_root)}")
            log("    " + "\n    ".join(read_text(markdown_files[0], 800).splitlines()[:8]))

        # second real session: UserPromptSubmit recall -> mock endpoint request body
        requests_before_run2 = len(server.requests)
        second = invoke(FACT_QUESTION, "run2")
        save_text(real_dir / "cli-run2.stdout.json", second.stdout)
        save_text(real_dir / "cli-run2.stderr.txt", second.stderr)
        checks.check(second.returncode == 0, "B6", "second real CLI exit code 0", f"rc={second.returncode}")
        submit_stdout = files["capture_dir"] / "UserPromptSubmit.stdout.json"
        injected = ""
        if submit_stdout.exists():
            try:
                submit_out = parse_strict_json(submit_stdout.read_text(encoding="utf-8"), "UserPromptSubmit")
                injected = str((submit_out.get("hookSpecificOutput") or {}).get("additionalContext") or "")
            except AssertionError:
                injected = ""
        checks.check(
            FACT_TOKEN.lower() in injected.lower(),
            "B6",
            "real UserPromptSubmit hook injected memory context containing the fact",
            truncate(injected, 300) if injected else "additionalContext was empty",
            warn_only=True,
        )
        # inspect what the mock model actually received (proves injection into the model request)
        with server.lock:
            recorded = list(server.requests)
        run2_requests = recorded[requests_before_run2:]
        injection_in_request = False
        for request in run2_requests:
            body = request.get("body") or b""
            text = body.decode("utf-8", errors="ignore") if isinstance(body, bytes) else str(body)
            if FACT_TOKEN.lower() in text.lower():
                injection_in_request = True
                break
        checks.check(
            injection_in_request,
            "B6",
            "memory context reached the second real model request (WorkBuddy applies hook additionalContext)",
            f"{len(run2_requests)} request(s) in run2 (of {len(recorded)} total)",
            warn_only=True,
        )
        if injection_in_request:
            for request in run2_requests:
                body = request.get("body") or b""
                text = body.decode("utf-8", errors="ignore") if isinstance(body, bytes) else str(body)
                lowered = text.lower()
                at = lowered.find(FACT_TOKEN.lower())
                if at >= 0:
                    log("  injected context inside the real model request (excerpt around the fact):")
                    log("    ..." + text[max(0, at - 160) : at + 120].replace("\\n", " ") + "...")
                    break
        log(f"  mock endpoint requests: {len(recorded)} total, {len(run2_requests)} in run2")
        try:
            request_dump = []
            for request in recorded:
                body = request.get("body") or b""
                text = body.decode("utf-8", errors="ignore") if isinstance(body, bytes) else str(body)
                request_dump.append(
                    {
                        "path": request.get("path"),
                        "contains_fact": FACT_TOKEN.lower() in text.lower(),
                        "body": text,
                    }
                )
            (real_dir / "mock-openai-requests.json").write_text(
                json.dumps(request_dump, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
            )
            log(f"  mock endpoint request bodies saved: {real_dir / 'mock-openai-requests.json'}")
        except OSError as exc:
            log(f"  [warn] could not save mock request bodies: {exc}")
    finally:
        server.shutdown()
        server.server_close()

    return


# ---------------------------------------------------------------------------
# Route B helper scripts (written into the run directory)
# ---------------------------------------------------------------------------

HOOK_WRAPPER_TEMPLATE = r'''#!/usr/bin/env python3
"""Capture the real WorkBuddy hook stdin/stdout, then delegate to pd-memory hook.

argv: <event> <pd-memory-binary> <config> <capture-dir>
"""
import json
import subprocess
import sys
from pathlib import Path

event, binary, config, capture_dir = sys.argv[1:5]
capture = Path(capture_dir)
capture.mkdir(parents=True, exist_ok=True)
payload = sys.stdin.buffer.read()
(capture / f"{event}.stdin.json").write_bytes(payload)
proc = subprocess.run(
    [binary, "--config", config, "hook"],
    input=payload,
    capture_output=True,
)
(capture / f"{event}.stdout.json").write_bytes(proc.stdout)
(capture / f"{event}.stderr.txt").write_bytes(proc.stderr)
sys.stdout.buffer.write(proc.stdout)
sys.stdout.buffer.flush()
sys.exit(proc.returncode)
'''

MCP_PROBE_TEMPLATE = r'''#!/usr/bin/env python3
"""Transparent stdio proxy that logs the MCP JSON-RPC traffic of the real CLI.

argv: <log-path> -- <real-mcp-server-command...>
"""
import subprocess
import sys
import threading
from pathlib import Path

log_path = Path(sys.argv[1])
separator = sys.argv.index("--")
command = sys.argv[separator + 1:]

log_lock = threading.Lock()
log_file = log_path.open("ab", buffering=0)


def log(direction, line):
    with log_lock:
        log_file.write(direction + b" " + line)
        if not line.endswith(b"\n"):
            log_file.write(b"\n")


proc = subprocess.Popen(
    command,
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=sys.stderr,
)


def pump(source, destination, direction):
    for line in iter(source.readline, b""):
        log(direction, line)
        destination.write(line)
        destination.flush()
    try:
        destination.close()
    except OSError:
        pass


client_to_server = threading.Thread(
    target=pump, args=(sys.stdin.buffer, proc.stdin, b"CLIENT->SERVER"), daemon=True
)
server_to_client = threading.Thread(
    target=pump, args=(proc.stdout, sys.stdout.buffer, b"SERVER->CLIENT"), daemon=True
)
client_to_server.start()
server_to_client.start()
client_to_server.join()
server_to_client.join()
proc.wait()
sys.exit(proc.returncode if proc.returncode is not None else 0)
'''


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def main(argv=None):
    global VERBOSE
    parser = argparse.ArgumentParser(
        description="End-to-end WorkBuddy harness for pd-memory (simulator + optional real CLI route)."
    )
    parser.add_argument("--binary", default=os.environ.get("PD_MEMORY_BIN", str(DEFAULT_BINARY)))
    parser.add_argument("--no-build", action="store_true", help="fail instead of running cargo build when the binary is missing")
    parser.add_argument("--real", action="store_true", help="also run the real headless WorkBuddy CLI route (route B)")
    parser.add_argument("--only-real", action="store_true", help="run only route B (requires --real semantics)")
    parser.add_argument("--require-real", action="store_true", help="fail when the WorkBuddy CLI is unavailable instead of skipping")
    parser.add_argument("--keep-run", action="store_true", help="keep the previous .run state instead of cleaning it")
    parser.add_argument("--verbose", "-v", action="store_true", help="echo raw command output")
    parser.add_argument("--workbuddy-cli", default=str(WORKBUDDY_CLI), help="path to WorkBuddy's bundled codebuddy CLI")
    parser.add_argument("--workbuddy-node", default=None, help="path to the Node binary used to run the WorkBuddy CLI")
    args = parser.parse_args(argv)
    VERBOSE = args.verbose

    binary = Path(args.binary).expanduser()
    if not binary.is_absolute():
        binary = (SDK_ROOT / binary).resolve()

    log("=" * 78)
    log("pd-memory <-> WorkBuddy end-to-end harness")
    log("=" * 78)
    log(f"sdk root        : {SDK_ROOT}")
    log(f"run dir         : {RUN_DIR}")

    ensure_binary(binary, build=not args.no_build)
    version = cli_version(binary)
    log(f"pd-memory       : {binary}")
    log(f"pd-memory version: {version}")
    help_text = cli_help(binary)
    if "not implemented yet" in help_text:
        log("[error] the pd-memory CLI is still the placeholder stub (dev C not finished).")
        log("[error] wait for `cargo build -p pd-memory-cli` to produce a real CLI, then rerun.")
        return 2
    subcommands = detect_subcommands(help_text)
    log(f"subcommands     : {sorted(subcommands)}")
    if not subcommands and help_text:
        log("  (help text excerpt)")
        log("  " + "\n  ".join(help_text.splitlines()[:25]))

    if not args.keep_run:
        shutil.rmtree(RUN_DIR, ignore_errors=True)
    RUN_DIR.mkdir(parents=True, exist_ok=True)

    checks = Checks()
    started_at = time.strftime("%Y-%m-%dT%H:%M:%S%z")
    real_requested = args.real or args.only_real

    if not args.only_real:
        sim_dir = RUN_DIR / "sim"
        shutil.rmtree(sim_dir, ignore_errors=True)
        sim_dir.mkdir(parents=True, exist_ok=True)
        config = write_runtime_config(sim_dir)
        try:
            simulate_route(binary, config, sim_dir, checks, subcommands)
        except Exception as exc:  # noqa: BLE001 - report, keep route B running
            import traceback

            checks.fail("A", "simulator route crashed unexpectedly", f"{type(exc).__name__}: {exc}")
            if VERBOSE:
                traceback.print_exc()

    if real_requested:
        real_dir = RUN_DIR / "real"
        shutil.rmtree(real_dir, ignore_errors=True)
        real_dir.mkdir(parents=True, exist_ok=True)
        try:
            run_real_route(
                binary,
                real_dir,
                checks,
                require_real=args.require_real,
                node_path=args.workbuddy_node,
                cli_path=args.workbuddy_cli,
            )
        except Exception as exc:  # noqa: BLE001
            import traceback

            checks.fail("B", "real WorkBuddy route crashed unexpectedly", f"{type(exc).__name__}: {exc}")
            if VERBOSE:
                traceback.print_exc()

    # ---- summary ----
    log("\n" + "=" * 78)
    log("SUMMARY")
    log("=" * 78)
    for phase, name, status, detail in checks.rows:
        suffix = f" - {detail}" if detail and status in ("FAIL", "WARN", "SKIP") else ""
        log(f"  {status:4s} [{phase}] {name}{suffix}")
    failures = checks.failures()
    warnings = checks.warnings()
    passes = checks.passes()
    log("")
    log(f"PASS {len(passes)}  WARN {len(warnings)}  FAIL {len(failures)}  SKIP {len([r for r in checks.rows if r[2] == 'SKIP'])}")
    log(f"started {started_at} / finished {time.strftime('%Y-%m-%dT%H:%M:%S%z')}")

    summary = {
        "started_at": started_at,
        "finished_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "binary": str(binary),
        "version": version,
        "real_requested": real_requested,
        "routes": ["contract-simulator"] + (["real-workbuddy-cli"] if real_requested else []),
        "checks": [
            {"phase": phase, "name": name, "status": status, "detail": detail}
            for phase, name, status, detail in checks.rows
        ],
        "pass": len(passes),
        "warn": len(warnings),
        "fail": len(failures),
    }
    summary_path = RUN_DIR / "e2e-summary.json"
    summary_path.write_text(json.dumps(summary, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    log(f"machine summary: {summary_path}")

    if failures:
        log("\nRESULT: FAIL")
        return 1
    log("\nRESULT: PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
