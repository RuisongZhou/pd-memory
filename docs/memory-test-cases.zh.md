# pd-memory Rust SDK — 记忆效果用例（C1–C8）

本文档对应 `crates/pd-memory-core/tests/cases_memory_effect.rs`。
它记录一组**面向“记忆效果”**的集成用例：从真实调用链出发，验证记忆系统是否真的
**记住 / 召回 / 隔离 / 合并 / 可回滚**，而不是只测内部函数或文件格式。

- 用例文件：`crates/pd-memory-core/tests/cases_memory_effect.rs`
- 运行命令（在 the repository root 下执行）：

```bash
cargo test -p pd-memory-core --test cases_memory_effect -- --nocapture
```

- 运行结果（2026-10-19，本机实际输出摘要）：

```text
running 7 tests
test c1_cross_session_memory_is_recalled_in_a_later_session ... ok
test c2_global_user_identity_is_shared_across_workspaces ... ok
test c3_project_memory_is_isolated_between_workspaces ... ok
test c4_capture_and_flush_are_idempotent ... ok
test c5_recall_degrades_gracefully_when_the_model_fails ... ok
test c6_dream_merges_feedback_files_and_rollback_restores_bytes ... ok
test c7_chinese_and_special_characters_round_trip_safely ... ok

test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

- 全仓回归（新增用例前为 289 个测试，新增 7 个后）：

```bash
cargo test --workspace       # 296 passed; 0 failed（32 个 test target 全绿，exit 0；含 doc-test）
cargo fmt --all -- --check   # 无输出，exit 0
```

---

## 1. 这组用例与其它测试的分工

| 测试层 | 位置 | 回答的问题 |
|---|---|---|
| 单元 / 存储层测试 | `pd-memory-core` 各模块 `#[cfg(test)]`、`tests/store_*.rs` | 单个函数/格式是否正确（frontmatter、manifest、队列、redact…） |
| 流水线测试 | `tests/index_*.rs`、`tests/recall_*.rs` | 单条流水线行为是否符合规格（capture→flush→文件、gate→select→render、dream 流程） |
| **记忆效果用例（本文）** | `tests/cases_memory_effect.rs` | **用户/宿主视角：换一个会话、换一个项目、模型故障、重复上报、Dream 回滚之后，记忆是否仍然符合预期** |
| MCP 工具面测试 | `crates/pd-memory-mcp/tests/engine_roundtrip.rs` | `memory_search` / `memory_overview` / `memory_add` 等工具协议层语义（C8） |
| TS↔Rust 差分测试 | `examples/parity/`（另一位同事负责） | **产物/行为是否与 PilotDeck TypeScript 实现一致**（文件字节、L0、manifest、路由、Dream 结果逐项 diff） |

**与 `examples/parity/` 的分工（重点）**：

- **parity 差分测试**回答的是“**产物与 PilotDeck 一致吗**”。它用同一个 mock 模型同时驱动
  TS 与 Rust，对规范化后的产物（`Project/*.md`、`Feedback/*.md`、`MEMORY.md`、
  `project.meta.md`、L0、retrieval 结果、dream 计数等）做逐项 diff，并维护
  `PARITY.md` 偏差白名单。它保证“移植得对”。
- **本组用例**回答的是“**记忆效果是否达成**”。它不关心 TS 是否也这样输出，而是从
  产品语义断言：跨会话还能不能想起约定、两个项目会不会串记忆、模型坏了召回会不会
  崩、Dream 合并后能不能回滚、中文/emoji/超长内容会不会坏。它保证“功能真的有用”。
- 两者互补：差分测试证明“同一输入 → 同一产物”；效果用例证明“这些产物确实被正确
  地写入、召回、隔离与可逆地整理”。差分测试依赖 TS 运行环境（Node + 编译产物），
  本组用例**纯 Rust、完全离线**，可作为不装 Node 的环境下的长期回归基线。

---

## 2. 用例构造方式（可读性约定）

