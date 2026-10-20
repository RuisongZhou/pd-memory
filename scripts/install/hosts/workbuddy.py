"""WorkBuddy / CodeBuddy: JSON MCP files and Claude-Code-style settings.json hooks."""

import json
import tempfile
from pathlib import Path

import common
from common import InstallError

ID = "workbuddy"
LABEL = "WorkBuddy"
DESCRIPTION = "Install pd-memory's CLI, model config, MCP and hooks. Standard library only."
WORKSPACE_HELP = "default: <workbuddy-home>/workspace"
HOOK_ARGS = ["hook", "--workbuddy"]
EVENTS = {"SessionStart": 150, "UserPromptSubmit": 300, "Stop": 300, "SessionEnd": 300}
# Backups predate per-host subdirectories; keep them where rollback finds them.
STATE_SUBDIR = None
CONFIG_NAME = "config.json"
WRAPPER_NAME = "pd-memory-workbuddy"
REQUIRED_HOOK_FLAG = None
ROLLBACK_CONFLICT_HINT = None


def add_arguments(parser, mode):
    parser.add_argument("--workbuddy-home", type=common.absolute)
    parser.add_argument("--mcp-target", choices=("gui", "cli", "both"), default="both")


def host_paths(args, paths):
    paths.host_home = args.workbuddy_home or paths.home / ".workbuddy"
    paths.settings = paths.host_home / "settings.json"
    names = ["mcp.json", ".mcp.json"] if args.mcp_target == "both" else [
        ".mcp.json" if args.mcp_target == "cli" else "mcp.json",
    ]
    paths.mcp_files = [paths.host_home / name for name in names]


def managed_files(args, paths):
    return [paths.settings, *paths.mcp_files]


def load_config(paths):
    return common.read_object(paths.config)


def default_workspace(args, paths):
    return paths.host_home / "workspace"


def merge_mcp(current, wrapper, cli=False):
    """GUI `mcp.json` and CLI `.mcp.json`; the CLI file can also disable servers."""
    result = common.merge_mcp_servers(current, {
        "type": "stdio", "command": str(wrapper),
        "args": ["serve", "mcp"], "timeout": 120000,
    })
    if cli:
        disabled = result.setdefault("disabledMcpServers", [])
        if not isinstance(disabled, list):
            raise InstallError("disabledMcpServers must be an array")
        result["disabledMcpServers"] = [s for s in disabled if s != common.SERVER]
    return result


def our_hook(event, wrapper):
    return {"type": "command", "command": common.hook_command(wrapper, HOOK_ARGS),
            "timeout": EVENTS[event]}


def merge_hooks(current, wrapper, binary):
    executables = common.default_executables(wrapper, binary)
    return common.merge_hooks(
        current, EVENTS, lambda event: our_hook(event, wrapper),
        lambda hook: common.owned_hook(hook, executables, ID),
        in_place=False, where="settings.hooks",
    )


def integration_changes(args, paths):
    changes = [
        (path, common.encode(merge_mcp(common.read_object(path), paths.wrapper, path.name == ".mcp.json")), 0o600)
        for path in paths.mcp_files
    ]
    hooks = merge_hooks(common.read_object(paths.settings), paths.wrapper, paths.binary)
    changes.append((paths.settings, common.encode(hooks), 0o600))
    return changes


def verify(args, paths, planned):
    verify_local(paths.wrapper, args.mock)
    if args.verify_model:
        verify_model(paths.wrapper)


def verify_local(wrapper, mock):
    # Verify in an isolated root; installation must not create test memories.
    with tempfile.TemporaryDirectory(prefix="pd-memory-check-") as temporary:
        command = [str(wrapper), "--data-root", temporary]
        common.verify_empty_root(command, mock)
        hook = common.run_json(command + HOOK_ARGS, {
            "hook_event_name": "SessionStart", "session_id": "installer-check",
        }, allow_mock=mock)
        if hook.get("continue") is not True:
            raise InstallError("Hook contract check failed")
        common.verify_mcp(command, mock)
    print("[check] MCP initialize, 7 tools, tool call and SessionStart hook passed")


def verify_model(wrapper):
    for event in ("Stop", "SessionEnd"):
        verify_model_lifecycle(wrapper, common.MODEL_CHECK_FACT, event)
    print("[check] real model: Stop -> first-session flush and SessionEnd -> flush; search and hook context passed")


def verify_model_lifecycle(wrapper, fact, event):
    question = common.MODEL_CHECK_QUESTION
    with tempfile.TemporaryDirectory(prefix="pd-memory-model-check-") as temporary:
        command = [str(wrapper), "--data-root", temporary]
        transcript = Path(temporary) / "transcript.jsonl"
        transcript.write_text(json.dumps({
            "type": "message", "role": "user", "content": [{
                "type": "input_text", "text": (
                    '<system-reminder data-role="user-context">Host instructions.</system-reminder>\n'
                    f'<user_query>{fact}</user_query>'
                ),
            }],
        }, ensure_ascii=False) + "\n", encoding="utf-8")
        common.run_json(command + HOOK_ARGS, {"hook_event_name": event,
            "session_id": "installer-model-check", "transcript_path": str(transcript),
            "last_assistant_message": common.MODEL_CHECK_ANSWER,
        }, timeout=240)
        if event == "SessionEnd":
            overview = common.run_json(command + ["overview"])
            if overview.get("pending_dialogue_turns") or not overview.get("total_files"):
                raise InstallError("SessionEnd did not index the test fact before recall")
        hook = common.run_json(command + HOOK_ARGS, {
            "hook_event_name": "UserPromptSubmit", "session_id": "installer-new-session",
            "prompt": question,
        }, timeout=300)
        if "Nacre-731" not in hook.get("hookSpecificOutput", {}).get("additionalContext", ""):
            raise InstallError("First-session flush and recall did not inject the test fact")
        overview = common.run_json(command + ["overview"])
        if overview.get("pending_dialogue_turns"):
            raise InstallError("First-session flush left an indexing backlog")
        listing = common.run_json(command + ["list"])
        if not listing.get("count"):
            raise InstallError("Model check produced no memory files")
        recall = common.run_json(command + ["search", question], timeout=120)
        if "Nacre-731" not in json.dumps(recall, ensure_ascii=False):
            raise InstallError("Model search did not recall the test fact")
        hook = common.run_json(command + HOOK_ARGS, {
            "hook_event_name": "UserPromptSubmit", "session_id": "installer-model-check",
            "prompt": question,
        }, timeout=120)
        if "Nacre-731" not in hook.get("hookSpecificOutput", {}).get("additionalContext", ""):
            raise InstallError("Model hook did not inject the test fact")


def post_install(args):
    print("[result] installed. Restart WorkBuddy; open Experts/Skills/Connectors ->")
    print("         Connectors -> Custom connector; trust pd-memory and confirm 7/7 tools.")
