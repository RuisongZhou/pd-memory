"""DeepSeek Harness (dsh): one MCP row in the home-level Cordis patch layer.

MCP tools only (L0): dsh's Claude Code / Codex hook bridges pass no transcript
and raise no session end (docs/hosts/deepseek-harness.md), so no hooks are
installed. The row lives in `<dsh-home>/cordis.patch.yml`, the patch layer
every profile, the Desktop app included, composes after its own. dsh refuses
to start when that file is not a YAML list, and the standard library has no
YAML parser: the row is one marker-delimited block edited as text, every byte
outside it is kept, and dsh's own `--dump-config` parses the result in a
temporary DSH_HOME before and after the change.
"""

import json
import os
import re
import shutil
import subprocess
import tempfile
from pathlib import Path

import common
from common import SERVER, InstallError

ID = "deepseek-harness"
LABEL = "DeepSeek Harness"
DESCRIPTION = "Install pd-memory's CLI, model config and MCP server for DeepSeek Harness. Standard library only."
WORKSPACE_HELP = "project directory this memory binds to"
# MCP tools only; see the module docstring for why there are no hooks.
HOOK_ARGS = None
STATE_SUBDIR = "deepseek-harness"
CONFIG_NAME = "dsh.json"
WRAPPER_NAME = "pd-memory-dsh"
REQUIRED_HOOK_FLAG = None
ROLLBACK_CONFLICT_HINT = (
    "cordis.patch.yml changed after installation; "
    "use --uninstall to remove only the pd-memory row"
)
ROW_ID = "memory-pd-memory"
# memory_flush / memory_dream call the model; dsh's default is 60 s.
TOOL_CALL_TIMEOUT_MS = 300000
BEGIN = "# >>> pd-memory: managed by scripts/install.sh --host deepseek-harness >>>"
END = "# <<< pd-memory <<<"
DESKTOP_CLI = Path("DeepSeek Harness.app/Contents/Resources/runtime/cli/bin/dsh")
ROW_ID_LINE = re.compile(r"""^\s*(?:-\s+)?id:\s*(?:"memory-pd-memory"|'memory-pd-memory'|memory-pd-memory)\s*(?:#.*)?$""")
SERVER_NAME_LINE = re.compile(r"""^\s*(?:-\s+)?serverName:\s*(?:"pd-memory"|'pd-memory'|pd-memory)\s*(?:#.*)?$""")
INSERT_ITEM = re.compile(r"^-\s+insert:\s*(?:#.*)?$")
MANUAL = "add the row by hand as in docs/hosts/deepseek-harness.md"


# --- cordis.patch.yml ---------------------------------------------------------

def _text(line):
    return line.rstrip("\r\n")


def _blank_or_comment(line):
    stripped = _text(line).strip()
    return not stripped or stripped.startswith("#")


def _indent(line):
    return len(line) - len(line.lstrip(" "))


def _item_start(line):
    """A top-level block-sequence item: `-` at column 0."""
    text = _text(line)
    return text == "-" or text.startswith("- ")


def block(wrapper):
    # A JSON string is also a valid YAML double-quoted scalar.
    command = json.dumps(str(wrapper), ensure_ascii=False)
    return [
        BEGIN,
        "- insert:",
        f"    - id: {ROW_ID}",
        "      name: '@deepseek-ai/dsh-mcp-client'",
        "      config:",
        f"        serverName: {SERVER}",
        "        transport: stdio",
        f"        command: {command}",
        "        args: [serve, mcp]",
        f"        toolCallTimeoutMs: {TOOL_CALL_TIMEOUT_MS}",
        END,
    ]


def marker_range(lines):
    begins = [i for i, line in enumerate(lines) if _text(line).strip().startswith(BEGIN[:15])]
    ends = [i for i, line in enumerate(lines) if _text(line).strip().startswith(END[:15])]
    if not begins and not ends:
        return None
    if len(begins) != 1 or len(ends) != 1 or ends[0] < begins[0]:
        raise InstallError("cordis.patch.yml: the pd-memory markers are damaged; delete the "
                           "pd-memory block by hand, then rerun")
    return begins[0], ends[0] + 1


