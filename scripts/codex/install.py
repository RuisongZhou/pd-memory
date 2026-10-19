#!/usr/bin/env python3
"""Install pd-memory's CLI, model config, MCP and hooks for Codex. Standard library only.

Shares the binary, model credentials, backup format and transaction logic with
the WorkBuddy installer; Codex gets its own wrapper, config file and backups.
"""

import argparse
import copy
import importlib.util
import json
import os
import re
import select
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:  # Python < 3.11
    tomllib = None

_SPEC = importlib.util.spec_from_file_location(
    "pd_memory_workbuddy_install", Path(__file__).resolve().parents[1] / "workbuddy/install.py",
)
wb = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(wb)

ROOT = wb.ROOT
InstallError = wb.InstallError
SERVER = "pd-memory"
# Codex clamps SessionEnd to 1..3 seconds; the hook only captures there.
EVENTS = {"SessionStart": 150, "UserPromptSubmit": 300, "Stop": 300, "SessionEnd": 3}
STARTUP_TIMEOUT_SEC = 30
TOOL_TIMEOUT_SEC = 300
OUR_TABLE = re.compile(
    r"""^\s*\[\s*mcp_servers\s*\.\s*(?:"pd-memory"|'pd-memory'|pd-memory)\s*(?:\.[^\]]*)?\]\s*(?:#.*)?$"""
)
ANY_TABLE = re.compile(r"""^\s*\[\[?\s*[A-Za-z0-9_"'-][^\]]*\]\]?\s*(?:#.*)?$""")


def server_entry(wrapper):
    return {
        "command": str(wrapper), "args": ["serve", "mcp"],
        "startup_timeout_sec": STARTUP_TIMEOUT_SEC, "tool_timeout_sec": TOOL_TIMEOUT_SEC,
    }


def toml_block(wrapper):
    value = lambda v: json.dumps(v, ensure_ascii=False)  # JSON strings are TOML basic strings
    entry = server_entry(wrapper)
    lines = [f"[mcp_servers.{SERVER}]"]
    lines += [f"{key} = {value(entry[key])}" for key in ("command", "args")]
    lines += [f"{key} = {entry[key]}" for key in ("startup_timeout_sec", "tool_timeout_sec")]
    return [line + "\n" for line in lines]


def parse_toml(text, what):
    if tomllib is None:
        raise InstallError("Python 3.11+ is required to edit Codex config.toml safely")
    try:
        return tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        raise InstallError(f"Invalid TOML in {what}; nothing changed") from error


def merge_config_toml(text, wrapper):
    """Set (wrapper) or remove (None) [mcp_servers.pd-memory]; keep every other byte.

    The edit is textual so comments, ordering and `notify` stay untouched; the
    result is accepted only if it parses to the original document with exactly
    our table changed.
    """
    before = parse_toml(text, "config.toml")
    servers = before.get("mcp_servers", {})
    if not isinstance(servers, dict):
        raise InstallError("config.toml: mcp_servers must be a table")
    lines = text.splitlines(keepends=True)
    if lines and not lines[-1].endswith("\n"):
        lines[-1] += "\n"
    block = toml_block(wrapper) if wrapper else []
    out, skipped, found, inside = [], [], False, False
    for line in lines:
        if OUR_TABLE.match(line):
            if not found:
                out.extend(block)
            found, inside = True, True
            continue
        if inside and ANY_TABLE.match(line):
            inside = False
            # Blank lines and comments just before the next table belong to it.
            tail = []
            while skipped and (not skipped[-1].strip() or skipped[-1].lstrip().startswith("#")):
                tail.insert(0, skipped.pop())
            out.extend(tail)
        if inside:
            skipped.append(line)
        else:
            out.append(line)
    if SERVER in servers and not found:
        raise InstallError(
            f"config.toml defines {SERVER} inline; move it to a [mcp_servers.{SERVER}] table "
            "or remove it, then rerun"
        )
    if not found and block:
        while out and not out[-1].strip():
            out.pop()
        out.extend((["\n"] if out else []) + block)
    result = "".join(out)
    if wrapper is None:
        result = result.rstrip("\n") + "\n" if result.strip() else ""
        if not found:
            result = text
    after = parse_toml(result, "the edited config.toml")
    expected = copy.deepcopy(before)
    if wrapper:
        expected.setdefault("mcp_servers", {})[SERVER] = server_entry(wrapper)
    elif SERVER in servers:
        del expected["mcp_servers"][SERVER]
        if not expected["mcp_servers"] and "mcp_servers" not in after:
            del expected["mcp_servers"]
    if after != expected or after.get("notify") != before.get("notify"):
        raise InstallError(
            f"Could not edit config.toml without changing other settings; "
            f"add [mcp_servers.{SERVER}] manually"
        )
    return result


