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

WorkBuddy 历史证据来自 macOS 上的 5.8.2 / CodeBuddy CLI 2.161.0；
2026-10-19 在 WorkBuddy 5.8.3 重新验证，模拟器与真实无头 CLI 共 56 项检查通过。
GUI 自定义 MCP 使用 `~/.workbuddy/mcp.json`，首次信任并重启后显示 7/7 个工具启用。
Qwen 真实模型的独立捕获、提取、搜索和 hook 上下文输出验证通过；
GUI 会话中的自动捕获与上下文注入未做同等端到端验证。

生产安装流程、备份回退与可复现命令见 [WorkBuddy 安装指南](workbuddy.md)。
安装器回归测试使用隔离 home、合成密钥、本地二进制和本地 HTTP fixture，不读取真实凭据。
