# 接入宿主总览

pd-memory 通过两条通道接入 Agent 宿主：stdio MCP（七个记忆工具）和 Claude Code 风格的命令 hooks
（每个生命周期事件启动一次 `pd-memory hook --host <id>`）。宿主之间的差异集中在两处，每个宿主各一个文件：

| 层 | 位置 | 负责 |
|---|---|---|
| Rust 适配器 | `crates/pd-memory-hooks/src/hosts/<id>.rs` | 事件名映射、输出 schema、transcript 读取、`HostCaps` |
| 安装器模块 | `scripts/install/hosts/<id>.py` | 配置路径、MCP 合并（JSON 或 TOML）、hooks 合并、安装后检查与提示 |

其余部分与宿主无关：

- `crates/pd-memory-hooks/src/policy.rs`：宿主锁、首次召回积压屏障、会话结束策略，只读 `HostCaps`；
- `crates/pd-memory-hooks/src/events.rs`：各规范事件的通用行为（概览、召回、捕获、flush）；
- `crates/pd-memory-hooks/src/transcript/`：`claude.rs`、`workbuddy.rs`、`codex.rs` 三种格式，
  默认读取器逐行自动识别；
- `scripts/install/common.py`：备份、清单与回退、安装锁、模型配置、wrapper、MCP 与模型验证；
- `scripts/install.sh --host <id>`：统一安装入口。

适配器不进入 `pd-memory-core`，也不读取环境变量或用户目录。

## 处理流程

```text
stdin JSON ─parse─> HookInput ─HostAdapter::normalize─> CanonicalEvent
   ─policy::Session（锁 control/<id>/hooks.lock、会话标记、HostCaps）─> events（通用行为）
   ─HostAdapter::render─> stdout 一行 JSON
```

规范事件：

| CanonicalEvent | 通用行为 |
|---|---|
| `Start` | 注入记忆概览，不调用模型 |
| `Recall { prompt }` | 本会话首次有效提问先清空积压，再召回并注入 `additionalContext` |
| `Capture` | 读取 transcript 捕获本轮，检查到期维护 |
| `End` | 捕获；预算足够时再做维护并清空积压，否则只捕获；清除会话标记 |
| `Compact` | flush 待处理批次 |
| `Ignore` | 只输出 `{"continue":true}` |

`HostCaps` 字段：

- `end_budget_ms`：宿主允许会话结束 hook 运行的时间。低于 30 秒时 `End` 只捕获，不做模型调用；
  `End` 等待宿主锁最多为预算的一半（上限 120 秒），其他事件最多 120 秒。
- `has_transcript`：payload 是否带可用的 `transcript_path`；为 `false` 时不读 transcript，
  只用 `last_assistant_message` 兜底捕获。
- `inject_on`：允许输出 `hookSpecificOutput` 的事件；其他事件一律只输出 `{"continue":true}`。

## 能力对照

已实现：

| | WorkBuddy / CodeBuddy | Codex CLI / App |
|---|---|---|
| 文档 | [workbuddy.md](workbuddy.md) | [codex.md](codex.md) |
| hook 命令 | `hook --workbuddy`（= `--host workbuddy`） | `hook --codex`（= `--host codex`） |
| 事件名 | Claude Code：SessionStart / UserPromptSubmit / Stop / SessionEnd / PreCompact | 同左 |
| `end_budget_ms` | 300000（安装器设置的超时） | 3000（Codex 上限 3 秒，默认 1 秒） |
| 会话结束 | 捕获 + 维护 + 清空积压 | 只捕获，积压由下个会话首次召回处理 |
| `has_transcript` | 是 | 是（可能为 `null`） |
| `inject_on` | Start、Recall | Start、Recall（输出 schema 禁止其他字段） |
| transcript | `projects/<编码 cwd>/<会话>.jsonl`，GUI 用 `<system-reminder>…<user_query>` 包裹 | rollout `{timestamp,type,payload}`，过滤注入上下文 |
| MCP 配置 | `~/.workbuddy/mcp.json`、`.mcp.json`（JSON `mcpServers`） | `~/.codex/config.toml`（TOML `[mcp_servers.pd-memory]`） |
| hooks 配置 | `~/.workbuddy/settings.json`，追加分组 | `~/.codex/hooks.json`，原位替换（信任按位置记录） |
| 需人工确认 | GUI 自定义连接器中信任 MCP | `/hooks` 或 Settings → Hooks 信任四个 hooks |
| wrapper / 配置 | `pd-memory-workbuddy` / `config.json` | `pd-memory-codex` / `codex.json` |
| 备份目录 | `installations/` | `installations/codex/` |

