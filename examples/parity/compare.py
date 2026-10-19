#!/usr/bin/env python3
"""Differential comparator for PD memory parity artifacts.

Reads the artifact JSON written by `ts_runner.mjs` and `rust_runner.py` for one
scenario and compares every observable memory artifact:

  * workspace memory files (file set + normalized bytes + frontmatter)
  * the derived `MEMORY.md` manifest
  * global (user identity) files
  * L0 capture rows (id / messages / indexed flag)
  * per-step API output: capture, flush, list, overview
  * auto recall (route, selected ids, rendered context) and explicit search
  * dream result (counts / status) and rollback readiness
  * project isolation for multi-workspace scenarios
  * the control-plane format (documented deviation, reported as INFO)

Observed results are PASS or DIFF; storage-format differences are reported as
INFO. Every observable output mismatch fails the comparison; no behavioral
exceptions or manifest-link rewriting are applied.

Usage:
    python3 compare.py --scenario P1 --run-dir <dir> [--verbose] [--report out.json]
"""

from __future__ import annotations

import argparse
import difflib
import json
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent

ISO_TS = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z")
HASH_SUFFIX = re.compile(r"-[0-9a-f]{10}\.md$")
FRONTMATTER = re.compile(r"\A---\n(.*?)\n---\n", re.DOTALL)


class Aspect:
    def __init__(self, name: str, verdict: str, detail: str) -> None:
        self.name = name
        self.verdict = verdict
        self.detail = detail

    def to_dict(self) -> dict:
        return {
            "aspect": self.name,
            "verdict": self.verdict,
            "detail": self.detail,
        }


def check(name: str, condition: bool, pass_detail: str, diff_detail: str) -> Aspect:
    return Aspect(name, "PASS" if condition else "DIFF", pass_detail if condition else diff_detail)


def info(name: str, detail: str) -> Aspect:
    return Aspect(name, "INFO", detail)


def load_json(path: Path) -> dict | None:
    if not path.exists():
        return None
    return json.loads(path.read_text(encoding="utf-8"))


def side_roots(artifact: dict) -> dict[str, str]:
    """Return the per-side root dir and per-workspace abs paths for normalization."""
    roots: list[str] = []
    for workspace in artifact.get("workspaces", {}).values():
        data_dir = workspace.get("dataDir")
        if data_dir:
            roots.append(str(Path(data_dir).parent.parent))
    roots = sorted({item for item in roots if item})
    return {"roots": roots}


def make_normalizer(artifact: dict):
    """Build a text normalizer bound to one side's transient paths/instants."""
    root_list = side_roots(artifact)["roots"]
    workspace_map = {
        workspace["workspacePath"]: f"<WS:{alias}>"
        for alias, workspace in artifact.get("workspaces", {}).items()
        if workspace.get("workspacePath")
    }

    def normalize(text: str) -> str:
        if not isinstance(text, str):
            text = json.dumps(text, ensure_ascii=False)
        for workspace_path in sorted(workspace_map, key=len, reverse=True):
            text = text.replace(workspace_path, workspace_map[workspace_path])
        for root in sorted(root_list, key=len, reverse=True):
            text = text.replace(root, "<ROOT>")
        return ISO_TS.sub("<TS>", text)

    return normalize


def normalize_recursive(value, normalize):
    if isinstance(value, str):
        return normalize(value)
    if isinstance(value, list):
        return [normalize_recursive(item, normalize) for item in value]
    if isinstance(value, dict):
        return {key: normalize_recursive(item, normalize) for key, item in value.items()}
    return value


def files_to_map(files: list[dict]) -> dict[str, str]:
    return {record["path"]: record["content"] for record in files}


def parse_frontmatter(content: str) -> dict[str, str]:
    match = FRONTMATTER.match(content)
    if not match:
        return {}
    fields: dict[str, str] = {}
    for line in match.group(1).splitlines():
        if ":" not in line:
            continue
        key, value = line.split(":", 1)
        fields[key.strip()] = value.strip()
    return fields


def describe_file_diff(path: str, left: str, right: str) -> str:
    diff = list(
        difflib.unified_diff(
            left.splitlines(),
            right.splitlines(),
            fromfile="ts",
            tofile="rust",
            lineterm="",
        )
    )
    body = "\n".join(diff[2:14])
    return f"{path}:\n{body}"


def rust_step_result(rust_ws: dict, step_id: str) -> dict | None:
    result = rust_ws["results"].get(step_id)
    if not result:
        return None
    payload = result.get("stdout")
    return payload if isinstance(payload, dict) else None


def ts_step_result(ts_ws: dict, step_id: str) -> dict | None:
    result = ts_ws["results"].get(step_id)
    return result if isinstance(result, dict) else None


# ---------------------------------------------------------------------------
# Aspect comparators
# ---------------------------------------------------------------------------


def compare_errors(ts: dict, rust: dict) -> list[Aspect]:
    aspects = []
    problems = []
    for side_name, artifact in (("ts", ts), ("rust", rust)):
        for alias, workspace in artifact.get("workspaces", {}).items():
            for step_id, result in workspace.get("results", {}).items():
                if result.get("op") in ("rollback",):
                    continue
                if not result.get("ok", False):
                    problems.append(f"{side_name}/{alias}/{step_id}: {result.get('stderr') or result.get('error')}")
    aspects.append(
        check(
            "steps",
            not problems,
            "every capture/flush/list/overview/retrieve/search/dream step succeeded on both sides",
            "step failures: " + " | ".join(problems[:4]),
        )
    )
    return aspects