def owned_hook(hook, wrapper, binary):
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
    executables = {
        str(wrapper), str(binary), str(ROOT / "target/debug/pd-memory"),
        str(ROOT / "target/release/pd-memory"),
    }
    return bool(
        words and words[0] in executables
        and (words[-1] == "hook" or words[-2:] == ["hook", "--codex"])
    )


def our_hook(event, wrapper):
    return {
        "type": "command", "command": shlex.quote(str(wrapper)) + " hook --codex",
        "timeout": EVENTS[event],
    }


def merge_hooks(current, wrapper, binary, install=True):
    """Install or remove our four hooks, keeping everything else.

    Codex keys hook trust by position (`hooks.json:<event>:<group>:<handler>`),
    so an existing pd-memory group is replaced in place instead of re-appended.
    """
    result = copy.deepcopy(current)
    if not install and "hooks" not in result:
        return result
    hooks = result.setdefault("hooks", {})
    if not isinstance(hooks, dict):
        raise InstallError("hooks.json: hooks must be an object")
    placed = set()
    for event, groups in list(hooks.items()):
        if not isinstance(groups, list):
            raise InstallError(f"hooks.json: hooks.{event} must be an array")
        kept = []
        for group in groups:
            if not isinstance(group, dict) or not isinstance(group.get("hooks"), list):
                raise InstallError(f"hooks.json: invalid hook group in {event}")
            owned = [h for h in group["hooks"] if owned_hook(h, wrapper, binary)]
            remaining = [h for h in group["hooks"] if not owned_hook(h, wrapper, binary)]
            if install and event in EVENTS and event not in placed and owned and not remaining:
                kept.append({"hooks": [our_hook(event, wrapper)]})
                placed.add(event)
            elif remaining:
                kept.append(dict(group, hooks=remaining) if owned else group)
        if kept or groups == []:
            hooks[event] = kept
        else:
            del hooks[event]
    if install:
        for event in EVENTS:
            if event not in placed:
                hooks.setdefault(event, []).append({"hooks": [our_hook(event, wrapper)]})
    return result


def codex_hook(command, payload, mock):
    return wb.run_json(command + ["hook", "--codex"], payload, allow_mock=mock)


def verify_local(wrapper, mock):
    with tempfile.TemporaryDirectory(prefix="pd-memory-check-") as temporary:
        command = [str(wrapper), "--data-root", temporary]
        overview = wb.run_json(command + ["overview"], allow_mock=mock)
        if overview.get("total_files") != 0:
            raise InstallError("Unexpected memory in verification directory")
        base = {"session_id": "installer-check", "transcript_path": None, "cwd": temporary,
                "model": "installer", "permission_mode": "default"}
        if codex_hook(command, dict(base, hook_event_name="SessionStart", source="startup"),
                      mock).get("continue") is not True:
            raise InstallError("Hook contract check failed")
        for event in ("Stop", "SessionEnd"):
            if codex_hook(command, dict(base, hook_event_name=event), mock) != {"continue": True}:
                raise InstallError(f"{event} output must be exactly {{\"continue\":true}} for Codex")
        wb.verify_mcp(command, mock)
    print("[check] MCP initialize, 7 tools, tool call and Codex hook output contract passed")


def codex_rollout(path, fact):
    def line(kind, payload):
        return json.dumps({"timestamp": "2026-01-01T00:00:00.000Z", "type": kind,
                           "payload": payload}, ensure_ascii=False)
    def message(role, text, phase=None):
        block = "output_text" if role == "assistant" else "input_text"
        payload = {"type": "message", "role": role, "content": [{"type": block, "text": text}]}
        if phase:
            payload["phase"] = phase
        return line("response_item", payload)
    path.write_text("\n".join([
        line("session_meta", {"id": "installer-model-check"}),
        message("user", "# AGENTS.md instructions for /repo\n\nHost instructions."),
        message("user", "<environment_context>\n  <cwd>/repo</cwd>\n</environment_context>"),
        message("user", fact),
        message("assistant", "已记住这两条项目规则。", "final_answer"),
    ]) + "\n", encoding="utf-8")


