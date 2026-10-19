#!/usr/bin/env python3
"""Deterministic, scenario-aware OpenAI-compatible mock model for parity tests.

Both the TypeScript memory core (``apiType: "openai-completions"``) and the
Rust SDK (``api_type: "openai-chat"``) talk to this server, so the *only*
difference between the two sides is the implementation under test.

The mock inspects the *system prompt* (the two implementations re-author the
same prompts, and the schema markers below are present in both) and returns a
schema-correct JSON object. Behaviour is switched with ``--scenario``:

  default : classify project+feedback rules; gate -> mix; select manifest ids;
            dream plans no clusters.
  none    : same extraction, but the recall gate answers ``{"route":"none"}``.
  user    : classification yields the ``user`` label; user-profile rewrite.
  dream   : dream cluster planning + refine actually merge same-kind files.

Requests are appended to ``<log-dir>/requests.jsonl`` (full system/user prompt
and response) so every comparison can be traced back to exact model input.

Only the Python standard library is used; no network access beyond localhost.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

# ---------------------------------------------------------------------------
# Prompt -> response schema detection
#
# Markers are chosen so that they appear in BOTH the TypeScript prompts
# (src/context/memory/edgeclaw-memory-core/lib/core/skills/llm-extraction.js)
# and the Rust re-authored prompts (crates/pd-memory-core/src/{extraction,dream,recall}.rs).
# ---------------------------------------------------------------------------

MARK_USER_NOTE = "append-only user memory note"
MARK_PROJECT_NOTE = "append-only project memory note"
MARK_FEEDBACK_NOTE = "append-only feedback memory note"
MARK_CLASSIFY = "should_store"
MARK_GATE = "single field route"
MARK_SELECT = "selected_ids"
MARK_PROFILE = "identity_background_markdown"
MARK_CLUSTER = "member_relative_paths"
MARK_REFINE = "refined file title"
MARK_META_REVIEW = "should_update"
MARK_GLOBAL_PLAN = "duplicate_topic_count"

# Fixed mirror content. Kept small on purpose: parity of *storage* is what we
# test, not model quality; every field must be deterministic.
USER_NOTE = {
    "skip": False,
    "reason": "",
    "name": "用户身份",
    "description": "用户是一名算法工程师，长期从事推荐系统",
    "markdown": "## 身份背景\n\n- 用户是一名算法工程师，长期从事推荐系统\n- 常驻杭州",
}

IDENTITY_BACKGROUND = "- 算法工程师，长期从事推荐系统\n- 常驻杭州"

PROJECT_NOTES = {
    "boreal": {
        "skip": False,
        "reason": "",
        "name": "Boreal 项目状态",
        "description": "Boreal 是本地知识库工具，处于设计阶段",
        "markdown": "## Summary\nBoreal 是本地知识库工具。\n\n## Current Stage\n设计阶段。",
    },
    "astrolabe": {
        "skip": False,
        "reason": "",
        "name": "Astrolabe 项目状态",
        "description": "Astrolabe 是天文观测记录工具，处于原型阶段",
        "markdown": "## Summary\nAstrolabe 是天文观测记录工具。\n\n## Current Stage\n原型阶段。",
    },
}

FEEDBACK_NOTES = {
    "language": {
        "skip": False,
        "reason": "",
        "name": "文档语言规则",
        "description": "所有文档使用中文",
        "markdown": "## Rule\n所有文档使用中文。\n\n## How To Apply\n交付文档一律中文。",
    },
    "pnpm": {
        "skip": False,
        "reason": "",
        "name": "包管理器规则",
        "description": "依赖安装只用 pnpm",
        "markdown": "## Rule\n依赖安装只用 pnpm。\n\n## How To Apply\n所有安装命令使用 pnpm。",
    },
}

MERGED_FEEDBACK = {
    "summary": "merged feedback collaboration rules",
    "name": "反馈规则合集",
    "description": "文档语言与包管理器规则合集",
    "markdown": (
        "## Rule\n所有文档使用中文。\n\n"
        "## Package Manager\n依赖安装只用 pnpm。\n\n"
        "## How To Apply\n交付文档一律中文，所有安装命令使用 pnpm。"
    ),
}

MERGED_PROJECT = {
    "summary": "merged project definition facts",
    "name": "Boreal 项目状态",
    "description": "Boreal 是本地知识库工具，处于设计阶段",
    "markdown": "## Summary\nBoreal 是本地知识库工具。\n\n## Current Stage\n设计阶段。",
}

CLUSTER_REASONS = {
    "feedback": "duplicate collaboration rules overlap, should be consolidated",
    "project": "duplicate project definition facts overlap, should be merged",
}


# ---------------------------------------------------------------------------
# Payload helpers
# ---------------------------------------------------------------------------


def _loads_json(text: str) -> Any:
    try:
        return json.loads(text)
    except (TypeError, ValueError):
        return None


def _focus_turn_content(user_payload: str) -> str:
    """Extract the focus user turn text from classify/create-note payloads."""
    payload = _loads_json(user_payload)
    if not isinstance(payload, dict):
        return user_payload
    turn = payload.get("focus_user_turn")
    if isinstance(turn, dict) and isinstance(turn.get("content"), str):
        return turn["content"]
    context = payload.get("context")
    if isinstance(context, dict):
        turn = context.get("focus_user_turn")
        if isinstance(turn, dict) and isinstance(turn.get("content"), str):
            return turn["content"]
    return ""


def _classification_labels(focus: str) -> list[dict[str, str]]:
    if any(keyword in focus for keyword in ("我是", "住在", "算法工程师", "身份", "个人")):
        return [
            {
                "type": "user",
                "reason": "durable cross-project identity/background fact",
                "evidence": focus[:60],
            }
        ]
    if "pnpm" in focus or "npm" in focus:
        return [
            {
                "type": "feedback",
                "reason": "durable collaboration/tooling rule",
                "evidence": focus[:60],
            }
        ]
    labels = [
        {
            "type": "project",
            "reason": "durable project fact",
            "evidence": focus[:60],
        }
    ]
    if any(
        keyword in focus
        for keyword in ("以后", "规则", "always", "用中文", "pnpm", "不要用")
    ):
        labels.append(
            {
                "type": "feedback",
                "reason": "durable rule",
                "evidence": focus[:60],
            }
        )
    return labels


def _project_kind(focus: str) -> str:
    return "astrolabe" if "Astrolabe" in focus else "boreal"


def _feedback_kind(focus: str) -> str:
    return "pnpm" if ("pnpm" in focus or "npm" in focus) else "language"


def _manifest_selected_ids(user_payload: str) -> list[str]:
    payload = _loads_json(user_payload)
    if not isinstance(payload, dict):
        return []
    manifest = payload.get("manifest")
    if not isinstance(manifest, list):
        return []
    ids: list[str] = []
    for item in manifest:
        if isinstance(item, dict) and isinstance(item.get("id"), str):
            if item["id"] not in ids:
                ids.append(item["id"])
    return ids[:5]


def _cluster_members(user_payload: str) -> tuple[str, list[str]]:
    payload = _loads_json(user_payload)
    if not isinstance(payload, dict):
        return "feedback", []
    category = payload.get("category")
    kind = category if category in ("feedback", "project") else "feedback"
    headers = payload.get("headers")
    members: list[str] = []
    if isinstance(headers, list):
        for header in headers:
            if isinstance(header, dict):
                path = header.get("relative_path")
                if isinstance(path, str) and path.endswith(".md"):
                    members.append(path)
    return kind, members


def _refine_category(user_payload: str) -> str:
    payload = _loads_json(user_payload)
    if isinstance(payload, dict):
        category = payload.get("category")
        if category in ("feedback", "project"):
            return category
    return "feedback"


def _current_project_meta(user_payload: str) -> dict[str, Any]:
    payload = _loads_json(user_payload)
    if isinstance(payload, dict):
        meta = payload.get("current_project_meta")
        if isinstance(meta, dict):
            return meta
    return {}


# ---------------------------------------------------------------------------
# Request -> (purpose, response)
# ---------------------------------------------------------------------------


def pick(system: str, user: str) -> tuple[str, Any]:
    s = system or ""
    if MARK_PROFILE in s:
        return "user_profile_rewrite", {"identity_background_markdown": IDENTITY_BACKGROUND}
    if MARK_CLUSTER in s:
        kind, members = _cluster_members(user)
        if _scenario() not in ("dream", "dream-reason") or len(members) < 2:
            return "dream_cluster_plan", {
                "summary": "not enough same-kind files to form clusters",
                "clusters": [],
            }
        return "dream_cluster_plan", {
            "summary": f"merge duplicate {kind} files",
            "clusters": [
                {
                    "member_relative_paths": members,
                    "reason": "同属一个工作组" if _scenario() == "dream-reason" else CLUSTER_REASONS[kind],
                }
            ],
        }
    if MARK_REFINE in s:
        category = _refine_category(user)
        return "dream_cluster_refine", (
            dict(MERGED_FEEDBACK) if category == "feedback" else dict(MERGED_PROJECT)
        )
    if MARK_META_REVIEW in s:
        meta = _current_project_meta(user)
        return "dream_project_meta_review", {
            "should_update": False,
            "reason": "current metadata still matches the refined memory files",
            "project_name": meta.get("project_name", ""),
            "description": meta.get("description", ""),
            "status": meta.get("status", "in_progress"),
        }
    if MARK_GLOBAL_PLAN in s:
        return "dream_global_plan", {
            "summary": "no reorganization needed",
            "duplicate_topic_count": 0,
            "conflict_topic_count": 0,
            "projects": [],
            "deleted_project_ids": [],
            "deleted_entry_ids": [],
        }
    if MARK_USER_NOTE in s:
        return "user_note_create", dict(USER_NOTE)
    if MARK_PROJECT_NOTE in s:
        focus = _focus_turn_content(user)
        return "project_note_create", dict(PROJECT_NOTES[_project_kind(focus)])
    if MARK_FEEDBACK_NOTE in s:
        focus = _focus_turn_content(user)
        return "feedback_note_create", dict(FEEDBACK_NOTES[_feedback_kind(focus)])
    if MARK_GATE in s:
        scenario = _scenario()
        return "recall_gate", {"route": scenario if scenario in ("none", "project", "user") else "mix"}
    if MARK_SELECT in s:
        return "recall_select", {"selected_ids": _manifest_selected_ids(user)}
    if MARK_CLASSIFY in s:
        focus = _focus_turn_content(user)
        return "classify_turn", {
            "should_store": True,
            "labels": _classification_labels(focus),
        }
    return "unknown", {}


def _scenario() -> str:
    return os.environ.get("PARITY_SCENARIO", os.environ.get("SCENARIO", "default"))


# ---------------------------------------------------------------------------
# HTTP server
# ---------------------------------------------------------------------------


class Handler(BaseHTTPRequestHandler):
    server_version = "pd-parity-mock/1.0"
    counter = 0
    log_path = ""

    def do_POST(self) -> None:  # noqa: N802 - http.server API
        length = int(self.headers.get("content-length", "0") or 0)
        raw = self.rfile.read(length) if length else b"{}"
        try:
            body = json.loads(raw or b"{}")
        except ValueError:
            body = {}
        messages = body.get("messages") or []
        system = "\n".join(
            str(message.get("content", ""))
            for message in messages
            if isinstance(message, dict) and message.get("role") == "system"
        )
        user = "\n".join(
            str(message.get("content", ""))
            for message in messages
            if isinstance(message, dict) and message.get("role") != "system"
        )
        purpose, payload = pick(system, user)
        content = json.dumps(payload, ensure_ascii=False)
        focus = _focus_turn_content(user)
        if (purpose == "classify_turn" and "FAULT_CLASSIFY" in focus) or (purpose.endswith("_create") and "FAULT_CREATE" in focus):
            content = "invalid JSON"
        if purpose.endswith("_create") and "LOOSE_CREATE" in focus:
            content = content.replace("Boreal 是本地知识库工具", 'Boreal 是"本地"知识库工具')
        if (purpose == "recall_gate" and "FAULT_GATE" in user) or (purpose == "recall_select" and "FAULT_SELECT" in user):
            content = "invalid JSON"
        status = 400 if purpose == "recall_gate" and "NO_JSON_MODE" in user and "response_format" in body else 200
        Handler.counter += 1
        if Handler.log_path:
            with open(Handler.log_path, "a", encoding="utf-8") as handle:
                handle.write(
                    json.dumps(
                        {
                            "n": Handler.counter,
                            "purpose": purpose,
                            "status": status,
                            "jsonMode": "response_format" in body,
                            "system": system,
                            "user": user,
                            "response": content,
                        },
                        ensure_ascii=False,
                    )
                    + "\n"
                )
        data = json.dumps(
            {
                "id": "mock-1",
                "object": "chat.completion",
                "created": 0,
                "model": body.get("model", "mock-model"),
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": content},
                        "finish_reason": "stop",
                    }
                ],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 10,
                    "total_tokens": 20,
                },
            },
            ensure_ascii=False,
        ).encode("utf-8")
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self) -> None:  # noqa: N802 - http.server API
        data = b'{"status":"ok","mock":"pd-parity"}'
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *args: Any) -> None:  # silence request logging
        pass


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=0, help="0 selects a free port")
    parser.add_argument(
        "--scenario",
        default=os.environ.get("PARITY_SCENARIO")
        or os.environ.get("SCENARIO")
        or "default",
        help="default | none | project | user | dream (env: PARITY_SCENARIO or SCENARIO)",
    )
    parser.add_argument("--log-dir", default="", help="directory for requests.jsonl")
    parser.add_argument(
        "--port-file",
        default="",
        help="write the bound port here once the server is listening",
    )
    args = parser.parse_args()

    os.environ["PARITY_SCENARIO"] = args.scenario
    Handler.log_path = ""
    if args.log_dir:
        os.makedirs(args.log_dir, exist_ok=True)
        Handler.log_path = os.path.join(args.log_dir, "requests.jsonl")

    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    port = server.server_address[1]
    if args.port_file:
        with open(args.port_file, "w", encoding="utf-8") as handle:
            handle.write(str(port))
    sys.stderr.write(f"mock model listening on 127.0.0.1:{port} scenario={args.scenario}\n")
    sys.stderr.flush()
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
