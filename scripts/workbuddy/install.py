#!/usr/bin/env python3
"""Install pd-memory's CLI, model config, MCP and hooks. Standard library only."""

import argparse
import copy
import datetime
import fcntl
import getpass
import hashlib
import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import uuid
from contextlib import contextmanager
from pathlib import Path
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[2]
EVENTS = {"SessionStart": 20, "UserPromptSubmit": 120, "Stop": 180}
TOOLS = {
    "memory_add", "memory_dream", "memory_flush", "memory_get",
    "memory_list", "memory_overview", "memory_search",
}
OVERRIDES = (
    "PD_MEMORY_PROVIDER", "PD_MEMORY_DATA_ROOT", "PD_MEMORY_WORKSPACE_ROOT",
    "PD_MEMORY_WORKSPACE_ID", "PD_MEMORY_MODEL", "PD_MEMORY_API_TYPE",
    "PD_MEMORY_BASE_URL", "PD_MEMORY_API_KEY_ENV",
)


class InstallError(Exception):
    pass


def absolute(value):
    return Path(os.path.abspath(os.path.expanduser(str(value))))


def encode(value):
    return (json.dumps(value, ensure_ascii=False, indent=2) + "\n").encode()


def digest(data):
    return hashlib.sha256(data).hexdigest() if data is not None else None


def snapshot(path):
    if path.is_symlink():
        raise InstallError(f"Refusing to replace a symlink: {path}")
    if not path.exists():
        return None, None
    if not path.is_file():
        raise InstallError(f"Expected a regular file: {path}")
    return path.read_bytes(), path.stat().st_mode & 0o777


def read_object(path):
    data, _ = snapshot(path)
    if data is None:
        return {}
    try:
        value = json.loads(data)
    except (ValueError, UnicodeError) as error:
        raise InstallError(f"Invalid JSON in {path}; nothing changed") from error
    if not isinstance(value, dict):
        raise InstallError(f"Expected a JSON object in {path}")
    return value


def config_path(current, field, default, base):
    value = current.get(field)
    if value is None:
        return default
    if not isinstance(value, str) or not value.strip():
        raise InstallError(f"config.{field} must be a non-empty path")
    # Match the CLI: paths in JSON resolve against the config directory.
    return absolute(base / value)


def atomic_write(path, data, mode):
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=".pd-memory-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            os.fchmod(stream.fileno(), mode)
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def merge_mcp(current, wrapper, cli=False):
    result = copy.deepcopy(current)
    servers = result.setdefault("mcpServers", {})
    if not isinstance(servers, dict):
        raise InstallError("mcpServers must be an object")
    servers["pd-memory"] = {
        "type": "stdio", "command": str(wrapper),
        "args": ["serve", "mcp"], "timeout": 120000,
    }
    if cli:
        disabled = result.setdefault("disabledMcpServers", [])
        if not isinstance(disabled, list):
            raise InstallError("disabledMcpServers must be an array")
        result["disabledMcpServers"] = [s for s in disabled if s != "pd-memory"]
    return result


def owned_hook(hook, wrapper, binary):
    if not isinstance(hook, dict):
        raise InstallError("Each hook must be an object")
    # Also adopt the unmarked wrapper hooks from the manual installation.
    try:
        words = shlex.split(hook.get("command", ""))
    except ValueError:
        return False
    # Earlier demo installs prefixed the executable with mock env assignments.
    if words and words[0] == "env":
        words = words[1:]
        while words and "=" in words[0] and not words[0].startswith("-"):
            words = words[1:]
    executables = {
        str(wrapper), str(binary), str(ROOT / "target/debug/pd-memory"),
        str(ROOT / "target/release/pd-memory"),
    }
    return bool(
        words and words[-1] == "hook"
        and words[0] in executables
    )


def merge_hooks(current, wrapper, binary):
    result = copy.deepcopy(current)
    hooks = result.setdefault("hooks", {})
    if not isinstance(hooks, dict):
        raise InstallError("settings.hooks must be an object")
    for event, groups in list(hooks.items()):
        if not isinstance(groups, list):
            raise InstallError(f"settings.hooks.{event} must be an array")
        kept = []
        for group in groups:
            if not isinstance(group, dict) or not isinstance(group.get("hooks"), list):
                raise InstallError(f"Invalid hook group in {event}")
            remaining = [h for h in group["hooks"] if not owned_hook(h, wrapper, binary)]
            if remaining:
                kept.append(dict(group, hooks=remaining))
        hooks[event] = kept
    for event, timeout in EVENTS.items():
        hooks.setdefault(event, []).append({"hooks": [{
            "type": "command", "command": shlex.quote(str(wrapper)) + " hook",
            "timeout": timeout,
        }]})
    return result


