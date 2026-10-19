#!/usr/bin/env python3
"""Rust side of the PD memory parity harness.

Runs ONE deterministic scenario (the same scenarios.json the TS runner reads)
through the `pd-memory` CLI with a JSON config pointing at the parity mock
model, then writes a normalized artifact JSON for `compare.py`.

Usage:
    python3 rust_runner.py --scenario P1 --run-dir <dir> --port <mock-port>
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
SDK_ROOT = HERE.parents[1]
DEFAULT_BINARY = SDK_ROOT / "target" / "debug" / "pd-memory"

STEP_TIMEOUT_SECONDS = 300


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scenario", required=True)
    parser.add_argument("--scenarios", default=str(HERE / "scenarios.json"))
    parser.add_argument("--run-dir", required=True)
    parser.add_argument("--port", required=True, type=int)
    parser.add_argument("--binary", default=str(DEFAULT_BINARY))
    parser.add_argument("--out", default=None)
    args = parser.parse_args(argv)
    # `abspath` (not `resolve`) on purpose: path strings must be byte-identical
    # to the ones the TS runner passes, and node's `path.resolve` does not
    # resolve symlinks (e.g. /tmp -> /private/tmp on macOS). The workspace path
    # string is hashed into the workspace key on both sides.
    args.run_dir = Path(os.path.abspath(args.run_dir))
    if not args.out:
        args.out = args.run_dir / "rust" / "artifact.json"
    args.out = Path(args.out)
    return args


def walk_files(root: Path) -> list[dict[str, str]]:
    if not root.exists():
        return []
    files: list[dict[str, str]] = []
    for path in sorted(root.rglob("*")):
        if not path.is_file():
            continue
        files.append(
            {
                "path": path.relative_to(root).as_posix(),
                "content": path.read_text(encoding="utf-8"),
            }
        )
    return files


def read_l0(control_dir: Path) -> list[dict]:
    """Parse the pure-Rust JSONL L0 queue into the TS sqlite row shape."""
    sessions = control_dir / "l0_sessions.jsonl"
    if not sessions.exists():
        return []
    records: list[dict] = []
    for line in sessions.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            raw = json.loads(line)
        except ValueError:
            continue
        records.append(
            {
                "l0IndexId": raw.get("l0_index_id"),
                "sessionKey": raw.get("session_key"),
                "timestamp": raw.get("timestamp"),
                "messages": raw.get("messages", []),
                "source": raw.get("source", ""),
                "indexed": raw.get("indexed"),
                "createdAt": raw.get("created_at"),
            }
        )
    records.sort(key=lambda item: (item.get("timestamp") or "", item.get("createdAt") or ""))
    return records


def read_json(path: Path, default=None):
    return json.loads(path.read_text(encoding="utf-8")) if path.exists() else default


def model_requests(path: Path, offset: int) -> list[dict]:
    if not path.exists():
        return []
    records = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]
    return records[offset:]


def workspace_hash(workspace_path: Path) -> str:
    import hashlib

    return hashlib.sha1(str(workspace_path).encode("utf-8")).hexdigest()[:10]


def write_config(path: Path, data_root: Path, workspace_path: Path, port: int) -> None:
    config = {
        "data_root": str(data_root),
        "workspace_root": str(workspace_path),
        "capture_strategy": "last_turn",
        "include_assistant": True,
        "max_message_chars": 6000,
        "heartbeat_batch_size": 30,
        "control_plane": "json",
        "model": {
            "api_type": "openai-chat",
            "provider": "mock",
            "model": "mock-model",
            "base_url": f"http://127.0.0.1:{port}/v1",
            "api_key": "sk-mock",
            "timeout_ms": 10000,
            "max_retries": 0,
        },
        "retrieval": {"cache_ttl_ms": 0},
        "redaction": {"enabled": True},
    }
    path.write_text(json.dumps(config, indent=2) + "\n", encoding="utf-8")


class CliRunner:
    def __init__(self, binary: Path, run_dir: Path) -> None:
        self.binary = binary
        self.run_dir = run_dir
        # Keep the CLI isolated from host memory configuration.
        self.env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith(("PD_MEMORY_", "PARITY_"))
        }

    def run(
        self,
        config: Path,
        args: list[str],
        stdin_text: str = "",
        cwd: Path | None = None,
    ) -> dict:
        command = [str(self.binary), "--config", str(config), *args]
        try:
            proc = subprocess.run(
                command,
                input=stdin_text,
                text=True,
                capture_output=True,
                timeout=STEP_TIMEOUT_SECONDS,
                cwd=str(cwd or self.run_dir),
                env=self.env,
            )
        except subprocess.TimeoutExpired:
            return {"ok": False, "exitCode": None, "error": "timeout", "stdout": "", "stderr": ""}
        parsed = None
        stdout = proc.stdout.strip()
        if stdout:
            try:
                parsed = json.loads(stdout)
            except ValueError:
                parsed = stdout
        return {
            "ok": proc.returncode == 0,
            "exitCode": proc.returncode,
            "stdout": parsed,
            "stderr": proc.stderr.strip(),
        }


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    spec = json.loads(Path(args.scenarios).read_text(encoding="utf-8"))["scenarios"][args.scenario]

    rust_root = args.run_dir / "rust" / "root"
    rust_root.mkdir(parents=True, exist_ok=True)

    workspaces: dict[str, dict] = {}
    for alias, workspace_spec in spec["workspaces"].items():
        workspace_path = Path(os.path.abspath(os.path.join(str(args.run_dir), workspace_spec["path"])))
        workspace_path.mkdir(parents=True, exist_ok=True)
        config_path = args.run_dir / "rust" / f"config-{alias}.json"
        write_config(config_path, rust_root, workspace_path, args.port)
        workspaces[alias] = {
            "alias": alias,
            "workspacePath": str(workspace_path),
            "hash": workspace_hash(workspace_path),
            "configPath": str(config_path),
            "results": {},
            "files": [],
            "l0": [],
        }

    runner = CliRunner(Path(args.binary), args.run_dir)
    failures: list[str] = []

    for step in spec["steps"]:
        alias = step["ws"]
        workspace = workspaces[alias]
        config = workspace["configPath"]
        op = step["op"]
        request_log = args.run_dir / "mock" / "requests.jsonl"
        request_offset = len(model_requests(request_log, 0))
        data_dir = rust_root / "workspaces" / workspace["hash"]
        result: dict
        if op == "runtime":
            proc = subprocess.run([str(Path(args.binary).parent / "examples" / "parity-runtime"), config],
                input=json.dumps(step, ensure_ascii=False), text=True, capture_output=True, timeout=120, env=runner.env)
            result = {"op": op, "ok": proc.returncode == 0, "exitCode": proc.returncode,
                "stdout": json.loads(proc.stdout) if proc.returncode == 0 else None, "stderr": proc.stderr}
        elif op == "capture":
            messages = json.dumps(step["messages"], ensure_ascii=False)
            result = runner.run(
                config,
                [
                    "add",
                    "--session",
                    step["session"],
                    "--timestamp",
                    step["timestamp"],
                    "--source",
                    step.get("source", "parity"),
                ],
                stdin_text=messages,
                cwd=Path(workspace["workspacePath"]),
            )
            result["op"] = "capture"
        elif op == "flush":
            result = runner.run(config, ["flush", "--reason", step.get("reason", "manual")])
            result["op"] = "flush"
        elif op == "list":
            result = runner.run(config, ["list"])
            result["op"] = "list"
        elif op == "overview":
            result = runner.run(config, ["overview"])
            result["op"] = "overview"
        elif op == "retrieve":
            hook_input = json.dumps(
                {
                    "hook_event_name": "UserPromptSubmit",
                    "session_id": step.get("session", "sess-1"),
                    "transcript_path": "",
                    "cwd": workspace["workspacePath"],
                    "prompt": step["query"],
                },
                ensure_ascii=False,
            )
            result = runner.run(
                config,
                ["hook"],
                stdin_text=hook_input,
                cwd=Path(workspace["workspacePath"]),
            )
            result["op"] = "retrieve"
        elif op == "search":
            result = runner.run(config, ["search", step["query"]])
            result["op"] = "search"
        elif op == "files":
            # No CLI invocation: snapshot the workspace memory tree as-is so
            # post-dream / post-rollback state can be compared mid-scenario.
            data_dir = rust_root / "workspaces" / workspace["hash"]
            result = {
                "op": "files",
                "ok": True,
                "files": walk_files(data_dir / "memory"),
            }
        elif op == "dream":
            result = runner.run(config, ["dream", "--trigger", step.get("trigger", "manual")])
            result["op"] = "dream"
        elif op == "rollback":
            result = runner.run(config, ["rollback"])
            result["op"] = "rollback"
            # A missing snapshot is an expected, comparable outcome, not a crash.
        else:
            raise SystemExit(f"unknown op {op}")
        state = read_json(data_dir / "control" / "pipeline_state.json", {})
        result["profilePresent"] = (rust_root / "global" / "UserIdentity" / "user-profile.md").is_file()
        result["runtimeState"] = {key: state[key] for key in
            ("lastDreamAt", "lastDreamStatus", "lastDreamSummary") if state.get(key) is not None}
        result["dreamSnapshot"] = read_json(data_dir / "last_dream" / "metadata.json")
        result["modelRequests"] = model_requests(request_log, request_offset)
        if op == "retrieve" and result["ok"]:
            trace = state["retrievalTraces"][0]
            result["stdout"]["route"] = trace["route"]
            result["stdout"]["selected_ids"] = trace["selected"]
        workspace["results"][step["id"]] = result
        if not result["ok"] and op != "rollback":
            failures.append(f"{step['id']}: exit={result['exitCode']} stderr={result['stderr'][:400]}")

    for alias, workspace in workspaces.items():
        data_dir = rust_root / "workspaces" / workspace["hash"]
        workspace["dataDir"] = str(data_dir)
        workspace["memoryDir"] = str(data_dir / "memory")
        workspace["files"] = walk_files(data_dir / "memory")
        workspace["l0"] = read_l0(data_dir / "control")

    artifact = {
        "side": "rust",
        "scenario": args.scenario,
        "title": spec.get("title", ""),
        "binary": str(args.binary),
        "pythonVersion": sys.version.split()[0],
        "controlPlane": {
            "format": "jsonl",
            "files": ["control/l0_sessions.jsonl", "control/pipeline_state.json"],
        },
        "workspaces": workspaces,
        "globalFiles": walk_files(rust_root / "global"),
    }
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(artifact, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"rust_runner: {args.scenario} -> {args.out}")
    if failures:
        sys.stderr.write(f"rust_runner: {len(failures)} step(s) failed:\n" + "\n".join(failures) + "\n")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
