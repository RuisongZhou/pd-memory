# DeepSeek Harness 接入

pd-memory 目前以 **L0（MCP 工具）** 接入 DeepSeek Harness（`dsh`）：通过 stdio MCP 提供七个记忆工具，由模型主动调用，
没有自动捕获和自动召回，原因见[为什么还没有自动捕获](#为什么还没有自动捕获)。等级定义和接入方法见
[宿主接入范式](playbook.md)，各宿主对照见[接入宿主总览](README.md)。

验证基线：DeepSeek Harness 桌面版 0.2.1-rc.2（macOS arm64，内置 `@deepseek-ai/dsh-mcp-client` 0.2.1-rc.2、
`@modelcontextprotocol/core` 2.2.0），对照源码 `deepseek-ai/deepseek-harness` master `687f282`（2026-10-17）。
dsh 仍是开发者预览版，升级后要从 V3 开始重验；重新运行安装器就会自动重做 V2、V3。

## 安装

前置条件：macOS 或 Linux、Python 3.9+、Rust stable / Cargo，以及 DeepSeek Harness（桌面版或 `dsh` CLI）。

```bash
bash scripts/install.sh --host deepseek-harness --apply --verify-model --workspace /absolute/path/to/project
```

不传 `--apply` 时只预览。模型名、服务前缀、密钥的询问方式，以及 `--api-type`、`--model`、`--base-url`、
`--api-key-env`、`--key-file`、`--non-interactive`、`--mock`、`--binary`、`--data-root`、`--home` 等参数，
都与 [WorkBuddy 安装器](workbuddy.md#参数与重复安装)相同。本宿主另有三个参数：

- `--dsh-home`：dsh 的数据目录，默认 `<home>/.dsh`。安装器不读取 `DSH_HOME` 环境变量。
- `--dsh-binary`：用来校验配置的 dsh CLI。默认先找 PATH 上的 `dsh`，再找桌面版自带的那个。
- `--skip-dsh-check`：不调用 dsh 校验配置。

`--workspace` 指定这份记忆绑定的项目目录，首次安装必填，之后沿用。这是单工作目录安装：dsh 里所有会话，
不论在哪个工作区，都读写同一份项目记忆。已经装过 WorkBuddy 或 Codex 时，会复用它们的模型配置、密钥和数据目录，
但**不会**继承它们的工作目录；想和它们共享记忆，就传同一个 `--workspace`。

安装器依次执行：

1. 读取现有的 `~/.dsh/cordis.patch.yml` 和 pd-memory 配置，确认补丁文件是 dsh 能接受的顶层 YAML 列表。
2. 构建 release（或使用 `--binary` 指定的二进制）；备份将要修改的文件；写入二进制、`pd-memory-dsh` wrapper、
   `~/.config/pd-memory/dsh.json`。
3. 在 `cordis.patch.yml` 中写入（或原地替换）带标记的 MCP 配置块。块外的字节保持不变。
4. 在临时数据目录里验证：MCP 握手、七个工具、一次工具调用；pd-memory 对 dsh 的 `server/discover` 探测立即返回 `-32601`。
5. 在临时 `DSH_HOME` 里，用 dsh 自己的 `--dump-config` 分别合成改动前后的补丁：新文件必须能被加载、恰好包含一行
   pd-memory 配置且命令路径与写入的一致，并且不能比改动前多出警告。
6. 指定 `--verify-model` 时，在临时数据目录里通过 MCP 走一遍 `memory_add` → `memory_flush` → `memory_search`，
   使用合成事实，和 dsh 里模型的调用路径一致。

任何一步失败都会自动恢复已修改的文件。安装后**重启 DeepSeek Harness，新建会话**，工具名形如
`mcp__pd-memory__memory_search`。不需要人工信任。

### 让 dsh 的 Agent 代为安装

也可以把下面这段话发给 DeepSeek Harness 的 Agent。把路径换成你的项目：

> 请安装 https://github.com/RuisongZhou/pd-memory 。先读 README.md 和 docs/hosts/deepseek-harness.md，
> 克隆仓库或复用本地副本，在仓库根目录执行
> `bash scripts/install.sh --host deepseek-harness --apply --verify-model --non-interactive --workspace /absolute/path/to/project`。
> 沿用本机已有的模型配置和密钥，不改代码。缺少模型配置时，先告诉我需要填写哪些信息；
> 密钥由我在本地终端通过隐藏输入提供，不要放进聊天或命令参数。

## 写入的配置

`~/.dsh/cordis.patch.yml` 是 dsh 的 home 级补丁层，对所有 profile 生效（包括桌面版），优先级高于各 profile 自己的
`cordis.patch.yml`。安装器在里面维护这一块：

```yaml
# >>> pd-memory: managed by scripts/install.sh --host deepseek-harness >>>
- insert:
    - id: memory-pd-memory
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        serverName: pd-memory
        transport: stdio
        command: "/Users/me/.local/bin/pd-memory-dsh"
        args: [serve, mcp]
        toolCallTimeoutMs: 300000
# <<< pd-memory <<<
```

- `command` 指向 wrapper。dsh 启动 MCP 子进程前会清除凭据形环境变量和 `DSH_*`，桌面应用也不继承终端环境，
  所以密钥由 wrapper 用绝对路径从 `model.env` 加载。
- `toolCallTimeoutMs` 默认只有 60 秒，不够 `memory_flush` 和 `memory_dream` 调用模型，所以设成 300 秒。
- 如果文件里已经有按旧版文档手写的那一行（`id: memory-pd-memory`，并且单独占一个 `- insert:` 项），
  安装器会原地接管它。手写行和其他配置混在同一个 `- insert:` 项里、补丁文件不是顶层 YAML 列表，
  或者另有一行也叫 `pd-memory` 的 MCP 服务时，安装器都会拒绝安装，并且不写任何文件。
- 不要改桌面版 profile 目录（`~/.dsh/profiles/desktop/`）下的 `cordis.patch.yml`，那里存放的是桌面应用自己的设置。
  安装器发现 profile 层也定义了同名服务时会提示。

## 使用

- **写入分两步。** `memory_add` 只把内容放进队列，`memory_flush` 才会把它提取成 Markdown 记忆，之后
  `memory_search` 才能检索到。只接 MCP 时没有 hooks，不会自动 flush。让模型记住内容时，可以请它顺带调用
  `memory_flush`，也可以在终端执行 `~/.local/bin/pd-memory-dsh flush --reason manual`。flush、检索、Dream
  都要调用记忆模型，可能需要等待几十秒。
- **可选的模型指令。** dsh 官方指南建议在模型指令里加一句：「When the user asks you to remember something, call a
  memory write tool. When historical information may be relevant, search memory and use relevant results.」
  对 pd-memory，可以再补一句「写入后调用 memory_flush」。

## 为什么还没有自动捕获

| 途径 | 能做到 | 缺口 |
|---|---|---|
| dsh 自带的 `dsh-hooks-claude-code` / `dsh-hooks-codex` 命令 hooks 桥 | L1：UserPromptSubmit 返回的 `additionalContext` 会被注入；SessionStart 也可以，但它在后台运行，可能赶不上第一次请求 | 到不了 L2：`transcript_path` 恒为空（会话日志是 zstd 压缩的，且不暴露路径）；`Stop` 不带 `last_assistant_message`；不发 `SessionEnd` / `PreCompact`。pd-memory 的 Stop hook 因此没有内容可捕获 |
| Cordis 原生插件桥（规划中，`integrations/deepseek-harness/`） | L2 | 待实现，事件映射如下 |

插件桥的事件映射：

- `agent/created`（serial）→ Start；
- `agent/pre-step`（waterfall，只取用户本人发的消息）→ Recall，注入的上下文追加到返回的消息列表；
- `turn/end` → 插件把本轮消息写成 Claude 格式的 JSONL 后触发 Capture；
- `agent/disposed` → End。它是 emit，宿主不会等待，所以按「只捕获」处理；
- `compaction/start` → Compact。

通过 hooks 桥做 L1 需要一个宿主 id 为 `deepseek-harness` 的适配器（`has_transcript: false`、没有 End），目前未实现，也未验证。

## 回退与卸载

```bash
bash scripts/install.sh --host deepseek-harness --rollback    # 按字节恢复安装前的文件
bash scripts/install.sh --host deepseek-harness --uninstall   # 只移除 pd-memory 配置块
```

备份位于 `~/.local/state/pd-memory/installations/deepseek-harness/<id>`。`--rollback` 要求目标文件自安装后没有变化；
如果你或 dsh 改过 `cordis.patch.yml`，回退会整体拒绝，并提示改用 `--uninstall`。`--uninstall` 同样先备份，
只删除配置块，保留二进制、模型配置和记忆数据。删除后文件里如果没有剩下任何条目，会写成 `[]`：dsh 不接受
不是 YAML 列表的补丁文件，空文件或只有注释的文件都会让它无法启动。

| 路径 | 内容 |
|---|---|
| `~/.local/bin/pd-memory` | release 二进制（与其他宿主共用） |
| `~/.local/bin/pd-memory-dsh` | MCP 和手动命令共用的 wrapper |
| `~/.config/pd-memory/dsh.json` | DeepSeek Harness 用的 SDK 配置，不含密钥 |
| `~/.config/pd-memory/model.env` | 私有密钥（与其他宿主共用） |
| `~/.local/share/pd-memory` | 记忆数据，回退和卸载都不删除 |
| `~/.dsh/cordis.patch.yml` | pd-memory 配置块，其余内容不变 |

## 验证范围

2026-10-20 在 macOS（arm64）上验证，使用桌面版 0.2.1-rc.2 自带的运行时：

| 级别 | 结果 |
|---|---|
| V1 离线契约 | 安装器单测：配置块的写入、原地替换、接管手写行、拒绝无法处理的文件、往返后字节一致、CRLF、回退、卸载；读回 dsh 的 YAML 输出（plain、引号、`>-` 折行）。未注册 hook 适配器 |
| V2 本地链路 | 通过，安装器每次都会检查：MCP 握手、七个工具、工具调用、`server/discover` 立即得到 `-32601` |
| V3 宿主读懂配置 | 通过，安装器每次都会检查：dsh 自己的 `--dump-config` 合成改动前后的补丁，确认配置行与命令路径，且不新增警告 |
| V4 宿主运行时连通 | 通过（一次性手动验证）：在临时 `DSH_HOME` 中启动 web profile，旁路记录到 `server/discover` → `-32601` → 新进程 `initialize`（2025-11-25）→ `tools/list` 返回七个工具；探针插件从 `ctx.tools.schemas()` 读到七个 `mcp__pd-memory__*` 工具 |
| V5 真实模型 | 通过：本机执行 `--verify-model`，Qwen `qwen-plus` 经 MCP 完成 `memory_add` → `memory_flush` → `memory_search`，召回了合成事实 |
| V6 真实会话 | 未验证：没有确认桌面版中模型实际调用工具，以及写入后新会话能召回 |

V4 的取证方法：dsh 的 web 和桌面 profile 没有挂载日志输出。做反向对照时，把 `command` 改成不存在的路径，
终端同样没有任何输出，所以「没看到报错」不能当作成功的证据。正面证据有两个来源：一个转发脚本，记录 dsh
与 pd-memory 之间的 JSON-RPC；一个只用于测试的插件，放在临时 `DSH_HOME` 下，补丁里写
`name: ./pd-memory-probe.mjs` 挂载：

```js
import { writeFileSync } from 'node:fs'

export const name = 'pd-memory-probe'
export const inject = ['tools']

export function apply(ctx) {
  const timer = setInterval(() => {
    const ours = ctx.tools.schemas().filter(schema => schema.name.startsWith('mcp__pd-memory__'))
    if (ours.length === 0) return
    clearInterval(timer)
    writeFileSync(new URL('./probe-result.json', import.meta.url), JSON.stringify(ours.map(s => s.name)))
  }, 500)
  ctx.effect(() => () => clearInterval(timer), 'pd-memory-probe: stop polling')
}
```

启动命令：`DSH_HOME=<临时目录> DSH_TELEMETRY_MODE=DISABLED "<应用>/Contents/Resources/runtime/cli/bin/dsh" --profile web --port <空闲端口> --no-open`。

## 隐私

dsh 使用 DeepSeek 官方 API 时，默认上传会话日志，其中包括工具调用的结果。也就是说，`memory_search`、
`memory_list`、`memory_get` 返回的记忆内容会随日志一起上传。关闭方法：在 Web / 桌面版的
Settings → General 中关掉「Upload Session Log when using the official model API」，或者把
`session-log-deepseek` 插件的 `enabled` 设为 `false`。

## 后续

Cordis 插件桥（L2）：宿主 id 为 `deepseek-harness`，hook 命令为 `hook --host deepseek-harness`。
安装器在现有模块上增加插件文件的放置和注册即可，一键安装的入口不变。