每个 `#[test]` 都：

1. 用 `tempfile::tempdir()` 建独立数据根，配一个显式 `workspace_id`；
2. 用 `MemoryEngine::new(...)` 构造真实引擎：`FileMemoryStore` + 两个
   `FileControlPlane`（queue/state）+ `NoopTelemetry` + `DefaultRedactor`；
3. 注入 **`FixedClock`**（本文件内实现）：时间不会自己走，`advance_minutes()` 显式推进，
   保证 capture 的 `created_at`、retrieval trace、Dream 快照元数据稳定；
4. 注入 **`EffectModel`**（本文件内实现的确定性 `ModelProvider`）：按 `schema_name`
   返回脚本化的中文记忆/路由/选择/Dream 响应，不访问网络、不读环境变量、不 sleep；
5. 断言**可观察记忆效果**：磁盘上的 Markdown 文件（路径/类型/作用域/正文片段）、
   `MEMORY.md` 条目、`retrieve_context` 的 `route` / `context` / `system_context`、
   `list` / `overview` 计数、以及回滚后的**文件字节**。

失败注入（C4/C5）通过 `EffectModel` 的 `FailMode` 开关完成：`InvalidJson` 返回不可解析
文本（单次解析失败，不请求 repair），`Transport` 直接返回 provider
错误，贴近真实模型服务“坏 JSON / 不可用”两类故障。

---

## 3. 用例总览

| 编号 | `#[test]` 名称 | 场景 | 输入 | 预期记忆效果（核心断言） | 结果 |
|---|---|---|---|---|---|
| C1 | `c1_cross_session_memory_is_recalled_in_a_later_session` | 跨会话记忆 | 会话 A：以后所有文档用中文写，项目统一使用 pnpm；会话 B：Boreal 项目目标是整理本地知识库；会话 C 提问 | Feedback/、Project/ 各落一条文件；`MEMORY.md` 有 Feedback + Project 两个分区；会话 C 的 `retrieve_context` 路由 ≠ none，`system_context` 含“中文”“pnpm”与项目事实 | PASS |
| C2 | `c2_global_user_identity_is_shared_across_workspaces` | 用户身份跨项目（global） | workspace A 的会话：请记住：我是后端工程师…；Dream 生成全局画像；workspace B 提问“我是谁” | user 笔记先落 `global/UserIdentityNotes/`（type=user, scope=global，不写项目目录）；Dream 后生成 `global/UserIdentity/user-profile.md` 并吸收原笔记；A/B 两个 workspace 的 user 路由召回都含“后端工程师” | PASS |
| C3 | `c3_project_memory_is_isolated_between_workspaces` | 项目隔离 | 同 data root 下 workspace A 写入 Boreal 项目事实；workspace B 问同一话题 | A 命中事实；B 的 `context` / `system_context` 为空、`selected_ids` 为空、`list` 为空、`MEMORY.md` 不含 A 的内容、B 目录无 A 的记忆文件 | PASS |
| C4 | `c4_capture_and_flush_are_idempotent` | 幂等去重 | 同一 `(timestamp, payload)` capture 两次；同 session 第二条对话在首次 flush 时触发模型故障；清除故障后再次 flush；再空 flush 一次 | 第二次 capture 仍返回 `captured=true` 且 pending=1；首次 flush 成功并降级第二条，保留第一条文件；两条 L0 均 indexed，后续不重放降级记录，文件总计 1 条；空 flush 写成 0 且文件字节不变 | PASS |
| C5 | `c5_recall_degrades_gracefully_when_the_model_fails` | 召回降级 | 已有一条项目记忆；分别让 gate 返回无效 JSON、gate 传输报错、selection 返回无效 JSON、selection 传输报错 | gate 故障 → `route=none`、无上下文、不 panic；selection 故障 → 最近候选兜底，仍渲染真实记忆；任何情况下上下文都不含模型错误文本 | PASS |
| C6 | `c6_dream_merges_feedback_files_and_rollback_restores_bytes` | Dream 合并 + 回滚 | 两条内容重复的 feedback 文件（字节先快照）+ scripted cluster/merge | Dream 后 Feedback 只剩 1 条、旧文件删除、`MEMORY.md` 更新、`lastDreamStatus=success` 且存在快照；rollback 后旧文件**字节级恢复**、合并文件消失、`lastDreamStatus` 回到 Dream 之前；`MEMORY.md` 内容恢复 | PASS |
| C7 | `c7_chinese_and_special_characters_round_trip_safely` | 中文与特殊字符 | 中文名“星尘项目发布清单 🚀”+ 中文描述/正文；第二条：72 个“长”+10 个 🚀 的超长名称、240 字符描述、12 行正文 | 文件名 slug 保留中文、hash 后缀 10 位；frontmatter 可解析且往返中文/emoji；`MEMORY.md` 链接可读；超长名称按 UTF-16 单元安全截断（不劈开 emoji，截为 72“长”+4🚀）、描述截到 180、召回按行截断以 `...` 结尾 | PASS |
| C8 | （未实现，见 §5） | MCP 工具面 | `memory_search` / `memory_overview` / `memory_add` | 由 `pd-memory-mcp/tests/engine_roundtrip.rs` 覆盖 | N/A（跨 crate 依赖环） |

