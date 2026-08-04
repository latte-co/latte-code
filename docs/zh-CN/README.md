# Latte Code 文档

- [项目说明](../../README.md)：安装、CLI/TUI 使用、分层 `latte-code.jsonc` Provider 配置、密钥环境引用、安全机制与开发检查。
- [能力 Roadmap](roadmap.md)：按依赖分层的产品能力 checklist；只有已实现且经过自动化验证的能力才会打勾。
- [架构](design/architecture-overview.md)：当前 Rust 实现的 crate 与运行时边界。
- [全局 Session 与数据存储](design/data-storage.md)：已实现的配置数据库路径、按 Workspace 隔离的 Session Catalog、Scoped Lease 与持久化脱敏 Provider 配置失败，以及设计中的全局 Home、JSONL、恢复和迁移契约。
- [斜杠命令](design/slash-commands.md)：已实现的 Built-in Catalog、Session 命令与 Composer Popup，以及设计中的 Prompt Command 扩展和其余 Typed Action。
- Agent Harness 设计方案：[异步 Turn Runner](design/agent-harness/asynchronous-turn-runner.md)、[Session 存储与恢复](design/agent-harness/session-store-and-recovery.md)、[Effect Authority、策略与隔离](design/agent-harness/effect-authority-and-policy.md)、[扩展与委派能力](design/agent-harness/extensions-and-delegation.md)、[事件、投影与回放](design/agent-harness/event-projection-and-replay.md)、[TUI Runtime 契约](design/agent-harness/tui-runtime-contract.md) 与 [验证 Harness 与确定性测试](design/agent-harness/verification-harness.md)。
- [UT / E2E 测试卡点](design/testing-gates.md)：测试分层、阻断矩阵、E2E 场景与分阶段落地方案。
- [E2E 编写手册](testing/e2e-authoring-guide.md)：功能开发的 UT/E2E 配套规则、Harness 用法、断言清单与 UT 95% / E2E 90% 独立覆盖率口径。
- [English documentation](../en-US/README.md)。

本文档树描述当前 Rust 实现，以及明确标注状态、尚未全部落地的设计方案。