def compare_files(ts: dict, rust: dict) -> list[Aspect]:
    aspects = []
    ts_norm = make_normalizer(ts)
    rust_norm = make_normalizer(rust)
    ts_aliases = set(ts.get("workspaces", {}))
    rust_aliases = set(rust.get("workspaces", {}))
    if ts_aliases != rust_aliases:
        return [Aspect("files", "DIFF", f"workspace aliases differ: ts={sorted(ts_aliases)} rust={sorted(rust_aliases)}")]
    for alias in sorted(ts_aliases):
        ts_files = files_to_map(ts["workspaces"][alias].get("files", []))
        rust_files = files_to_map(rust["workspaces"][alias].get("files", []))
        ts_map = {path: ts_norm(content) for path, content in ts_files.items()}
        rust_map = {path: rust_norm(content) for path, content in rust_files.items()}
        missing = sorted(set(ts_map) - set(rust_map))
        extra = sorted(set(rust_map) - set(ts_map))
        changed = sorted(path for path in set(ts_map) & set(rust_map) if ts_map[path] != rust_map[path])
        count = len(ts_map)
        if not missing and not extra and not changed:
            aspects.append(Aspect("files", "PASS", f"ws {alias}: {count} memory file(s) byte-identical after timestamp normalization ({', '.join(sorted(ts_map))})"))
            continue
        parts = []
        if missing:
            parts.append(f"missing in rust: {missing}")
        if extra:
            parts.append(f"extra in rust: {extra}")
        if changed:
            parts.append("; ".join(describe_file_diff(path, ts_map[path], rust_map[path]) for path in changed[:3]))
        aspects.append(Aspect("files", "DIFF", f"ws {alias}: " + " | ".join(parts)))
    return aspects


def compare_frontmatter(ts: dict, rust: dict) -> list[Aspect]:
    ts_norm = make_normalizer(ts)
    rust_norm = make_normalizer(rust)
    total_fields = 0
    problems: list[str] = []
    for alias in sorted(ts.get("workspaces", {})):
        ts_files = files_to_map(ts["workspaces"][alias].get("files", []))
        rust_files = files_to_map(rust["workspaces"][alias].get("files", []))
        for path in sorted(set(ts_files) & set(rust_files)):
            ts_fields = parse_frontmatter(ts_files[path])
            rust_fields = parse_frontmatter(rust_files[path])
            if not ts_fields and not rust_fields:
                continue
            total_fields += len(ts_fields)
            ts_norm_fields = {key: ts_norm(value) for key, value in ts_fields.items()}
            rust_norm_fields = {key: rust_norm(value) for key, value in rust_fields.items()}
            if ts_norm_fields != rust_norm_fields:
                keys = sorted(set(ts_norm_fields) | set(rust_norm_fields))
                delta = {
                    key: {"ts": ts_norm_fields.get(key), "rust": rust_norm_fields.get(key)}
                    for key in keys
                    if ts_norm_fields.get(key) != rust_norm_fields.get(key)
                }
                problems.append(f"{path}: {delta}")
    if problems:
        return [Aspect("frontmatter", "DIFF", " | ".join(problems[:3]))]
    return [Aspect("frontmatter", "PASS", f"frontmatter fields identical for all files ({total_fields} fields compared)")]


def compare_manifest(ts: dict, rust: dict) -> list[Aspect]:
    """Compare MEMORY.md exactly, apart from timestamps and transient roots."""
    ts_norm = make_normalizer(ts)
    rust_norm = make_normalizer(rust)
    aspects: list[Aspect] = []
    for alias in sorted(ts.get("workspaces", {})):
        ts_memory = files_to_map(ts["workspaces"][alias].get("files", []))
        rust_memory = files_to_map(rust["workspaces"][alias].get("files", []))
        if "MEMORY.md" not in ts_memory and "MEMORY.md" not in rust_memory:
            continue
        left = ts_norm(ts_memory.get("MEMORY.md", "<missing>"))
        right = rust_norm(rust_memory.get("MEMORY.md", "<missing>"))
        aspects.append(check(
            "manifest", left == right,
            f"ws {alias}: MEMORY.md identical after normalization",
            f"ws {alias}: " + describe_file_diff("MEMORY.md", left, right),
        ))
    return aspects or [Aspect("manifest", "PASS", "no MEMORY.md in this scenario")]


def compare_global_files(ts: dict, rust: dict) -> list[Aspect]:
    ts_norm = make_normalizer(ts)
    rust_norm = make_normalizer(rust)
    ts_files = files_to_map(ts.get("globalFiles", []))
    rust_files = files_to_map(rust.get("globalFiles", []))
    if not ts_files and not rust_files:
        return [Aspect("global-files", "PASS", "no global user files on either side")]
    missing = sorted(set(ts_files) - set(rust_files))
    extra = sorted(set(rust_files) - set(ts_files))
    changed = []
    for path in sorted(set(ts_files) & set(rust_files)):
        ts_content = ts_norm(ts_files[path])
        rust_content = rust_norm(rust_files[path])
        if ts_content == rust_content:
            continue
        changed.append(describe_file_diff(path, ts_content, rust_content))
    if not missing and not extra and not changed:
        return [
            Aspect(
                "global-files",
                "PASS",
                f"{len(ts_files)} global user file(s) byte-identical after normalization ({', '.join(sorted(ts_files))})",
            )
        ]
    parts = []
    if missing:
        parts.append(f"missing in rust: {missing}")
    if extra:
        parts.append(f"extra in rust: {extra}")
    if changed:
        parts.append("; ".join(changed[:3]))
    return [Aspect("global-files", "DIFF", " | ".join(parts))]


def canonical_capture_ts(result: dict) -> dict:
    return {
        "captured": result.get("captured"),
        "sessionKey": result.get("sessionKey"),
        "messages": result.get("normalizedMessages", []),
    }


def canonical_capture_rust(result: dict) -> dict:
    payload = result.get("stdout") or {}
    return {
        "captured": payload.get("captured"),
        "sessionKey": payload.get("session_key"),
        "messages": payload.get("normalized_messages", []),
    }