def wrapper_script(binary, config, credentials, mock):
    lines = ["#!/bin/sh", "set -eu", "umask 077"]
    lines.append("unset " + " ".join(OVERRIDES))
    if mock:
        lines.extend(["PD_MEMORY_PROVIDER=mock", "export PD_MEMORY_PROVIDER"])
    else:
        lines.extend(["set -a", ". " + shlex.quote(str(credentials)), "set +a"])
    lines.append(
        "exec " + shlex.quote(str(binary)) + " --config "
        + shlex.quote(str(config)) + ' "$@"'
    )
    return ("\n".join(lines) + "\n").encode()


def credential_from_file(path):
    if not path.exists():
        return None
    lines = path.read_text(encoding="utf-8").splitlines()
    if len(lines) != 1 or not lines[0].startswith("PD_MEMORY_API_KEY="):
        raise InstallError(f"Expected one PD_MEMORY_API_KEY assignment in {path}")
    try:
        values = shlex.split(lines[0].split("=", 1)[1])
    except ValueError as error:
        raise InstallError(f"Invalid credential file: {path}") from error
    return values[0] if len(values) == 1 else None


def choose(label, supplied, existing, default, interactive):
    if supplied is not None:
        return supplied
    if existing:
        return existing
    if interactive:
        value = input(f"{label}" + (f" [{default}]" if default else "") + ": ").strip()
        return value or default
    return default


def model_config(args, current, credentials):
    model = copy.deepcopy(current.get("model", {}))
    if not isinstance(model, dict):
        raise InstallError("config.model must be an object")
    interactive = args.apply and not args.non_interactive and sys.stdin.isatty()
    model["api_type"] = choose(
        "API type", args.api_type, model.get("api_type"), "openai-chat", False,
    )
    model["model"] = choose("Model", args.model, model.get("model"), "qwen-plus", interactive)
    model["base_url"] = choose("API base URL", args.base_url, model.get("base_url"), None, interactive)
    if not isinstance(model["model"], str) or not model["model"].strip():
        raise InstallError("Model must be a non-empty string")
    if model["base_url"] is not None and not isinstance(model["base_url"], str):
        raise InstallError("API base URL must be a string")
    if model["api_type"] not in ("openai-chat", "anthropic"):
        raise InstallError("Only openai-chat and anthropic are implemented")
    if args.apply and not args.mock and (not model["model"] or not model["base_url"]):
        raise InstallError("Set --model and --base-url (or run interactively)")
    if model["base_url"]:
        url = urlsplit(model["base_url"])
        if url.scheme not in ("http", "https") or not url.hostname or url.username or url.password:
            raise InstallError("base URL must be an HTTP(S) URL without credentials")
        if url.query or url.fragment:
            raise InstallError("base URL must not contain a query or fragment")
    secret = None
    if args.apply and not args.mock:
        if args.key_file:
            secret = absolute(args.key_file).read_text(encoding="utf-8").strip()
        else:
            secret = (
                os.environ.get(args.api_key_env)
                or credential_from_file(credentials)
                or model.get("api_key")
            )
        if not secret and interactive:
            secret = getpass.getpass("Memory model API key (hidden): ")
        if not isinstance(secret, str) or not secret.strip() or any(c in secret for c in "\r\n\0"):
            raise InstallError("API key required: use --key-file, --api-key-env, or the hidden prompt")
    model.pop("api_key", None)
    model["api_key_env"] = "PD_MEMORY_API_KEY"
    return model, secret


def run_json(command, payload=None, timeout=30, allow_mock=False):
    result = subprocess.run(
        command, input=json.dumps(payload) if payload is not None else None,
        capture_output=True, text=True, timeout=timeout,
    )
    if result.returncode:
        # Never echo model errors or HTTP response bodies containing credentials.
        raise InstallError(f"{command[-1]} failed (exit {result.returncode})")
    try:
        value = json.loads(result.stdout)
    except ValueError as error:
        raise InstallError("Installed command did not return JSON") from error
    if not allow_mock and "MockProvider" in result.stderr:
        raise InstallError("Unexpected fallback to MockProvider")
    return value