def legacy_range(lines):
    """The row docs/hosts/deepseek-harness.md once told users to add by hand: one
    top-level `- insert:` item holding only that row. Adopted so a reinstall
    manages it; anything else naming the row is refused."""
    hits = [i for i, line in enumerate(lines) if ROW_ID_LINE.match(_text(line))]
    if not hits:
        return None
    if len(hits) > 1:
        raise InstallError(f"cordis.patch.yml defines {ROW_ID} more than once; keep one, then rerun")
    hit = hits[0]
    starts = [i for i in range(hit + 1) if _item_start(lines[i])]
    if not starts:
        raise InstallError(f"cordis.patch.yml: {ROW_ID} is not inside a top-level list item; {MANUAL}")
    start = starts[-1]
    end = next((i for i in range(start + 1, len(lines))
                if not _blank_or_comment(lines[i]) and not lines[i][:1].isspace()), len(lines))
    while end > start + 1 and _blank_or_comment(lines[end - 1]):
        end -= 1  # trailing comments and blank lines belong to what follows
    nested = [line for line in lines[start + 1:end] if not _blank_or_comment(line)]
    dashes = [line for line in nested if _text(line).lstrip().startswith("-")]
    rows = [line for line in dashes if _indent(line) == min(map(_indent, dashes))] if dashes else []
    siblings = [line for line in nested if _indent(line) <= 2 and line not in dashes]
    if not (start < hit < end) or not INSERT_ITEM.match(_text(lines[start])) or siblings or len(rows) != 1:
        raise InstallError(
            f"cordis.patch.yml: {ROW_ID} shares its `- insert:` item with other settings; "
            "move it to its own item or delete it, then rerun"
        )
    return start, end


def layout(lines):
    """`empty`, `flow-empty` (a lone `[]`) or `list`; refuse anything else, because
    an appended `- insert:` item is only valid after a top-level block list."""
    content = [line for line in lines if not _blank_or_comment(line)]
    if not content:
        return "empty"
    if len(content) == 1 and _text(content[0]) == "[]":
        return "flow-empty"
    for line in content:
        if line is content[0] and line[:1].isspace():
            raise InstallError(f"cordis.patch.yml: the top-level list is indented; {MANUAL}")
        if not line[:1].isspace() and not _item_start(line):
            raise InstallError(
                f"cordis.patch.yml is not a plain top-level YAML list (line `{_text(line)[:40]}`); {MANUAL}"
            )
    return "list"


def merge_patch(text, wrapper):
    """Set (wrapper) or remove (None) the pd-memory row; keep every other byte.

    Our row is one marker-delimited block that replaces itself in place or is
    appended after the user's last line. Removing the last remaining item
    leaves `[]`, because dsh refuses a patch file that is not a YAML list.
    """
    lines = text.splitlines(keepends=True)
    newline = "\r\n" if "\r\n" in text else "\n"
    ours = marker_range(lines) or legacy_range(lines)
    if wrapper is None and ours is None:
        return text
    head, tail = (lines[:ours[0]], lines[ours[1]:]) if ours else (lines, [])
    shape = layout(head + tail)
    for line in head + tail:
        if not _blank_or_comment(line) and (ROW_ID_LINE.match(_text(line)) or SERVER_NAME_LINE.match(_text(line))):
            raise InstallError(
                f"cordis.patch.yml already has another MCP row named {SERVER}; delete it, then rerun"
            )
    if wrapper is None:
        result = head + tail
        if layout(result) == "empty":
            if result and not result[-1].endswith("\n"):
                result[-1] += newline
            result.append("[]" + newline)
        return "".join(result)
    if shape == "flow-empty":
        # `[]` cannot be followed by list items; it only stood for the empty list.
        head = [line for line in head if _text(line) != "[]"]
        tail = [line for line in tail if _text(line) != "[]"]
    if head and not head[-1].endswith("\n"):
        head[-1] += newline
    return "".join(head + [line + newline for line in block(wrapper)] + tail)


def note_profile_duplicates(paths):
    """A profile's own layer naming pd-memory would collide with ours at load time."""
    for patch in sorted((paths.host_home / "profiles").glob("*/cordis.patch.yml")):
        try:
            text = patch.read_text(encoding="utf-8")
        except (OSError, UnicodeError):
            continue
        if any(SERVER_NAME_LINE.match(line) for line in text.splitlines()):
            print(f"[note] {patch} also defines an MCP server named {SERVER}; "
                  "dsh loads only one of them, remove that row")


# --- checks -------------------------------------------------------------------

def find_dsh(args, paths):
    if args.dsh_binary:
        return args.dsh_binary
    found = shutil.which("dsh")
    if found:
        return Path(found)
    for applications in (Path("/Applications"), paths.home / "Applications"):
        if (applications / DESKTOP_CLI).is_file():
            return applications / DESKTOP_CLI
    return None


def dump_config(dsh, text):
    """Compose dsh's `web` profile with `text` as the home patch in a private DSH_HOME."""
    with tempfile.TemporaryDirectory(prefix="pd-memory-dsh-check-") as temporary:
        if text:
            (Path(temporary) / "cordis.patch.yml").write_text(text, encoding="utf-8")
        result = subprocess.run(
            [str(dsh), "--profile", "web", "--dump-config"], cwd=temporary,
            env=dict(os.environ, DSH_HOME=temporary, DSH_TELEMETRY_MODE="DISABLED"),
            stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=180,
        )
        warnings = {line.replace(temporary, "<dsh-home>") for line in result.stderr.splitlines() if line.strip()}
        return result.returncode, result.stdout, warnings