def compare_captures(ts: dict, rust: dict, steps: list[dict]) -> list[Aspect]:
    aspects: list[Aspect] = []
    for step in steps:
        if step["op"] != "capture":
            continue
        alias = step["ws"]
        ts_result = ts_step_result(ts["workspaces"][alias], step["id"])
        rust_result = rust_step_result(rust["workspaces"][alias], step["id"])
        if ts_result is None or rust_result is None:
            aspects.append(Aspect(f"capture/{step['id']}", "DIFF", "capture step missing on one side"))
            continue
        ts_capture = canonical_capture_ts(ts_result)
        rust_capture = canonical_capture_rust({"stdout": rust_result})
        same_messages = ts_capture["messages"] == rust_capture["messages"]
        same_session = ts_capture["sessionKey"] == rust_capture["sessionKey"]
        if ts_capture["captured"] == rust_capture["captured"] and same_messages and same_session:
            aspects.append(
                Aspect(
                    f"capture/{step['id']}",
                    "PASS",
                    f"captured={ts_capture['captured']}, sessionKey={ts_capture['sessionKey']}, "
                    f"{len(ts_capture['messages'])} normalized message(s) identical",
                )
            )
            continue
        aspects.append(
            Aspect(
                f"capture/{step['id']}",
                "DIFF",
                f"TS={ts_capture} rust={rust_capture}",
            )
        )
    return aspects


def compare_runtime(ts: dict, rust: dict, steps: list[dict]) -> list[Aspect]:
    aspects = []
    for step in steps:
        if step["op"] != "runtime":
            continue
        alias, step_id = step["ws"], step["id"]
        left = ts_step_result(ts["workspaces"][alias], step_id) or {}
        right = rust_step_result(rust["workspaces"][alias], step_id) or {}
        a, b = left.get("events", []), right.get("events", [])
        aspects.append(check(f"runtime/{step_id}/count", len(a) == len(b) == len(step["actions"]),
            f"{len(a)} runtime events", f"event counts: TS={len(a)}, Rust={len(b)}"))
        for index, action in enumerate(step["actions"]):
            x, y = (a[index] if index < len(a) else {}), (b[index] if index < len(b) else {})
            name = f"runtime/{step_id}/{index}-{action['op']}"
            aspects.append(check(name, x == y, "runtime results and anchors identical",
                f"TS={json.dumps(x, ensure_ascii=False)} Rust={json.dumps(y, ensure_ascii=False)}"))
            if "expect" in action:
                expected = action["expect"]
                aspects.append(check(f"{name}/expected", all(x.get(k) == v and y.get(k) == v for k, v in expected.items()),
                    f"both satisfy {expected}", f"expected {expected}; TS={x} Rust={y}"))
    return aspects


def compare_flushes(ts: dict, rust: dict, steps: list[dict]) -> list[Aspect]:
    aspects: list[Aspect] = []
    for step in steps:
        if step["op"] != "flush":
            continue
        alias = step["ws"]
        ts_result = ts_step_result(ts["workspaces"][alias], step["id"])
        rust_result = rust_step_result(rust["workspaces"][alias], step["id"])
        if ts_result is None or rust_result is None:
            aspects.append(Aspect(f"flush/{step['id']}", "DIFF", "flush step missing on one side"))
            continue
        ts_stats = ts_result.get("stats") or {}
        rust_stats = rust_result or {}
        ts_written = ts_stats.get("writtenFiles")
        rust_written = rust_stats.get("written_files")
        ts_errors = ts_stats.get("failedSessions")
        rust_errors = len(rust_stats.get("errors") or [])
        ts_processed = ts_stats.get("capturedSessions")
        rust_processed = rust_stats.get("processed_sessions")
        if ts_written == rust_written and ts_errors == rust_errors and ts_processed == rust_processed:
            aspects.append(
                Aspect(
                    f"flush/{step['id']}",
                    "PASS",
                    f"processed_rows={ts_processed}, written_files={ts_written}, failed_sessions={ts_errors}",
                )
            )
            continue
        aspects.append(
            Aspect(
                f"flush/{step['id']}",
                "DIFF",
                f"TS processed={ts_processed} written={ts_written} failed={ts_errors} vs Rust processed={rust_processed} written={rust_written} errors={rust_errors}",
            )
        )
    return aspects


def canonical_ts_entry(entry: dict) -> dict:
    return {
        "name": entry.get("name"),
        "description": entry.get("description"),
        "type": entry.get("type"),
        "scope": entry.get("scope"),
        "projectId": entry.get("projectId"),
        "capturedAt": entry.get("capturedAt"),
        "updatedAt": entry.get("updatedAt"),
        "sourceSessionKey": entry.get("sourceSessionKey"),
        "relativePath": entry.get("relativePath"),
        "deprecated": entry.get("deprecated"),
        "dreamAttempts": entry.get("dreamAttempts"),
    }


def canonical_rust_entry(entry: dict) -> dict:
    return {
        "name": entry.get("name"),
        "description": entry.get("description"),
        "type": entry.get("type"),
        "scope": entry.get("scope"),
        "projectId": entry.get("project_id"),
        "capturedAt": entry.get("captured_at"),
        "updatedAt": entry.get("updated_at"),
        "sourceSessionKey": entry.get("source_session_key"),
        "relativePath": entry.get("relative_path"),
        "deprecated": entry.get("deprecated"),
        "dreamAttempts": entry.get("dream_attempts"),
    }


def compare_lists(ts: dict, rust: dict, steps: list[dict]) -> list[Aspect]:
    ts_norm = make_normalizer(ts)
    rust_norm = make_normalizer(rust)
    aspects: list[Aspect] = []
    for step in steps:
        if step["op"] != "list":
            continue
        alias = step["ws"]
        ts_result = ts_step_result(ts["workspaces"][alias], step["id"]) or {}
        rust_result = rust_step_result(rust["workspaces"][alias], step["id"]) or {}
        ts_entries = [canonical_ts_entry(entry) for entry in ts_result.get("entries", [])]
        rust_entries = [canonical_rust_entry(entry) for entry in rust_result.get("entries", [])]
        ts_entries.sort(key=lambda entry: entry["relativePath"] or "")
        rust_entries.sort(key=lambda entry: entry["relativePath"] or "")
        ts_norm_entries = normalize_recursive(ts_entries, ts_norm)
        rust_norm_entries = normalize_recursive(rust_entries, rust_norm)
        paths_ts = sorted(entry["relativePath"] for entry in ts_norm_entries)
        if ts_norm_entries == rust_norm_entries:
            aspects.append(
                Aspect(
                    f"list/{step['id']}",
                    "PASS",
                    f"ws {alias}: {len(ts_norm_entries)} entry/entries identical ({', '.join(paths_ts)})",
                )
            )
        else:
            aspects.append(Aspect(f"list/{step['id']}", "DIFF",
                f"entries differ: TS={ts_norm_entries} Rust={rust_norm_entries}"))
        if "expect_count" in step:
            expected = step["expect_count"]
            aspects.append(check(f"list/{step['id']}/expected", len(ts_entries) == expected and len(rust_entries) == expected,
                f"both contain {expected} memories", f"expected {expected} memories"))
    return aspects


