# pd-memory × WorkBuddy 集成指南

本文说明如何把 `pd-memory`（Rust 记忆 SDK）接入 WorkBuddy（腾讯 CodeBuddy 家族的桌面 Agent），
以及如何在本机**无登录、无外网**地验证 MCP stdio 与生命周期 hooks 两条链路。

- 基线版本：**WorkBuddy 5.8.2**，内置 **CodeBuddy Code CLI 2.161.0**（`bin/codebuddy` + `dist/codebuddy-lite-wb.mjs`）
- 验证环境：macOS（本机 `/Applications/WorkBuddy.app`），日期见 `examples/workbuddy/EVIDENCE.md`
- 事实来源：[上游与验证说明](upstream.md) + 本目录脚本的实测输出

---

## 1. 一句话结论

| 链路 | 状态 | 说明 |
|---|---|---|
| MCP stdio server | ✅ 真实 CLI 已验证 | `pd-memory serve mcp` 能被真实 WorkBuddy CLI 拉起，完成 `initialize`（`protocolVersion=2025-11-25`）→ `notifications/initialized` → `tools/list`（7 个工具） |
| Hooks（SessionStart / UserPromptSubmit / Stop） | ✅ 真实 CLI 已验证 | 真实 WorkBuddy CLI `-p` 会话会以**真实 snake_case stdin JSON** 调用 hook；`Stop` 携带 `transcript_path` 与 `last_assistant_message`；`UserPromptSubmit` 的 `hookSpecificOutput.additionalContext` 会被注入模型请求 |
| 无头端到端（无登录/无网络） | ✅ 已验证 | 用 `models.json` 自定义模型指向本地 SSE mock 端点，绕过默认模型登录限制 |
| SessionEnd / PreCompact | ⚠️ 未触发 | 单次 `-p` 运行不触发；事件存在于运行时枚举，载荷字段已知，GUI 交互场景预期会触发但未实测 |
| GUI 内注入 | ⚠️ 未验证 | 不启动 GUI 的约束下未做人工冒烟；GUI 以 `--setting-sources=user --strict-mcp-config` 启动（见 §6.4），因此 hooks 走用户级设置、MCP 走 GUI 连接器/插件 |

---

## 2. 本机安装布局（已核对）

| 路径 | 用途 |
|---|---|
| `/Applications/WorkBuddy.app` | 桌面应用（5.8.2） |
| `.../Contents/Resources/app.asar.unpacked/cli/bin/codebuddy` | CodeBuddy Code CLI 入口（需要 Node 运行） |
| `.../cli/dist/codebuddy-lite-wb.mjs` | 唯一实际存在的应用 bundle（必须强制 lite-wb 才能启动） |
| `~/.workbuddy/binaries/node/versions/22.22.2-6/bin/node` | 应用自带的 Node（v22.22.2），无需 PATH 里有 node |
| `~/.workbuddy/settings.json` | 用户级设置（GUI 只加载 user 级；hooks 写在这里） |
| `~/.workbuddy/.mcp.json` | CLI 用户级 MCP 配置（CLI `mcp` 子命令的 user scope） |
| `~/.workbuddy/connectors/default/mcp.json` | GUI「连接器中心」注册表（`mcpServers` map；GUI 实际通过 `CODEBUDDY_MCP_CONFIG` 注入） |
| `~/.workbuddy/plugins/cache/**` | 已安装插件（插件可同时携带 `.mcp.json` 与 `hooks/hooks.json`） |
| `~/.workbuddy/projects/<encoded-cwd>/<sessionId>.jsonl` | 桌面端会话 transcript（hook 输入的 `transcript_path` 指向同类文件） |
| `~/.workbuddy/logs/daemon.log` | GUI sidecar 启动命令行（集成排查入口） |

> 关键限制（来自 `daemon.log` 实测）：**GUI 用 `--setting-sources=user --strict-mcp-config` 启动 CLI**。
> 即：项目级 `.codebuddy/settings.json` 的 hooks **不会**被 GUI 加载；用户手写 `.mcp.json` 也**不会**被 GUI 采用，
> GUI 里的 MCP 必须落到「连接器中心」或插件里。

