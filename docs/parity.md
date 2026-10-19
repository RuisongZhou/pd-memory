# PilotDeck ↔ pd-memory 单项目行为差分报告

验证日期：2026-10-19（Asia/Singapore）。基线为 `PILOTDECK_ROOT` 指向的上游 checkout 中的
`src/context/memory/edgeclaw-memory-core`，执行其已有 `lib/index.js`；Rust 侧执行
`target/debug/pd-memory`，连续实例探针执行仅用于测试的 `parity-runtime`。测试不修改上游 TypeScript，也不连接外部模型。

## 已修正的移植偏差

- 新用户消息即使没有助手回复，也进入分类和记忆生成；后续只追加助手回复不会重复抽取。
- 自动召回和显式搜索均执行 gate → manifest → 模型选择 → 文件读取；删除额外的 BM25
  预筛实现与配置，候选集合和顺序直接对齐上游。
- 最终注入文本使用上游的 User Profile、Project Meta、Project Memory、Feedback Memory
  四段结构，保留“当前轮明确用户指令优先”的规则；原始 recall block 仍保留在 API 结果中。
- Dream 快照保存前后运行状态；回滚安装反向快照，可再次回滚以恢复 Dream 后的状态。
  不调用模型，并恢复原始文件字节、时间、状态和摘要。
- 空记忆或没有修改的 Dream 返回 success，保留之前的回滚快照。用户画像整理时，在 staged
  store 中补齐上游已有的项目元信息初始化。

- 模型分类或生成失败按上游降级为空结果，不再请求 JSON repair，也不重放已索引的降级记录；
  生成笔记保留上游 loose parser。持久化失败仅影响当前 L0，后续记录继续处理。
- 调度使用注入时钟，锚点从待处理记录和索引状态恢复；索引后重新评估 Dream。手动操作、
  session 筛选、禁用间隔、阈值触发、调度原因与全局身份笔记边界均对齐。
- 召回缓存覆盖完整文件内容和 lastDreamAt，TTL 从完成时起算；flush、设置变更、导入和
  清理后的缓存与状态重置对齐。移除额外的缓存容量和索引扫描上限。

## 复现

```bash
PILOTDECK_ROOT=/path/to/PilotDeck bash examples/parity/run_parity.sh
PILOTDECK_ROOT=/path/to/PilotDeck bash examples/parity/run_parity.sh --scenario P5,P6,P8,P9,P10,P11,P12
python3 -m unittest discover -s examples/parity -p 'test_*.py' -v
```

独立仓库通过 `PILOTDECK_ROOT` 指定外部 PilotDeck checkout。构建、单元测试及部署不依赖它。
从上游准备与 `upstream.json` 对应的基线：

```bash
git clone https://github.com/OpenBMB/PilotDeck.git /path/to/PilotDeck
git -C /path/to/PilotDeck checkout 52660d51a9292032ae9ca485eafb30175858741d
# 该提交已包含 lib/，通常无需构建。
# 只有修改上游源码后需要重新构建时，才执行：
cd /path/to/PilotDeck/src/context/memory/edgeclaw-memory-core
npm install --workspaces=false
npm run build
```

然后回到 pd-memory 根目录，用 Node 22.13+ 运行差分。若已有相同版本的上游 `lib/`，
无需重装依赖；报告会记录 src/lib 的摘要，方便核对基线。

全量入口先运行比较器回归测试，再构建 CLI 和测试用持久实例驱动。运行产物在 `examples/parity/.run/`：
`report.json` 汇总逐项结果；`environment.json` 保存 commit、运行时版本、上游 src/lib 的
SHA-256 摘要；每个场景保留两侧 artifact 与 mock 的完整请求日志。

`--no-ts` 只用于 Rust 冒烟，报告为 SKIP，不能作为对齐证据。

## 场景与观测量

| 场景 | 检查内容 |
|---|---|
| P1 | 对话生成 project、feedback，文件、frontmatter、manifest、L0 一致 |
| P2 | 重复捕获仍返回 captured=true；不新增 L0、不再次抽取 |
| P3 | 全局身份笔记的存储与 user 路由行为 |
| P4 | 自动召回 gate=none，无注入 |
| P5 | 自动召回、显式搜索，以及无词法重叠 query 的完整候选和最终文本 |
| P6 | Dream 合并、后续 no-op 保留快照、连续三次回滚；恢复文件与运行状态 |
| P7 | 同一数据根下的 workspace 隔离 |
| P8 | 尚无助手回复时立即抽取；补上回复后的 flush 不重复抽取 |
| P9 | 显式搜索 gate=project，路由和选中数符合预期 |
| P10 | 显式搜索 gate=none，只执行 gate 且无注入 |
| P11 | 空记忆、非空但无修改的 Dream 均 success，不创建新快照 |
| P12 | 身份笔记整理为用户画像后，user gate、选择和最终注入一致 |
| P13 | classify/create 无效 JSON 降级、loose create 解析、无额外 repair 或重放 |
| P14 | 锚点恢复、注入时钟、索引后 Dream 判断、手动及筛选 flush、阈值与调度原因 |
| P15 | 同一实例缓存命中、正文编辑失效、flush/设置重置、TTL、模式/近期消息键、导入与清理 |
| P16 | gate/selection 解析失败降级、OpenAI response_format 回退及请求次数 |
| P17 | Dream 合并理由中“同属工作组”不再被误判为同项目理由 |

对召回直接比较 gate 路由、选中文件及顺序、最终 systemContext。显式搜索还比较原始
recall block。模型请求日志用于核对调用阶段顺序，以及传入模型的完整 manifest ids/order。
P3/P8/P9/P10/P12 及运行时探针含明确预期，防止两侧都跳过关键流程而误判通过。
运行时探针在同一实例连续执行操作，逐步比较 pending、文件数、精确锚点、缓存命中和
模型调用顺序；虚拟时钟用于控制间隔边界，不需要真实等待。文件存储仍使用自身写入时钟，
调度场景在首次 Dream 前完成新增文件操作，避免两种时钟造成伪差分。

回滚比较器检查快照存在性、sourceAction 和前后状态字段；各侧还必须精确恢复到前一步
保存的自身运行状态，避免仅归一化时间后掩盖状态丢失。文件恢复检查保留真实字节，只有派生的
MEMORY.md 更新时间不纳入同侧回滚字节断言。

比较器自测会故意改变注入文本、选择顺序、显式搜索路由、模型候选和反向快照，确认得到 DIFF。
旧 `expect_dev` 标记不再豁免这些差异，文件 hash 后缀差异也不再被屏蔽。

## 当前结果与边界

最近全量验证：**17/17 场景 PASS，DIFF=0、SKIP=0、ERROR=0**。

全局用户 manifest 链接按实际目录计算相对路径，与上游一致；重复捕获返回 captured=true，
空输入返回 false，队列继续去重。CLI/MCP 直接传递捕获结果，不附加旧 duplicate 标签。
**DEV=0：比较器不再包含行为豁免或链接前缀替换**；JSONL/JSON 与 SQLite 的控制面实现差异
仅记为 INFO。比较器新增回归检查，旧链接格式或重复捕获标志再次出现时必须判为 DIFF。

此结果验证当前单项目范围内的上述流程，不表示整个上游模块的全部能力已经移植。
General 多项目路由、外部镜像和未支持的模型协议仍保持现有范围；其他登记过的策略边界
见 `DEVIATIONS.md`。本轮异常处理、调度与缓存差异均已修正，未引入新能力。
确定性 mock 验证结构和流程，不用于评价真实模型的记忆质量。未运行 Android 真机测试。