def compare_overviews(ts: dict, rust: dict, steps: list[dict]) -> list[Aspect]:
    aspects: list[Aspect] = []
    for step in steps:
        if step["op"] != "overview":
            continue
        alias = step["ws"]
        ts_result = ts_step_result(ts["workspaces"][alias], step["id"]) or {}
        rust_result = rust_step_result(rust["workspaces"][alias], step["id"]) or {}
        ts_overview = ts_result.get("overview") or {}
        canonical_ts = {
            "workspaceMode": ts_overview.get("workspaceMode"),
            "projectMemoryCount": ts_overview.get("projectMemoryCount"),
            "feedbackMemoryCount": ts_overview.get("feedbackMemoryCount"),
            "userProfileCount": ts_overview.get("userProfileCount"),
            "pendingSessions": ts_overview.get("pendingSessions"),
            "projectMetaPresent": ts_overview.get("projectMetaPresent"),
            "lastIndexedAt": ts_overview.get("lastIndexedAt"),
        }
        # TS `userProfileCount` is 1 iff the global profile document exists;
        # Rust's `user_profiles` counter counts User-kind notes, which is a
        # different (admin-telemetry only) notion. Compare the same semantic:
        # the existence of the global profile file on the Rust artifact.
        rust_profile_present = rust["workspaces"][alias]["results"][step["id"]]["profilePresent"]
        canonical_rust = {
            "workspaceMode": rust_result.get("workspace_mode"),
            "projectMemoryCount": rust_result.get("project_memories"),
            "feedbackMemoryCount": rust_result.get("feedback_memories"),
            "userProfileCount": 1 if rust_profile_present else 0,
            "pendingSessions": rust_result.get("pending_sessions"),
            "projectMetaPresent": rust_result.get("project_meta") is not None,
            "lastIndexedAt": rust_result.get("last_indexed_at"),
        }
        if canonical_ts == canonical_rust:
            aspects.append(
                Aspect(
                    f"overview/{step['id']}",
                    "PASS",
                    f"ws {alias}: {json.dumps(canonical_ts, ensure_ascii=False)} "
                    f"(Rust overview.user_profiles={rust_result.get('user_profiles')} counts user notes; "
                    f"profile document present={rust_profile_present})",
                )
            )
        else:
            aspects.append(
                Aspect(
                    f"overview/{step['id']}",
                    "DIFF",
                    f"ws {alias}: TS={json.dumps(canonical_ts, ensure_ascii=False)} "
                    f"Rust={json.dumps(canonical_rust, ensure_ascii=False)}",
                )
            )
    return aspects


def compare_retrievals(ts: dict, rust: dict, steps: list[dict]) -> list[Aspect]:
    ts_norm, rust_norm = make_normalizer(ts), make_normalizer(rust)
    aspects: list[Aspect] = []
    for step in steps:
        if step["op"] not in ("retrieve", "search"):
            continue
        alias, step_id = step["ws"], step["id"]
        left = ts_step_result(ts["workspaces"][alias], step_id) or {}
        right = rust_step_result(rust["workspaces"][alias], step_id) or {}
        prefix = f"{step['op']}/{step_id}"
        route = left.get("intent")
        aspects.append(check(f"{prefix}/route", route == right.get("route"),
            f"route={route}", f"route differs: TS={route} Rust={right.get('route')}"))
        left_ids = left.get("selectedFileIds") or []
        right_ids = right.get("selected_ids") or []
        aspects.append(check(f"{prefix}/selected", left_ids == right_ids,
            f"selected ids and order identical: {left_ids}",
            f"selected ids differ: TS={left_ids} Rust={right_ids}"))
        if step["op"] == "retrieve":
            right_system = (right.get("hookSpecificOutput") or {}).get("additionalContext") or ""
        else:
            right_system = right.get("system_context") or ""
            left_raw = ts_norm(left.get("context") or "")
            right_raw = rust_norm(right.get("context") or "")
            aspects.append(check(f"{prefix}/render", left_raw == right_raw,
                "raw recall block identical", describe_file_diff("recall", left_raw, right_raw)))
        left_system = ts_norm(left.get("systemContext") or "")
        right_system = rust_norm(right_system)
        aspects.append(check(f"{prefix}/system-context", left_system == right_system,
            "final injected system context identical",
            describe_file_diff("system-context", left_system, right_system)))
        if "expect_route" in step:
            expected = step["expect_route"]
            aspects.append(check(f"{prefix}/expected-route", route == expected and right.get("route") == expected,
                f"both routes match {expected}", f"expected route {expected}"))
        if "expect_selected_count" in step:
            expected = step["expect_selected_count"]
            aspects.append(check(f"{prefix}/expected-selection", len(left_ids) == expected and len(right_ids) == expected,
                f"both selected {expected} files", f"expected {expected} selected files"))
    return aspects