---

## 3. 目录内容

```
examples/workbuddy/
├── pd-memory.config.json        # 演示用 SDK 配置（相对路径 data_root，见 §5.1 注意事项）
├── workbuddy.mcp.json           # GUI 连接器注册表用的 mcpServers 片段（绝对路径占位）
├── workbuddy.cli.mcp.json       # CodeBuddy CLI 原生 .mcp.json 片段（user/project scope / --mcp-config）
├── hooks/
│   ├── hooks.json               # 插件级 hooks（插件 hooks/hooks.json，含顶层 description）
│   ├── settings.json            # 完整 settings 对象（{"hooks":{...}}，用于 --settings 或合并用户设置）
│   └── README.md                # 已验证/未验证清单与安装方式
├── install_workbuddy.sh         # 合并 MCP(+可选 hooks) 到 WorkBuddy 配置：默认 dry-run，--apply 带备份，--revert 恢复
├── simulate_workbuddy.py        # 端到端 harness：路线 A 契约模拟器 + 路线 B 真实 CLI 无头
├── run_e2e.sh                   # 一条命令：cargo build + 模拟器 + tee EVIDENCE.log
├── EVIDENCE.md                  # 最近一次真实运行捕获的输出证据
└── .run/                        # 运行时状态（gitignored）：data root、transcripts、CLI 配置、探针日志
```

---

## 4. MCP 接入

### 4.1 字段格式（来自真实文件/实测）

stdio server 的最小片段：

```json
{
  "mcpServers": {
    "pd-memory": {
      "type": "stdio",
      "command": "/abs/path/target/debug/pd-memory",
      "args": ["--config", "/abs/path/examples/workbuddy/pd-memory.config.json", "serve", "mcp"],
      "env": { "PD_MEMORY_PROVIDER": "mock", "PD_MEMORY_DATA_ROOT": "/abs/path/examples/workbuddy/.run/data" },
      "timeout": 120000,
      "disabled": false
    }
  }
}
```

| 字段 | 说明 |
|---|---|
| `type` | `stdio`（本地进程）；远端还有 `sse` / `http` / `streamableHttp` |
| `command` / `args` | 可执行文件与参数（stdio 必填 `command`） |
| `env` | 子进程环境变量；真实 WorkBuddy 注册表里大量使用 `${VAR}` 展开 |
| `timeout` | 毫秒；CLI 内置 MCP 连接默认 20s |
| `disabled` | 仅 GUI 连接器注册表使用（`false` 表示启用）；CLI 原生 `.mcp.json` 不用这个字段，而是顶层 `disabledMcpServers` |
| `runtime` / `staticEnv` / `npmRegistry` | WorkBuddy 连接器专用扩展（npx/uvx 场景），本地二进制不需要 |

### 4.2 放哪里

| 场景 | 文件 | 备注 |
|---|---|---|
| 真实 CLI 集成测试（推荐） | 任意路径 + `--mcp-config <file> --strict-mcp-config` | ✅ 实测会真正拉起 stdio server，且发生在模型调用之前 |
| CLI 用户级 | `~/.workbuddy/.mcp.json`（`CODEBUDDY_CONFIG_DIR=~/.workbuddy` 时） | 手写条目在 `mcp list` 里可能显示 `Needs approval`；`codebuddy mcp add -s user ...` 写入的显示 `✓ Connected` |
| CLI 项目级 | `<project>/.mcp.json` | 同上审批问题 |
| GUI 连接器注册表 | `~/.workbuddy/connectors/default/mcp.json` | 形状见 `workbuddy.mcp.json`；**手动改后 GUI 是否接受未验证**（受 connector-states / marketplace 元数据约束，需重启 GUI） |
| 插件（推荐分发形态） | `<plugin>/.codebuddy-plugin/plugin.json` 声明 `"mcpServers": "./.mcp.json"` | ✅ CLI `--plugin-dir` 实测可拉起插件 MCP；GUI 插件安装流程未验证 |

### 4.3 安装脚本（默认 dry-run，绝不破坏无关条目）

