"""Shared pd-memory installer: binary, model config, backup/rollback, checks.

Standard library only. Each agent host is one module in `hosts/` that supplies
its paths, MCP merge, hooks merge, checks and post-install notes; this module
owns everything else. Entry point: `scripts/install.sh --host <id>`
(`install.py`).

Host module interface (see hosts/workbuddy.py for a complete example). The
module for host id `<id>` is `hosts/<id with - replaced by _>.py`:

    ID, LABEL, DESCRIPTION, WORKSPACE_HELP
    HOOK_ARGS            CLI arguments the hooks run, e.g. ["hook", "--codex"];
                         None for an MCP-only host (no hooks installed)
    STATE_SUBDIR         backup directory under installations/ (None: shared root)
    CONFIG_NAME          private config file name under ~/.config/pd-memory
    WRAPPER_NAME         wrapper script name next to the binary
    REQUIRED_HOOK_FLAG   flag `pd-memory hook --help` must list, or None
    ROLLBACK_CONFLICT_HINT  extra advice when rollback finds edited files, or None
    add_arguments(parser, mode)        host flags (mode: --apply/--rollback group)
    host_paths(args, paths)            add host directories/files to `paths`
    managed_files(args, paths)         host files this installation edits
    load_config(paths)                 current private config (dict)
    default_workspace(args, paths)     workspace when none is configured, or None
    integration_changes(args, paths)   [(path, bytes, mode)] for MCP (+ hooks)
    verify(args, paths, planned)       post-install checks; raise InstallError
    post_install(args)                 print what the user must do next
    uninstall(args, paths)             optional; enables --uninstall
"""

import argparse
import copy
import datetime
import fcntl
import getpass
import hashlib
import importlib
import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import types
import uuid
from contextlib import contextmanager
from pathlib import Path
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[2]
HOSTS = ("workbuddy", "codex", "deepseek-harness")
TOOLS = {
    "memory_add", "memory_dream", "memory_flush", "memory_get",
    "memory_list", "memory_overview", "memory_search",
}
OVERRIDES = (
    "PD_MEMORY_PROVIDER", "PD_MEMORY_DATA_ROOT", "PD_MEMORY_WORKSPACE_ROOT",
    "PD_MEMORY_WORKSPACE_ID", "PD_MEMORY_MODEL", "PD_MEMORY_API_TYPE",
    "PD_MEMORY_BASE_URL", "PD_MEMORY_API_KEY_ENV",
)
SERVER = "pd-memory"
# Synthetic data for --verify-model; written only to temporary data roots.
MODEL_CHECK_FACT = "Boreal 项目的内部代号固定为 Nacre-731，全部交付文档必须使用中文。请长期记住这两条项目规则。"
MODEL_CHECK_QUESTION = "Boreal 项目之前约定的代号和文档语言是什么？"
MODEL_CHECK_ANSWER = "已记住这两条项目规则。"


class InstallError(Exception):
    pass


# --- files -----------------------------------------------------------------

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


def read_text(path):
    data, _ = snapshot(path)
    if data is None:
        return ""
    try:
        return data.decode("utf-8")
    except UnicodeError as error:
        raise InstallError(f"{path} is not UTF-8; nothing changed") from error


def mode_of(path, default=0o600):
    return snapshot(path)[1] or default


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


# --- host configuration merges ---------------------------------------------

def merge_mcp_servers(current, entry, name=SERVER):
    """Set `mcpServers.<name>` in a JSON MCP config, keeping everything else."""
    result = copy.deepcopy(current)
    servers = result.setdefault("mcpServers", {})
    if not isinstance(servers, dict):
        raise InstallError("mcpServers must be an object")
    servers[name] = entry
    return result


def hook_command(wrapper, hook_args):
    return " ".join([shlex.quote(str(wrapper)), *hook_args])