def compare_model_calls(ts: dict, rust: dict, steps: list[dict]) -> list[Aspect]:
    """Check invocation order and every recall candidate before model selection."""
    aspects: list[Aspect] = []
    for step in steps:
        if step["op"] not in ("retrieve", "search", "flush", "runtime"):
            continue
        alias, step_id = step["ws"], step["id"]
        left = ts["workspaces"][alias]["results"][step_id].get("modelRequests", [])
        right = rust["workspaces"][alias]["results"][step_id].get("modelRequests", [])
        purposes_left = [call["purpose"] for call in left]
        purposes_right = [call["purpose"] for call in right]
        aspects.append(check(f"model/{step_id}/sequence", purposes_left == purposes_right,
            f"model stages identical: {purposes_left}",
            f"model stages differ: TS={purposes_left} Rust={purposes_right}"))
        transport_left = [(call.get("status"), call.get("jsonMode")) for call in left]
        transport_right = [(call.get("status"), call.get("jsonMode")) for call in right]
        aspects.append(check(f"model/{step_id}/transport", transport_left == transport_right,
            "HTTP statuses and JSON-mode fallback identical", f"TS={transport_left} Rust={transport_right}"))
        if "expect_model_calls" in step:
            expected = step["expect_model_calls"]
            aspects.append(check(f"model/{step_id}/expected-calls", purposes_left == expected and purposes_right == expected,
                f"model stages match {expected}", f"expected model stages {expected}"))
        for index, (a, b) in enumerate(zip(left, right)):
            if a["purpose"] != "recall_select" or b["purpose"] != "recall_select":
                continue
            a_ids = [item["id"] for item in json.loads(a["user"])["manifest"]]
            b_ids = [item["id"] for item in json.loads(b["user"])["manifest"]]
            aspects.append(check(f"model/{step_id}/manifest-{index}", a_ids == b_ids,
                f"selection candidates identical: {a_ids}", f"manifest differs: TS={a_ids} Rust={b_ids}"))
    return aspects


def compare_dream_snapshots(ts: dict, rust: dict, steps: list[dict]) -> list[Aspect]:
    aspects: list[Aspect] = []
    for step in steps:
        if step["op"] not in ("dream", "rollback"):
            continue
        alias, step_id = step["ws"], step["id"]
        def snapshot_shape(result):
            snapshot = result.get("dreamSnapshot")
            if snapshot is None:
                return None
            return {
                "action": snapshot["sourceAction"],
                "states": {phase: sorted(snapshot[phase].get("runtimeState", {})) for phase in ("before", "after")},
            }
        left = ts["workspaces"][alias]["results"][step_id]
        right = rust["workspaces"][alias]["results"][step_id]
        a, b = snapshot_shape(left), snapshot_shape(right)
        aspects.append(check(f"snapshot/{step_id}", a == b,
            f"snapshot action and saved state fields identical: {a}", f"snapshot differs: TS={a} Rust={b}"))
        if "expect_snapshot" in step:
            expected = step["expect_snapshot"]
            aspects.append(check(f"snapshot/{step_id}/expected", bool(a) == expected and bool(b) == expected,
                f"snapshot present={expected}", f"expected snapshot present={expected}"))
        if "expect_snapshot_from" in step:
            previous = step["expect_snapshot_from"]
            for side, artifact in [("TS", ts), ("Rust", rust)]:
                results = artifact["workspaces"][alias]["results"]
                aspects.append(check(f"snapshot/{step_id}/preserved-{side}",
                    results[step_id].get("dreamSnapshot") == results[previous].get("dreamSnapshot"),
                    f"{side} preserved the existing snapshot", f"{side} replaced the existing snapshot during a no-op"))
        if "expect_state_from" in step:
            previous = step["expect_state_from"]
            for side, artifact in [("TS", ts), ("Rust", rust)]:
                results = artifact["workspaces"][alias]["results"]
                restored = results[step_id].get("runtimeState") or {}
                original = results[previous].get("runtimeState") or {}
                aspects.append(check(f"snapshot/{step_id}/state-{side}", restored == original,
                    f"{side} restored exact runtime state from {previous}",
                    f"{side} runtime state differs: expected={original} actual={restored}"))
    return aspects


def compare_dreams(ts: dict, rust: dict, steps: list[dict]) -> list[Aspect]:
    aspects: list[Aspect] = []
    for step in steps:
        if step["op"] != "dream":
            continue
        alias = step["ws"]
        ts_result = ts_step_result(ts["workspaces"][alias], step["id"]) or {}
        rust_result = rust_step_result(rust["workspaces"][alias], step["id"]) or {}
        ts_dream = {
            "reviewedFiles": ts_result.get("reviewedFiles"),
            "rewrittenProjects": ts_result.get("rewrittenProjects"),
            "deletedProjects": ts_result.get("deletedProjects"),
            "deletedFiles": ts_result.get("deletedFiles"),
            "profileUpdated": ts_result.get("profileUpdated"),
            "duplicateTopicCount": ts_result.get("duplicateTopicCount"),
            "conflictTopicCount": ts_result.get("conflictTopicCount"),
            "trigger": ts_result.get("trigger"),
        }
        rust_dream = {
            "reviewedFiles": rust_result.get("reviewed_files"),
            "rewrittenProjects": rust_result.get("rewritten_projects"),
            "deletedProjects": rust_result.get("deleted_projects"),
            "deletedFiles": rust_result.get("deleted_files"),
            "profileUpdated": rust_result.get("profile_updated"),
            "duplicateTopicCount": rust_result.get("duplicate_topic_count"),
            "conflictTopicCount": rust_result.get("conflict_topic_count"),
            "trigger": rust_result.get("trigger"),
        }
        if ts_dream == rust_dream:
            aspects.append(
                Aspect(f"dream/{step['id']}", "PASS", f"dream counts identical: {json.dumps(ts_dream, ensure_ascii=False)}")
            )
        else:
            aspects.append(
                Aspect(
                    f"dream/{step['id']}",
                    "DIFF",
                    f"dream counts differ: TS={json.dumps(ts_dream, ensure_ascii=False)} "
                    f"Rust={json.dumps(rust_dream, ensure_ascii=False)}",
                )
            )
        ts_status = ts_result.get("status")
        rust_status = rust_result.get("status")
        if ts_status == rust_status:
            aspects.append(Aspect(f"dream/{step['id']}/status", "PASS", f"status={ts_status}"))
        else:
            aspects.append(
                Aspect(
                    f"dream/{step['id']}/status",
                    "DIFF",
                    f"dream status differs: TS={ts_status} Rust={rust_status}",
                )
            )
    return aspects