规划中（**未实现、未验证**，以下只是接入时要确认的差异）：

| 宿主 | 预期接入方式 | 待处理的差异 |
|---|---|---|
| Qwen Code、Trae | 命令 hooks + MCP，接近直接复用 | 配置文件位置、超时、transcript 格式 |
| Gemini CLI | 命令 hooks + MCP | 事件改名（BeforeAgent / AfterAgent / PreCompress），在 `normalize` 中映射 |
| Kimi | 命令 hooks + MCP | 无 `transcript_path`：`has_transcript = false` |
| OpenCode、DeepSeek Harness | JS / Cordis 插件放在 `integrations/<host>/`，调用 `pd-memory hook --host <id>` | 插件负责把宿主事件转成 hook JSON |

## 新增宿主

1. **Rust 适配器**：新建 `crates/pd-memory-hooks/src/hosts/<id>.rs`，实现 `HostAdapter`
   （`id`、`label`、`caps`、`normalize`；输出 schema 或 transcript 格式不同时再覆盖 `render`、
   `read_transcript`），并加入 `hosts::all()`。`hook --host <id>` 自动可用。新 transcript 格式放在
   `transcript/<format>.rs`。
2. **契约夹具**：新建 `crates/pd-memory-hooks/tests/fixtures/<id>/`：
   - `caps.json`：声明的 `HostCaps`；
   - `transcript.jsonl` 与 `transcript.expected.json`：宿主真实形态的 transcript 与应读出的消息；
   - `cases.json`：按顺序执行的 hook 调用（payload、规范事件、积压数、召回结果、stdout、引擎调用序列）。

   `tests/host_contract.rs` 遍历所有已注册宿主，缺夹具即失败。
3. **安装器**：新建 `scripts/install/hosts/<id>.py`（接口见 `common.py` 顶部说明），把 `<id>`
   加入 `common.HOSTS`。JSON `mcpServers` 用 `common.merge_mcp_servers`；Claude Code 风格 hooks 用
   `common.merge_hooks`（`in_place=True` 用于按位置记录信任的宿主）。写入的 hook 命令建议用
   `hook --host <id>`。新宿主的备份放在 `installations/<id>/`。
4. **文档**：新建 `docs/hosts/<id>.md`，并在上面的能力对照表加一列。

不变量：

- 锁与会话标记路径为 `<数据目录>/workspaces/<工作区>/control/<id>/`（`hooks.lock`、
  `<sha1(会话键)>.checked`），由 `id` 决定，改名会让已有安装的标记失效。
- 已发布宿主写入配置的 hook 命令（`hook --workbuddy`、`hook --codex`）保持不变：Codex 按命令哈希记录信任。
- 任何失败都输出 `{"continue":true}` 且退出码为 0；hook 不阻断宿主。

## 测试

```bash
cargo test --locked -p pd-memory-hooks --test host_contract   # 所有宿主的契约夹具
cargo test --locked --workspace
cargo build --locked -p pd-memory-cli
python3 -m unittest discover -s scripts/install/tests -p 'test_*.py' -v
```

测试均离线、使用确定性的 mock 模型或假引擎，不代表真实模型的召回质量或 GUI 中的实际表现。