def owned_hook(hook, executables, host_id):
    """Whether `hook` runs pd-memory's hook (ours to replace).

    Matches `<executable> [global options] hook`, `... hook --host <id>` and
    the alias `... hook --<id>`, also behind `env VAR=value` prefixes left by
    earlier demo installs. `executables` are paths that count as pd-memory.
    """
    if not isinstance(hook, dict):
        raise InstallError("Each hook must be an object")
    command = hook.get("command", "")
    try:
        words = shlex.split(command) if isinstance(command, str) else []
    except ValueError:
        return False
    if words and words[0] == "env":
        words = words[1:]
        while words and "=" in words[0] and not words[0].startswith("-"):
            words = words[1:]
    tails = (["hook"], ["hook", f"--{host_id}"], ["hook", "--host", host_id])
    return bool(
        words and words[0] in {str(path) for path in executables}
        and any(words[-len(tail):] == tail for tail in tails)
    )


def default_executables(wrapper, binary):
    return {wrapper, binary, ROOT / "target/debug/pd-memory", ROOT / "target/release/pd-memory"}


def merge_hooks(current, events, make_hook, is_owned, *, in_place, install=True, where="hooks"):
    """Install (or remove) one pd-memory hook group per event; keep the rest.

    `events` maps event name -> timeout, `make_hook(event)` builds our handler
    and `is_owned(handler)` recognizes earlier pd-memory handlers.

    - `in_place=False` (append): drop owned handlers everywhere, drop groups
      left empty, then append our group to every event.
    - `in_place=True`: hosts that key hook trust by position (Codex) keep our
      existing group's index; an event emptied by removal is deleted.
    """
    result = copy.deepcopy(current)
    if not install and "hooks" not in result:
        return result
    hooks = result.setdefault("hooks", {})
    if not isinstance(hooks, dict):
        raise InstallError(f"{where} must be an object")
    placed = set()
    for event, groups in list(hooks.items()):
        if not isinstance(groups, list):
            raise InstallError(f"{where}.{event} must be an array")
        kept = []
        for group in groups:
            if not isinstance(group, dict) or not isinstance(group.get("hooks"), list):
                raise InstallError(f"Invalid hook group in {where}.{event}")
            owned = [h for h in group["hooks"] if is_owned(h)]
            remaining = [h for h in group["hooks"] if not is_owned(h)]
            if not in_place:
                if remaining:
                    kept.append(dict(group, hooks=remaining))
            elif install and event in events and event not in placed and owned and not remaining:
                kept.append({"hooks": [make_hook(event)]})
                placed.add(event)
            elif remaining:
                kept.append(dict(group, hooks=remaining) if owned else group)
        if not in_place or kept or groups == []:
            hooks[event] = kept
        else:
            del hooks[event]
    if install:
        for event in events:
            if event not in placed:
                hooks.setdefault(event, []).append({"hooks": [make_hook(event)]})
    return result


# --- wrapper and model ------------------------------------------------------

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


# --- checks -----------------------------------------------------------------

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


def verify_mcp(command, mock):
    """MCP handshake, exactly the seven tools and one tool call."""
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


def verify_empty_root(command, mock):
    """The isolated verification root starts without memory."""
    overview = run_json(command + ["overview"], allow_mock=mock)
    if overview.get("total_files") != 0:
        raise InstallError("Unexpected memory in verification directory")


# --- transaction ------------------------------------------------------------

def restore(backup, manifest, keep=frozenset()):
    """Restore every manifest file except the paths in `keep`, which stay as they are."""
    files = [item for item in manifest["files"] if item["path"] not in keep]
    # Preflight every file, so a conflict never causes a partial rollback.
    for item in files:
        path = Path(item["path"])
        current, mode = snapshot(path)
        if digest(current) != item["after_sha256"] or mode != item["after_mode"]:
            raise InstallError(f"Changed since installation; nothing restored: {path}")
        if item["backup"]:
            before = (backup / item["backup"]).read_bytes()
            if digest(before) != item["before_sha256"]:
                raise InstallError(f"Invalid backup for {path}")
    for item in reversed(files):
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


# --- flow -------------------------------------------------------------------

def load_host(host_id):
    if host_id not in HOSTS:
        raise InstallError(f"Unknown host {host_id!r}; choose one of: {', '.join(HOSTS)}")
    return importlib.import_module("hosts." + host_id.replace("-", "_"))


