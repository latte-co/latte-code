# Latte Code 文档

- [项目说明](../README.md)：安装、CLI/TUI 使用、分层 `latte-code.jsonc` Provider 配置、密钥环境引用、安全机制与开发检查。
- [能力 Roadmap](roadmap.md)：按依赖分层的产品能力 checklist；只有已实现且经过自动化验证的能力才会打勾。
- [架构](design/architecture-overview.md)：当前 Rust 实现的 crate 与运行时边界。
- [概念体系对齐](design/concept-alignment.md)：Session/Turn/Round 概念层级、Thread→Session 与 Run→Turn 改名批次及剩余结构性工作。
- [全局 Session 与数据存储](design/data-storage.md)：已实现全局 Product Home、Project/Workspace 注册、Session 分区 Lease、每 Session JSONL 读取权威与尾行修复、幂等旧库导入，以及仍待实现的发现与治理契约。
- [斜杠命令](design/slash-commands.md)：已实现的 Built-in Catalog、Session 命令与 Composer Popup，以及设计中的 Prompt Command 扩展和其余 Typed Action。
- [HTTP Server](design/server.md)：多 Workspace HTTP REST + SSE API、运行模式与多 Server 语义、幂等创建故障模型，通过 `latte-code serve` 启动。
- [版本化 RPC 与 Event Backpressure 契约](design/versioned-rpc-contract.md)：类型化 DTO、错误枚举、cursor 分页与 SSE resync 契约。
- [Harness Profile 抽象](design/harness-profile.md)：按 binding 解析预算与 prompt 槽位的数据化 profile、解析管道与权限天花板。
- [Context 架构与上下文管理](design/context-design.md)：分层 context 模型、SummarizeOnDiscard/确定性省略压缩、手动 `/compact`、前缀稳定性契约与只读用量投影。
- [Agent Runtime 运行契约](design/agent-runtime.md)：已实现的 Runner/Mailbox、Session 存储与恢复、Effect Authority、事件投影、TUI 分层与验证 Harness 规范；未实现提案以 Roadmap 为准。
- [E2E 编写手册](testing/e2e-authoring-guide.md)：功能开发的 UT/E2E 配套规则、Harness 用法、断言清单与 UT 95% / E2E 90% 独立覆盖率口径。

本文档树描述当前 Rust 实现，以及明确标注状态、尚未全部落地的设计方案。
