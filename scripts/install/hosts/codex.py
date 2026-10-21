"""Codex CLI / App: TOML `[mcp_servers]` in config.toml and hooks.json.

Shares the binary, model credentials, backup format and transaction logic with
the other hosts; Codex gets its own wrapper, config file and backups.
"""

import copy
import json
import os
import re
import select
import shutil
import subprocess
import tempfile
import time
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:  # Python < 3.11
    tomllib = None

import common
from common import SERVER, InstallError

ID = "codex"
LABEL = "Codex"
DESCRIPTION = "Install pd-memory's CLI, model config, MCP and hooks for Codex. Standard library only."
WORKSPACE_HELP = "project directory this memory binds to"
HOOK_ARGS = ["hook", "--codex"]
# Codex clamps SessionEnd to 1..3 seconds; the hook only captures there.
EVENTS = {"SessionStart": 150, "UserPromptSubmit": 300, "Stop": 300, "SessionEnd": 3}
STATE_SUBDIR = "codex"
CONFIG_NAME = "codex.json"
WRAPPER_NAME = "pd-memory-codex"
REQUIRED_HOOK_FLAG = "--codex"
ROLLBACK_CONFLICT_HINT = (
    "Codex edits config.toml (for example when you trust hooks); "
    "use --uninstall to remove only pd-memory entries"
)
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

    def keep_trailing_comments():
        # Blank lines and comments ending our table belong to what follows it
        # (the next table, or the end of the file); we never write comments.
        tail = []
        while skipped and (not skipped[-1].strip() or skipped[-1].lstrip().startswith("#")):
            tail.insert(0, skipped.pop())
        out.extend(tail)

    for line in lines:
        if OUR_TABLE.match(line):
            if not found:
                out.extend(block)
            found, inside = True, True
            continue
        if inside and ANY_TABLE.match(line):
            inside = False
            keep_trailing_comments()
        if inside:
            skipped.append(line)
        else:
            out.append(line)
    if inside:
        keep_trailing_comments()
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


def our_hook(event, wrapper):
    return {"type": "command", "command": common.hook_command(wrapper, HOOK_ARGS),
            "timeout": EVENTS[event]}


def merge_hooks(current, wrapper, binary, install=True):
    """Install or remove our four hooks, keeping everything else.

    Codex keys hook trust by position (`hooks.json:<event>:<group>:<handler>`),
    so an existing pd-memory group is replaced in place instead of re-appended.
    """
    executables = common.default_executables(wrapper, binary)
    return common.merge_hooks(
        current, EVENTS, lambda event: our_hook(event, wrapper),
        lambda hook: common.owned_hook(hook, executables, ID),
        in_place=True, install=install, where="hooks.json: hooks",
    )


def codex_hook(command, payload, mock):
    return common.run_json(command + HOOK_ARGS, payload, allow_mock=mock)


def verify_local(wrapper, mock):
    with tempfile.TemporaryDirectory(prefix="pd-memory-check-") as temporary:
        command = [str(wrapper), "--data-root", temporary]
        common.verify_empty_root(command, mock)
        base = {"session_id": "installer-check", "transcript_path": None, "cwd": temporary,
                "model": "installer", "permission_mode": "default"}
        if codex_hook(command, dict(base, hook_event_name="SessionStart", source="startup"),
                      mock).get("continue") is not True:
            raise InstallError("Hook contract check failed")
        for event in ("Stop", "SessionEnd"):
            if codex_hook(command, dict(base, hook_event_name=event), mock) != {"continue": True}:
                raise InstallError(f"{event} output must be exactly {{\"continue\":true}} for Codex")
        common.verify_mcp(command, mock)
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
        message("assistant", common.MODEL_CHECK_ANSWER, "final_answer"),
    ]) + "\n", encoding="utf-8")


