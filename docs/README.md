# Latte Code documentation

- [English](en-US/README.md)
- [中文](zh-CN/README.md)

Current designs and implementation notes:

- Capability roadmap: [English](en-US/roadmap.md) | [中文](zh-CN/roadmap.md)
- Global session and data storage (partially implemented): [English](en-US/design/data-storage.md) | [中文](zh-CN/design/data-storage.md)
- Slash commands (first built-in slice implemented): [English](en-US/design/slash-commands.md) | [中文](zh-CN/design/slash-commands.md)
- Agent harness designs: [asynchronous turn runner](en-US/design/agent-harness/asynchronous-turn-runner.md) | [异步 Turn Runner](zh-CN/design/agent-harness/asynchronous-turn-runner.md); [session storage and recovery](en-US/design/agent-harness/session-store-and-recovery.md) | [Session 存储与恢复](zh-CN/design/agent-harness/session-store-and-recovery.md); [effect authority and policy](en-US/design/agent-harness/effect-authority-and-policy.md) | [Effect Authority、策略与隔离](zh-CN/design/agent-harness/effect-authority-and-policy.md); [extension and delegation](en-US/design/agent-harness/extensions-and-delegation.md) | [扩展与委派能力](zh-CN/design/agent-harness/extensions-and-delegation.md); [events and replay](en-US/design/agent-harness/event-projection-and-replay.md) | [事件、投影与回放](zh-CN/design/agent-harness/event-projection-and-replay.md); [TUI runtime](en-US/design/agent-harness/tui-runtime-contract.md) | [TUI Runtime 契约](zh-CN/design/agent-harness/tui-runtime-contract.md); [verification harness](en-US/design/agent-harness/verification-harness.md) | [验证 Harness 与确定性测试](zh-CN/design/agent-harness/verification-harness.md).

The formal architecture documents are maintained in both languages. Implementation behavior is defined by the Rust workspace and verified tests.