def compare_rollbacks(ts: dict, rust: dict, steps: list[dict]) -> list[Aspect]:
    aspects: list[Aspect] = []
    for step in steps:
        if step["op"] != "rollback":
            continue
        alias = step["ws"]
        ts_result = ts_step_result(ts["workspaces"][alias], step["id"]) or {}
        rust_raw = rust["workspaces"][alias]["results"].get(step["id"]) or {}
        ts_ok = bool(ts_result.get("ok"))
        rust_ok = bool(rust_raw.get("ok"))
        if ts_ok and rust_ok:
            aspects.append(
                Aspect(
                    f"rollback/{step['id']}",
                    "PASS",
                    "both sides installed a dream snapshot and rolled back successfully",
                )
            )
            continue
        if not ts_ok and not rust_ok:
            ts_error = (ts_result.get("error") or "").replace("Error: ", "")
            rust_error = (rust_raw.get("stderr") or "").strip()
            if "No last Dream snapshot" in ts_error and "No last Dream snapshot" in rust_error:
                aspects.append(
                    Aspect(
                        f"rollback/{step['id']}",
                        "PASS",
                        "neither side had a rollback snapshot (identical 'No last Dream snapshot' refusal)",
                    )
                )
            else:
                aspects.append(
                    Aspect(
                        f"rollback/{step['id']}",
                        "PASS" if "no longer matches" in ts_error and "no longer matches" in rust_error else "DIFF",
                        f"rollback refusal reasons (TS: {ts_error[:120]}; Rust: {rust_error[:120]})",
                    )
                )
            continue
        aspects.append(
            Aspect(
                f"rollback/{step['id']}",
                "DIFF",
                f"rollback success differs: TS ok={ts_ok} error={ts_result.get('error')} vs "
                f"Rust ok={rust_ok} stderr={(rust_raw.get('stderr') or '')[:200]}",
            )
        )
    return aspects


def compare_control_plane(ts: dict, rust: dict) -> list[Aspect]:
    ts_control = ts.get("controlPlane", {})
    rust_control = rust.get("controlPlane", {})
    return [
        info(
            "control-plane",
            "Queue/state storage format is informational, not a behavioral exemption "
            f"(TS={ts_control.get('format')}, Rust={rust_control.get('format')})",
        )
    ]


def compare_l0(ts: dict, rust: dict) -> list[Aspect]:
    ts_norm = make_normalizer(ts)
    rust_norm = make_normalizer(rust)
    aspects: list[Aspect] = []
    for alias in sorted(ts.get("workspaces", {})):
        ts_rows = ts["workspaces"][alias].get("l0", [])
        rust_rows = rust["workspaces"][alias].get("l0", [])
        ts_canon = normalize_recursive(
            [
                {
                    "id": row.get("l0IndexId"),
                    "sessionKey": row.get("sessionKey"),
                    "timestamp": row.get("timestamp"),
                    "messages": row.get("messages"),
                    "source": row.get("source"),
                    "indexed": row.get("indexed"),
                }
                for row in ts_rows
            ],
            ts_norm,
        )
        rust_canon = normalize_recursive(
            [
                {
                    "id": row.get("l0IndexId"),
                    "sessionKey": row.get("sessionKey"),
                    "timestamp": row.get("timestamp"),
                    "messages": row.get("messages"),
                    "source": row.get("source"),
                    "indexed": row.get("indexed"),
                }
                for row in rust_rows
            ],
            rust_norm,
        )
        if ts_canon == rust_canon:
            aspects.append(
                Aspect(
                    "l0",
                    "PASS",
                    f"ws {alias}: {len(ts_canon)} L0 row(s) identical "
                    f"(id={[row['id'] for row in ts_canon]}, indexed={[row['indexed'] for row in ts_canon]})",
                )
            )
        else:
            aspects.append(
                Aspect(
                    "l0",
                    "DIFF",
                    f"ws {alias}: TS={json.dumps(ts_canon, ensure_ascii=False)[:700]} "
                    f"Rust={json.dumps(rust_canon, ensure_ascii=False)[:700]}",
                )
            )
    return aspects


def compare_idempotency(ts: dict, rust: dict) -> list[Aspect]:
    aspects: list[Aspect] = []
    for alias in sorted(ts.get("workspaces", {})):
        ts_rows = ts["workspaces"][alias].get("l0", [])
        rust_rows = rust["workspaces"][alias].get("l0", [])
        ts_ok = len(ts_rows) == 1 and bool(ts_rows[0].get("indexed"))
        rust_ok = len(rust_rows) == 1 and bool(rust_rows[0].get("indexed"))
        aspects.append(
            check(
                "idempotency",
                ts_ok and rust_ok,
                f"ws {alias}: exactly one L0 row after two identical captures, indexed on both sides",
                f"ws {alias}: L0 rows TS={len(ts_rows)} indexed={[row.get('indexed') for row in ts_rows]} "
                f"Rust={len(rust_rows)} indexed={[row.get('indexed') for row in rust_rows]}",
            )
        )
    return aspects