def verify_model(wrapper):
    fact, question = common.MODEL_CHECK_FACT, common.MODEL_CHECK_QUESTION
    for event in ("Stop", "SessionEnd"):
        with tempfile.TemporaryDirectory(prefix="pd-memory-model-check-") as temporary:
            command = [str(wrapper), "--data-root", temporary]
            transcript = Path(temporary) / "rollout-installer.jsonl"
            codex_rollout(transcript, fact)
            base = {"transcript_path": str(transcript), "cwd": temporary, "model": "installer",
                    "permission_mode": "default"}
            common.run_json(command + HOOK_ARGS, dict(
                base, hook_event_name=event, session_id="installer-model-check",
                last_assistant_message=common.MODEL_CHECK_ANSWER,
            ), timeout=240)
            hook = common.run_json(command + HOOK_ARGS, dict(
                base, hook_event_name="UserPromptSubmit", session_id="installer-new-session",
                transcript_path=None, prompt=question,
            ), timeout=300)
            if "Nacre-731" not in hook.get("hookSpecificOutput", {}).get("additionalContext", ""):
                raise InstallError(f"{event} capture -> first-recall flush did not inject the test fact")
            if common.run_json(command + ["overview"]).get("pending_dialogue_turns"):
                raise InstallError("First-session flush left an indexing backlog")
            recall = common.run_json(command + ["search", question], timeout=120)
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
        try:
            process.stdin.write("".join(json.dumps(r) + "\n" for r in requests).encode())
            process.stdin.flush()
        except BrokenPipeError:
            # Exited before reading (e.g. a CLI without `app-server`): no answers.
            return responses
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
        except (ProcessLookupError, PermissionError):
            pass  # already gone; macOS reports EPERM for a group of zombies
        process.wait()
        try:
            process.stdin.close()  # flushes whatever a failed write left buffered
        except BrokenPipeError:
            pass
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


def add_arguments(parser, mode):
    mode.add_argument("--uninstall", action="store_true",
                      help="remove pd-memory from Codex config.toml and hooks.json (backed up)")
    parser.add_argument("--codex-home", type=common.absolute, help="default: <home>/.codex")
    parser.add_argument("--skip-codex-check", action="store_true", help="do not run the codex CLI to parse the result")


def host_paths(args, paths):
    paths.host_home = args.codex_home or paths.home / ".codex"
    paths.config_toml = paths.host_home / "config.toml"
    paths.hooks_json = paths.host_home / "hooks.json"


def managed_files(args, paths):
    return [paths.config_toml, paths.hooks_json]


def load_config(paths):
    if paths.config.exists():
        return common.read_object(paths.config)
    # Reuse another host's model and data root, never its workspace binding.
    return {k: v for k, v in common.read_object(paths.shared_config).items()
            if k not in ("workspace_root", "workspace_id")}


def default_workspace(args, paths):
    return None


def refuse_toml_hooks(text):
    """Codex reads hooks from hooks.json or from [hooks] in config.toml, not both
    in one layer. Only [hooks.state] (trust records Codex writes) may coexist."""
    hooks = parse_toml(text, "config.toml").get("hooks", {})
    defined = sorted(hooks) if isinstance(hooks, dict) else ["hooks"]
    defined = [key for key in defined if key != "state"]
    if defined:
        raise InstallError(
            "config.toml already defines Codex hooks under [hooks] ("
            + ", ".join(defined) + "); pd-memory installs hooks.json and Codex does not "
            "allow both in one layer. Move those hooks into hooks.json or remove them, then rerun"
        )


def integration_changes(args, paths):
    text = common.read_text(paths.config_toml)
    refuse_toml_hooks(text)
    config_text = merge_config_toml(text, paths.wrapper)
    hooks_data = common.encode(merge_hooks(common.read_object(paths.hooks_json), paths.wrapper, paths.binary))
    return [
        (paths.config_toml, config_text.encode(), common.mode_of(paths.config_toml)),
        (paths.hooks_json, hooks_data, common.mode_of(paths.hooks_json)),
    ]


def verify(args, paths, planned):
    verify_local(paths.wrapper, args.mock)
    if not args.skip_codex_check:
        verify_with_codex(planned[paths.config_toml].decode("utf-8"), planned[paths.hooks_json], paths.wrapper)
    if args.verify_model:
        verify_model(paths.wrapper)


def uninstall(args, paths):
    changes = []
    text = common.read_text(paths.config_toml)
    if text:
        changes.append((paths.config_toml, merge_config_toml(text, None).encode(), common.mode_of(paths.config_toml)))
    if common.snapshot(paths.hooks_json)[0] is not None:
        hooks = merge_hooks(common.read_object(paths.hooks_json), paths.wrapper, paths.binary, install=False)
        changes.append((paths.hooks_json, common.encode(hooks), common.mode_of(paths.hooks_json)))
    for path, _, _ in changes:
        print(f"[plan] remove pd-memory from {path}")
    with common.installation_lock(paths.state_root):
        paths.state.mkdir(mode=0o700, exist_ok=True)
        common.apply_changes(changes, paths.state, lambda: None)
    print("[uninstall] pd-memory removed from Codex config; binary, model config and memory kept.")
    print("            Restart Codex. Revert with --rollback if needed.")


def post_install(args):
    print("[result] installed. Restart Codex, then trust the hooks: Codex runs no new or edited")
    print("         hook until you review it. In the CLI run /hooks; in the app open Settings ->")
    print("         Hooks. Trust the 4 pd-memory hooks (SessionStart, UserPromptSubmit, Stop,")
    print("         SessionEnd). Any later change to them needs trust again.")