def verify_model(wrapper):
    fact = "Boreal 项目的内部代号固定为 Nacre-731，全部交付文档必须使用中文。请长期记住这两条项目规则。"
    question = "Boreal 项目之前约定的代号和文档语言是什么？"
    for event in ("Stop", "SessionEnd"):
        with tempfile.TemporaryDirectory(prefix="pd-memory-model-check-") as temporary:
            command = [str(wrapper), "--data-root", temporary]
            transcript = Path(temporary) / "rollout-installer.jsonl"
            codex_rollout(transcript, fact)
            base = {"transcript_path": str(transcript), "cwd": temporary, "model": "installer",
                    "permission_mode": "default"}
            wb.run_json(command + ["hook", "--codex"], dict(
                base, hook_event_name=event, session_id="installer-model-check",
                last_assistant_message="已记住这两条项目规则。",
            ), timeout=240)
            hook = wb.run_json(command + ["hook", "--codex"], dict(
                base, hook_event_name="UserPromptSubmit", session_id="installer-new-session",
                transcript_path=None, prompt=question,
            ), timeout=300)
            if "Nacre-731" not in hook.get("hookSpecificOutput", {}).get("additionalContext", ""):
                raise InstallError(f"{event} capture -> first-recall flush did not inject the test fact")
            if wb.run_json(command + ["overview"]).get("pending_dialogue_turns"):
                raise InstallError("First-session flush left an indexing backlog")
            recall = wb.run_json(command + ["search", question], timeout=120)
            if "Nacre-731" not in json.dumps(recall, ensure_ascii=False):
                raise InstallError("Model search did not recall the test fact")
    print("[check] real model: Stop and SessionEnd capture -> first-recall flush; search passed")