```bash
cd pd-memory

# 1) 先看 diff（不写任何文件）
./examples/workbuddy/install_workbuddy.sh                      # 默认目标：GUI 连接器注册表
./examples/workbuddy/install_workbuddy.sh --mcp-target both --with-hooks   # 同时看 hooks 合并 diff

# 2) 确认后应用（自动备份为 *.bak.<timestamp>）
./examples/workbuddy/install_workbuddy.sh --apply --mcp-target both --with-hooks

# 3) 回滚到最近一次备份（当前文件会另存为 *.pre-revert.bak.<timestamp>）
./examples/workbuddy/install_workbuddy.sh --revert --mcp-target both --with-hooks
```

- 合并是**幂等**的：只替换 `mcpServers["pd-memory"]`，其它 server 原样保留；
- hooks 合并只替换 `description` 以 `pd-memory:` 开头的组（本仓库生成的组不带 description，改按命令指纹识别），其它事件/组保留；
- `--workbuddy-home` / `--binary` / `--config` / `--data-root` 可覆盖路径；
- 脚本不会删除任何用户数据；写坏的 JSON 会拒绝处理（打印错误退出码 2）。

### 4.4 MCP server 纪律

- **stdout 只能输出 JSON-RPC**（逐行 JSON）；日志一律走 stderr，否则 CLI 解析失败。
- `initialize` 必须回显客户端请求的 `protocolVersion`（WorkBuddy CLI 发 `2025-11-25`）。
- 工具清单固定 7 个：`memory_search`、`memory_list`、`memory_get`、`memory_overview`、`memory_flush`、`memory_dream`、`memory_add`。

---

## 5. Hooks 接入

### 5.1 配置位置与结构

```json
{
  "hooks": {
    "SessionStart": [
      { "hooks": [ { "type": "command", "command": "<pd-memory> --config <cfg> hook", "timeout": 20 } ] }
    ],
    "UserPromptSubmit": [
      { "hooks": [ { "type": "command", "command": "...", "timeout": 60 } ] }
    ],
    "Stop": [
      { "hooks": [ { "type": "command", "command": "...", "timeout": 60 } ] }
    ],
    "SessionEnd": [ { "hooks": [ { "type": "command", "command": "...", "timeout": 20 } ] } ],
    "PreCompact": [ { "hooks": [ { "type": "command", "command": "...", "timeout": 120 } ] } ]
  }
}
```

| 配置位置 | 作用域 | 状态 |
|---|---|---|
| `~/.workbuddy/settings.json` 顶层 `"hooks"` | 用户级（GUI 唯一加载的设置来源） | ✅ CLI 实测触发；GUI 侧按启动参数推断会加载（未做 GUI 冒烟） |
| `<project>/.codebuddy/settings.json` | 项目级 | ✅ CLI 实测（`--setting-sources project`）；❌ GUI 不加载 |
| `<plugin>/hooks/hooks.json` + `plugin.json.hooks` | 插件级 | ✅ CLI `--plugin-dir` 实测；GUI 插件安装未验证 |
| `--settings <file>` | 单次运行 | ✅ 测试最方便（本仓库 `hooks/settings.json`） |

**只使用真实插件里出现过的字段**：command hook 用 `type` / `command` / `timeout`（可选 `statusMessage`、`shell`、`once`、`async`…）；
`description` 只允许出现在 hooks.json 顶层，**不要**放进 command 对象（避免严格校验时静默不加载）。

> **相对路径语义（已实现并验证）**：CLI 的 `config_loader` 把配置里的相对 `data_root` / `workspace_root` 一律按
> **配置文件所在目录**解析（不是进程 cwd）——因为 hook 子进程 cwd 不固定（`SessionStart` 甚至没有 `cwd`）。
> 因此本仓库 `pd-memory.config.json` 使用 `"data_root": ".run/data"`，无论从哪个目录启动都指向 `examples/workbuddy/.run/data`。
> `install_workbuddy.sh` 与 `simulate_workbuddy.py` 仍统一生成绝对路径，避免歧义。

### 5.2 事件映射