def verify_local(wrapper, mock):
    # Verify in an isolated root; installation must not create test memories.
    with tempfile.TemporaryDirectory(prefix="pd-memory-check-") as temporary:
        command = [str(wrapper), "--data-root", temporary]
        overview = run_json(command + ["overview"], allow_mock=mock)
        if overview.get("total_files") != 0:
            raise InstallError("Unexpected memory in verification directory")
        hook = run_json(command + ["hook"], {
            "hook_event_name": "SessionStart", "session_id": "installer-check",
        }, allow_mock=mock)
        if hook.get("continue") is not True:
            raise InstallError("Hook contract check failed")
        requests = [
            {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "pd-memory-installer", "version": "1"},
            }},
            {"jsonrpc": "2.0", "method": "notifications/initialized"},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
            {"jsonrpc": "2.0", "id": 3, "method": "tools/call",
             "params": {"name": "memory_overview", "arguments": {}}},
        ]
        result = subprocess.run(
            command + ["serve", "mcp"], input="".join(json.dumps(r) + "\n" for r in requests),
            text=True, capture_output=True, timeout=30,
        )
        if result.returncode or (not mock and "MockProvider" in result.stderr):
            raise InstallError("MCP startup failed")
        responses = {r.get("id"): r for r in map(json.loads, result.stdout.splitlines())}
        if "result" not in responses.get(1, {}):
            raise InstallError("MCP initialize failed")
        listed = responses.get(2, {}).get("result", {}).get("tools", [])
        if {t["name"] for t in listed} != TOOLS:
            raise InstallError("MCP must expose exactly the seven memory tools")
        if "result" not in responses.get(3, {}) or responses[3]["result"].get("isError"):
            raise InstallError("MCP memory_overview failed")
    print("[check] MCP initialize, 7 tools, tool call and SessionStart hook passed")


def verify_model(wrapper):
    fact = "Boreal 项目的内部代号固定为 Nacre-731，全部交付文档必须使用中文。请长期记住这两条项目规则。"
    with tempfile.TemporaryDirectory(prefix="pd-memory-model-check-") as temporary:
        command = [str(wrapper), "--data-root", temporary]
        transcript = Path(temporary) / "transcript.jsonl"
        transcript.write_text(json.dumps({
            "type": "user", "message": {"role": "user", "content": fact},
        }, ensure_ascii=False) + "\n", encoding="utf-8")
        run_json(command + ["hook"], {"hook_event_name": "Stop",
            "session_id": "installer-model-check", "transcript_path": str(transcript),
            "last_assistant_message": "已记住这两条项目规则。",
        }, timeout=240)
        stats = run_json(command + ["flush", "--reason", "installer-model-check"], timeout=240)
        if stats.get("errors"):
            raise InstallError("Model indexing failed; check endpoint, model and key")
        listing = run_json(command + ["list"])
        if not listing.get("count"):
            raise InstallError("Model check produced no memory files")
        recall = run_json(command + ["search", "Boreal 项目之前约定的代号和文档语言是什么？"], timeout=120)
        if "Nacre-731" not in json.dumps(recall, ensure_ascii=False):
            raise InstallError("Model search did not recall the test fact")
        hook = run_json(command + ["hook"], {
            "hook_event_name": "UserPromptSubmit", "session_id": "installer-model-check",
            "prompt": "Boreal 项目之前约定的代号和文档语言是什么？",
        }, timeout=120)
        if "Nacre-731" not in hook.get("hookSpecificOutput", {}).get("additionalContext", ""):
            raise InstallError("Model hook did not inject the test fact")
    print("[check] real model: Stop capture -> flush -> search -> hook context passed")


def restore(backup, manifest):
    # Preflight every file, so a conflict never causes a partial rollback.
    for item in manifest["files"]:
        path = Path(item["path"])
        current, mode = snapshot(path)
        if digest(current) != item["after_sha256"] or mode != item["after_mode"]:
            raise InstallError(f"Changed since installation; nothing restored: {path}")
        if item["backup"]:
            before = (backup / item["backup"]).read_bytes()
            if digest(before) != item["before_sha256"]:
                raise InstallError(f"Invalid backup for {path}")
    for item in reversed(manifest["files"]):
        path = Path(item["path"])
        if item["backup"]:
            atomic_write(path, (backup / item["backup"]).read_bytes(), item["before_mode"])
        else:
            path.unlink()
    manifest["status"] = "rolled-back"
    atomic_write(backup / "manifest.json", encode(manifest), 0o600)


