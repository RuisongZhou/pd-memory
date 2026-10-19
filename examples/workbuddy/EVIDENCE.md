# pd-memory × WorkBuddy — 端到端证据（EVIDENCE）

> 本文保留先前运行的历史证据摘要；发布时将开发机绝对路径替换为 `/path/to/pd-memory`。2026-10-19 的流程对齐后，最终注入文本已恢复上游四段结构，
> no-op Dream 返回 success 且不替换快照；下文旧输出中的 `<memory-context>` / skipped 仅代表历史行为。
> 本次重新运行了离线 contract simulator：30 PASS、0 WARN、0 FAIL；未重跑真实 WorkBuddy CLI。

> 本文件是 `examples/workbuddy/run_e2e.sh` **真实运行**的捕获摘要。
> 所有数据均为合成数据；不包含真实 API key、用户配置或真实会话内容。
> 原始完整日志：`examples/workbuddy/EVIDENCE.log`；机器可读明细：`examples/workbuddy/.run/e2e-summary.json`。

## 0. 运行基线与结论

| 项 | 值 |
|---|---|
| 运行时间 | **2026-10-19 00:46:22 → 00:46:25 (+08:00)** |
| 结果 | **PASS — 56 passed / 0 warning / 0 failed / 0 skipped** |
| 路线 | `contract-simulator`（契约模拟器）+ `real-workbuddy-cli`（真实 WorkBuddy CLI 无头） |
| pd-memory | `pd-memory 0.1.0`（`target/debug/pd-memory`） |
| WorkBuddy | **5.8.2**（`/Applications/WorkBuddy.app`） |
| 内置 CodeBuddy Code CLI | **2.161.0**（`.../app.asar.unpacked/cli/bin/codebuddy`） |
| 内置 Node | **v22.22.2**（`~/.workbuddy/binaries/node/versions/22.22.2-6/bin/node`） |
| 宿主 | macOS 27.0.1（本机） |
| 网络/登录 | 无外网、无登录；SDK 用 `PD_MEMORY_PROVIDER=mock`，Agent 模型用本地 SSE mock 端点 |

复现：

```bash
cd /path/to/pd-memory
bash examples/workbuddy/run_e2e.sh          # 构建 + 两条路线 + tee 到 EVIDENCE.log
bash examples/workbuddy/run_e2e.sh --no-real    # 仅契约模拟器（CI/无 WorkBuddy 环境）
bash examples/workbuddy/run_e2e.sh --only-real  # 仅真实 WorkBuddy CLI
```

---

## 1. Route A — 契约模拟器（WorkBuddy 形状的 hook + MCP stdio）

模拟器按 WorkBuddy/CodeBuddy 真实契约驱动本仓库编译出的二进制：
`Stop` hook stdin（snake_case，含 `transcript_path` / `last_assistant_message`）
→ `flush` → `UserPromptSubmit` hook → MCP `initialize/tools/list/tools/call`
→ `dream` + `rollback`。

### 1.1 合成 WorkBuddy transcript（6 条 JSONL，引用真实记录类型）

```text
.run/sim/transcripts/wb-sim-1.jsonl
{"type":"session-meta",...}
{"type":"message","role":"user","content":[{"type":"input_text","text":"请记住：WorkBuddy 集成项目的代号是 ORION-7421，主要使用 Rust 实现。"}]}
{"type":"reasoning","rawContent":[...]}
{"type":"function_call","name":"Read","arguments":"{...}","callId":"call-sim-1"}
{"type":"function_call_result","name":"Read",...}
{"type":"message","role":"assistant","content":[{"type":"output_text","text":"已记住：项目代号 ORION-7421，主语言 Rust。"}]}
```

### 1.2 Phase 1 — Stop hook

```text
[PASS] Stop hook exit code 0 - rc=0
[PASS] Stop hook stdout is one-line JSON - {"continue":true}
[PASS] Stop hook JSON has 'continue' - {"continue": true}
[PASS] Stop hook captured the turn into the data root - 5 file(s):
       workspaces/workbuddy-demo/control/l0_sessions.jsonl,
       workspaces/workbuddy-demo/memory/Project/请记住-...-450d754f2d.md,
       workspaces/workbuddy-demo/memory/Feedback/请记住-...-89976a75f3.md,
       workspaces/workbuddy-demo/memory/MEMORY.md,
       workspaces/workbuddy-demo/memory/project.meta.md
```

### 1.3 Phase 2 — flush 后落盘（目录 `<data_root>/workspaces/workbuddy-demo/memory/`）