| 事件 | 触发时机 | pd-memory 动作 |
|---|---|---|
| `SessionStart` | CLI 进程启动/恢复会话 | 召回：返回 `hookSpecificOutput.additionalContext` 注入记忆上下文 |
| `UserPromptSubmit` | 用户提交 prompt、模型调用前 | 召回：注入相关记忆（真实 CLI 会把 `additionalContext` 放进模型请求） |
| `Stop` | 一轮结束、拿到最终回复 | 捕获：读 `transcript_path` / `last_assistant_message` 入 L0 队列，按阈值 flush |
| `SessionEnd` | 会话结束（`-p` 不触发） | 捕获：会话尾部 |
| `PreCompact` | 上下文压缩前（`-p` 不触发） | flush / dream，避免压缩丢信息 |

`matcher` 是**正则**（不是通配符）：空 / `"*"` / 缺省匹配全部；`SessionStart` 匹配 `source`，`PreCompact` 匹配 `trigger`；
`UserPromptSubmit` / `Stop` 无匹配值，matcher 被忽略。本仓库配置不写 matcher，以免漏掉 `resume` 等来源。

### 5.3 输入契约（真实 CLI 实测，snake_case）

```json
{"hook_event_name":"SessionStart","session_id":"01a1...","transcript_path":"/…/projects/<enc>/<sid>.jsonl","source":"startup","permission_mode":"default","client":"CLI","version":"2.161.0","model":"mock-model"}
{"hook_event_name":"UserPromptSubmit","session_id":"01a1...","transcript_path":"…","cwd":"/…/project","prompt":"请记住：…","is_internal_continuation":false,"permission_mode":"default",…}
{"hook_event_name":"Stop","session_id":"01a1...","transcript_path":"…","cwd":"…","stop_hook_active":false,"agent_type":"cli","last_assistant_message":"MOCK_REPLY: …","background_tasks":[],"session_crons":[],…}
```

- `SessionStart` **没有 `cwd`**：用 `CODEBUDDY_PROJECT_DIR` / `CLAUDE_PROJECT_DIR` 环境变量或从 `transcript_path` 反推。
- 所有事件都有 `transcript_path`；`Stop` 直接带最终回复，优先用它做抽取，需要工具轨迹再读 transcript。
- hook 子进程还会收到 `CODEBUDDY_PLUGIN_ROOT` / `CLAUDE_PLUGIN_ROOT` 等插件变量。

### 5.4 输出契约

`pd-memory hook` 在 stdout 输出**一行 JSON**（解析失败时 CLI 会把 stdout 当普通消息文本）：

```jsonc
{
  "continue": true,
  "systemMessage": "可选：展示给用户",
  "hookSpecificOutput": {
    "hookEventName": "UserPromptSubmit",
    "additionalContext": "## ClawXMemory Recall\n\n…"
  }
}
```

- `additionalContext` 会被注入模型上下文（真实 CLI + 本地 mock 模型实测：记忆内容出现在模型请求体里）；
- `{"decision":"block","reason":"…"}` 或 `exit 2` + stderr 会阻断当前操作（记忆 SDK 正常路径不要用）；
- `Stop` hook 注意 `stop_hook_active`，避免自我递归阻塞；
- hook 同步执行，默认超时 60s；重活建议拆到 `PreCompact`/后台，或输出 `{"async":true}`。

---

## 6. 无头运行 WorkBuddy CLI（真实链路）

### 6.1 前提

```bash
NODE=~/.workbuddy/binaries/node/versions/22.22.2-6/bin/node
CLI=/Applications/WorkBuddy.app/Contents/Resources/app.asar.unpacked/cli/bin/codebuddy
export CODEBUDDY_FORCE_LITE_WB_BUNDLE=1   # 必须；否则 -p 报 Cannot find module '../dist/codebuddy'
export CODEBUDDY_CONFIG_DIR=$TDIR          # CLI home：settings.json / projects/ / sessions/
export WORKBUDDY_CONFIG_DIR=$TDIR          # WorkBuddy 专有数据：models.json
```

### 6.2 默认模型会被登录拦截（且 rc=0！）

```bash
"$NODE" "$CLI" -p 'hello' --output-format json
# stderr: Authentication required. Please use /login command to sign in to your account
# exit code: 0  ← 不要用退出码判断成功
```