def compare_dream_transition(ts: dict, rust: dict, steps: list[dict]) -> list[Aspect]:
    """Compare the file-set transitions around dream/rollback (P6)."""
    aspects: list[Aspect] = []
    for alias in sorted(ts.get("workspaces", {})):
        steps_by_id = {step["id"]: step for step in steps if step["ws"] == alias and step["op"] == "list"}
        pre_id = next((step_id for step_id in steps_by_id if step_id.startswith("list1")), None)
        post_id = next((step_id for step_id in steps_by_id if step_id.startswith("list2")), None)
        restored_id = next((step_id for step_id in steps_by_id if step_id.startswith("list3")), None)
        if not (pre_id and post_id and restored_id):
            continue
        ts_ws = ts["workspaces"][alias]
        rust_ws = rust["workspaces"][alias]

        def hash_free(relative_path: str) -> str:
            return HASH_SUFFIX.sub("-<HASH>.md", relative_path or "")

        ts_lists = {
            step_id: sorted(
                hash_free(entry.get("relativePath"))
                for entry in (ts_step_result(ts_ws, step_id) or {}).get("entries", [])
            )
            for step_id in (pre_id, post_id, restored_id)
        }
        rust_lists = {
            step_id: sorted(
                hash_free(entry.get("relative_path") or "")
                for entry in (rust_step_result(rust_ws, step_id) or {}).get("entries", [])
            )
            for step_id in (pre_id, post_id, restored_id)
        }
        if ts_lists == rust_lists and len(ts_lists[post_id]) < len(ts_lists[pre_id]):
            aspects.append(
                Aspect(
                    "dream-transition",
                    "PASS",
                    f"ws {alias}: {len(ts_lists[pre_id])} files -> dream -> {len(ts_lists[post_id])} "
                    f"file(s) ({ts_lists[post_id]}) -> rollback -> {len(ts_lists[restored_id])} files restored",
                )
            )
        else:
            aspects.append(
                Aspect(
                    "dream-transition",
                    "DIFF",
                    f"ws {alias}: TS transitions {ts_lists} vs Rust transitions {rust_lists}"
                    + ("" if len(ts_lists.get(post_id, [])) < len(ts_lists.get(pre_id, [])) else " (dream did not merge on TS side)"),
                )
            )
        # Post-rollback bytes are covered by the generic `files` aspect and by
        # the restored file set above.
    return aspects


def compare_step_files(ts: dict, rust: dict, steps: list[dict]) -> list[Aspect]:
    aspects: list[Aspect] = []
    for step in steps:
        if step["op"] != "files":
            continue
        alias, step_id = step["ws"], step["id"]
        files = []
        for artifact in (ts, rust):
            result = artifact["workspaces"][alias]["results"][step_id]
            norm = make_normalizer(artifact)
            files.append({record["path"]: norm(record["content"])
                for record in result.get("files", [])})
        aspects.append(check(f"files/{step_id}", files[0] == files[1],
            f"file paths and bytes identical: {sorted(files[0])}",
            describe_file_diff("files", json.dumps(files[0], ensure_ascii=False, sort_keys=True), json.dumps(files[1], ensure_ascii=False, sort_keys=True))))
        if "expect_files_from" in step:
            for side, artifact in [("TS", ts), ("Rust", rust)]:
                results = artifact["workspaces"][alias]["results"]
                def memory_files(result):
                    return {record["path"]: record["content"] for record in result["files"] if record["path"] != "MEMORY.md"}
                current, previous = memory_files(results[step_id]), memory_files(results[step["expect_files_from"]])
                aspects.append(check(f"files/{step_id}/restored-{side}", current == previous,
                    f"{side} restored exact memory bytes", f"{side} did not restore the saved bytes"))
    return aspects


def compare_isolation(ts: dict, rust: dict, steps: list[dict]) -> list[Aspect]:
    """Project isolation: two workspaces under one data root must not leak.

    Layout files (`MEMORY.md`, `project.meta.md`) intentionally share their
    relative name in every workspace, so leakage is detected via file
    *contents*, listed entry paths and recall context content, not via path
    names alone.
    """
    ts_ws = ts.get("workspaces", {})
    rust_ws = rust.get("workspaces", {})
    if len(ts_ws) < 2:
        return []
    aliases = sorted(ts_ws)
    problems: list[str] = []
    facts: list[str] = []

    for alias in aliases:
        if ts_ws[alias].get("hash") != rust_ws.get(alias, {}).get("hash"):
            problems.append(
                f"workspace hash mismatch for {alias}: ts={ts_ws[alias].get('hash')} "
                f"rust={rust_ws.get(alias, {}).get('hash')}"
            )
    hashes = [ts_ws[alias].get("hash") for alias in aliases]
    if len(set(hashes)) != len(hashes):
        problems.append(f"workspace hashes are not distinct: {dict(zip(aliases, hashes))}")
    else:
        facts.append(f"distinct workspace hashes {dict(zip(aliases, hashes))}")

    ts_norm = make_normalizer(ts)
    a, b = aliases[0], aliases[1]
    a_files = {record["path"]: ts_norm(record["content"]) for record in ts_ws[a].get("files", [])}
    b_files = {record["path"]: ts_norm(record["content"]) for record in ts_ws[b].get("files", [])}
    shared_content = sorted(set(a_files.values()) & set(b_files.values()))
    if shared_content:
        problems.append(f"{len(shared_content)} memory file content(s) are shared between {a} and {b}")
    else:
        facts.append(
            f"ws {a} files={sorted(a_files)}; ws {b} files={sorted(b_files)}; no shared memory content"
        )

    # Unique (non-layout) files of A must never appear in B's entries or recall.
    layout_names = {"MEMORY.md", "project.meta.md"}
    a_unique = {path for path in a_files if path not in layout_names}
    b_unique = {path for path in b_files if path not in layout_names}
    b_context = ""
    for side_name, side in (("ts", ts), ("rust", rust)):
        ws = side["workspaces"]
        if a not in ws or b not in ws:
            continue
        list_b = ts_step_result(ws[b], "listB") if side_name == "ts" else rust_step_result(ws[b], "listB")
        entries = (list_b or {}).get("entries", [])
        listed_paths = {
            (entry.get("relativePath") if side_name == "ts" else entry.get("relative_path"))
            for entry in entries
        }
        leaked = sorted((a_unique | b_unique) & listed_paths - b_unique)
        if leaked:
            problems.append(f"workspace {a} files leaked into {side_name} list for {b}: {leaked}")
        retrieve_b = ts_step_result(ws[b], "retrieveB") if side_name == "ts" else rust_step_result(ws[b], "retrieveB")
        if side_name == "ts":
            context = (retrieve_b or {}).get("context") or ""
        else:
            hook = (retrieve_b or {}).get("hookSpecificOutput") or {}
            context = hook.get("additionalContext") or ""
        context = ts_norm(context) if side_name == "ts" else make_normalizer(rust)(context)
        b_context = context
        leaked_context = sorted(path for path in a_unique if path in context)
        if leaked_context:
            problems.append(f"workspace {a} files leaked into {side_name} recall context for {b}: {leaked_context}")
        # A's memory names/descriptions must not appear in B's injected context.
        a_names = set()
        for record in ts_ws[a].get("files", []):
            parsed = parse_frontmatter(record["content"])
            if parsed.get("name"):
                a_names.add(parsed["name"])
            if parsed.get("description"):
                a_names.add(parsed["description"])
        leaked_text = sorted(name for name in a_names if name and name in context)
        if leaked_text:
            problems.append(f"{a} memory text leaked into {side_name} recall context for {b}: {leaked_text}")
    if not problems:
        facts.append(f"recall for ws {b} injects only {b}-scoped files")
    if problems:
        return [Aspect("isolation", "DIFF", " | ".join(problems))]
    return [Aspect("isolation", "PASS", "; ".join(facts))]


