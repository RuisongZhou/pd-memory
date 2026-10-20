# Codex 安装与集成

pd-memory 通过 stdio MCP 为 Codex 提供七个工具，通过用户级 hooks 捕获对话并注入召回上下文。
接入方式与 [WorkBuddy](workbuddy.md) 相同：一个 wrapper、一份私有模型配置、一个数据目录，
不需要常驻服务。宿主差异只在 `pd-memory hook --codex`（即 `hook --host codex`）和安装器中处理，记忆引擎行为不变。
各宿主能力对照见 [接入宿主总览](README.md)。

## 安装

前置条件：macOS 或 Linux、Codex CLI / App（按 0.154 的 hooks 契约实现）、**Python 3.11+**
（安装器用标准库 `tomllib` 校验 `config.toml`）、Rust stable / Cargo。

```bash
bash scripts/install.sh --host codex --apply --verify-model --workspace /absolute/path/to/project
```

旧入口 `bash scripts/codex/install.sh` 仍可用，等价于 `scripts/install.sh --host codex`。
不传 `--apply` 时只预览。首次运行询问模型名、API 服务前缀和密钥（隐藏输入），选项与 WorkBuddy
安装器相同：`--api-type`、`--model`、`--base-url`、`--api-key-env`、`--key-file`、`--non-interactive`、
`--mock`、`--binary`、`--data-root`、`--home`；另有 `--codex-home`（默认 `<home>/.codex`，
不读取 `CODEX_HOME` 环境变量）与 `--skip-codex-check`。

`--workspace` 是这份记忆绑定的项目目录，首次安装必填，之后沿用。这是单工作目录安装：所有 Codex
会话（无论在哪个目录启动）都读写同一份记忆。已安装 WorkBuddy 时会复用其模型配置、密钥和数据目录，
但**不会**继承 WorkBuddy 的工作目录；两边想共享记忆就传同一个 `--workspace`。

安装器执行以下步骤：

1. 读取并校验现有 `config.toml`、`hooks.json` 和 pd-memory 配置。
2. 构建 release（或用 `--binary`），确认二进制支持 `hook --codex`。
3. 备份将修改的文件，写入二进制、`pd-memory-codex` wrapper、`~/.config/pd-memory/codex.json`。
4. 在 `~/.codex/config.toml` 写入 `[mcp_servers.pd-memory]`，在 `~/.codex/hooks.json` 合并四个 hooks。
5. 在临时数据目录验证 MCP 握手、七个工具、工具调用，以及 Stop / SessionEnd 输出恰为 `{"continue":true}`。
6. `codex` 在 PATH 上时，把两份合并结果复制到临时 `CODEX_HOME`，用 `codex mcp get` 与 app-server
   的 `hooks/list` 确认 Codex 自己能读出 MCP 条目和四个 hooks。不会启动会话或调用 OpenAI。
7. `--verify-model` 时用合成的 Codex rollout 分别验证 Stop、SessionEnd 捕获 → 新会话首次召回 flush → 检索。

任一步失败自动恢复已修改文件。

### 信任 hooks（必须手动完成）

Codex 不运行未经审阅的 hook。安装后重启 Codex，然后：

- CLI：在会话里执行 `/hooks`，审阅并信任四个 pd-memory hooks；
- App：打开 Settings → Hooks，信任同样四项。

Codex 按「事件 + 位置」记录信任，并保存配置哈希（写在 `config.toml` 的 `[hooks.state]` 下）。
hook 命令、超时或位置任何改动都需要重新信任。安装器重复安装时原位替换已有的 pd-memory 条目，
内容不变即不改动文件，因此已给出的信任保持有效；更换安装路径或升级改变命令时需要重新信任。
信任前 MCP 工具可用，但不会自动捕获和注入。

安装器不写 `[hooks.state]`，也不会添加、修改或删除 `notify`：合并后重新解析 `config.toml`，
只有 `mcp_servers.pd-memory` 之外的内容完全一致才写入，注释和顺序按原字节保留。

## 写入的配置

```toml
[mcp_servers.pd-memory]
command = "/Users/me/.local/bin/pd-memory-codex"
args = ["serve", "mcp"]
startup_timeout_sec = 30
tool_timeout_sec = 300
```

`hooks.json` 中每个事件一个独立分组，命令为 `'<wrapper>' hook --codex`：

| 事件 | 行为 | 超时 |
|---|---|---|
| SessionStart | 注入记忆概览，不 flush | 150 秒 |
| UserPromptSubmit | 本会话首次有效提问先处理积压，再召回并输出 `additionalContext` | 300 秒 |
| Stop | 从 rollout 捕获本轮，检查到期维护；只输出 `{"continue":true}` | 300 秒 |
| SessionEnd | 只捕获，不 flush、不做维护；只输出 `{"continue":true}` | 3 秒 |

## `hook --codex` 与普通 hook 的差别

- **输出严格**：Codex 的 hook 输出 schema 是 `additionalProperties: false`，Stop 没有
  `hookSpecificOutput`。`--codex` 只在 SessionStart、UserPromptSubmit 输出 `hookSpecificOutput`，
  其他事件一律 `{"continue":true}`。