---

## 4. 逐用例说明

### C1 跨会话记忆

- **场景**：用户在前两个会话分别交代“文档语言 + 包管理器”约定和项目事实；第三个
  会话（新 session）提问时，助手应能拿到这些长期记忆。
- **输入**：
  - session-a：`以后所有文档用中文写，项目统一使用 pnpm。`（→ feedback）
  - session-b：`再记录一下：Boreal 项目目标是整理本地知识库。`（→ project）
  - session-c 提问：`我们文档应该用什么语言写？项目用哪个包管理器？`
- **预期记忆效果**：
  - `Feedback/<中文slug>-<hash10>.md` 与 `Project/<中文slug>-<hash10>.md` 各一条；
  - 两条文件的 frontmatter 分别为 `type: feedback / scope: project` 与
    `type: project / scope: project`，正文分别含“中文”“pnpm”与“本地知识库整理工具”；
  - `MEMORY.md` 同时出现 `## Feedback Memory`、`## Project Memory`；
  - session-c 的 `retrieve_context`：`route != none`，`context`/`system_context` 同时
    含“中文”“pnpm”，`system_context` 使用上游四段结构。
- **实现要点**：capture 两个 session → 各自 flush；`EffectModel` 按 focus 文本关键词
  分类（出现“以后/所有文档/约定/规则”→feedback，出现“身份/我是”→user，否则 project），
  使这条测试同时覆盖两类记忆文件；检索时 gate 返回 project、selection 选择全部候选。

### C2 用户身份跨项目（global）

- **场景**：用户在项目 A 交代稳定身份。全局用户记忆应与项目记忆隔离：笔记写在
  `global/`，任何项目（workspace）的召回都能使用，但不会出现在项目自己的 `memory/` 目录。
- **输入**：workspace A，session：`请记住：我是后端工程师，长期负责支付系统。`
- **预期记忆效果**：
  1. flush 后笔记落在 `<data_root>/global/UserIdentityNotes/`，frontmatter
     `type=user / scope=global`，正文含“后端工程师”；workspace A 的 `memory/` 目录
     没有记录文件（不污染项目目录）；
  2. Dream 调 `dream_user_profile` 把笔记合并成 `global/UserIdentity/user-profile.md`，
     吸收（删除）原笔记，`overview.user_profiles == 1`；
  3. workspace A、B 两个**不同 workspace** 的 `retrieve_context`（`route=user`）都能
     渲染全局画像并包含“后端工程师”。