```text
    - Feedback/请记住-workbuddy-集成项目的代号是-orion-7421-主要使用-rust-实现-89976a75f3.md (816 bytes)
    - MEMORY.md (1063 bytes)
    - Project/请记住-workbuddy-集成项目的代号是-orion-7421-主要使用-rust-实现-450d754f2d.md (815 bytes)
    - project.meta.md (545 bytes)
```

`MEMORY.md`（真实内容节选）：

```markdown
# EdgeClaw Memory

Updated: 2026-10-18T16:46:22.8xxZ

## Current Project Meta
- [请记住：WorkBuddy 集成项目的代号是 ORION-7421，主要使用 Rust 实现。](project.meta.md) — ...

## Project Memory
- [请记住：WorkBuddy 集成项目的代号是 ORION-7421，主要使用 Rust 实现。](Project/请记住-...-450d754f2d.md) — ...

## Feedback Memory
- [请记住：WorkBuddy 集成项目的代号是 ORION-7421，主要使用 Rust 实现。](Feedback/请记住-...-89976a75f3.md) — ...
```

记忆文件正文（节选，真实 frontmatter + Captured 段）：

```markdown
---
name: 请记住：WorkBuddy 集成项目的代号是 ORION-7421，主要使用 Rust 实现。
description: 请记住：WorkBuddy 集成项目的代号是 ORION-7421，主要使用 Rust 实现。 已记住：项目代号 ORION-7421，主语言 Rust。
type: project
scope: project
project_id: current_project
updated_at: 2026-10-18T16:46:22.813Z
captured_at: 2026-10-18T16:46:22.812Z
source_session_key: wb-sim-1
---
## 请记住：WorkBuddy 集成项目的代号是 ORION-7421，主要使用 Rust 实现。
...
```

### 1.4 Phase 3 — UserPromptSubmit 注入 `additionalContext`

输入 prompt：`我们项目的代号是什么？请只回答代号。`

```json
{"continue":true,"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"<memory-context>\n## ClawXMemory Recall\nroute=mix\n\n### [project_meta] project.meta.md (...)\n## Project Name\n请记住：WorkBuddy 集成项目的代号是 ORION-7421，主要使用 Rust 实现。\n..."}}
```

断言：

```text
[PASS] additionalContext is non-empty
[PASS] additionalContext contains the stored fact ORION-7421
```

### 1.5 Phase 4 — MCP stdio JSON-RPC

```text
[PASS] MCP initialize ok - {"id": 1, "jsonrpc": "2.0", "result": {
         "capabilities": {"tools": {"listChanged": false}},
         "protocolVersion": "2025-11-25",
         "serverInfo": {"name": "pd-memory", "version": "0.1.0"}}}
[PASS] tools/list returns exactly 7 tools
       ['memory_add', 'memory_dream', 'memory_flush', 'memory_get',
        'memory_list', 'memory_overview', 'memory_search']
[PASS] all documented tools are present - missing=[]
[PASS] tools/call memory_overview returns content
       {"changed_files_since_last_dream": 2, "feedback_memories": 1,
        "pending_dialogue_turns": 0, "project_memories": 1, ...}
[PASS] tools/call memory_search returns content
[PASS] memory_search result contains the stored fact ORION-7421
```

`memory_search` 返回的 `context` 片段（真实捕获，含所选文件 id）：

```text
"context": "## ClawXMemory Recall\nroute=mix\n\n### [project_meta] project.meta.md (...)\n## Project Name\n请记住：WorkBuddy 集成项目的代号是 ORION-7421，主要使用 Rust 实现。...\n### [project] Project/请记住-...-450d754f2d.md ...\n### [feedback] Feedback/请记住-...-89976a75f3.md ..."
"selected_ids": ["Project/请记住-...-450d754f2d.md", "Feedback/请记住-...-89976a75f3.md"]
```

### 1.6 Phase 5 — dream + rollback

先通过第二个真实 Stop hook 会话（`wb-sim-2`）捕获第二组记忆，使 mock dream 具备可合并材料；
`dream` 后 `last_dream/` 快照目录生成，`rollback` 成功恢复：