- **SessionEnd 不 flush**：Codex 给 SessionEnd 默认 1 秒、上限 3 秒，超时即终止进程。flush
  需要模型调用，因此这里只捕获；积压由下一个会话的首次召回处理（与 WorkBuddy 相同的首次召回屏障，
  标记位于工作目录数据下的 `control/codex/`）。锁 `control/hooks.lock` 由同一工作目录的所有宿主共用，
  与 WorkBuddy 共享工作目录时两边的 hooks 互相串行。SessionEnd 等锁最多 1 秒（预算的三分之一），
  拿不到就放弃，把剩余约 2 秒留给打开引擎、读取 rollout 尾部和捕获；最后一轮通常已由 Stop 捕获。
- **`transcript_path` 可为 `null`**：Codex 无 rollout 时传 `null`，按空值处理。
- 其他事件（PreCompact、SubagentStop、Interrupt 等）未安装；若被调用也只输出 `{"continue":true}`。

## Rollout 解析

`transcript_path` 指向 `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`，每行
`{timestamp, type, payload}`。只读取 `type=response_item` 且 `payload.type=message` 的记录：

- 丢弃 `role=developer`（权限、协作模式等宿主指令）；
- 丢弃 Codex 作为 user 消息注入的上下文：`# AGENTS.md instructions for …`、`<environment_context>`、
  `<recommended_plugins>`、`<skill>`、`<subagent_notification>`、`<image …>` 标记等整块标签；
  附件前言 `# Files mentioned by the user: … ## My request:` 只保留请求正文；
- 助手消息优先 `phase=final_answer`；某一轮没有最终答案（例如被中断）时只保留最后一条
  `commentary`；没有 `phase` 的旧格式照常保留；
- `event_msg`、`reasoning`、工具调用及输出、`turn_context`、`compacted` 等均忽略。

整块用户标签按白名单判定：`environment_context`、`user_instructions`、`recommended_plugins`、`skill`、
`image`、`INSTRUCTIONS`、`subagent_notification`、`turn_aborted`、`send_user_message_question_reply`、
`in-app-browser-context`，以及前缀 `external_codex_apps_`（来自 codex-rs 0.154 与本机真实 rollout）。
整段文本被这些标签之一包住才视为宿主上下文；用户粘贴的其他整块标签（如 `<error-log>…</error-log>`）
照常保留。`<send_user_message_question_reply>`（对 Codex 提问的表单回复）因此不进入记忆。
Codex 新增注入标签时需要补充白名单（`crates/pd-memory-hooks/src/transcript/codex.rs`）。

## 回退与卸载

```bash
bash scripts/install.sh --host codex --rollback     # 按字节恢复安装前的文件
bash scripts/install.sh --host codex --uninstall    # 只移除 pd-memory 的 MCP 表和 hooks
```

备份位于 `~/.local/state/pd-memory/installations/codex/<id>`，与 WorkBuddy 的备份分开，但两个安装器
共用一把安装锁。`--rollback` 要求目标文件自安装后没有变化；信任 hooks 后 Codex 会改写 `config.toml`，
这时回退会整体拒绝并提示改用 `--uninstall`。`--uninstall` 同样先备份，保留二进制、模型配置和记忆数据；
它不删除 `[hooks.state]` 里残留的信任记录（由 Codex 管理，留着无害）。

| 路径 | 内容 |
|---|---|
| `~/.local/bin/pd-memory` | release 二进制（与 WorkBuddy 共用） |
| `~/.local/bin/pd-memory-codex` | MCP、hooks 和手动命令共用的 wrapper |
| `~/.config/pd-memory/codex.json` | Codex 用的 SDK 配置，不含密钥 |
| `~/.config/pd-memory/model.env` | 私有密钥（与 WorkBuddy 共用） |
| `~/.local/share/pd-memory` | 记忆数据，回退和卸载都不删除 |
| `~/.codex/config.toml` | `[mcp_servers.pd-memory]`，其余内容不变 |
| `~/.codex/hooks.json` | 四个 hooks，其余条目不变 |

## 验证范围

```bash
cargo build --locked -p pd-memory-cli
python3 -m unittest discover -s scripts/install/tests -p 'test_*.py' -v
cargo test --locked -p pd-memory-hooks -p pd-memory-cli
```

- Rust：合成 rollout 的解析单测；CLI 集成测试按 Codex 0.154 的 payload（含 `null`）验证 Stop
  只输出 `{"continue":true}`、SessionEnd 只捕获不 flush、新会话首次召回清空积压并注入上下文。
  模型为确定性 mock。
- 安装器：TOML / hooks 合并、幂等、回退、卸载、失败自动回退；本机有 `codex` 时，由真实 Codex CLI
  （0.154.0）解析安装结果，并验证模拟信任后重装的 hook key 和信任状态不变。
- 本机真实数据：用 mock 模型对 8 个真实 Codex rollout 执行 `hook --codex`，每个会话捕获到一问一答，
  没有 AGENTS.md、环境上下文、技能或图片标记混入。

**未验证**：真实 Codex 会话中由 Codex 自己触发 hooks 的完整流程（需要在 `/hooks` 中人工信任，
并消耗模型调用）、Codex App 的 Hooks 设置界面、真实记忆模型下的召回质量、Windows。