- **实现要点**：两个 `MemoryEngine` 共用同一 `data_root`、不同 `workspace_id`；
  `get_user_summary().identity_background` 与 user 路由 context 双重断言，确保
  “全局可用 + 项目隔离”同时成立。
- **行为说明（非 bug）**：user 路由渲染的身份背景来自全局画像
  `global/UserIdentity/user-profile.md`；单独的 `UserIdentityNotes/` 笔记在
  `get_user_summary()` 中可见，但只有 Dream 把它合并进画像之后才会被注入 user 路由召回。
  这正是本用例把“笔记落盘 → Dream 合并 → 跨项目召回”串起来验证的原因。

### C3 项目隔离

- **场景**：同一台机器/同一 data root 下有多个项目目录（workspace），项目 A 的记忆
  不能串到项目 B。
- **输入**：workspace A：`项目叫 Boreal，目标是整理本地知识库，目前处于设计阶段。`
  workspace B 问同一话题。
- **预期记忆效果**：A 召回命中（`context` 含“Boreal 是一个本地知识库整理工具”）；
  B 的 `context`/`system_context` 为空、`selected_ids` 为空、`list` 为空、
  `MEMORY.md` 不含 A 的名称/事实、B 的 `memory/` 目录没有记录文件。
- **实现要点**：workspace A/B 使用同一 data root、不同 `workspace_id`；断言物理目录
  不同 + 逻辑检索隔离，覆盖“路径隔离”和“召回隔离”两个层面。

### C4 幂等去重与模型故障降级

- **输入**：同一 `(timestamp, payload)` 连续捕获两次，再在同一 session 捕获带
  `FAULT_MARKER` 的第二条约定；首次 flush 对第二条分类注入传输错误。
- **预期**：重复捕获仍返回 `captured=true`，队列只新增一条；首次 flush 写入第一条，
  第二条按上游降级为空结果，两条 L0 均标记 indexed。清除故障后再次 flush 不重放，
  后续空 flush 的写入数为 0，文件字节不变。
- 持久化错误的逐记录隔离及重试由 `index_heartbeat.rs` 独立验证；模型错误不采用该重试策略。

### C5 召回降级

- **场景**：模型服务可能返回坏 JSON 或直接不可用；召回不能 panic，也不能把错误文本
  当记忆内容注入 prompt。
- **输入**：一条项目记忆；四种故障组合：
  1. gate 返回无效 JSON（单次解析失败，不请求 repair）；
  2. gate 传输报错；
  3. gate 正常、selection 返回无效 JSON；
  4. gate 正常、selection 传输报错。
- **预期记忆效果**：1/2 返回 `route=none`、`context`/`system_context`/`selected_ids`
  均为空；3/4 仍返回 `route=project`，通过最近候选兜底渲染出真实项目记忆，
  `system_context` 为上游四段结构的注入文本；所有上下文都不含
  `not a parseable JSON object`、`scripted`、`model request failed` 等错误文本。
- **实现要点**：`FailMode::{InvalidJson, Transport}` 区分解析失败与 provider 不可用；
  `assert_failure_text_absent()` 统一检查错误文本泄漏。

### C6 Dream 合并 + 回滚

- **场景**：项目里有两条内容重复的交付规则 feedback 文件。Dream 应合并成 1 条并清理
  旧文件，且整个操作可回滚；回滚后旧文件要与操作前完全一致。
- **输入**：直接向真实 `FileMemoryStore` 写入两条 feedback 笔记（先快照旧文件字节）；
  `EffectModel` 的 `dream_cluster` 自动把全部 header 合成一簇，`dream_merge` 返回
  脚本化合并结果。
- **预期记忆效果**：
  - Dream 后：Feedback 只剩 1 条（名称“交付规则（合并）”）、两条旧文件从磁盘删除、
    合并文件存在且正文含合并内容、`MEMORY.md` 出现新名称且不再出现旧名称；
  - `lastDreamStatus == success`、`lastDreamAt` 有值、`last_dream_snapshot()` 存在；
  - rollback 后：两条旧文件**逐字节恢复**（`std::fs::read` 对比操作前快照）、合并文件
    消失、Feedback 恢复为 2 条、`MEMORY.md` 恢复旧条目；`lastDreamStatus` 回到
    Dream 之前的状态（本次是首次 Dream，回滚后为 `None`）。