def apply_changes(changes, state, verifier, expected=None):
    changed = []
    for path, data, mode in changes:
        before, before_mode = snapshot(path)
        if expected is not None and (before, before_mode) != expected[path]:
            raise InstallError(f"Configuration changed during installation: {path}")
        if before != data or before_mode != mode:
            changed.append((path, data, mode, before, before_mode))
    if not changed:
        verifier()
        print("[result] already installed; no configuration changes")
        return None
    backup = state / (datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ-") + uuid.uuid4().hex[:8])
    backup.mkdir(mode=0o700)
    manifest = {"version": 1, "status": "prepared", "files": []}
    for i, (path, data, mode, before, before_mode) in enumerate(changed):
        name = f"{i}.before" if before is not None else None
        if name:
            atomic_write(backup / name, before, 0o600)
        manifest["files"].append({
            "path": str(path), "backup": name, "before_sha256": digest(before),
            "before_mode": before_mode, "after_sha256": digest(data), "after_mode": mode,
        })
    atomic_write(backup / "manifest.json", encode(manifest), 0o600)
    applied = []
    try:
        for item, change in zip(manifest["files"], changed):
            path, data, mode, before, before_mode = change
            if snapshot(path) != (before, before_mode):
                raise InstallError(f"Configuration changed during installation: {path}")
            atomic_write(path, data, mode)
            applied.append(item)
        verifier()
        manifest["status"] = "installed"
        atomic_write(backup / "manifest.json", encode(manifest), 0o600)
        atomic_write(state / "latest", (str(backup) + "\n").encode(), 0o600)
    except BaseException:
        partial = dict(manifest, files=applied)
        restore(backup, partial)
        manifest["status"] = "rolled-back"
        atomic_write(backup / "manifest.json", encode(manifest), 0o600)
        print("[rollback] installation failed; original files restored", file=sys.stderr)
        raise
    print(f"[backup] {backup}")
    return backup


@contextmanager
def installation_lock(state):
    state.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(state, 0o700)
    with (state / "lock").open("a") as lock:
        os.chmod(state / "lock", 0o600)
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise InstallError("Another pd-memory installer is running") from error
        yield


def arguments(argv=None):
    parser = argparse.ArgumentParser(prog="install.sh", description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--apply", action="store_true", help="build, install and verify (default: preview)")
    mode.add_argument("--dry-run", action="store_true", help="preview only; no writes or model calls")
    mode.add_argument("--rollback", nargs="?", const="latest", metavar="BACKUP",
                      help="restore latest or specified installation; keep memory data")
    parser.add_argument("--home", type=absolute, default=Path.home(), help="installation home (for isolated installs)")
    parser.add_argument("--workbuddy-home", type=absolute)
    parser.add_argument("--workspace", type=absolute, help="default: <workbuddy-home>/workspace")
    parser.add_argument("--data-root", type=absolute)
    parser.add_argument("--mcp-target", choices=("gui", "cli", "both"), default="both")
    parser.add_argument("--binary", type=absolute, help="install this binary instead of building release")
    parser.add_argument("--api-type", choices=("openai-chat", "anthropic"))
    parser.add_argument("--base-url", help="service prefix; openai-chat appends /chat/completions")
    parser.add_argument("--model")
    parser.add_argument("--api-key-env", default="PD_MEMORY_API_KEY", help="read key from this environment variable")
    parser.add_argument("--key-file", help="read a plain API key from this file (never from an argument)")
    parser.add_argument("--non-interactive", action="store_true", help="fail rather than prompt for missing values")
    parser.add_argument("--mock", action="store_true", help="explicit offline demo; NOT a real memory model")
    parser.add_argument("--verify-model", action="store_true", help="make model calls with isolated synthetic test data")
    args = parser.parse_args(argv)
    if args.mock and args.verify_model:
        parser.error("--verify-model requires a real model; remove --mock")
    return args


def install(args):
    home = args.home
    wb = args.workbuddy_home or home / ".workbuddy"
    private = home / ".config/pd-memory"
    state = home / ".local/state/pd-memory/installations"
    binary = home / ".local/bin/pd-memory"
    wrapper = binary.with_name("pd-memory-workbuddy")
    config = private / "config.json"
    credentials = private / "model.env"
    if args.rollback:
        if args.rollback == "latest" and not (state / "latest").is_file():
            raise InstallError(f"No installation backup found under {state}")
        backup = absolute(args.rollback) if args.rollback != "latest" else Path(
            (state / "latest").read_text(encoding="utf-8").strip()
        )
        manifest = read_object(backup / "manifest.json")
        if manifest.get("version") != 1 or manifest.get("status") != "installed":
            raise InstallError("Not an installed pd-memory backup")
        with installation_lock(state):
            restore(backup, manifest)
        print("[rollback] original files restored; memory data retained. Restart WorkBuddy.")
        return
    mcp_names = ["mcp.json", ".mcp.json"] if args.mcp_target == "both" else [
        ".mcp.json" if args.mcp_target == "cli" else "mcp.json",
    ]
    expected = {p: snapshot(p) for p in [
        binary, wrapper, config, credentials, wb / "settings.json",
        *(wb / name for name in mcp_names),
    ]}
    current = read_object(config)
    model, secret = model_config(args, current, credentials)
    data = args.data_root or config_path(current, "data_root", home / ".local/share/pd-memory", private)
    workspace = args.workspace or config_path(current, "workspace_root", wb / "workspace", private)
    updated = copy.deepcopy(current)
    updated.update(data_root=str(data), workspace_root=str(workspace), workspace_mode="single", model=model)
    updated.pop("workspace_id", None)
    changes = [
        (config, encode(updated), 0o600),
        (wrapper, wrapper_script(binary, config, credentials, args.mock), 0o755),
    ]
    if secret:
        changes.append((credentials, ("PD_MEMORY_API_KEY=" + shlex.quote(secret) + "\n").encode(), 0o600))
    for name in mcp_names:
        path = wb / name
        changes.append((path, encode(merge_mcp(read_object(path), wrapper, name == ".mcp.json")), 0o600))
    settings = wb / "settings.json"
    changes.append((settings, encode(merge_hooks(read_object(settings), wrapper, binary)), 0o600))
    print(f"[plan] model={model['model']} api={model['api_type']} mode={'MOCK DEMO' if args.mock else 'real'}")
    print(f"[plan] workspace={workspace}\n[plan] data={data}\n[plan] binary={binary}")
    for path, _, _ in changes:
        snapshot(path)
        print(f"[plan] manage {path}")
    if not args.apply:
        print("[result] preview only. Use --apply; credentials are never displayed.")
        return
    if not workspace.is_dir():
        raise InstallError(f"Workspace does not exist: {workspace}; create it or use --workspace")
    source = args.binary
    if source is None:
        if not shutil.which("cargo"):
            raise InstallError("Rust/Cargo required; install it or supply --binary")
        subprocess.run(["cargo", "build", "--locked", "--release", "-p", "pd-memory-cli"],
                       cwd=ROOT, check=True)
        source = ROOT / "target/release/pd-memory"
    if not source.is_file() or not os.access(source, os.X_OK):
        raise InstallError(f"Executable not found: {source}")
    version = subprocess.run([str(source), "--version"], text=True, capture_output=True,
                             timeout=10, check=True).stdout.strip()
    if not version.startswith("pd-memory "):
        raise InstallError("Selected binary is not pd-memory")
    changes.insert(0, (binary, source.read_bytes(), 0o755))
    private.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(private, 0o700)
    with installation_lock(state):
        def verify():
            verify_local(wrapper, args.mock)
            if args.verify_model:
                verify_model(wrapper)
        apply_changes(changes, state, verify, expected)
    print("[result] installed. Restart WorkBuddy; open Experts/Skills/Connectors ->")
    print("         Connectors -> Custom connector; trust pd-memory and confirm 7/7 tools.")
    if not args.mock and not args.verify_model:
        print("[note] model connectivity NOT checked; add --verify-model to validate it.")


def main(argv=None):
    try:
        install(arguments(argv))
        return 0
    except (InstallError, OSError, ValueError, subprocess.SubprocessError, EOFError) as error:
        message = str(error) if isinstance(error, InstallError) else type(error).__name__
        print(f"[error] {message}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("[error] interrupted", file=sys.stderr)
        return 130


if __name__ == "__main__":
    sys.exit(main())
