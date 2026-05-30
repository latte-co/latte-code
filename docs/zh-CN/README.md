# Fluxcode 文档

本目录收录 Fluxcode 相关的工程调研、架构设计与阶段性路线图。正式文档应与英文目录 [`docs/en-US/`](../en-US/README.md) 保持结构和语义对齐。

## 文档索引

### 当前正式设计文档

- [架构设计总览](./design/architecture-overview.md)
  - 当前顶层架构入口。先给出 Fluxcode externally 是 code agent `Data Plane` 的顶层定位，并明确 `Control Plane Authority` 仅指 Fluxcode internal runtime authority；再展开 code agent 工作模型，以及与普通 `ReAct` / transcript-driven agent 的区别；随后说明 harness-native runtime 如何作为治理底座支撑 code-agent 行为，并覆盖运行闭环、关键模块权威归属、外部协作与治理边界、非目标和下钻文档索引。
- [Runtime Kernel Roadmap v0.1-v0.5](./design/runtime-kernel-roadmap-v0.1-v0.5.md)
  - 当前 v0.1-v0.5 阶段目标和跨版本不变量。
- [Runtime Kernel 任务拆分 v0.1-v0.5](./design/runtime-kernel-task-breakdown.md)
  - 独立任务拆分文档，记录每个版本的任务、依赖、验收和非目标。
- 模块技术设计：
  - [`ActionGraph`](./design/modules/action-graph.md)
  - [`StateStore`](./design/modules/state-store.md)
  - [`Scheduler`](./design/modules/scheduler.md)
  - [`EffectLedger`](./design/modules/effect-ledger.md)
  - [`TransactionManager`](./design/modules/transaction-manager.md)
  - [`Reconciler`](./design/modules/reconciler.md)
  - [`Policy Core and Guard`](./design/modules/policy-core-and-guard.md)
  - [`Capability Adapter`](./design/modules/capability-adapter.md)
  - [`ContextProjection`](./design/modules/context-projection.md)
  - [`NodeExecutor`](./design/modules/node-executor.md)

### 调研文档

- [Code Agent 横向调研](./research/code-agent-survey.md)
  - 覆盖 `claude-code`、`codex`、`CodeWhale`、`opencode`、`oh-my-openagent` 五个系统。
  - 记录各系统的架构设计、能力边界、核心工具、agent loop、可借鉴能力和 graph-native 缺口。

## 中英文对齐状态

| 中文文档 | 英文对应 | 状态 |
| --- | --- | --- |
| [`design/architecture-overview.md`](./design/architecture-overview.md) | [`../en-US/design/architecture-overview.md`](../en-US/design/architecture-overview.md) | 已对齐 |
| [`design/runtime-kernel-roadmap-v0.1-v0.5.md`](./design/runtime-kernel-roadmap-v0.1-v0.5.md) | [`../en-US/design/runtime-kernel-roadmap-v0.1-v0.5.md`](../en-US/design/runtime-kernel-roadmap-v0.1-v0.5.md) | 已对齐 |
| [`design/runtime-kernel-task-breakdown.md`](./design/runtime-kernel-task-breakdown.md) | [`../en-US/design/runtime-kernel-task-breakdown.md`](../en-US/design/runtime-kernel-task-breakdown.md) | 已对齐 |
| [`design/modules/action-graph.md`](./design/modules/action-graph.md) | [`../en-US/design/modules/action-graph.md`](../en-US/design/modules/action-graph.md) | 已对齐 |
| [`design/modules/state-store.md`](./design/modules/state-store.md) | [`../en-US/design/modules/state-store.md`](../en-US/design/modules/state-store.md) | 已对齐 |
| [`design/modules/scheduler.md`](./design/modules/scheduler.md) | [`../en-US/design/modules/scheduler.md`](../en-US/design/modules/scheduler.md) | 已对齐 |
| [`design/modules/effect-ledger.md`](./design/modules/effect-ledger.md) | [`../en-US/design/modules/effect-ledger.md`](../en-US/design/modules/effect-ledger.md) | 已对齐 |
| [`design/modules/transaction-manager.md`](./design/modules/transaction-manager.md) | [`../en-US/design/modules/transaction-manager.md`](../en-US/design/modules/transaction-manager.md) | 已对齐 |
| [`design/modules/reconciler.md`](./design/modules/reconciler.md) | [`../en-US/design/modules/reconciler.md`](../en-US/design/modules/reconciler.md) | 已对齐 |
| [`design/modules/policy-core-and-guard.md`](./design/modules/policy-core-and-guard.md) | [`../en-US/design/modules/policy-core-and-guard.md`](../en-US/design/modules/policy-core-and-guard.md) | 已对齐 |
| [`design/modules/capability-adapter.md`](./design/modules/capability-adapter.md) | [`../en-US/design/modules/capability-adapter.md`](../en-US/design/modules/capability-adapter.md) | 已对齐 |
| [`design/modules/context-projection.md`](./design/modules/context-projection.md) | [`../en-US/design/modules/context-projection.md`](../en-US/design/modules/context-projection.md) | 已对齐 |
| [`design/modules/node-executor.md`](./design/modules/node-executor.md) | [`../en-US/design/modules/node-executor.md`](../en-US/design/modules/node-executor.md) | 已对齐 |
| [`research/code-agent-survey.md`](./research/code-agent-survey.md) | 暂无 | 暂缓翻译：调研文档体量较大，后续如英文读者需要应补齐英文 `research/` 对应文档 |

## 维护约定

- `research/`：记录调研事实、横向对比和可复查的观察结论，避免混入尚未验证的设计承诺。
- `design/`：记录设计建议、架构分层、接口模型、模块技术设计和阶段性路线图。
- 新增文档时优先补充本 README 的索引，确保索引只指向实际存在且需要维护的正式文档。
- 新增或大幅更新正式中文文档时，必须同步新增或更新英文对应文档；若暂缓翻译，必须在中英文索引中标明原因和后续动作。

## 术语边界

- **调研事实**：来自横向调研和可复查观察，用于描述已有系统的能力和限制。
- **设计建议**：基于调研事实推导出的 code-agent operating model 与 harness-native runtime 设计，不表示既有系统已经具备该能力。
- **Data Plane**：从整个软件工程系统视角，Code Agent / Fluxcode 的外部定位；Fluxcode 执行任务并产出证据，但不取代外部治理系统。
- **Control Plane Authority**：仅指 Fluxcode internal runtime authority，负责内部事实、调度、副作用、事务和恢复语义；不得理解为外部工程治理控制平面。
- **ActionGraph**：执行账本、调度表面、恢复入口、审计索引和 UX 可视化表面，不是全知状态容器。
