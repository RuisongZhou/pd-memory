# WorkBuddy 安装与集成

pd-memory 通过 stdio MCP 提供七个工具，通过用户级 hooks 捕获对话并注入召回上下文。
不需要部署 HTTP 服务、数据库或常驻调度进程。安装脚本只完成宿主接线，不改变记忆引擎行为。

## 安装

前置条件：macOS、已安装 WorkBuddy、Python 3.9+、Rust stable / Cargo、系统 C 编译工具链。
WorkBuddy 5.8.3 已做本机验收。脚本使用 POSIX shell 和 Python 标准库；Windows 未覆盖。

在仓库根目录执行：

```bash
bash scripts/workbuddy/install.sh --apply --verify-model
```

首次运行依次询问模型名（默认 `qwen-plus`）、API 服务前缀和密钥。密钥使用隐藏输入。
`openai-chat` 会在服务前缀后追加 `/chat/completions`；`anthropic` 使用 Messages API，
通过 `--api-type anthropic --model <name> --base-url <prefix>` 选择。
`openai-responses`、`google` 尚未实现，安装器不接受这些类型。

默认绑定已有的 `~/.workbuddy/workspace`。其他工作目录用：

```bash
bash scripts/workbuddy/install.sh --apply --verify-model \
  --workspace /absolute/path/to/project
```

这是单工作目录安装：用户级 hooks 和 MCP 都固定使用该目录。WorkBuddy 切换空间不会自动切换
pd-memory 的记忆库。安装前应选定要绑定的目录；SDK 当前不包含 General 多项目路由。

安装器执行以下步骤：

1. 读取并验证已有 JSON，解析绝对路径，读取现有或新提供的模型配置。
2. 从当前源码构建 release；有现成二进制时可用 `--binary /absolute/path/to/pd-memory`。
3. 备份将修改的文件，安装二进制、wrapper 和私有模型配置。
4. 合并 GUI/CLI MCP 和 `SessionStart`、`UserPromptSubmit`、`Stop` hooks，保留其他条目。
5. 在临时数据目录验证 MCP 握手、七个工具、工具调用与 SessionStart hook。
6. 指定 `--verify-model` 时，额外验证 Stop 捕获 → 提取 → 搜索 → UserPromptSubmit 注入。

模型验收只发送合成的 Boreal 项目事实，会产生少量 API 调用；测试目录结束后删除。
不传 `--verify-model` 只检查本地链路，输出会明确提示模型连通性未验证。

安装后重启 WorkBuddy，进入「专家·技能·连接器 → 连接器 → 自定义连接器」，
首次点击 pd-memory 的「信任」，确认 **7/7 个工具已启用**。信任记录由 WorkBuddy 管理，
安装器不写审批文件，也不自动重启应用。再次安装同一路径时通常可沿用现有信任。

## 参数与重复安装

```bash
# 默认也是预览：不编译、不写文件、不读入新密钥、不发模型请求
bash scripts/workbuddy/install.sh --dry-run

# 再次运行：复用已有模型、密钥和工作目录，不重复添加 hooks
bash scripts/workbuddy/install.sh --apply --verify-model

# 由密钥管理器预先导出 QWEN_API_KEY，适合无交互环境
bash scripts/workbuddy/install.sh --apply --non-interactive --verify-model \
  --model qwen-plus --base-url "$QWEN_BASE_URL" --api-key-env QWEN_API_KEY

# 或读取只包含 API key 的文件；不要把密钥直接写进命令参数
bash scripts/workbuddy/install.sh --apply --non-interactive \
  --model qwen-plus --base-url "$QWEN_BASE_URL" --key-file /private/path/api-key
```

`QWEN_BASE_URL` 由调用者设置为自己的服务前缀。默认从环境变量 `PD_MEMORY_API_KEY` 读取密钥；
其次复用本机 `model.env`。文件权限设为 `600`，配置目录和备份目录设为 `700`；原配置的备份可能包含旧密钥。
wrapper 固定读取同一份配置和密钥，并清除可能导致分库或误用 mock 的 `PD_MEMORY_*` 配置覆盖变量。
手动命令仍可显式使用 `--data-root` 或 `--workspace`。

| 参数 | 用途 |
|---|---|
| `--apply` | 正式安装；不传时只预览 |
| `--verify-model` | 用临时数据做真实模型验收；与 `--mock` 互斥 |
| `--mcp-target gui / cli / both` | 默认同时接入 GUI 和 CLI；三个 hooks 默认安装 |
| `--home <dir>` | 安装根目录，默认当前用户 home；用于隔离测试 |
| `--workbuddy-home <dir>` | 自定义 WorkBuddy 用户目录，默认 `<home>/.workbuddy` |
| `--workspace <dir>` | 绑定的已有工作目录；重复安装默认沿用原值 |
| `--data-root <dir>` | 记忆数据目录；重复安装默认沿用原值 |
| `--binary <path>` | 使用现有可执行文件，跳过 Cargo 构建 |
| `--non-interactive` | 缺少配置时退出，不询问输入 |
| `--rollback [backup-dir]` | 恢复最近一次或指定安装前的文件 |
| `--mock` | 显式离线演示，不能替代真实记忆模型 |

旧入口 `examples/workbuddy/install_workbuddy.sh` 转发到同一实现。旧脚本的 `--with-hooks` 已不需要，
旧的 `--mcp-target connector` 改为 `gui`；GUI 写入位置已经修正。旧文件旁的 `.bak.*` 不属于新安装器的
回退记录，需要按原备份手动恢复。

## 文件布局与备份回退