# ---------------------------------------------------------------------------
# Driver
# ---------------------------------------------------------------------------


def scenario_aspects(ts: dict, rust: dict, spec: dict) -> list[Aspect]:
    steps = spec["steps"]
    aspects: list[Aspect] = []
    captures = compare_captures(ts, rust, steps)
    aspects.extend(compare_errors(ts, rust))
    aspects.extend(compare_files(ts, rust))
    aspects.extend(compare_frontmatter(ts, rust))
    aspects.extend(compare_manifest(ts, rust))
    aspects.extend(compare_global_files(ts, rust))
    aspects.extend(compare_l0(ts, rust))
    aspects.extend(captures)
    aspects.extend(compare_flushes(ts, rust, steps))
    aspects.extend(compare_runtime(ts, rust, steps))
    aspects.extend(compare_lists(ts, rust, steps))
    aspects.extend(compare_overviews(ts, rust, steps))
    aspects.extend(compare_retrievals(ts, rust, steps))
    aspects.extend(compare_model_calls(ts, rust, steps))
    aspects.extend(compare_dream_snapshots(ts, rust, steps))
    aspects.extend(compare_dreams(ts, rust, steps))
    aspects.extend(compare_rollbacks(ts, rust, steps))
    aspects.extend(compare_dream_transition(ts, rust, steps))
    aspects.extend(compare_step_files(ts, rust, steps))
    aspects.extend(compare_isolation(ts, rust, steps))
    captures = [step for step in steps if step["op"] == "capture"]
    seen_payloads = set()
    has_duplicate_capture = False
    for step in captures:
        fingerprint = json.dumps(
            {
                "ws": step["ws"],
                "session": step["session"],
                "timestamp": step["timestamp"],
                "messages": step["messages"],
            },
            ensure_ascii=False,
            sort_keys=True,
        )
        if fingerprint in seen_payloads:
            has_duplicate_capture = True
            break
        seen_payloads.add(fingerprint)
    if has_duplicate_capture:
        aspects.extend(compare_idempotency(ts, rust))
    aspects.extend(compare_control_plane(ts, rust))
    return aspects


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scenario", required=True)
    parser.add_argument("--scenarios", default=str(HERE / "scenarios.json"))
    parser.add_argument("--run-dir", required=True)
    parser.add_argument("--report", default=None, help="write the JSON report here")
    parser.add_argument("--verbose", action="store_true")
    args = parser.parse_args(argv)

    run_dir = Path(args.run_dir)
    spec = json.loads(Path(args.scenarios).read_text(encoding="utf-8"))["scenarios"][args.scenario]
    ts_artifact = load_json(run_dir / "ts" / "artifact.json")
    rust_artifact = load_json(run_dir / "rust" / "artifact.json")

    if rust_artifact is None:
        print(f"[ERROR] {args.scenario}: missing Rust artifact {run_dir / 'rust' / 'artifact.json'}")
        return 2
    if ts_artifact is None:
        print(f"=== {args.scenario}: {spec.get('title', '')} ===")
        print("[SKIP] TS artifact missing (--no-ts Rust-only run); cross-implementation comparison not performed")
        report = {
            "scenario": args.scenario,
            "title": spec.get("title", ""),
            "verdict": "SKIP",
            "counts": {"pass": 0, "dev": 0, "diff": 0, "info": 0},
            "aspects": [],
            "note": "Rust-only run: no TS artifact to diff against.",
        }
        if args.report:
            Path(args.report).write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        return 0

    aspects = scenario_aspects(ts_artifact, rust_artifact, spec)
    counts = {"pass": 0, "dev": 0, "diff": 0, "info": 0}
    for aspect in aspects:
        counts[aspect.verdict.lower()] += 1
    verdict = "DIFF" if counts["diff"] else "PASS"

    print(f"=== {args.scenario}: {spec.get('title', '')} ===")
    for aspect in aspects:
        detail = aspect.detail if args.verbose else aspect.detail.split("\n")[0]
        print(f"  {aspect.verdict}  {aspect.name:<28} {detail}")
    print(
        f"  VERDICT: {verdict} ({counts['pass']} pass, {counts['dev']} dev, "
        f"{counts['diff']} diff, {counts['info']} info)\n"
    )

    if args.report:
        report = {
            "scenario": args.scenario,
            "title": spec.get("title", ""),
            "verdict": verdict,
            "counts": counts,
            "aspects": [aspect.to_dict() for aspect in aspects],
            "environment": {
                "tsNodeVersion": ts_artifact.get("nodeVersion"),
                "tsModule": ts_artifact.get("tsModule"),
                "rustBinary": rust_artifact.get("binary"),
                "rustPythonVersion": rust_artifact.get("pythonVersion"),
            },
        }
        Path(args.report).write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return 0 if verdict == "PASS" else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
