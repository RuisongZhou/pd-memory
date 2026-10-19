# pd-memory

从 [PilotDeck](https://github.com/OpenBMB/PilotDeck) 记忆模块抽出的 Rust SDK，提供 CLI、
MCP stdio、Agent 生命周期 hooks 和 Android C ABI / JNI 接口。当前功能与流程以 PilotDeck
单项目模式为基线；独立仓库不需要安装或运行 PilotDeck。

记忆以 Markdown 文件保存，JSONL/JSON 保存原始对话队列和运行状态。捕获时只入队，
随后由模型提取记忆；召回经过模型 gate、manifest 文件选择和正文读取。Dream 在暂存副本上
整理记忆，提交后可以回滚。无需额外部署数据库、向量库或 HTTP 服务。

## 一键接入 WorkBuddy

也可以把下面这段话直接发给 WorkBuddy Agent，让它执行仓库现有脚本：

> 请安装 https://github.com/RuisongZhou/pd-memory 。先读 README.md 和 docs/workbuddy.md，
> 克隆仓库或复用本地副本，执行已有安装脚本并验收真实模型。沿用本机已有模型配置和密钥，
> 默认绑定 WorkBuddy 工作目录，保留其他 MCP、hooks 和已有记忆，不改代码、不打包插件。
> 缺少模型配置时先告诉我需要填写哪些信息，密钥通过本地终端隐藏输入，不放进聊天或命令参数。

这条路径仍会经过 WorkBuddy 的命令审批；安装后需要重启，首次接入需要在 GUI 中信任 MCP。
具体执行方式和本机验证结果见 [通过 Agent 安装](docs/workbuddy.md#通过-workbuddy-agent-安装)。

先安装 WorkBuddy、Rust stable / Cargo 和 Python 3.9+，然后执行：

```bash
git clone https://github.com/RuisongZhou/pd-memory.git
cd pd-memory
bash scripts/workbuddy/install.sh --apply --verify-model
```

首次运行会询问模型名（默认 `qwen-plus`）、服务前缀地址和 API key；密钥输入不回显。
服务地址使用你的供应商提供的 OpenAI 兼容地址，程序会追加 `/chat/completions`。
脚本从本地源码构建 release，安装命令和配置，合并 GUI/CLI 的 MCP 与四个用户级 hooks，
然后在临时目录验证真实模型的捕获、提取、搜索和上下文注入。验证会产生少量模型调用，
测试事实不会进入正式记忆。已有安装再次运行会复用模型配置和密钥。

收到 WorkBuddy 的 SessionEnd 事件时会主动处理已捕获的积压；新会话首次召回前也会检查并补处理，
因此不依赖 Agent 主动调用 `memory_flush`。本机 GUI 切换、`/clear` 和退出未观察到 SessionEnd，
由新会话首次召回兜底，详见[验收记录](examples/workbuddy/EVIDENCE.md)。
处理需要调用记忆模型，存在积压时首问可能需要等待；失败不阻断聊天，后续提问会重试检查。

默认绑定 `~/.workbuddy/workspace`；绑定其他已有目录时加 `--workspace /absolute/path/to/project`。
运行配置中的密钥保存在 `~/.config/pd-memory/model.env`（权限 `600`），不进入 MCP JSON、
命令行参数或仓库；回退备份可能包含旧密钥，备份目录权限为 `700`。

安装后重启 WorkBuddy，进入「专家·技能·连接器 → 连接器 → 自定义连接器」，首次点击
pd-memory 的「信任」，确认 **7/7 个工具已启用**。脚本不会代替宿主完成信任确认。
已在 macOS / WorkBuddy 5.8.3 验证；Windows 没有对应的一键脚本。

```bash
# 只预览，不编译、不写文件、不调用模型
bash scripts/workbuddy/install.sh --dry-run

# 恢复最近一次安装前的文件，保留记忆数据
bash scripts/workbuddy/install.sh --rollback
```

重复安装不会重复添加 hooks；原有 MCP、hooks 和其他设置会保留。安装或验收失败时自动恢复
原文件；回退遇到安装后的用户修改会停止，避免覆盖新设置。
无交互安装、离线演示、目录布局及排查见 [安装与集成指南](docs/workbuddy.md)。
以下章节保留手动配置方法。

## 1. 编译与安装

macOS / Linux 需要 Rust stable、Cargo、Git 和系统 C 编译工具链。
本文命令在 macOS、Rust/Cargo 1.99.0 验证；Python 3 用于安装示例和集成测试。

```bash
git clone https://github.com/RuisongZhou/pd-memory.git
cd pd-memory
cargo build --locked --release -p pd-memory-cli

mkdir -p "$HOME/.local/bin"
install -m 755 target/release/pd-memory "$HOME/.local/bin/pd-memory"
"$HOME/.local/bin/pd-memory" --version
```

也可以在仓库根目录执行 `cargo install --locked --path crates/pd-memory-cli`，二进制会安装到
Cargo 的 bin 目录。后文采用 `~/.local/bin/pd-memory`；若改用 Cargo 安装，请相应修改路径。

MCP 服务由 WorkBuddy 拉起，hooks 每次事件启动一个进程，不需要另配后台守护进程。

## 2. 先做离线验证

在仓库根目录运行：

```bash
export PD_MEMORY_PROVIDER=mock
BIN="$HOME/.local/bin/pd-memory"
CFG="$PWD/examples/workbuddy/pd-memory.config.json"

"$BIN" --config "$CFG" add --session demo "Boreal 是整理本地知识库的项目，先实现 Markdown 导入。"
"$BIN" --config "$CFG" add --session demo "以后这个项目的交付文档都使用中文。"
"$BIN" --config "$CFG" flush
"$BIN" --config "$CFG" list
"$BIN" --config "$CFG" search "Boreal 的项目目标和文档语言是什么？"
"$BIN" --config "$CFG" doctor
unset PD_MEMORY_PROVIDER
```

示例数据写在 `examples/workbuddy/.run/data/`，不会混入生产数据。mock 是确定性的测试模型，
只能验证链路，不能作为真实记忆提取服务。

## 3. 配置真实模型和数据目录

以下示例固定接入一个项目。先创建私有配置目录：

```bash
mkdir -p "$HOME/.config/pd-memory" "$HOME/.local/share/pd-memory"
chmod 700 "$HOME/.config/pd-memory"
```

新建 `~/.config/pd-memory/config.json`，把路径、模型和服务地址替换为实际值。
**JSON 中使用绝对路径，不要写 `~` 或 `$HOME`。** `base_url` 是服务前缀，程序会追加
`/chat/completions`，不要在这里重复填写该后缀。

```json
{
  "data_root": "/absolute/path/to/pd-memory-data",
  "workspace_root": "/absolute/path/to/your-project",
  "workspace_mode": "single",
  "capture_strategy": "last_turn",
  "include_assistant": true,
  "heartbeat_batch_size": 30,
  "control_plane": "json",
  "model": {
    "api_type": "openai-chat",
    "model": "your-model-name",
    "base_url": "https://your-model-service.example/v1",
    "api_key_env": "PD_MEMORY_API_KEY",
    "max_retries": 2
  },
  "default_settings": {
    "reasoning_mode": "answer_first",
    "auto_index_interval_minutes": 30,
    "auto_dream_interval_minutes": 60
  },
  "retrieval": {
    "manifest_limit": 200,
    "selection_limit": 5,
    "cache_ttl_ms": 30000
  },
  "redaction": { "enabled": true }
}
```

`api_type` 还支持 `anthropic`；`openai-responses` 和 `google` 目前没有实现。
记忆模块使用独立模型配置，不会自动读取 WorkBuddy 当前所选模型的账号或凭据。

桌面应用不一定继承终端中 `export` 的环境变量。为保证 MCP 和 hooks 使用同一份配置，
新建 `~/.config/pd-memory/model.env`：

```sh
PD_MEMORY_API_KEY='replace-with-your-model-api-key'
```

然后执行 `chmod 600 "$HOME/.config/pd-memory/model.env"`。该文件不放进 Git 仓库。
新建 `~/.local/bin/pd-memory-workbuddy`，内容如下：

```sh
#!/bin/sh
set -eu
set -a
. "$HOME/.config/pd-memory/model.env"
set +a
unset PD_MEMORY_PROVIDER
exec "$HOME/.local/bin/pd-memory" --config "$HOME/.config/pd-memory/config.json" "$@"
```

```bash
chmod 755 "$HOME/.local/bin/pd-memory-workbuddy"
"$HOME/.local/bin/pd-memory-workbuddy" doctor
```

`doctor` 用于检查路径和 provider 配置；真实模型连通性还应通过一次 `add → flush → search`
验证。所有手动命令也可使用该 wrapper，例如 `pd-memory-workbuddy overview`。

固定 `workspace_root` 能保证 WorkBuddy 的 MCP 进程、hooks 与手动 CLI 命令定位到同一项目。
需要多个项目时分别准备配置和 wrapper；共用同一个 `data_root` 时，项目记忆隔离、用户身份
记忆全局共享。不要把示例中的固定 `workspace_id=workbuddy-demo` 用于全部真实项目。

## 4. 接入 WorkBuddy

完整接入需要 **MCP + hooks**：MCP 提供可主动调用的工具，hooks 负责自动捕获对话和召回注入。
只安装 MCP 不会自动保存每轮聊天。

### 4.1 添加 MCP 服务

在 WorkBuddy 的「连接器 → 自定义连接器 → 添加 MCP」中添加本地 stdio 服务：

| 字段 | 值 |
|---|---|
| 名称 | `pd-memory` |
| command | `~/.local/bin/pd-memory-workbuddy` 的实际绝对路径 |
| args | `serve`, `mcp` |
| 环境变量 | wrapper 已加载模型凭据，不必另填 mock 配置 |

等价的 `mcpServers` 片段如下。将 `/Users/yourname` 改为你的真实 home 路径：

```json
{
  "mcpServers": {
    "pd-memory": {
      "type": "stdio",
      "command": "/Users/yourname/.local/bin/pd-memory-workbuddy",
      "args": ["serve", "mcp"],
      "timeout": 120000,
      "disabled": false
    }
  }
}
```

本机验证过的 WorkBuddy 版本使用以下配置位置：

- GUI 自定义 MCP：`~/.workbuddy/mcp.json`（5.8.3 界面显示并验证）。
- `~/.workbuddy/connectors/default/mcp.json` 是市场连接器注册表，本安装流程不修改它。
- CodeBuddy CLI 用户级 MCP：`~/.workbuddy/.mcp.json`，原生 CLI 片段见
  [workbuddy.cli.mcp.json](examples/workbuddy/workbuddy.cli.mcp.json)。
- GUI 使用自己的严格 MCP 配置，单独改 CLI 的 `.mcp.json` 不代表 GUI 已接入。
  首次连接需在自定义连接器中点击「信任」；本机已验证重启后仍显示 7/7 工具。

服务提供 7 个工具：`memory_add`、`memory_flush`、`memory_search`、`memory_list`、
`memory_get`、`memory_overview`、`memory_dream`。

### 4.2 添加用户级 hooks

把下面的 `hooks` 合并到 `~/.workbuddy/settings.json`，保留已有设置和其他 hooks。
替换所有 `/Users/yourname`。命令中的引号用于支持带空格的可执行路径。

```json
{
  "hooks": {
    "SessionStart": [{ "hooks": [{
      "type": "command",
      "command": "\"/Users/yourname/.local/bin/pd-memory-workbuddy\" hook",
      "timeout": 20
    }] }],
    "UserPromptSubmit": [{ "hooks": [{
      "type": "command",
      "command": "\"/Users/yourname/.local/bin/pd-memory-workbuddy\" hook",
      "timeout": 60
    }] }],
    "Stop": [{ "hooks": [{
      "type": "command",
      "command": "\"/Users/yourname/.local/bin/pd-memory-workbuddy\" hook",
      "timeout": 120
    }] }]
  }
}
```

使用用户级设置是因为已核对的 GUI 启动参数只加载 user settings，项目目录下的 hooks
不一定被读取。保存后重启 WorkBuddy 并打开配置中的项目目录。

- `UserPromptSubmit`：按当前问题召回，注入本轮上下文。
- `Stop`：从 transcript 捕获本轮对话，再检查是否到期运行索引或 Dream。
- 默认索引间隔为 30 分钟，或待处理 L0 记录达到 20 条时触发；Dream 默认间隔为 60 分钟，
  并要求项目记忆发生变化。没有常驻计时线程，检查在宿主事件发生时执行。
- 因此刚聊完一条不一定立即生成 Markdown；验证时可主动调用 `memory_flush`，
  或执行 `"$HOME/.local/bin/pd-memory-workbuddy" flush --reason manual`。
  hooks 的 timeout 可按模型时延调整，大量积压可用 CLI/MCP 手动处理。

### 4.3 验收与排查

1. 在连接器中确认服务启动，能列出上述 7 个工具。
2. 在目标项目的新会话中说明一条项目事实或规则，然后结束一轮对话。
3. 调用 `memory_overview` 检查 pending，再调用 `memory_flush` 和 `memory_list` 检查文件。
4. 新建会话询问该事实，检查 `memory_search` 返回内容及 `UserPromptSubmit` 召回注入。

常见问题：

| 现象 | 检查项 |
|---|---|
| 有 MCP 工具，但对话没有自动捕获 | hooks 是否放在用户级 settings，是否有 transcript、是否重启宿主 |
| 只看到 pending，没有 Markdown | 尚未达到调度条件；手动执行 flush |
| 手动 CLI 能用，WorkBuddy 失败 | command 是否为绝对路径；wrapper 是否可执行；是否能加载 model.env |
| MCP 和 hooks 看到不同记忆 | 两者是否使用同一 wrapper、config、data_root 和 workspace_root |
| 返回演示内容 | 是否仍设置 `PD_MEMORY_PROVIDER=mock`；不要用离线示例安装结果接生产模型 |
| 缺少真实记忆但 hooks 没有阻止聊天 | hooks 设计为出错后继续；查看 stderr，执行 doctor 和手动 flush 定位 |

生产安装入口为 `scripts/workbuddy/install.sh`；
`examples/workbuddy/install_workbuddy.sh` 也转发到同一实现。默认预览，`--apply` 才写入。
只有显式传入 `--mock` 才安装演示模型：

```bash
bash scripts/workbuddy/install.sh --dry-run
```

不连接真实模型的 WorkBuddy 契约测试：

```bash
bash examples/workbuddy/run_e2e.sh --no-real
```

详细载荷、安装回退和历史验证范围见 [WorkBuddy 集成指南](docs/workbuddy.md)。
WorkBuddy 5.8.3 的 56 项模拟器/真实 CLI 检查、GUI 7/7 工具及独立 Qwen 链路已验证；
GUI 会话中的自动注入仍需单独验收，见 [验证说明](docs/upstream.md)。

## 5. 数据与日常操作

```text
<data_root>/
├── workspaces/<workspace-key>/
│   ├── memory/MEMORY.md          # 派生清单
│   ├── memory/project.meta.md
│   ├── memory/Project/*.md
│   ├── memory/Feedback/*.md
│   ├── control/l0_sessions.jsonl
│   ├── control/pipeline_state.json
│   └── last_dream/                # 最近一次可回滚快照
└── global/                       # 全局用户身份笔记与画像
```

备份整个 `data_root` 可保留记忆、队列和回滚状态；`export` / `import` 用于逻辑迁移。
不同进程共享同一数据目录时，请串行执行 flush、Dream 和导入等维护操作。

```bash
"$HOME/.local/bin/pd-memory-workbuddy" overview
"$HOME/.local/bin/pd-memory-workbuddy" list
"$HOME/.local/bin/pd-memory-workbuddy" search "项目约束是什么？"
"$HOME/.local/bin/pd-memory-workbuddy" dream
"$HOME/.local/bin/pd-memory-workbuddy" rollback
"$HOME/.local/bin/pd-memory-workbuddy" --help
```

## 6. 作为 SDK 使用

- Rust：引入 `crates/pd-memory-core`，通过 `MemoryEngine::open` 使用文件存储，或通过
  `MemoryEngineOptions` 注入 model/store/clock 等端口。默认引擎为同步接口，不依赖 async runtime。
- Android：[构建与 Kotlin 接入](android/README.md)。C ABI / JNI 位于 `crates/pd-memory-ffi`；
  NDK 构建执行 `bash scripts/android-build.sh`，无 NDK 编译检查执行 `bash scripts/android-check.sh`。
  三个 Android target 已通过编译，尚未完成真机测试。

## 7. 测试与上游对齐

在独立仓库根目录执行：

```bash
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets
cargo test --locked --workspace
python3 -m unittest discover -s examples/parity -p 'test_*.py' -v
```

已验证：307 个 Rust 测试、12 个比较器测试，17/17 个 TypeScript ↔ Rust 场景通过，
DEV=0、DIFF=0。差分需要 Node 22.13+（建议使用上游声明的 22.x，需含 `node:sqlite`）、Python 3，以及已构建的上游模块：

```bash
PILOTDECK_ROOT=/absolute/path/to/PilotDeck bash examples/parity/run_parity.sh
```

准备基线的方法和范围见 [差分报告](docs/parity.md)；精确上游提交记录在
[upstream.json](upstream.json)。不支持 General 多项目模式和向量检索。

## 许可证与来源

[AGPL-3.0-only](LICENSE)。保留 PilotDeck 的许可证，来源和第三方说明见
[NOTICE.md](NOTICE.md)。此仓库不属于 WorkBuddy、CodeBuddy 或其厂商的官方产品。
