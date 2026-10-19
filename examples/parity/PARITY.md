# PilotDeck ↔ pd-memory 差分测试

同一输入、同一 mock 模型、同一 workspace 路径，比较 TypeScript 原实现与 Rust SDK。
完整范围、结果和边界见 [差分报告](../../docs/parity.md)。

```bash
PILOTDECK_ROOT=/path/to/PilotDeck bash examples/parity/run_parity.sh
PILOTDECK_ROOT=/path/to/PilotDeck bash examples/parity/run_parity.sh --scenario P6,P8,P9,P10,P11,P12 --verbose
python3 -m unittest discover -s examples/parity -p 'test_*.py' -v
```

P1–P17 覆盖捕获、抽取、模型 gate、完整 manifest 候选、最终四段注入、项目隔离、
用户画像、Dream no-op、快照和多次回滚。并覆盖异常降级、调度锚点和同一实例的缓存失效。当前全量 17/17 PASS。

比较器没有行为差异豁免，DEV=0。全局 manifest 相对链接与重复捕获返回标志均直接
比较；控制面格式差异仅作 INFO。12 项比较器自测确认旧差异不能被放行。

`.run/` 保存报告、两侧 artifact、逐步模型调用和完整 mock 日志；环境记录包含上游 src/lib 摘要。
退出码：0 无差分失败，1 检出 DIFF，2 构建或运行失败。`--no-ts` 只作冒烟并报告 SKIP。