- **实现要点**：`FileMemoryStore` 的 Dream 快照/回滚为目录级原子替换；断言回滚
  “记录文件字节级一致”。注意 `MEMORY.md` 是派生文件，回滚后会被重新渲染
  （`Updated:` 行取当前 wall clock），因此对其断言**条目内容恢复**而非整文件字节。

### C7 中文与特殊字符

- **场景**：产品面向中文用户，标题/描述/正文会包含中文、emoji、超长内容；从文件名
  slug、frontmatter 解析、`MEMORY.md` 链接到按行读取召回都必须安全。
- **输入**：
  - 第一条：名称 `星尘项目发布清单 🚀`，中文描述与正文（同 session，T0）；
  - 第二条：名称 = 72 个“长” + 10 个 🚀（120 个 UTF-16 单元，超 80 上限）、描述 =
    “描述”×120（240 单元，超 180 上限）、12 行正文（T2）。
- **预期记忆效果**：
  - 文件名形如 `Project/星尘项目发布清单-<10位hex>.md`（`slugify` 保留 CJK，emoji 折叠
    为分隔符后修剪）；
  - frontmatter 可被 `parse_markdown` 解析，`name` 往返出 `星尘项目发布清单 🚀`，正文
    保留 emoji；
  - `MEMORY.md` 中有可读的 markdown 链接 `[星尘项目发布清单 🚀](Project/...md) — 描述`；
  - 超长名称截断为 `长`×72 + `🚀`×4（正好 80 个 UTF-16 单元，**不劈开 surrogate
    pair**，字符数 76）；描述截断为 180 单元；正文完整落盘，而 `get(max_lines=3)`
    按行截断为“3 行 + `...`”。
- **实现要点**：截断由 `extraction::truncate_utf16` 完成（按 UTF-16 单元、不切断字符），
  不是按字节截断；用例同时覆盖写入截断（名称/描述）与读取截断（召回行数）。

---

## 5. C8（MCP 工具面语义）为何不在本文件

C8 要求验证 `memory_search` 返回包含事实的 context、`memory_overview` 计数正确、
`memory_add` 重复 turn 仍返回 `captured=true`，队列继续去重。这些语义需要通过
`pd-memory-mcp::handle_line` 调用。

但 `pd-memory-mcp` 本身依赖 `pd-memory-core`；若在 `pd-memory-core` 的集成测试里再
把 `pd-memory-mcp` 加为 dev-dependency，会形成 **cargo 依赖环**（package 不能通过
dev-dependencies 依赖回自己）。因此按任务约定跳过跨 crate 实现，并说明已有覆盖：

- `crates/pd-memory-mcp/tests/engine_roundtrip.rs`
  - `mcp_flush_list_and_search_roundtrip`：`memory_search` 返回真实记忆上下文；
  - `mcp_initialize_tools_list_and_overview_with_engine`：`memory_overview` 计数；
  - `mcp_add_is_captured_and_deduplicated`：`memory_add` 重复 turn 仍返回 `captured=true`，队列继续去重。
- `crates/pd-memory-mcp/src/lib.rs` 的单元测试进一步覆盖 tools/list、错误处理、
  `memory_dream` / `memory_flush` 等工具。

如果未来需要把 C8 纳入同一份“记忆效果”套件，正确做法是**在 `pd-memory-mcp` crate 下**
新增 `tests/cases_mcp_effect.rs`（它依赖 core 是正常方向），而不是从 core 反向依赖 mcp。

---

## 6. 结果表（实际运行记录）