def arguments(argv=None):
    """Parse `--host <id>` plus the common and host-specific options."""
    pre = argparse.ArgumentParser(prog="install.sh", add_help=False)
    pre.add_argument("--host", choices=HOSTS)
    known, _ = pre.parse_known_args(argv)
    if known.host is None:
        parser = argparse.ArgumentParser(prog="install.sh", description=__doc__.splitlines()[0])
        parser.add_argument("--host", choices=HOSTS, required=True, help="agent host to install into")
        parser.parse_args(argv)
        raise AssertionError("unreachable: --host is required")
    host = load_host(known.host)
    parser = argparse.ArgumentParser(prog="install.sh", description=host.DESCRIPTION)
    parser.add_argument("--host", choices=HOSTS, required=True, help="agent host to install into")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--apply", action="store_true", help="build, install and verify (default: preview)")
    mode.add_argument("--dry-run", action="store_true", help="preview only; no writes or model calls")
    mode.add_argument("--rollback", nargs="?", const="latest", metavar="BACKUP",
                      help="restore latest or specified installation; keep memory data")
    parser.add_argument("--home", type=absolute, default=Path.home(), help="installation home (for isolated installs)")
    parser.add_argument("--workspace", type=absolute, help=host.WORKSPACE_HELP)
    parser.add_argument("--data-root", type=absolute)
    parser.add_argument("--binary", type=absolute, help="install this binary instead of building release")
    parser.add_argument("--api-type", choices=("openai-chat", "anthropic"))
    parser.add_argument("--base-url", help="service prefix; openai-chat appends /chat/completions")
    parser.add_argument("--model")
    parser.add_argument("--api-key-env", default="PD_MEMORY_API_KEY", help="read key from this environment variable")
    parser.add_argument("--key-file", help="read a plain API key from this file (never from an argument)")
    parser.add_argument("--non-interactive", action="store_true", help="fail rather than prompt for missing values")
    parser.add_argument("--mock", action="store_true", help="explicit offline demo; NOT a real memory model")
    parser.add_argument("--verify-model", action="store_true", help="make model calls with isolated synthetic test data")
    host.add_arguments(parser, mode)
    args = parser.parse_args(argv)
    if args.mock and args.verify_model:
        parser.error("--verify-model requires a real model; remove --mock")
    return host, args


def state_dir(host, state_root):
    return state_root / host.STATE_SUBDIR if host.STATE_SUBDIR else state_root


def paths_for(host, args):
    home = args.home
    paths = types.SimpleNamespace(home=home)
    paths.private = home / ".config/pd-memory"
    paths.state_root = home / ".local/state/pd-memory/installations"
    paths.state = state_dir(host, paths.state_root)
    paths.binary = home / ".local/bin/pd-memory"
    paths.wrapper = paths.binary.with_name(host.WRAPPER_NAME)
    paths.shared_config = paths.private / "config.json"
    paths.config = paths.private / host.CONFIG_NAME
    paths.credentials = paths.private / "model.env"
    host.host_paths(args, paths)
    return paths


def rollback(host, paths, target):
    state = paths.state
    if target == "latest" and not (state / "latest").is_file():
        raise InstallError(f"No {host.LABEL} installation backup found under {state}")
    backup = absolute(target) if target != "latest" else Path(
        (state / "latest").read_text(encoding="utf-8").strip()
    )
    manifest = read_object(backup / "manifest.json")
    if manifest.get("version") != 1 or manifest.get("status") != "installed":
        raise InstallError("Not an installed pd-memory backup")
    try:
        with installation_lock(paths.state_root):
            # The binary and model.env are shared by every host's wrapper.
            users = other_installed_hosts(host, paths.state_root)
            shared = {str(paths.binary), str(paths.credentials)} if users else set()
            keep = {item["path"] for item in manifest["files"] if item["path"] in shared}
            restore(backup, manifest, keep)
    except InstallError as error:
        if host.ROLLBACK_CONFLICT_HINT and "Changed since installation" in str(error):
            raise InstallError(f"{error}. {host.ROLLBACK_CONFLICT_HINT}") from error
        raise
    for path in sorted(keep):
        print(f"[rollback] kept {path}: still used by {', '.join(users)}")
    print(f"[rollback] original files restored; memory data retained. Restart {host.LABEL}.")