def dumped_command(dumped):
    """`config.command` of our row in `--dump-config` output. js-yaml re-serializes
    the value: plain, quoted, or folded (`>-`) once it passes 80 columns."""
    lines = dumped.splitlines()
    start = next((i for i, line in enumerate(lines) if ROW_ID_LINE.match(line)), None)
    if start is None:
        return None
    for i in range(start + 1, len(lines)):
        line = lines[i]
        if line.strip() and _indent(line) <= _indent(lines[start]):
            return None
        key, _, value = line.strip().partition(":")
        if key != "command":
            continue
        value = value.strip()
        if value[:1] in (">", "|"):
            body = []
            for more in lines[i + 1:]:
                if more.strip() and _indent(more) <= _indent(line):
                    break
                body.append(more.strip())
            return (" " if value[0] == ">" else "\n").join(body).strip()
        if value.startswith('"'):
            try:
                return json.loads(value)
            except ValueError:
                return value
        if value.startswith("'"):
            return value[1:-1].replace("''", "'")
        return value
    return None


def same_command(dumped, wrapper):
    command = dumped_command(dumped)
    if command is None:
        return False
    # Exact after decoding; whitespace-insensitive for quoting/folding styles not decoded above.
    return command == str(wrapper) or re.sub(r"\s+", "", command) == re.sub(r"\s+", "", str(wrapper))


def verify_with_dsh(dsh, original, planned, wrapper):
    """dsh itself must compose the planned file, list our row once, and warn no more than before."""
    code, dumped, warnings = dump_config(dsh, planned)
    before_code, _, before_warnings = dump_config(dsh, original)
    if code:
        if before_code:
            print("[note] dsh cannot compose the current cordis.patch.yml either; its parsing NOT checked")
            return
        raise InstallError(f"dsh could not compose cordis.patch.yml with the pd-memory row (exit {code})")
    rows = [line for line in dumped.splitlines() if ROW_ID_LINE.match(line)]
    if len(rows) != 1 or not same_command(dumped, wrapper):
        raise InstallError("dsh did not read the pd-memory row as written")
    # A file dsh could not load before has no baseline; one that it could must not gain warnings.
    if not before_code and warnings - before_warnings:
        raise InstallError(f"dsh reports {len(warnings - before_warnings)} new warning(s) for cordis.patch.yml")
    print("[check] dsh composed cordis.patch.yml with the pd-memory row")


def mcp_calls(command, calls, timeout, mock):
    """One MCP session: initialize, then `tools/call` each (name, arguments); results in order."""
    requests = [
        {"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": {"name": "pd-memory-installer", "version": "1"},
        }},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
    ] + [{"jsonrpc": "2.0", "id": i, "method": "tools/call", "params": {"name": name, "arguments": arguments}}
         for i, (name, arguments) in enumerate(calls, 1)]
    result = subprocess.run(command + ["serve", "mcp"], input="".join(json.dumps(r) + "\n" for r in requests),
                            text=True, capture_output=True, timeout=timeout)
    if result.returncode or (not mock and "MockProvider" in result.stderr):
        raise InstallError("MCP session failed")
    responses = {r.get("id"): r for r in map(json.loads, result.stdout.splitlines())}
    results = []
    for i, (name, _) in enumerate(calls, 1):
        value = responses.get(i, {}).get("result")
        if value is None or value.get("isError"):
            # Never echo tool output: model errors can carry provider responses.
            raise InstallError(f"MCP {name} failed")
        results.append(value)
    return results


def verify_discover_probe(command):
    """dsh's MCP SDK first sends `server/discover` (2026-07-28 era) and falls back to
    `initialize` only after an answer; a silent server would stall dsh for a minute."""
    probe = {"jsonrpc": "2.0", "id": "probe", "method": "server/discover", "params": {}}
    result = subprocess.run(command + ["serve", "mcp"], input=json.dumps(probe) + "\n",
                            text=True, capture_output=True, timeout=30)
    replies = [json.loads(line) for line in result.stdout.splitlines() if line.strip()]
    if not any(r.get("id") == "probe" and r.get("error", {}).get("code") == -32601 for r in replies):
        raise InstallError("MCP server must answer dsh's server/discover probe with -32601")


def verify_local(wrapper, mock):
    with tempfile.TemporaryDirectory(prefix="pd-memory-check-") as temporary:
        command = [str(wrapper), "--data-root", temporary]
        common.verify_empty_root(command, mock)
        common.verify_mcp(command, mock)
        verify_discover_probe(command)
    print("[check] MCP initialize, 7 tools, tool call and dsh's server/discover fallback passed")