| 路径 | 内容 |
|---|---|
| `~/.local/bin/pd-memory` | release 二进制 |
| `~/.local/bin/pd-memory-workbuddy` | MCP、hooks 和手动命令共用的 wrapper |
| `~/.config/pd-memory/config.json` | SDK 配置；不包含 API key |
| `~/.config/pd-memory/model.env` | 私有密钥 |
| `~/.local/share/pd-memory` | 正式记忆数据，备份回退不删除它 |
| `~/.workbuddy/mcp.json` | GUI「自定义连接器」配置；5.8.3 界面实测 |
| `~/.workbuddy/.mcp.json` | CodeBuddy CLI 用户级 MCP 配置 |
| `~/.workbuddy/settings.json` | 用户级 hooks 与原有其他设置 |
| `~/.local/state/pd-memory/installations/<id>` | 本次改动清单、原文件字节与权限 |

`~/.workbuddy/connectors/default/mcp.json` 是市场连接器注册表，安装器不修改它。
GUI 使用自己的严格 MCP 配置，单独修改 CLI 的 `.mcp.json` 不代表 GUI 已接入。

```bash
bash scripts/workbuddy/install.sh --rollback
# 或恢复某次指定安装；从安装输出取得实际目录
bash scripts/workbuddy/install.sh --rollback /absolute/path/to/installations/<id>
```

每次有改动的安装保存一个 manifest；重复执行且内容相同不产生新备份。
安装、协议检查或模型验收失败时自动恢复已修改文件。安装器会阻止并发安装，并在写入前检查文件
是否被 WorkBuddy 或其他进程改动。

回退前会校验全部目标文件和备份摘要；若安装后文件又被修改，整次回退停止，不会覆盖新设置。
此时保留备份，按 manifest 对照合并，或先另存用户修改后再恢复。回退不会删除记忆目录；
首次安装新建的二进制和配置文件会被移除，已有文件恢复原字节与权限。

## MCP 与 hooks 的设计

```text
WorkBuddy 工具调用 → wrapper → pd-memory serve mcp → MemoryEngine
WorkBuddy 生命周期 → wrapper → pd-memory hook → 同一配置、同一工作目录、同一数据目录
```

MCP 提供：`memory_add`、`memory_flush`、`memory_search`、`memory_list`、
`memory_get`、`memory_overview`、`memory_dream`。stdout 只输出 JSON-RPC，诊断走 stderr。

GUI/CLI 的 MCP 条目均指向 wrapper，配置不包含密钥：

```json
{
  "mcpServers": {
    "pd-memory": {
      "type": "stdio",
      "command": "/absolute/path/to/.local/bin/pd-memory-workbuddy",
      "args": ["serve", "mcp"],
      "timeout": 120000
    }
  }
}
```

| 事件 | 行为 | 安装器设置的超时 |
|---|---|---|
| SessionStart | 注入记忆概览，不调用模型 | 20 秒 |
| UserPromptSubmit | 按问题召回，输出 additionalContext | 120 秒 |
| Stop | 从 transcript 捕获对话，再检查到期维护 | 180 秒 |

hook 输入使用 snake_case，输出遵循宿主协议：

```json
{"hook_event_name":"UserPromptSubmit","session_id":"example","cwd":"/project","prompt":"之前的约定是什么？"}
```

```json
{"continue":true,"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"..."}}
```

Stop 使用 `transcript_path`，并以 `last_assistant_message` 补齐可能尚未落盘的最后一条回复。
异常写 stderr，hook 继续宿主任务。因此不能只用 hook 的退出码判断模型是否可用；
`--verify-model` 会断言生成文件和召回内容。

默认索引间隔 30 分钟，或待处理 L0 达到 20 条触发；Dream 默认 60 分钟且要求项目记忆发生变化。
检查发生在宿主事件到达时，没有常驻计时线程。立即生成记忆可调用 `memory_flush` 或执行：

```bash
"$HOME/.local/bin/pd-memory-workbuddy" flush --reason manual
```

## 验收与排查

```bash
# 配置和本地状态
"$HOME/.local/bin/pd-memory-workbuddy" doctor
"$HOME/.local/bin/pd-memory-workbuddy" overview

# 安装器回归测试（不联网，先准备 CLI）
cargo build --locked -p pd-memory-cli
python3 -m unittest discover -s scripts/workbuddy -p 'test_*.py' -v

# WorkBuddy 契约模拟器 + 已安装的真实 CLI；模型请求使用本地 mock
bash examples/workbuddy/run_e2e.sh
# 无 WorkBuddy 环境
bash examples/workbuddy/run_e2e.sh --no-real
```

GUI 验收：在绑定工作目录新建任务，给出一条项目事实；一轮结束后检查 `memory_overview` 的 pending，
调用 `memory_flush`，再从新会话召回该事实。MCP 工具可见、CLI 注入成功与 GUI 会话自动注入是不同的验证项。

| 现象 | 检查 |
|---|---|
| GUI 看不到 MCP | 是否写入 `mcp.json`，是否重启，是否在自定义连接器点击「信任」 |
| 能调用工具但不自动捕获 | 用户级 settings 是否保留三个 hooks；transcript 是否存在 |
| 只有 pending | 还未达到调度条件，主动调用 memory_flush |
| 模型失败但聊天继续 | hooks 为尽力执行；运行 --verify-model，核对服务地址、模型名和密钥 |
| 不同项目混在一起 | 此安装固定单工作目录，不跟随 WorkBuddy 的空间自动切换 |
| 回退被拒绝 | 安装后目标文件有新修改；对照备份合并，避免覆盖新设置 |

当前验证范围及历史 payload、流量与上下文注入证据见
[上游说明](upstream.md)和 [EVIDENCE.md](../examples/workbuddy/EVIDENCE.md)。