```text
[PASS] dream exit code 0
       {"reviewed_files": 2, "rewritten_projects": 0, "deleted_projects": 0,
        "deleted_files": 0, "profile_updated": false,
        "summary": "Dream completed without requiring any memory changes.",
        "trigger": "manual", "status": "skipped", ...}
[PASS] dream snapshot directory exists
       .run/sim/data/workspaces/workbuddy-demo/last_dream
       snapshot files: metadata.json, workspace/MEMORY.md,
                       workspace/Project/*.md, workspace/Feedback/*.md, workspace/project.meta.md
[PASS] rollback exit code 0
       {"rolled_back_at": "2026-10-18T16:46:22.892Z",
        "snapshot_captured_at": "2026-10-18T16:46:22.862Z",
        "last_dream_at": null, "last_dream_status": null, "last_dream_summary": null}
[PASS] MEMORY.md still present after rollback
```

> 说明：mock 模型下 dream 状态可能为 `skipped`（无需改写），但实现仍会生成可回滚快照，
> 因此 `rollback` 的“快照恢复”路径可被真实断言。真实模型下 dream 会做实际合并改写。

---

## 2. Route B — 真实 WorkBuddy 无头 CLI（无登录、无外网）

Harness 在本机启动一个 Python stdlib 的 OpenAI-compatible SSE mock 端点，
写入隔离的 `CODEBUDDY_CONFIG_DIR` / `WORKBUDDY_CONFIG_DIR`：

```text
.run/real/
├── models.json        # 自定义模型 mock-model → http://127.0.0.1:<port>/v1
├── settings.json      # hooks: SessionStart / UserPromptSubmit / Stop → pd-memory hook
├── mcp.json           # MCP server = pd-memory serve mcp（前面套一层透明 probe 记录 JSON-RPC）
├── projects/…/<session>.jsonl   # 真实 CLI 生成的 transcript
├── hooks/*.stdin.json           # 真实 CLI 传给 hook 的原始 payload
├── mcp-probe.log                # CLI ↔ pd-memory MCP 的真实 JSON-RPC 流量
└── mock-openai-requests.json    # CLI 发给 mock 模型的真实请求体
```

命令（harness 等价执行）：

```bash
NODE=~/.workbuddy/binaries/node/versions/22.22.2-6/bin/node
CLI=/Applications/WorkBuddy.app/Contents/Resources/app.asar.unpacked/cli/bin/codebuddy
CODEBUDDY_FORCE_LITE_WB_BUNDLE=1 \
CODEBUDDY_CONFIG_DIR=$TDIR WORKBUDDY_CONFIG_DIR=$TDIR HOME=$TDIR/home \
"$NODE" "$CLI" -p '请记住：WorkBuddy 集成项目的代号是 ORION-7421，主要使用 Rust 实现。' \
  --model mock-model \
  --mcp-config $TDIR/mcp.json --strict-mcp-config \
  --settings $TDIR/settings.json \
  --output-format json
```

### 2.1 真实 CLI 会话结果（模型请求确实到达本地 mock 端点）

```text
[PASS] real CLI exit code 0 - rc=0
[PASS] real CLI emitted JSON output - list with 4 item(s)
[PASS] real CLI reported a session result - MOCK_REPLY: remembered that you like Rust.
[PASS] real model call reached the local mock OpenAI endpoint - 1 model request(s)
```

CLI result 记录（真实 stdout 数组中的 `type=result` 元素）：

```json
{"type":"result","subtype":"success","is_error":false,
 "result":"MOCK_REPLY: remembered that you like Rust.",
 "session_id":"01a14fe8-…","num_turns":2,
 "usage":{"input_tokens":10,"output_tokens":8},
 "__timestamp":"2026-10-18T16:46:23.xxxZ"}
```

### 2.2 真实 hook payload（WorkBuddy → `pd-memory hook`，原始 stdin）

`Stop`（`.run/real/hooks/Stop.stdin.json`，节选）：

```json
{"hook_event_name":"Stop",
 "session_id":"01a14fe8-4536-7f7c-bb17-b591eec9332d",
 "transcript_path":"…/.run/real/projects/Users-ruisong-code-PilotDeck-sdk-pd-memory/01a14fe8-….jsonl",
 "cwd":"/path/to/pd-memory",
 "stop_hook_active":false,
 "last_assistant_message":"MOCK_REPLY: remembered that you like Rust.",
 "permission_mode":"default","client":"CLI","version":"2.161.0","model":"mock-model"}
```

`UserPromptSubmit`（第二次真实会话）：

```json
{"hook_event_name":"UserPromptSubmit",
 "session_id":"01a14fe8-4536-…",
 "transcript_path":"…/01a14fe8-….jsonl",
 "cwd":"/path/to/pd-memory",
 "prompt":"我们项目的代号是什么？请只回答代号。",
 "is_internal_continuation":false,
 "permission_mode":"default","client":"CLI","version":"2.161.0","model":"mock-model"}
```