def other_installed_hosts(host, state_root):
    """Labels of the other hosts whose latest installation is still in place."""
    users = []
    for other_id in HOSTS:
        if other_id == host.ID:
            continue
        other = load_host(other_id)
        latest = state_dir(other, state_root) / "latest"
        if not latest.is_file():
            continue
        try:
            backup = Path(latest.read_text(encoding="utf-8").strip())
            status = read_object(backup / "manifest.json").get("status")
        except (InstallError, OSError, UnicodeError):
            status = "installed"  # unreadable: assume it is still in use
        if status == "installed":
            users.append(other.LABEL)
    return users


def build_binary(args, required_flag=None):
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
    if required_flag:
        help_text = subprocess.run([str(source), "hook", "--help"], text=True, capture_output=True,
                                   timeout=10).stdout
        if required_flag not in help_text:
            raise InstallError(f"Selected binary predates `hook {required_flag}`; rebuild it")
    return source


def install(host, args):
    paths = paths_for(host, args)
    if args.rollback:
        rollback(host, paths, args.rollback)
        return
    if getattr(args, "uninstall", False):
        host.uninstall(args, paths)
        return
    expected = {p: snapshot(p) for p in [
        paths.binary, paths.wrapper, paths.config, paths.credentials,
        *host.managed_files(args, paths),
    ]}
    current = host.load_config(paths)
    model, secret = model_config(args, current, paths.credentials)
    data = args.data_root or config_path(current, "data_root", paths.home / ".local/share/pd-memory", paths.private)
    workspace = args.workspace or config_path(
        current, "workspace_root", host.default_workspace(args, paths), paths.private,
    )
    updated = copy.deepcopy(current)
    fields = {"data_root": str(data)}
    if workspace:
        fields["workspace_root"] = str(workspace)
    updated.update(fields, workspace_mode="single", model=model)
    updated.pop("workspace_id", None)
    changes = [
        (paths.config, encode(updated), 0o600),
        (paths.wrapper, wrapper_script(paths.binary, paths.config, paths.credentials, args.mock), 0o755),
    ]
    if secret:
        changes.append((paths.credentials, ("PD_MEMORY_API_KEY=" + shlex.quote(secret) + "\n").encode(), 0o600))
    changes.extend(host.integration_changes(args, paths))
    print(f"[plan] model={model['model']} api={model['api_type']} mode={'MOCK DEMO' if args.mock else 'real'}")
    print(f"[plan] workspace={workspace or '<required: --workspace /absolute/project>'}")
    print(f"[plan] data={data}\n[plan] binary={paths.binary}")
    for path, _, _ in changes:
        print(f"[plan] manage {path}")
    if not args.apply:
        print("[result] preview only. Use --apply; credentials are never displayed.")
        return
    if not workspace:
        raise InstallError("Choose the project this memory binds to: --workspace /absolute/path")
    if not workspace.is_dir():
        raise InstallError(f"Workspace does not exist: {workspace}; create it or use --workspace")
    source = build_binary(args, host.REQUIRED_HOOK_FLAG)
    changes.insert(0, (paths.binary, source.read_bytes(), 0o755))
    paths.private.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(paths.private, 0o700)
    planned = {path: data for path, data, _ in changes}
    with installation_lock(paths.state_root):
        paths.state.mkdir(mode=0o700, exist_ok=True)
        apply_changes(changes, paths.state, lambda: host.verify(args, paths, planned), expected)
    host.post_install(args)
    if not args.mock and not args.verify_model:
        print("[note] model connectivity NOT checked; add --verify-model to validate it.")


def main(argv=None):
    try:
        install(*arguments(argv))
        return 0
    except (InstallError, OSError, ValueError, subprocess.SubprocessError, EOFError) as error:
        message = str(error) if isinstance(error, InstallError) else type(error).__name__
        print(f"[error] {message}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("[error] interrupted", file=sys.stderr)
        return 130