def verify_model(wrapper, mock=False):
    """The path a dsh model takes: memory_add -> memory_flush -> memory_search."""
    with tempfile.TemporaryDirectory(prefix="pd-memory-model-check-") as temporary:
        command = [str(wrapper), "--data-root", temporary]
        _, _, found = mcp_calls(command, [
            ("memory_add", {"session_id": "installer-model-check", "messages": [
                {"role": "user", "content": common.MODEL_CHECK_FACT},
                {"role": "assistant", "content": common.MODEL_CHECK_ANSWER},
            ]}),
            ("memory_flush", {"reason": "installer"}),
            ("memory_search", {"query": common.MODEL_CHECK_QUESTION}),
        ], timeout=600, mock=mock)
        if "Nacre-731" not in json.dumps(found, ensure_ascii=False):
            raise InstallError("memory_search did not recall the test fact after memory_flush")
        if common.run_json(command + ["overview"], allow_mock=mock).get("pending_dialogue_turns"):
            raise InstallError("memory_flush left an indexing backlog")
    print("[check] real model: memory_add -> memory_flush -> memory_search over MCP passed")


# --- host interface -----------------------------------------------------------

def add_arguments(parser, mode):
    mode.add_argument("--uninstall", action="store_true",
                      help="remove the pd-memory row from cordis.patch.yml (backed up)")
    parser.add_argument("--dsh-home", type=common.absolute, help="default: <home>/.dsh (DSH_HOME is not read)")
    parser.add_argument("--dsh-binary", type=common.absolute,
                        help="dsh CLI that checks the result (default: dsh on PATH, then the Desktop app's)")
    parser.add_argument("--skip-dsh-check", action="store_true", help="do not run dsh to parse the result")


def host_paths(args, paths):
    paths.host_home = args.dsh_home or paths.home / ".dsh"
    paths.patch = paths.host_home / "cordis.patch.yml"


def managed_files(args, paths):
    return [paths.patch]


def load_config(paths):
    if paths.config.exists():
        return common.read_object(paths.config)
    # Reuse another host's model and data root, never its workspace binding.
    for other in (paths.shared_config, paths.private / "codex.json"):
        current = common.read_object(other)
        if current:
            return {k: v for k, v in current.items() if k not in ("workspace_root", "workspace_id")}
    return {}


def default_workspace(args, paths):
    return None


def integration_changes(args, paths):
    paths.original_patch = common.read_text(paths.patch)
    if not paths.host_home.is_dir():
        print(f"[note] {paths.host_home} not found; the row takes effect once DeepSeek Harness runs")
    note_profile_duplicates(paths)
    merged = merge_patch(paths.original_patch, paths.wrapper)
    return [(paths.patch, merged.encode(), common.mode_of(paths.patch))]


def check_with_dsh(args, paths, planned_text, wrapper):
    if args.skip_dsh_check:
        return
    dsh = find_dsh(args, paths)
    if dsh is None:
        print("[note] dsh CLI not found; DeepSeek Harness's own parsing of cordis.patch.yml NOT checked")
        return
    if wrapper is None:
        code, _, _ = dump_config(dsh, planned_text)
        if code:
            raise InstallError(f"dsh could not compose cordis.patch.yml without the pd-memory row (exit {code})")
        return
    verify_with_dsh(dsh, paths.original_patch, planned_text, wrapper)


def verify(args, paths, planned):
    verify_local(paths.wrapper, args.mock)
    check_with_dsh(args, paths, planned[paths.patch].decode("utf-8"), paths.wrapper)
    if args.verify_model:
        verify_model(paths.wrapper)


def uninstall(args, paths):
    text = common.read_text(paths.patch)
    merged = merge_patch(text, None)
    if merged == text:
        print(f"[uninstall] no pd-memory row in {paths.patch}; nothing to remove")
        return
    print(f"[plan] remove pd-memory from {paths.patch}")
    with common.installation_lock(paths.state_root):
        paths.state.mkdir(mode=0o700, exist_ok=True)
        common.apply_changes([(paths.patch, merged.encode(), common.mode_of(paths.patch))], paths.state,
                             lambda: check_with_dsh(args, paths, merged, None))
    print("[uninstall] pd-memory removed from cordis.patch.yml; binary, model config and memory kept.")
    print(f"            Restart {LABEL}. Revert with --rollback if needed.")


def post_install(args):
    print("[result] installed. Restart DeepSeek Harness and start a new session; the tools appear")
    print("         as mcp__pd-memory__*. memory_add only queues: memory_flush makes it searchable.")
    print("         With the official DeepSeek API, dsh uploads session logs (tool results included)")
    print("         by default; to opt out, turn it off in Settings -> General.")