`SessionStart`（注意：WorkBuddy 的 SessionStart **没有 `cwd`**）：

```json
{"hook_event_name":"SessionStart","session_id":"01a14fe8-…",
 "transcript_path":"…/01a14fe8-….jsonl","source":"startup",
 "permission_mode":"default","client":"CLI","version":"2.161.0","model":"mock-model"}
```

断言：

```text
[PASS] real hook stdin has hook_event_name=SessionStart
[PASS] pd-memory hook produced valid JSON for SessionStart - {"continue": true, "hookSpecificOutput": {...}}
[PASS] real hook stdin has hook_event_name=UserPromptSubmit
[PASS] pd-memory hook produced valid JSON for UserPromptSubmit
[PASS] real hook stdin has hook_event_name=Stop
[PASS] pd-memory hook produced valid JSON for Stop - {"continue":true,...}
[PASS] real Stop hook carries last_assistant_message - MOCK_REPLY: remembered that you like Rust.
[PASS] real transcript file exists - …/projects/…/01a14fe8-….jsonl
```

### 2.3 捕获落盘（真实会话 → SDK 数据根）

```text
[PASS] real hooks captured the conversation into the SDK data root - 6 file(s)
  - workspaces/workbuddy-demo/control/l0_sessions.jsonl (443 bytes)
  - workspaces/workbuddy-demo/control/pipeline_state.json (879 bytes)
  - workspaces/workbuddy-demo/memory/Project/请记住-…-b1a7bb3723.md (804 bytes)
  - workspaces/workbuddy-demo/memory/Feedback/请记住-…-cb5ba5ecc0.md (805 bytes)
  - workspaces/workbuddy-demo/memory/MEMORY.md (1024 bytes)
  - workspaces/workbuddy-demo/memory/project.meta.md (519 bytes)
```

L0 队列真实内容（两轮会话，assistant 回复来自真实 Stop payload）：

```json
{"l0_index_id":"01a14fe8-40ac-…_909b2dde32_raw","session_key":"01a14fe8-40ac-…",
 "messages":[{"role":"user","content":"请记住：WorkBuddy 集成项目的代号是 ORION-7421，主要使用 Rust 实现。"},
             {"role":"assistant","content":"MOCK_REPLY: remembered that you like Rust."}],
 "source":"hook:Stop","indexed":true}
{"l0_index_id":"01a14fe8-4536-…_0f38151b52_raw","session_key":"01a14fe8-4536-…",
 "messages":[{"role":"user","content":"我们项目的代号是什么？请只回答代号。"},
             {"role":"assistant","content":"MOCK_REPLY: remembered that you like Rust."}],
 "source":"hook:Stop","indexed":true}
```

### 2.4 MCP 握手（真实 CLI ↔ 我们的 `pd-memory serve mcp`）

`CLIENT` = 真实 WorkBuddy CLI；`SERVER` = `pd-memory 0.1.0`（probe 原样转发）。

```text
CLIENT->SERVER {"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"pd-memory","version":"1.0.0"}},"jsonrpc":"2.0","id":0}
SERVER->CLIENT {"id":0,"jsonrpc":"2.0","result":{"capabilities":{"tools":{"listChanged":false}},"protocolVersion":"2025-11-25","serverInfo":{"name":"pd-memory","version":"0.1.0"}}}
CLIENT->SERVER {"method":"notifications/initialized","jsonrpc":"2.0"}
CLIENT->SERVER {"method":"tools/list","jsonrpc":"2.0","id":1}
SERVER->CLIENT {"id":1,"jsonrpc":"2.0","result":{"tools":[ … 7 个工具 … ]}}
```

```text
[PASS] real CLI MCP handshake contains initialize
[PASS] real CLI MCP handshake contains notifications/initialized
[PASS] real CLI MCP handshake contains tools/list
[PASS] our MCP server answered initialize
```

### 2.5 注入证明：记忆上下文进入真实模型请求

第二次真实会话（prompt = `我们项目的代号是什么？请只回答代号。`）中，
`UserPromptSubmit` hook 返回 1139 字符的 `additionalContext`（`<memory-context>…`），
真实 CLI 把它作为 `hook` system-reminder 放进模型请求；mock 端点捕获的请求体中可检索到事实 token：

```text
[PASS] real UserPromptSubmit hook injected memory context containing the fact
[PASS] memory context reached the second real model request (WorkBuddy applies hook additionalContext)
       - 1 request(s) in run2 (of 2 total)
  injected context inside the real model request (excerpt around the fact):
    ..."<system-reminder data-role=\"hook\"><memory-context>
       ## ClawXMemory Recall
       route=mix
       ### [project_meta] project.meta.md (...)
       ## Project Name
       请记住：WorkBuddy 集成项目的代号是 ORION-7421，主要使用 Rust 实现。
       ..."
```