### 6.3 绕过登录：`models.json` + 本地 OpenAI 兼容 SSE 端点

`$TDIR/models.json`：

```json
{
  "models": [{
    "id": "mock-model", "name": "Mock Model", "vendor": "mock",
    "url": "http://127.0.0.1:<port>/v1", "apiKey": "sk-mock-test",
    "maxOutputTokens": 2048, "maxInputTokens": 32000, "supportsToolCall": true
  }],
  "availableModels": ["mock-model"]
}
```

端点要求：`POST /v1/chat/completions`，请求体 `stream:true`，响应必须是 SSE：

```
data: {"id":"chatcmpl-mock1","object":"chat.completion.chunk","created":0,"model":"mock-model","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}

data: {"id":"chatcmpl-mock1","object":"chat.completion.chunk","created":0,"model":"mock-model","choices":[{"index":0,"delta":{"content":"MOCK_REPLY: …"},"finish_reason":null}]}

data: {"id":"chatcmpl-mock1","object":"chat.completion.chunk","created":0,"model":"mock-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":8,"total_tokens":18}}

data: [DONE]
```

### 6.4 一次真实的 WorkBuddy 会话（hooks + MCP）

```bash
"$NODE" "$CLI" -p '请记住：WorkBuddy 集成项目的代号是 ORION-7421' \
  --model mock-model \
  --mcp-config $TDIR/mcp.json --strict-mcp-config \
  --settings $TDIR/settings.json \
  --output-format json
```

- `-p` 实测触发顺序：`SessionStart` → `UserPromptSubmit` →（模型调用）→ `Stop`；
- `--strict-mcp-config` 让 CLI 只使用 `--mcp-config` 指定的 MCP servers（GUI 也用它）；
- `--output-format json` 输出可能是一个 result 对象，也可能是会话事件数组（本机实测为数组），断言时都要兼容；
- 该命令在本地 mock 模型下无需登录、无外网。

> GUI 的启动方式（`~/.workbuddy/logs/daemon.log`）：
> `Electron <cli/bin/codebuddy> --serve --no-session-persistence --setting-sources=user --strict-mcp-config`。
> 这解释了为什么 GUI 只认 user 级 hooks、只认 GUI 自己组装的 MCP。

---

## 7. 一键验证

```bash
cd pd-memory

# 构建 + 契约模拟器（默认；WorkBuddy CLI 存在时自动追加真实 CLI 路线）
bash examples/workbuddy/run_e2e.sh

# 只跑契约模拟器（CI 环境、无 WorkBuddy 安装）
bash examples/workbuddy/run_e2e.sh --no-real

# 只跑真实 WorkBuddy CLI（自动探测 app 内置 Node/CLI）
bash examples/workbuddy/run_e2e.sh --only-real

# 调试：回显每条命令的原始 stdout/stderr
bash examples/workbuddy/run_e2e.sh --verbose
```

产物：

- `examples/workbuddy/EVIDENCE.log`：完整运行日志（tee）
- `examples/workbuddy/EVIDENCE.md`：人工整理的证据摘要（最近一次真实运行）
- `examples/workbuddy/.run/e2e-summary.json`：机器可读的 PASS/FAIL 明细
- `examples/workbuddy/.run/sim/**`（路线 A）与 `.run/real/**`（路线 B）：数据根、transcript、真实 hook stdin/stdout、MCP 探针日志

### 7.1 校验清单（手工核对）

- [ ] `.run/sim/data/workspaces/workbuddy-demo/memory/MEMORY.md` 存在且非空
- [ ] `.run/sim/data/workspaces/workbuddy-demo/memory/{Project,Feedback}/**.md` 至少 1 个记忆文件
- [ ] `.run/sim/data/queue/*.jsonl`（L0 队列）含会话内容 → hook 捕获成功
- [ ] 路线 A 的 MCP：`memory_search` 结果含事实 token，工具数为 7
- [ ] 路线 B：`.run/real/hooks/{SessionStart,UserPromptSubmit,Stop}.stdin.json` 是真实 CLI payload
- [ ] 路线 B：`.run/real/hooks/Stop.stdin.json` 的 `last_assistant_message` 等于 mock 回复
- [ ] 路线 B：`.run/real/mcp-probe.log` 含 `initialize` / `notifications/initialized` / `tools/list`
- [ ] 路线 B：第二轮 `UserPromptSubmit` 注入的记忆上下文出现在 mock 模型请求体（harness 自动断言）
- [ ] `dream` 后出现 `workspaces/workbuddy-demo/last_dream/`，`rollback` 后 `MEMORY.md` 仍在

