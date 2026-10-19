#!/usr/bin/env bash
# Merge (or revert) the pd-memory MCP server entry into WorkBuddy's config.
#
#   default      : dry-run, prints the JSON diff, writes NOTHING
#   --apply      : backs up the target file, then writes the merged JSON
#   --revert     : restores the newest backup (keeps a safety copy of the
#                  current file as *.pre-revert.bak.<timestamp>)
#
# Targets:
#   --mcp-target connector  ~/.workbuddy/connectors/default/mcp.json  (GUI connector registry; default)
#   --mcp-target cli        ~/.workbuddy/.mcp.json                    (CodeBuddy CLI user scope)
#   --mcp-target both       both of the above
#   --with-hooks            also merge examples/workbuddy/hooks/settings.json into
#                           ~/.workbuddy/settings.json under the top-level "hooks" key
#
# Unrelated entries are never deleted. All paths are made absolute. Nothing is
# written unless --apply (or --revert) is passed.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SDK_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
export PD_SDK_ROOT="$SDK_ROOT"

exec python3 - "$@" <<'PYEOF'
import argparse
import datetime as _dt
import difflib
import json
import os
import shutil
import sys
from pathlib import Path

PROG = "install_workbuddy.sh"
PLACEHOLDERS = ("__PD_MEMORY_BIN__", "__PD_MEMORY_CONFIG__", "__PD_MEMORY_DATA_ROOT__")
MARKER = "pd-memory:"


def eprint(*args):
    print(*args, file=sys.stderr)


def now_stamp():
    return _dt.datetime.now().strftime("%Y%m%d-%H%M%S")


def abs_path(value, base=None):
    path = Path(value).expanduser()
    if not path.is_absolute():
        path = (base or Path.cwd()) / path
    return Path(os.path.abspath(str(path)))


def read_json(path):
    if not path.exists():
        return None, None
    text = path.read_text(encoding="utf-8")
    try:
        return json.loads(text), text
    except json.JSONDecodeError as exc:
        return None, text


def dump_json(data):
    return json.dumps(data, ensure_ascii=False, indent=2) + "\n"


def print_diff(path, old_text, new_text, label):
    old_lines = (old_text or "").splitlines(keepends=True)
    new_lines = new_text.splitlines(keepends=True)
    diff = list(
        difflib.unified_diff(
            old_lines,
            new_lines,
            fromfile=f"{path} (current)",
            tofile=f"{path} (merged)",
            lineterm="\n",
        )
    )
    if not diff:
        print(f"[{label}] no changes needed: {path}")
        return False
    print(f"[{label}] diff for {path}:")
    sys.stdout.writelines(diff)
    return True


def backup_file(path):
    stamp = now_stamp()
    backup = path.with_name(f"{path.name}.bak.{stamp}")
    suffix = 1
    while backup.exists():
        backup = path.with_name(f"{path.name}.bak.{stamp}-{suffix}")
        suffix += 1
    path.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(path, backup)
    print(f"[backup] {path} -> {backup}")
    return backup


def write_json(path, data, old_text):
    new_text = dump_json(data)
    if old_text == new_text:
        return False
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp.pd-memory")
    tmp.write_text(new_text, encoding="utf-8")
    os.replace(tmp, path)
    print(f"[write] {path}")
    return True


def merge_plan(path, base, entry, label):
    current, old_text = read_json(path)
    if current is None and old_text is not None:
        eprint(f"[error] {path} exists but is not valid JSON; refusing to touch it")
        sys.exit(2)
    data = current if isinstance(current, dict) else base
    servers = data.setdefault("mcpServers", {})
    if not isinstance(servers, dict):
        eprint(f"[error] {path}: mcpServers is not an object; refusing to touch it")
        sys.exit(2)
    servers["pd-memory"] = entry
    if label == "cli":
        disabled = data.setdefault("disabledMcpServers", [])
        if isinstance(disabled, list):
            data["disabledMcpServers"] = [item for item in disabled if item != "pd-memory"]
    return data, old_text


def is_pd_memory_group(group, command_fingerprint):
    if not isinstance(group, dict):
        return False
    for hook in group.get("hooks", []) or []:
        if not isinstance(hook, dict):
            continue
        description = str(hook.get("description", "") or "")
        command = str(hook.get("command", "") or "")
        if description.startswith(MARKER) or command_fingerprint in command:
            return True
    return False


def substitute(value):
    if isinstance(value, str):
        for token, replacement in substitutes.items():
            value = value.replace(token, replacement)
        return value
    if isinstance(value, list):
        return [substitute(item) for item in value]
    if isinstance(value, dict):
        return {key: substitute(item) for key, item in value.items()}
    return value


def merge_hooks_plan(path, fragment_path, command_fingerprint):
    fragment_raw, _ = read_json(fragment_path)
    if not isinstance(fragment_raw, dict) or not isinstance(fragment_raw.get("hooks"), dict):
        eprint(f"[error] hooks fragment not found or invalid (expected {{\"hooks\": {{...}}}}): {fragment_path}")
        sys.exit(2)
    fragment = substitute(fragment_raw["hooks"])
    current, old_text = read_json(path)
    if current is None and old_text is not None:
        eprint(f"[error] {path} exists but is not valid JSON; refusing to touch it")
        sys.exit(2)
    data = current if isinstance(current, dict) else {}
    hooks = data.get("hooks")
    if not isinstance(hooks, dict):
        hooks = {}
    for event, groups in fragment.items():
        existing = [g for g in (hooks.get(event) or []) if not is_pd_memory_group(g, command_fingerprint)]
        hooks[event] = existing + groups
    data["hooks"] = hooks
    return data, old_text


