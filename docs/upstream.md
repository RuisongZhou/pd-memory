# 上游基线与验证范围

pd-memory 从 PilotDeck 的 SDK 工作区独立发布。运行和构建 SDK 不需要 PilotDeck；
只有 TypeScript ↔ Rust 差分测试需要单独准备上游 checkout。

- 上游：[OpenBMB/PilotDeck](https://github.com/OpenBMB/PilotDeck)。
- 已验证提交：`52660d51a9292032ae9ca485eafb30175858741d`。
- 记忆模块：[edgeclaw-memory-core](https://github.com/OpenBMB/PilotDeck/tree/52660d51a9292032ae9ca485eafb30175858741d/src/context/memory/edgeclaw-memory-core)。
- 文件摘要及版本记录：[upstream.json](../upstream.json)。
- 功能与流程差分范围：[parity.md](parity.md)。
- WorkBuddy 历史验证：[EVIDENCE.md](../examples/workbuddy/EVIDENCE.md)。

当前覆盖单项目模式的捕获、分类与生成、文件存储、召回、Dream、回滚、调度与缓存。
不包含 PilotDeck UI、General 多项目路由和外部镜像。部署方式变化不扩展记忆引擎能力。

WorkBuddy 证据来自 macOS 上的 WorkBuddy 5.8.2 / CodeBuddy CLI 2.161.0。
真实无头 CLI 的 MCP、SessionStart、UserPromptSubmit、Stop 及上下文注入曾验证通过；
GUI 连接器注册和 GUI 内注入未做同等验证，升级后的宿主格式需要重新检查。