---

## 8. 为什么模拟器是可信的回退

`simulate_workbuddy.py` 的路线 A 不是"随便造个 JSON"，它的 fixture 全部来自真实采样（`docs/upstream.md` §3.5 / §4.2）：

- stdin payload 字段（`hook_event_name` / `session_id` / `transcript_path` / `cwd` / `prompt` / `last_assistant_message` …）逐字段对照真实 CLI 输出；
- transcript JSONL 采用桌面端真实记录类型（`message` / `reasoning` / `function_call` / `function_call_result`，`content[].type=input_text|output_text`）；
- MCP 客户端握手使用 WorkBuddy 实际发送的 `protocolVersion=2025-11-25` 和 `clientInfo`；
- 路线 B 用**真实 CLI** 跑同一套断言，因此路线 A 是对真实契约的回归，而不是替代品。

WorkBuddy 升级后需要重新采样 fixture（路线 B 会直接失败给出信号）。

---

## 9. 限制与风险

1. **版本强耦合**：格式来自 WorkBuddy 5.8.2 / CLI 2.161.0；升级后 `models.json`、hook 字段、`--settings`、bundle 名字都可能变。EVIDENCE 里记录了基线版本。
2. **GUI 未做人工冒烟**：`~/.workbuddy/settings.json` hooks 是否在 GUI 会话触发是"按启动参数推断 + CLI 实测"，未重启 GUI 验证；MCP 在 GUI 里需要连接器/插件，`connectors/default/mcp.json` 手改是否生效也未验证。
3. **`-p` 的退出码不可信**：模型鉴权失败时仍返回 0；必须断言 stdout/stderr/数据目录。
4. **MCP 审批**：手写 `.mcp.json` 在非交互 `mcp list` 中可能显示 `Needs approval`；测试用 `--mcp-config --strict-mcp-config` 最稳，GUI 用连接器/插件。
5. **hook 同步阻塞**：默认 60s 超时，超时被杀；记忆抽取要快或放到 `PreCompact`/异步。
6. **Stop 递归**：必须检查 `stop_hook_active`，否则可能死循环。
7. **transcript 敏感**：真实会话含路径、命令输出甚至 token；落盘/入模前由 SDK 脱敏（`redaction.enabled`）。
8. **相对路径歧义**：hooks 子进程 cwd 不固定（`SessionStart` 无 cwd），配置里一律用绝对路径（安装脚本/harness 自动处理）。
9. **mock 仅是测试**：`PD_MEMORY_PROVIDER=mock` 与本地 SSE mock 端点只用于离线确定性验证，不代表真实模型质量；生产用真实 provider + `api_key_env`。
10. **不要修改 app 包**：`app.asar` / `app.asar.unpacked` 属于签名应用，集成只通过配置/插件/环境变量完成。

---

## 10. 安全须知

- **绝不提交**真实的 `~/.workbuddy/settings.json`、`.mcp.json`、`models.json`、API key、transcript。
  本仓库所有示例均使用 `__PD_MEMORY_*__` 占位符或 mock 值。
- `models.json` 的 `apiKey` 是明文；只把 mock 端点和假 key 用于测试。
- 修改 `~/.workbuddy` 前**必须备份**：`install_workbuddy.sh` 默认 dry-run，`--apply` 自动写
  `*.bak.<timestamp>`；`~/.workbuddy/settings.json` 被 WorkBuddy 自身列为受保护文件，改前关闭 GUI。
- 不替用户登录、不读取/导出凭据；集成测试全部在 `mktemp -d` 或 `examples/workbuddy/.run/` 隔离目录完成。
- `install_workbuddy.sh --revert` 只恢复自己的备份，不删除无关条目。