def find_newest_backup(path):
    candidates = sorted(path.parent.glob(path.name + ".bak.*"))
    return candidates[-1] if candidates else None


def do_revert(paths):
    changed = False
    for path in paths:
        backup = find_newest_backup(path)
        if backup is None:
            print(f"[revert] no backup found for {path}; nothing to restore")
            continue
        current_text = path.read_text(encoding="utf-8") if path.exists() else ""
        backup_text = backup.read_text(encoding="utf-8")
        if path.exists() and current_text != backup_text:
            safety = path.with_name(f"{path.name}.pre-revert.bak.{now_stamp()}")
            shutil.copy2(path, safety)
            print(f"[backup] current {path} -> {safety}")
        print_diff(path, current_text, backup_text, "revert")
        path.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(backup, path)
        print(f"[revert] restored {path} from {backup}")
        changed = True
    return changed


def main():
    global substitutes
    parser = argparse.ArgumentParser(prog=PROG, description="Install pd-memory into WorkBuddy config (dry-run by default).")
    parser.add_argument("--apply", action="store_true", help="write changes (after backing up targets)")
    parser.add_argument("--revert", action="store_true", help="restore the newest backup of the target files")
    parser.add_argument("--dry-run", action="store_true", help="explicit dry-run (default)")
    parser.add_argument("--with-hooks", action="store_true", help="also merge hooks into ~/.workbuddy/settings.json")
    parser.add_argument("--mcp-target", choices=["connector", "cli", "both"], default="connector")
    parser.add_argument("--binary", default=os.environ.get("PD_MEMORY_BIN", "target/debug/pd-memory"))
    parser.add_argument("--config", default=os.environ.get("PD_MEMORY_CONFIG", "examples/workbuddy/pd-memory.config.json"))
    parser.add_argument("--data-root", default=os.environ.get("PD_MEMORY_DATA_ROOT", "examples/workbuddy/.run/data"))
    parser.add_argument("--workbuddy-home", default=os.environ.get("WORKBUDDY_HOME", "~/.workbuddy"))
    args = parser.parse_args()

    if args.apply and args.revert:
        parser.error("--apply and --revert are mutually exclusive")

    sdk_root = Path(os.environ["PD_SDK_ROOT"])
    wb_home = abs_path(args.workbuddy_home)
    binary = abs_path(args.binary, base=sdk_root)
    config = abs_path(args.config, base=sdk_root)
    data_root = abs_path(args.data_root, base=sdk_root)
    fragment_path = sdk_root / "examples" / "workbuddy" / "hooks" / "settings.json"

    apply = args.apply and not args.revert
    mode = "REVERT" if args.revert else ("APPLY" if apply else "DRY-RUN")

    command = (
        f"env PD_MEMORY_PROVIDER=mock PD_MEMORY_DATA_ROOT={data_root} "
        f"{binary} --config {config} hook"
    )
    substitutes = {
        "__PD_MEMORY_BIN__": str(binary),
        "__PD_MEMORY_CONFIG__": str(config),
        "__PD_MEMORY_DATA_ROOT__": str(data_root),
    }

    print(f"== pd-memory <-> WorkBuddy config ({mode}) ==")
    print(f"workbuddy home : {wb_home}")
    print(f"binary         : {binary}")
    print(f"config         : {config}")
    print(f"data root      : {data_root}")
    print(f"mcp target     : {args.mcp_target}")
    print(f"with hooks     : {'yes' if args.with_hooks else 'no'}")
    if not binary.exists():
        eprint(f"[warn] binary not found: {binary}\n       build it first: cd {sdk_root} && cargo build -p pd-memory-cli")
    print()

    mcp_entry = {
        "type": "stdio",
        "command": str(binary),
        "args": ["--config", str(config), "serve", "mcp"],
        "env": {
            "PD_MEMORY_PROVIDER": "mock",
            "PD_MEMORY_DATA_ROOT": str(data_root),
        },
        "timeout": 120000,
    }

    plans = []  # (path, data, old_text, label)
    if args.mcp_target in ("connector", "both"):
        path = wb_home / "connectors" / "default" / "mcp.json"
        entry = dict(mcp_entry, disabled=False)
        data, old_text = merge_plan(path, {"mcpServers": {}}, entry, "connector")
        plans.append((path, data, old_text, "connector registry"))
    if args.mcp_target in ("cli", "both"):
        path = wb_home / ".mcp.json"
        data, old_text = merge_plan(path, {"mcpServers": {}, "disabledMcpServers": []}, mcp_entry, "cli")
        plans.append((path, data, old_text, "CLI .mcp.json"))
    if args.with_hooks:
        path = wb_home / "settings.json"
        data, old_text = merge_hooks_plan(path, fragment_path, str(binary))
        plans.append((path, data, old_text, "settings hooks"))

    if not plans:
        print("nothing selected; use --mcp-target ... and/or --with-hooks")
        return 0

    if args.revert:
        changed = do_revert([item[0] for item in plans])
        print()
        print("[result] reverted backup(s)" if changed else "[result] nothing reverted")
        return 0

    changed = False
    for path, data, old_text, label in plans:
        new_text = dump_json(data)
        if print_diff(path, old_text, new_text, label):
            changed = True
            if apply:
                if old_text is not None and not path.exists():
                    pass
                if path.exists():
                    backup_file(path)
                write_json(path, data, old_text)
    print()
    if apply:
        print("[result] applied. Restart WorkBuddy for connector-registry changes to be picked up.")
        print("[note] ~/.workbuddy/settings.json is a WorkBuddy-protected file; backups are kept next to it.")
    else:
        print("[result] dry-run only. Re-run with --apply to write the changes shown above.")
    return 0 if changed or True else 0


if __name__ == "__main__":
    sys.exit(main())
PYEOF