原始请求体保存在 `examples/workbuddy/.run/real/mock-openai-requests.json`
（两条：run1 直接 prompt 命中；run2 由 hook 注入命中；`contains_fact=true`）。

---

## 3. 已验证 / 未验证清单

| 能力 | 状态 | 证据 |
|---|---|---|
| `pd-memory hook` 接受真实 WorkBuddy snake_case stdin，输出单行 JSON | ✅ 真实 CLI 实测 | §2.2 |
| `Stop` 携带 `transcript_path` + `last_assistant_message`，可落 L0 | ✅ 真实 CLI 实测 | §2.2 / §2.3 |
| `UserPromptSubmit.additionalContext` 被真实 CLI 注入模型请求 | ✅ 真实 CLI 实测 | §2.5 |
| `pd-memory serve mcp` 被真实 CLI 拉起并完成 initialize/tools/list | ✅ 真实 CLI 实测 | §2.4 |
| MCP 7 工具、`memory_overview`/`memory_search` 内容正确 | ✅ 模拟器 + 真实 CLI | §1.5 / §2.4 |
| Stop hook → flush → Markdown + MEMORY.md | ✅ 两条路线 | §1.3 / §2.3 |
| dream 快照 + rollback 恢复 | ✅ 模拟器（mock 模型） | §1.6 |
| WorkBuddy `-p` 会话触发 `SessionStart`/`UserPromptSubmit`/`Stop` | ✅ 真实 CLI 实测 | §2.2 |
| `SessionEnd` / `PreCompact` 触发 | ⚠️ 未触发（`-p` 单次运行不产生） | 事件存在于运行时枚举，载荷字段已知；GUI 交互/压缩场景未实测 |
| GUI 内 hooks/MCP 生效 | ⚠️ 未验证（不启动 GUI 的约束） | GIU 以 `--setting-sources=user --strict-mcp-config` 启动；配置位置已实测可用 |
| 真实云端模型质量 | ❌ 不在验证范围 | 使用本地 mock SSE 模型，仅验证协议/链路 |

## 4. 观察与风险

1. **真实 CLI 的 `-p` 退出码不可信**：模型鉴权失败时仍返回 0；harness 全部断言 stdout/stderr/数据根。
2. **transcript 落盘时序**：偶发 `Stop` hook 触发时 assistant 行尚未写入 transcript。`pd-memory-hooks`
   已用 `last_assistant_message` 兜底（无 assistant 行时补一条合成 assistant 消息），本次运行未触发该竞态。
3. **mock dream 语义**：mock 模型下 dream 常为 `skipped`（无需改写）但快照仍生成；真实模型下会有实际合并。
4. **WorkBuddy 自带 memory reminder**：真实 CLI 输出里会看到 WorkBuddy 自身的 auto-memory 提示（`data-role="memory"`），
   与 pd-memory 无关，harness 只断言 pd-memory 的 hook/MCP 输出。
5. **版本敏感**：所有格式来自 WorkBuddy 5.8.2 / CLI 2.161.0；升级后需重新采样（harness 会直接失败）。
6. 本证据中的路径/会话 id/模型回复均为本机合成数据，无敏感信息。

## 5. 证据文件索引

| 文件 | 内容 |
|---|---|
| `examples/workbuddy/EVIDENCE.log` | 本次运行完整日志（56 项断言逐条 PASS） |
| `examples/workbuddy/.run/e2e-summary.json` | 机器可读检查明细（pass/warn/fail + 每条 detail） |
| `examples/workbuddy/.run/sim/**` | Route A 数据根、transcript、memory 文件、last_dream 快照 |
| `examples/workbuddy/.run/real/hooks/*.stdin.json` | Route B 真实 CLI hook payload 原文 |
| `examples/workbuddy/.run/real/hooks/*.stdout.json` | `pd-memory hook` 对这些 payload 的真实输出 |
| `examples/workbuddy/.run/real/mcp-probe.log` | 真实 CLI ↔ `pd-memory serve mcp` 的 JSON-RPC 原文 |
| `examples/workbuddy/.run/real/mock-openai-requests.json` | 真实 CLI 发给本地 mock 模型的请求体（注入证明） |
| `examples/workbuddy/.run/real/projects/**/*.jsonl` | 真实 CLI 生成的 transcript |