| 检查项 | 命令（`cd pd-memory`） | 实际结果 |
|---|---|---|
| 本套件 | `cargo test -p pd-memory-core --test cases_memory_effect -- --nocapture` | **7 passed; 0 failed; 0 ignored**（finished in 0.04s） |
| 本套件确定性 | 连续运行 3 次 | 3 次均 7 passed；无 sleep、无网络、无随机时间戳 |
| 全仓回归 | `cargo test --workspace` | **296 passed; 0 failed**；32 个 test target（含 7 个 doc-test target）全部 `ok`，exit 0（新增前基线 289） |
| 格式 | `cargo fmt --all -- --check` | 通过（无输出，exit 0） |

> 说明：本组用例不修改任何 `src/` 代码，也没有 `#[ignore]`。C1–C7 全部按当前实现
> 通过，未发现需要上报的核心行为 bug；观察到的边界行为记录在 §7。

---

## 7. 行为观察与边界（非 bug，维护时注意）

1. **user 笔记 → 全局画像是两步语义**：`UserIdentityNotes/` 笔记先落盘（global scope），
   但 user 路由召回的身份背景只来自 Dream 重写后的
   `global/UserIdentity/user-profile.md`。未跑 Dream 时，身份笔记不会注入 user 路由
   context（它仍是全局可见、可被后续 Dream 吸收的原材料）。C2 按“笔记落盘 → Dream →
   跨项目召回”完整验证。
2. **Dream 画像“正文未变则不写”**：Dream 重写用户画像前会比较新旧正文，若
   `identity_background_markdown` 生成的 body 与旧画像一致，则返回 `Skipped` 并保留
   user 笔记（不做无意义的写入与删除）。开发 C2 时曾用“笔记正文 = 目标画像正文”的
   fixture，Dream 正确地什么都不做；这是符合设计的保护逻辑。
3. **模型故障降级后不重放**：C4 保留已写入记忆，将分类失败记录标记 indexed；
   后续 flush 不写文件。持久化失败的记录保留 pending，由独立心跳测试验证再次处理。
4. **回滚恢复记录文件字节，`MEMORY.md` 是派生文件**：rollback 会恢复 Dream 前的
   memory/global 文件树；随后 `ensure_layout()` 重新渲染 `MEMORY.md`（`Updated:` 行用
   当前 wall clock），所以清单文件字节会变、条目内容会正确恢复。C6 分开断言这两点。
5. **gate 故障与 selection 故障的降级级别不同**：gate 失败 → `route=none`（宁缺勿错，
   不注入任何记忆）；selection 失败 → 最近候选兜底（仍注入真实记忆）。这符合
   `recall.rs` 的 graceful degradation 设计，C5 固定了该行为。
6. **截断单位是 UTF-16 单元**：名称/描述上限 80/180，与 JS `String#slice` 语义一致，
   不会把 emoji（代理对）切成两半；正文在写入时不截断，召回读取时按行截断为
   `N 行 + "..."`。C7 固定了该行为。

---

## 8. 维护指南（如何新增/修改用例）

1. 在 `cases_memory_effect.rs` 内新增 `#[test]`，按 C 编号命名
   （`cN_<english_snake_description>`），并在上方写中文注释块：
   **场景 / 输入 / 预期记忆效果 / 断言点**。
2. 复用文件内辅助函数：`test_config`、`open_engine`、`capture_turn`、`flush`、
   `retrieve`、`list_entries`、`record_files`、`manifest_text`、`assert_failure_text_absent`。
3. 需要新的模型行为时扩展 `EffectModel` / `Plan`（按 `schema_name` 分支），不要引入
   网络或真实时间；需要“时间流逝”时用 `FixedClock::advance_minutes`。
4. 不要在断言里依赖 wall clock 产生的 `updated_at`（存储层使用系统时间）；需要字节级
   比较时用 `std::fs::read` 前后快照（参考 C6）。
5. 同步更新本文件的 §3 总览表、§4 说明与 §6 结果表。
