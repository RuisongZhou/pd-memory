# 宿主接入范式

本文把 WorkBuddy、Codex、DeepSeek Harness 三次接入的经验整理成固定流程：**画像 → 定级 → 实现 → 举证**。
现有架构和各宿主的能力对照见[接入宿主总览](README.md)。本文回答的是：接下一个宿主时要查什么、怎么选、
改哪些文件、验证到什么程度才算完成。

## 1. 模型

pd-memory 只有一个记忆引擎，对宿主暴露两种协议：MCP 工具由模型主动调用，hook 命令由宿主在生命周期事件上调用。
宿主之间的差异只允许出现在下面四处，其余代码与宿主无关：

| 层 | 位置 | 内容 | 何时需要 |
|---|---|---|---|
| 安装器模块 | `scripts/install/hosts/<id>.py` | 配置路径、MCP 与 hooks 合并、宿主侧校验、人工步骤提示 | 所有宿主 |
| 运行时适配器 | `crates/pd-memory-hooks/src/hosts/<id>.rs`；新格式另加 `transcript/<格式>.rs`；夹具放 `tests/fixtures/<id>/` | 事件映射、`HostCaps`、输出 schema、transcript 读取 | L1 起 |
| 插件桥 | `integrations/<id>/` | 运行在宿主进程内的薄插件，把宿主事件转成 hook JSON，再调用 `pd-memory hook --host <id>` | 宿主没有可用的命令 hooks 时 |
| 文档 | `docs/hosts/<id>.md`、[能力对照](README.md#能力对照) | 安装、配置、差异、验证范围、限制 | 所有宿主 |

```text
宿主 ─MCP────────────────────> wrapper ─> pd-memory serve mcp ────────> MemoryEngine
宿主 ─命令 hooks─────────────> wrapper ─> pd-memory hook --host <id> ─> 适配器 ─> 策略层 ─> MemoryEngine
宿主 ─插件 API─> 插件桥 ─────> wrapper ─> pd-memory hook --host <id> ─> （同上）
```

不变量（任何宿主都不能改）：

- 引擎行为、七个 MCP 工具及其语义，以及规范事件（Start / Recall / Capture / End / Compact）的通用行为；
- 同一工作目录只有一把 hook 锁 `control/hooks.lock`，所有宿主共用；
- 已发布的 hook 命令和宿主 id 不改名。Codex 按命令哈希记录信任，会话标记目录 `control/<id>/` 由 id 决定；
- hook 无论出什么错，都输出 `{"continue":true}` 且退出码为 0，不阻断宿主；
- 密钥只放在 `~/.config/pd-memory/model.env`（权限 600），由 wrapper 加载。宿主配置文件和命令参数里不出现密钥；
- 插件桥只做事件转换，不实现记忆逻辑。

## 2. 接入等级

等级说的是用户得到什么，机制说的是怎么接上，两者独立。

| 等级 | 用户得到 | 宿主必须提供 |
|---|---|---|
| **L0 工具** | 模型可以调用七个记忆工具；写入后要 `memory_flush` 才能检索到 | stdio MCP |
| **L1 自动召回** | 会话开始时注入记忆概览；每次提问前自动注入相关记忆 | 提问事件，且能返回注入的上下文 |
| **L2 自动捕获** | 每轮结束自动保存对话；会话结束、上下文压缩前处理积压；新会话首次提问前补处理 | 回合结束事件，且能拿到本轮内容（可读的 transcript 或内联的最终回复） |

| 宿主 | 等级 | 机制 |
|---|---|---|
| WorkBuddy / CodeBuddy | L2 | MCP + Claude Code 方言的命令 hooks |
| Codex CLI / App | L2 | MCP + Claude Code 方言的命令 hooks（输出 schema 严格） |
| DeepSeek Harness | L0 | MCP（YAML 补丁行）。它自带的命令 hooks 桥最多到 L1，L2 需要 Cordis 插件桥 |

等级只说明实现到了哪一步，验证到了哪一级另外标注（见第 6 节和[能力对照](README.md#能力对照)）。
例如 Codex 已实现 L2，但有据可查的验证只到 V3。

做法：**先交付 L0**。所有支持 stdio MCP 的宿主都能用，代价最小。之后逐级往上，每升一级都单独验证、单独写进文档。

**按 payload 的实际内容定级，不要看事件名。** dsh 的 hooks 桥也有 `Stop` 事件，但 payload 里既没有 transcript
也没有最终回复，所以到不了 L2。

## 3. 宿主画像

写代码前先回答下面这些问题。答案写进宿主文档，并作为能力对照表里的一列。每个答案都要注明来源
（官方文档、源码的某个提交，或本机实测）和宿主版本。

**配置**

| 问题 | 决定什么 | 已遇到的情况 |
|---|---|---|
| 目标界面（GUI / CLI）实际读取哪个文件？什么格式、什么作用域？ | 写哪些文件 | WorkBuddy 的 GUI 读 `mcp.json`、CLI 读 `.mcp.json`；GUI 只加载用户级 `settings.json` 里的 hooks |
| 宿主有没有自带的解析或加载入口？能否指向一个隔离的 home？ | V3 怎么做 | `CODEX_HOME` + `codex mcp get` / `app-server hooks/list`；`DSH_HOME` + `dsh --dump-config` |
| 和其他配置的共存规则是什么？ | 合并还是拒绝 | Codex 不允许 `config.toml` 的 `[hooks]` 与 `hooks.json` 并存，安装器检测到就拒绝 |

**MCP**

| 问题 | 决定什么 | 已遇到的情况 |
|---|---|---|
| 子进程的环境从哪来？是否继承终端？会不会被清洗？ | wrapper 怎么写 | 桌面应用不继承终端环境；dsh 启动子进程前会清除凭据形变量和 `DSH_*` |
| 协议版本怎么协商？ | 服务端兼容性 | dsh 的 MCP SDK 先发 `server/discover` 探测新协议，失败再走 `initialize` |
| 工具调用超时的默认值？ | 配置值 | dsh 默认 60 秒；`memory_flush` / `memory_dream` 需要约 300 秒 |
| 需要人工信任或审批吗？ | 安装后的提示 | WorkBuddy GUI 要在自定义连接器里点「信任」 |
| 模型可见的工具会不会被预设或白名单过滤？ | 工具是否真能被调用 | dsh 的 agent preset 不过滤全局工具（读源码确认） |

**Hooks**

| 问题 | 决定什么 | 已遇到的情况 |
|---|---|---|
| 支持哪些事件？叫什么名字？ | `normalize`、能到哪一级 | 三家都用 Claude Code 事件名；dsh 的桥没有 `SessionEnd`、`PreCompact` |
| payload 里有哪些字段？哪些可能为空？ | `has_transcript`、兜底路径 | Codex 的 `transcript_path` 可能是 `null`；dsh 的桥里恒为空字符串 |
| 哪些事件允许输出注入的上下文？多余字段会被拒绝吗？ | `inject_on`、`render` | Codex 输出 schema 为 `additionalProperties: false`，只有 SessionStart / UserPromptSubmit 有 `hookSpecificOutput` |
| 每个事件的超时上限是多少？ | `end_budget_ms`、安装器写的超时 | Codex 的 SessionEnd 默认 1 秒、上限 3 秒 |
| 宿主怎么记录对 hook 的信任？ | 是否原位替换、命令能否改 | Codex 按「事件 + 位置」和配置哈希记录信任 |
| 结束事件能可靠送达吗？ | 是否只能依赖首次召回屏障 | WorkBuddy GUI 切换会话、`/clear`、退出时都没有观察到 SessionEnd |

**Transcript**

| 问题 | 决定什么 | 已遇到的情况 |
|---|---|---|
| 在哪、什么格式、多大？ | 读取器 | Codex rollout 能到几十 MB，超过 1 MiB 就从尾部分块读 |
| 写入时间会不会晚于回合结束事件？ | 内联兜底 | WorkBuddy 的最后一条回复在 Stop 之后才落盘，用 `last_assistant_message` 补齐 |
| 宿主往用户消息里注入了什么？ | 过滤规则 | WorkBuddy 的 `<system-reminder>…<user_query>`；Codex 的 AGENTS.md、`<environment_context>` 等标签 |
| hook 进程能读到它吗？ | 命令 hooks 能否到 L2 | dsh 的会话日志是 zstd 压缩的，也不暴露路径 |

**插件 API（宿主没有可用 hooks 时）**

| 问题 | 决定什么 | 已遇到的情况 |
|---|---|---|
| 会话开始、提问、回合结束、会话结束、压缩分别对应哪个扩展点？ | 桥的事件映射 | dsh：`agent/created`、`agent/pre-step`、`turn/end`、`agent/disposed`、`compaction/start` |
| 宿主会等会话结束的监听者跑完吗？ | 桥的 `end_budget_ms` | dsh 的 `agent/disposed` 是 emit，不等待，所以按「只捕获」处理 |
| 怎么注入上下文？怎么拿到本轮消息？ | 桥的实现 | dsh：在 `agent/pre-step` 的返回值里追加消息；从 `session/event` 取消息 |
| 本地插件怎么加载？ | 安装方式 | dsh 的补丁行可以用相对路径引用补丁文件旁边的插件 |

**可观测性与隐私**

| 问题 | 决定什么 | 已遇到的情况 |
|---|---|---|
| 宿主日志写在哪？配置出错会显示吗？ | V4 怎么取证 | dsh 的 web / 桌面 profile 不输出日志，命令写错也没有任何提示 |
| 宿主会上传会话数据吗？ | 文档里的隐私说明 | dsh 使用官方 API 时默认上传会话日志 |

## 4. 决策规则

| 画像结论 | 做法 | 实现位置 |
|---|---|---|
| 支持 stdio MCP | 先交付 L0 | `integration_changes` 只写 MCP 条目，`HOOK_ARGS = None` |
| 子进程不继承环境，或会被清洗 | wrapper 用绝对路径并自行加载 `model.env`；宿主配置里只写 wrapper 路径 | `common.wrapper_script` |
| 工具超时默认值太小 | 设到 300 秒左右 | 各宿主模块 |
| 事件名不是 Claude Code 方言 | 在 `normalize` 里映射 | `hosts/<id>.rs` |
| 输出 schema 严格 | `inject_on` 只列允许注入的事件，其余事件一律输出 `{"continue":true}` | `HostCaps.inject_on`、`render_claude_code` |
| transcript 可读 | `has_transcript: true`；新格式加读取器，并接入逐行自动识别；宿主注入的内容按白名单整块丢弃，不按黑名单删用户内容 | `transcript/<格式>.rs` |
| transcript 可能晚于事件落盘 | 末条是还没回复的用户消息时，用内联的最终回复补上 | `events.rs` 的 `needs_synthetic_assistant_reply` |
| 没有 transcript，但有内联的最终回复 | `has_transcript: false`，只能捕获助手的回复 | `capture_transcript` 的兜底分支 |
| transcript 和内联回复都没有 | 命令 hooks 最多到 L1；要到 L2 就得做插件桥 | `integrations/<id>/` |
| 会话结束的预算 ≥ 30 秒 | End 时捕获、做维护并清空积压 | `HostCaps.end_can_flush` |
| 预算 < 30 秒，或结束事件不可靠 | End 只捕获，等锁时间不超过预算的三分之一；积压留给新会话的首次召回屏障（所有宿主都开启） | `HostCaps.lock_wait_ms`、`policy.rs` |
| 宿主按位置或哈希记录信任 | hooks 原位替换，内容没变就不写文件；已发布的命令不改 | `common.merge_hooks(in_place=True)` |
| JSON 配置 | 结构化合并，保留其他条目 | `common.merge_mcp_servers`、`common.merge_hooks` |
| TOML 配置 | 按文本编辑，保留注释和顺序；再对比编辑前后的解析结果，只允许我们的表发生变化 | `hosts/codex.py` 的 `merge_config_toml` |
| 标准库没有解析器的格式（如 YAML） | 用带标记的文本块；合成后的结果交给宿主自己的解析器，在隔离的 home 里校验；读回宿主输出时按格式规则解码 | `hosts/deepseek_harness.py` 的 `merge_patch`、`verify_with_dsh` |
| 宿主有自带的解析或加载入口 | 安装器把合并结果复制到临时 home，让宿主自己读一遍 | `hosts/codex.py` 的 `verify_with_codex` |
| 没有命令 hooks，但有插件 API | 插件只负责把事件转成 hook JSON；本轮消息写成 Claude 格式的 JSONL，放在私有临时文件里，作为 `transcript_path` 传给 hook | `integrations/<id>/` |

## 5. 实现清单

**一键安装（所有等级都必须）**

每个宿主都要做到 `bash scripts/install.sh --host <id> --apply --verify-model` 一条命令完成全部配置。用户需要手动做的事，
只能是宿主不允许程序代劳的那些（信任、重启），并由 `post_install` 打印出来。统一约定：

| 约定 | 做法 |
|---|---|
| 参数一致 | `--dry-run`（默认就是预览）、`--apply`、`--verify-model`、`--rollback`、`--non-interactive`、`--workspace`、`--binary` 和模型参数由 `common.arguments` 统一提供，`test_common` 会检查每个宿主都有。`--uninstall` 由宿主模块实现：Codex、DeepSeek Harness 已有，WorkBuddy 暂缺 |
| 复用但不串用 | 复用其他宿主的模型配置、密钥和数据目录；工作目录不继承，首次安装需要 `--workspace`（宿主有固定默认工作区的除外，如 WorkBuddy） |
| 可回退 | 只通过 `common.apply_changes` 写文件：先备份、写入后验证，失败时自动恢复；内容没变的文件不写 |
| 宁可拒绝也不猜 | 宿主配置无法安全合并时报错并说明手动做法，不写任何文件 |
| 隔离验证 | 检查只在临时数据目录和临时宿主 home 中进行；不打印密钥和模型返回的原文 |
| Agent 代装 | 宿主文档提供一段提示词，让宿主自己的 Agent 执行同一条命令（见 [workbuddy.md](workbuddy.md#通过-workbuddy-agent-安装)、[deepseek-harness.md](deepseek-harness.md#让-dsh-的-agent-代为安装)） |

**L0**

- [ ] `scripts/install/hosts/<id>.py`（id 中的 `-` 在文件名里写成 `_`）：`ID`、`LABEL`、`CONFIG_NAME`、`WRAPPER_NAME`、
  `STATE_SUBDIR`（新宿主用 `<id>`）、`HOOK_ARGS = None`、`host_paths`、`managed_files`、`load_config`、`default_workspace`、
  `integration_changes`、`verify`（`common.verify_mcp` 加宿主侧解析校验）、`post_install`（提示人工步骤）、`uninstall`；
  再把 id 加进 `common.HOSTS`。完整的 L0 示例见 `hosts/deepseek_harness.py`。
- [ ] 安装器单测（离线、临时 home）：保留其他条目、重复安装时文件字节不变、回退、卸载、失败时自动回退；
  宿主自带解析器的校验放在 `skipUnless` 用例里，本机装了该宿主时会自动运行。
- [ ] 文档：`docs/hosts/<id>.md`、在能力对照表里加一列、在根 README 的一键安装表里加一行。

**L1 / L2（命令 hooks）**，在 L0 的基础上：

- [ ] 在真实宿主里抓取每个事件的原始 payload 和 transcript，脱敏后作为夹具；先用它们回答第 3 节里 Hooks 和 Transcript 两组问题。
- [ ] `hosts/<id>.rs`：实现 `id`、`label`、`caps`、`normalize`；输出 schema 或 transcript 格式不同时，再覆盖 `render`、`read_transcript`。
  加入 `hosts::all()`，命令统一用 `hook --host <id>`。
- [ ] 遇到新的 transcript 格式时：新建 `transcript/<格式>.rs`，接入自动识别，并配上解析单测。
- [ ] 夹具 `tests/fixtures/<id>/`：`caps.json`、`transcript.jsonl` 和 `transcript.expected.json`、`cases.json`。
  `cases.json` 要覆盖：会话开始、首次提问清积压、后续提问、回合结束、会话结束、未知事件。
- [ ] 安装器：hooks 的合并方式（是否原位替换）、各事件的超时、`HOOK_ARGS`、`REQUIRED_HOOK_FLAG`。
  `--verify-model` 要用宿主格式的合成 transcript 走完整条链路：捕获 → flush → 召回 → 检索 → 注入。

**插件桥**，在 L1 / L2 的基础上：

- [ ] `integrations/<id>/`：只处理主会话，子 agent 的会话是否入记忆另行评估；提问时只取用户本人的消息；
  任何调用失败都放行；遵守宿主对插件的约束（响应取消信号、及时释放资源）。
- [ ] 桥的离线测试：用一个假的 `pd-memory` 可执行文件，断言桥写到 stdin 的 payload，以及它如何处理 stdout。
- [ ] 安装器负责放置插件文件，并在宿主里注册。

## 6. 验证阶梯

| 级别 | 证明什么 | 做法 | 已有实例 |
|---|---|---|---|
| V1 离线契约 | 实现符合我们对契约的理解 | `host_contract` 夹具和安装器单测，用 mock 模型和临时目录 | WorkBuddy、Codex |
| V2 本地链路 | 二进制、wrapper、配置都能用 | 在临时数据目录里做 MCP 握手、列出七个工具、调用工具、跑一次 hook 冒烟 | `common.verify_mcp` |
| V3 宿主读懂配置 | 我们写的文件被宿主按预期解析 | 用宿主自带的解析器读取隔离 home 里的配置副本；能自动化的放进安装器的 `verify`，每次安装都检查 | Codex：`codex mcp get`、`hooks/list`；dsh：改动前后各跑一次 `--dump-config` |
| V4 宿主运行时连通 | 宿主真的拉起了服务、注册了工具或触发了 hook | 在隔离 home 里启动真实运行时，旁路记录 JSON-RPC；必要时用探针读取宿主内部状态 | dsh：`server/discover` → `initialize` → `tools/list`，注册表里有七个工具 |
| V5 真实模型闭环 | 在真实模型下，捕获、提取、召回都成立 | `--verify-model`：合成事实、临时数据目录；走宿主实际会用的那条路径（hooks 或 MCP 工具） | WorkBuddy（hooks）、DeepSeek Harness（MCP：add → flush → search），均为 Qwen `qwen-plus` |
| V6 真实会话 | 用户实际看到的效果 | 按脚本人工操作 GUI / CLI，记录日期、版本和结论 | WorkBuddy 5.8.3 GUI（[验收记录](../../examples/workbuddy/EVIDENCE.md)） |

举证规则：

- 宿主文档逐级写明「已验证 / 未验证」，不能跳级声称。mock 只能证明链路通，不能证明记忆质量。
- 宿主不输出日志时，「没看到报错」不算证据。先做反向对照（故意写错命令），确认失败能被观察到，
  再用旁路记录或探针拿到正面证据。
- 所有验证都用合成数据和临时目录，不在用户真实的数据目录里留下测试记忆。必须用到真实 home 时只读。
- 记录宿主版本，以及对照过的源码提交。宿主升级后从 V3 开始重验；dsh 这类预览版宿主尤其要这样做。

## 7. 踩坑目录

| 现象 | 根因 | 规则 | 来源 |
|---|---|---|---|
| 终端里能用，桌面应用里找不到密钥 | GUI 不继承终端环境 | wrapper 用绝对路径加载 `model.env` | WorkBuddy |
| MCP 子进程拿不到密钥 | 宿主清除了凭据形环境变量 | 同上；宿主配置里不写密钥 | dsh |
| 只改了 CLI 的配置，GUI 没接上 | GUI 和 CLI 读不同的文件 | 先确认目标界面读的是哪个文件 | WorkBuddy |
| 捕获到一条没有回复的问题，提取时被跳过 | transcript 比 Stop 晚落盘 | 用内联的最终回复补齐 | WorkBuddy |
| 记忆里混进了宿主的指令 | 宿主把上下文当成用户消息注入 | 按白名单整块丢弃；宿主新增标签时补进白名单 | WorkBuddy、Codex |
| 宿主拒绝 hook 的输出 | 输出 schema 禁止多余字段 | 按 `inject_on` 渲染输出 | Codex |
| 会话结束的 hook 被杀掉 | 结束事件只给 1～3 秒 | 只捕获；等锁时间受预算约束；积压交给首次召回屏障 | Codex |
| 关掉会话后积压一直没处理 | 结束事件没有送达 | 首次召回屏障始终开启 | WorkBuddy |
| 重装后 hooks 要重新信任 | 信任按位置和哈希记录 | 原位替换，命令字符串保持不变 | Codex |
| 两套 hooks 配置同时存在，宿主报错 | 宿主不允许两者并存 | 检测到就拒绝安装，不自动迁移 | Codex |
| 启动时卡住约 60 秒 | 服务端不回应新协议的探测请求 | 遇到未知方法立即返回 `-32601` | dsh |
| hooks 桥接上了，却捕获不到内容 | 桥丢掉了 transcript 和最终回复 | 按 payload 的实际内容定级，不看事件名 | dsh |
| `memory_flush` / `memory_dream` 超时 | 宿主的工具超时默认只有 60 秒 | 配到 300 秒左右 | dsh |
| 看不到任何报错，也确认不了是否成功 | 宿主不输出日志 | 反向对照 + 旁路记录 + 探针 | dsh |
| 多个宿主同时写同一个工作目录 | 捕获和 flush 并发执行 | 所有宿主共用一把工作目录锁；会话标记按宿主 id 分目录 | WorkBuddy + Codex |
| transcript 太大，hook 变慢 | 文件能到几十 MB | 超过 1 MiB 就从尾部分块读 | Codex |
| 召回的记忆被宿主上传 | 宿主默认上传会话日志 | 文档里写明影响和关闭方法 | dsh |
| 补丁文件只剩注释后，宿主无法启动 | 宿主只接受顶层 YAML 列表，空文件也不行 | 安装前确认文件是普通列表；卸载后没有条目就写成 `[]` | dsh |
| 写进去的值，在宿主输出里匹配不到 | 宿主会重新序列化配置（js-yaml 超过 80 列就折行） | 按格式规则解码后再比较，不按行做字符串匹配 | dsh |

## 8. 交付模板

`docs/hosts/<id>.md` 按下面的顺序组织，可参考 [codex.md](codex.md) 和 [deepseek-harness.md](deepseek-harness.md)：

1. 一句话说明：接入等级、机制、验证过的宿主版本（以及对照过的源码提交）。
2. 安装：命令，以及必须的人工步骤（信任、重启、新建会话）。
3. 写入的配置：文件路径和配置片段。
4. 事件与行为（L1 起）：每个事件的行为和超时。
5. 与 Claude Code 契约的差异：分别落在 `HostCaps`、`normalize`、`render` 的哪里。
6. transcript 解析（L2）：只读哪些记录，过滤掉哪些宿主注入的内容。
7. 回退、卸载与文件布局。
8. 验证范围：V1～V6 逐级写明已验证还是未验证。
9. 限制与隐私。

同时要更新三处：在[能力对照](README.md#能力对照)里加一列；在根 README 加入口；
真实会话的验收（V6）记录到 `examples/<id>/EVIDENCE.md`。