def codex_app_server(codex_home, requests, timeout=30):
    """Send JSON-RPC requests to `codex app-server`; return responses by id."""
    process = subprocess.Popen(
        ["codex", "app-server"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL, cwd=codex_home, start_new_session=True,
        env=dict(os.environ, CODEX_HOME=str(codex_home)),
    )
    wanted = {r["id"] for r in requests if "id" in r}
    responses, buffer = {}, b""
    try:
        process.stdin.write("".join(json.dumps(r) + "\n" for r in requests).encode())
        process.stdin.flush()
        deadline = time.monotonic() + timeout
        while wanted - responses.keys() and time.monotonic() < deadline:
            if not select.select([process.stdout], [], [], 0.2)[0]:
                continue
            chunk = os.read(process.stdout.fileno(), 65536)
            if not chunk:
                break
            buffer += chunk
            while b"\n" in buffer:
                line, buffer = buffer.split(b"\n", 1)
                try:
                    message = json.loads(line)
                except ValueError:
                    continue
                if message.get("id") in wanted:
                    responses[message["id"]] = message
    finally:
        try:
            os.killpg(process.pid, 9)
        except ProcessLookupError:
            pass
        process.wait()
        process.stdin.close()
        process.stdout.close()
    return responses


def verify_with_codex(config_text, hooks_data, wrapper):
    """Let the installed Codex CLI parse copies of both files in a private CODEX_HOME."""
    if not shutil.which("codex"):
        print("[note] codex CLI not on PATH; Codex's own parsing of the files NOT checked")
        return
    with tempfile.TemporaryDirectory(prefix="pd-memory-codex-check-") as temporary:
        home = Path(temporary)
        (home / "config.toml").write_text(config_text, encoding="utf-8")
        (home / "hooks.json").write_bytes(hooks_data)
        env = dict(os.environ, CODEX_HOME=temporary)
        result = subprocess.run(["codex", "mcp", "get", SERVER, "--json"], env=env, cwd=temporary,
                                stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=60)
        try:
            server = json.loads(result.stdout) if result.returncode == 0 else {}
        except ValueError:
            server = {}
        transport = server.get("transport", {})
        if transport.get("command") != str(wrapper) or transport.get("args") != ["serve", "mcp"]:
            raise InstallError("Codex did not read [mcp_servers.pd-memory] from config.toml")
        responses = codex_app_server(home, [
            {"jsonrpc": "2.0", "id": 1, "method": "initialize",
             "params": {"clientInfo": {"name": "pd-memory-installer", "version": "1"}}},
            {"jsonrpc": "2.0", "method": "initialized"},
            {"jsonrpc": "2.0", "id": 2, "method": "hooks/list", "params": {"cwds": [temporary]}},
        ])
        if 2 not in responses:
            print("[note] codex app-server did not answer hooks/list; hook parsing NOT checked")
            return
        entries = (responses[2].get("result", {}).get("data") or [{}])[0]
        for problem in entries.get("errors", []) + entries.get("warnings", []):
            print(f"[note] codex reports: {problem}")
        ours = {h.get("eventName"): h.get("timeoutSec") for h in entries.get("hooks", [])
                if h.get("command") == our_hook("Stop", wrapper)["command"]}
        expected = {event[0].lower() + event[1:]: timeout for event, timeout in EVENTS.items()}
        if ours != expected:
            raise InstallError("Codex did not load the four pd-memory hooks as written")
    print("[check] codex CLI parsed the pd-memory MCP server and 4 hooks")


def arguments(argv=None):
    parser = argparse.ArgumentParser(prog="install.sh", description=__doc__.splitlines()[0])
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--apply", action="store_true", help="build, install and verify (default: preview)")
    mode.add_argument("--dry-run", action="store_true", help="preview only; no writes or model calls")
    mode.add_argument("--rollback", nargs="?", const="latest", metavar="BACKUP",
                      help="restore latest or specified installation; keep memory data")
    mode.add_argument("--uninstall", action="store_true",
                      help="remove pd-memory from Codex config.toml and hooks.json (backed up)")
    parser.add_argument("--home", type=wb.absolute, default=Path.home(), help="installation home (for isolated installs)")
    parser.add_argument("--codex-home", type=wb.absolute, help="default: <home>/.codex")
    parser.add_argument("--workspace", type=wb.absolute, help="project directory this memory binds to")
    parser.add_argument("--data-root", type=wb.absolute)
    parser.add_argument("--binary", type=wb.absolute, help="install this binary instead of building release")
    parser.add_argument("--api-type", choices=("openai-chat", "anthropic"))
    parser.add_argument("--base-url", help="service prefix; openai-chat appends /chat/completions")
    parser.add_argument("--model")
    parser.add_argument("--api-key-env", default="PD_MEMORY_API_KEY", help="read key from this environment variable")
    parser.add_argument("--key-file", help="read a plain API key from this file (never from an argument)")
    parser.add_argument("--non-interactive", action="store_true", help="fail rather than prompt for missing values")
    parser.add_argument("--mock", action="store_true", help="explicit offline demo; NOT a real memory model")
    parser.add_argument("--verify-model", action="store_true", help="make model calls with isolated synthetic test data")
    parser.add_argument("--skip-codex-check", action="store_true", help="do not run the codex CLI to parse the result")
    args = parser.parse_args(argv)
    if args.mock and args.verify_model:
        parser.error("--verify-model requires a real model; remove --mock")
    return args


def read_text(path):
    data, _ = wb.snapshot(path)
    if data is None:
        return ""
    try:
        return data.decode("utf-8")
    except UnicodeError as error:
        raise InstallError(f"{path} is not UTF-8; nothing changed") from error


def mode_of(path, default=0o600):
    return wb.snapshot(path)[1] or default


def install(args):
    home = args.home
    codex = args.codex_home or home / ".codex"
    private = home / ".config/pd-memory"
    state_root = home / ".local/state/pd-memory/installations"
    state = state_root / "codex"
    binary = home / ".local/bin/pd-memory"
    wrapper = binary.with_name("pd-memory-codex")
    shared_config = private / "config.json"
    config = private / "codex.json"
    credentials = private / "model.env"
    config_toml = codex / "config.toml"
    hooks_json = codex / "hooks.json"
    if args.rollback:
        if args.rollback == "latest" and not (state / "latest").is_file():
            raise InstallError(f"No Codex installation backup found under {state}")
        backup = wb.absolute(args.rollback) if args.rollback != "latest" else Path(
            (state / "latest").read_text(encoding="utf-8").strip()
        )
        manifest = wb.read_object(backup / "manifest.json")
        if manifest.get("version") != 1 or manifest.get("status") != "installed":
            raise InstallError("Not an installed pd-memory backup")
        try:
            with wb.installation_lock(state_root):
                wb.restore(backup, manifest)
        except InstallError as error:
            if "Changed since installation" in str(error):
                raise InstallError(f"{error}. Codex edits config.toml (for example when you trust "
                                   "hooks); use --uninstall to remove only pd-memory entries") from error
            raise
        print("[rollback] original files restored; memory data retained. Restart Codex.")
        return
    if args.uninstall:
        changes = []
        text = read_text(config_toml)
        if text:
            changes.append((config_toml, merge_config_toml(text, None).encode(), mode_of(config_toml)))
        if wb.snapshot(hooks_json)[0] is not None:
            hooks = merge_hooks(wb.read_object(hooks_json), wrapper, binary, install=False)
            changes.append((hooks_json, wb.encode(hooks), mode_of(hooks_json)))
        for path, _, _ in changes:
            print(f"[plan] remove pd-memory from {path}")
        with wb.installation_lock(state_root):
            state.mkdir(mode=0o700, exist_ok=True)
            wb.apply_changes(changes, state, lambda: None)
        print("[uninstall] pd-memory removed from Codex config; binary, model config and memory kept.")
        print("            Restart Codex. Revert with --rollback if needed.")
        return
    expected = {p: wb.snapshot(p) for p in [binary, wrapper, config, credentials, config_toml, hooks_json]}
    if config.exists():
        current = wb.read_object(config)
    else:
        # Reuse the WorkBuddy model and data root, never its workspace binding.
        current = {k: v for k, v in wb.read_object(shared_config).items()
                   if k not in ("workspace_root", "workspace_id")}
    model, secret = wb.model_config(args, current, credentials)
    data = args.data_root or wb.config_path(current, "data_root", home / ".local/share/pd-memory", private)
    workspace = args.workspace or wb.config_path(current, "workspace_root", None, private)
    updated = copy.deepcopy(current)
    updated.update(data_root=str(data), workspace_mode="single", model=model)
    if workspace:
        updated["workspace_root"] = str(workspace)
    updated.pop("workspace_id", None)
    changes = [
        (config, wb.encode(updated), 0o600),
        (wrapper, wb.wrapper_script(binary, config, credentials, args.mock), 0o755),
    ]
    if secret:
        changes.append((credentials, ("PD_MEMORY_API_KEY=" + shlex.quote(secret) + "\n").encode(), 0o600))
    config_text = merge_config_toml(read_text(config_toml), wrapper)
    hooks_data = wb.encode(merge_hooks(wb.read_object(hooks_json), wrapper, binary))
    changes.append((config_toml, config_text.encode(), mode_of(config_toml)))
    changes.append((hooks_json, hooks_data, mode_of(hooks_json)))
    print(f"[plan] model={model['model']} api={model['api_type']} mode={'MOCK DEMO' if args.mock else 'real'}")
    print(f"[plan] workspace={workspace or '<required: --workspace /absolute/project>'}")
    print(f"[plan] data={data}\n[plan] binary={binary}")
    for path, _, _ in changes:
        print(f"[plan] manage {path}")
    if not args.apply:
        print("[result] preview only. Use --apply; credentials are never displayed.")
        return
    if not workspace:
        raise InstallError("Choose the project this memory binds to: --workspace /absolute/path")
    if not workspace.is_dir():
        raise InstallError(f"Workspace does not exist: {workspace}")
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
    help_text = subprocess.run([str(source), "hook", "--help"], text=True, capture_output=True,
                               timeout=10).stdout
    if "--codex" not in help_text:
        raise InstallError("Selected binary predates `hook --codex`; rebuild it")
    changes.insert(0, (binary, source.read_bytes(), 0o755))
    private.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(private, 0o700)
    with wb.installation_lock(state_root):
        state.mkdir(mode=0o700, exist_ok=True)
        def verify():
            verify_local(wrapper, args.mock)
            if not args.skip_codex_check:
                verify_with_codex(config_text, hooks_data, wrapper)
            if args.verify_model:
                verify_model(wrapper)
        wb.apply_changes(changes, state, verify, expected)
    print("[result] installed. Restart Codex, then trust the hooks: Codex runs no new or edited")
    print("         hook until you review it. In the CLI run /hooks; in the app open Settings ->")
    print("         Hooks. Trust the 4 pd-memory hooks (SessionStart, UserPromptSubmit, Stop,")
    print("         SessionEnd). Any later change to them needs trust again.")
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
