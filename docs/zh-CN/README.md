# Lattecode 文档

本目录收录 Lattecode 相关的工程调研、已固化设计、提案和里程碑。正式文档应与英文目录 [`docs/en-US/`](../en-US/README.md) 保持结构和语义对齐。

当前设计节奏是渐进式：先实现一个基础、可工作的 local-first code agent，再把执行轨迹、事实、证据、副作用、事务、调度和恢复逐步结构化为 harness-native runtime。文档中的 runtime 术语表示演进目标和接口约束，不表示当前 `src/` 已经具备完整 runtime-kernel 能力。

## 目录分层

| 目录 | 用途 |
| --- | --- |
| [`design/`](./design/architecture-overview.md) | 当前正式设计入口；顶层架构总览仍保留在 `design/architecture-overview.md` |
| [`design/modules/`](./design/modules/README.md) | 当前 / 近期模块级技术设计，主要对齐 `v0.1` 基础 code agent 实现 |
| [`design/runtime-evolution/`](./design/runtime-evolution/README.md) | 已接受的长期 runtime 演进目标；不是普通未采纳提案，也不表示当前已完整实现 |
| [`proposals/`](./proposals/README.md) | 尚未纳入当前设计集的提案和想法；不得暗示已经实现 |
| [`milestones/`](./milestones/README.md) | 里程碑管理，区分目标、计划和已完成记录 |
| [`research/`](./research/code-agent-survey.md) | 调研事实、横向对比和可复查观察结论 |

## 当前正式设计文档

- [架构设计总览](./design/architecture-overview.md)
  - 当前顶层架构入口。定义 code-agent-first 演进路线、模块渐进关系、外部边界和非目标。
- 当前 / 近期模块级技术设计：
  - [`Code Agent Loop`](./design/modules/code-agent-loop.md)
  - [`Context Management and Compression`](./design/modules/context-management-and-compression.md)
  - [`Provider Compatibility Layer`](./design/modules/provider-compatibility-layer.md)
- 已接受的长期 runtime 演进目标：
  - [`Runtime Evolution`](./design/runtime-evolution/README.md)
  - [`ActionGraph`](./design/runtime-evolution/modules/action-graph.md)
  - [`StateStore`](./design/runtime-evolution/modules/state-store.md)
  - [`Scheduler`](./design/runtime-evolution/modules/scheduler.md)
  - [`EffectLedger`](./design/runtime-evolution/modules/effect-ledger.md)
  - [`TransactionManager`](./design/runtime-evolution/modules/transaction-manager.md)
  - [`Reconciler`](./design/runtime-evolution/modules/reconciler.md)
  - [`Policy Core and Guard`](./design/runtime-evolution/modules/policy-core-and-guard.md)
  - [`Capability Adapter`](./design/runtime-evolution/modules/capability-adapter.md)
  - [`ContextProjection`](./design/runtime-evolution/modules/context-projection.md)
  - [`NodeExecutor`](./design/runtime-evolution/modules/node-executor.md)
- 里程碑目标：
  - [`Code Agent Evolution Roadmap v0.1-v0.5`](./milestones/targets/runtime-kernel-roadmap-v0.1-v0.5.md)
  - [`Code Agent Evolution 任务拆分 v0.1-v0.5`](./milestones/targets/runtime-kernel-task-breakdown.md)
  - [`v0.1 Engineering Baseline`](./milestones/targets/v0.1-engineering-baseline.md)
  - [`v0.1 Implementation Plan and Technical Review`](./milestones/targets/v0.1-implementation-plan-review.md)

## 调研文档

- [Code Agent 横向调研](./research/code-agent-survey.md)
  - 覆盖 `claude-code`、`codex`、`CodeWhale`、`opencode`、`oh-my-openagent` 五个系统。
  - 记录各系统的架构设计、能力边界、核心工具、agent loop、可借鉴能力和 graph-native 缺口。

## 中英文对齐状态

| 中文文档 | 英文对应 | 状态 |
| --- | --- | --- |
| [`design/architecture-overview.md`](./design/architecture-overview.md) | [`../en-US/design/architecture-overview.md`](../en-US/design/architecture-overview.md) | 已对齐 |
| [`design/modules/code-agent-loop.md`](./design/modules/code-agent-loop.md) | [`../en-US/design/modules/code-agent-loop.md`](../en-US/design/modules/code-agent-loop.md) | 已对齐 |
| [`design/modules/context-management-and-compression.md`](./design/modules/context-management-and-compression.md) | [`../en-US/design/modules/context-management-and-compression.md`](../en-US/design/modules/context-management-and-compression.md) | 已对齐 |
| [`design/modules/provider-compatibility-layer.md`](./design/modules/provider-compatibility-layer.md) | [`../en-US/design/modules/provider-compatibility-layer.md`](../en-US/design/modules/provider-compatibility-layer.md) | 已对齐 |
| [`design/runtime-evolution/README.md`](./design/runtime-evolution/README.md) | [`../en-US/design/runtime-evolution/README.md`](../en-US/design/runtime-evolution/README.md) | 已对齐 |
| [`design/runtime-evolution/modules/README.md`](./design/runtime-evolution/modules/README.md) | [`../en-US/design/runtime-evolution/modules/README.md`](../en-US/design/runtime-evolution/modules/README.md) | 已对齐 |
| [`design/runtime-evolution/modules/action-graph.md`](./design/runtime-evolution/modules/action-graph.md) | [`../en-US/design/runtime-evolution/modules/action-graph.md`](../en-US/design/runtime-evolution/modules/action-graph.md) | 已对齐 |
| [`design/runtime-evolution/modules/state-store.md`](./design/runtime-evolution/modules/state-store.md) | [`../en-US/design/runtime-evolution/modules/state-store.md`](../en-US/design/runtime-evolution/modules/state-store.md) | 已对齐 |
| [`design/runtime-evolution/modules/scheduler.md`](./design/runtime-evolution/modules/scheduler.md) | [`../en-US/design/runtime-evolution/modules/scheduler.md`](../en-US/design/runtime-evolution/modules/scheduler.md) | 已对齐 |
| [`design/runtime-evolution/modules/effect-ledger.md`](./design/runtime-evolution/modules/effect-ledger.md) | [`../en-US/design/runtime-evolution/modules/effect-ledger.md`](../en-US/design/runtime-evolution/modules/effect-ledger.md) | 已对齐 |
| [`design/runtime-evolution/modules/transaction-manager.md`](./design/runtime-evolution/modules/transaction-manager.md) | [`../en-US/design/runtime-evolution/modules/transaction-manager.md`](../en-US/design/runtime-evolution/modules/transaction-manager.md) | 已对齐 |
| [`design/runtime-evolution/modules/reconciler.md`](./design/runtime-evolution/modules/reconciler.md) | [`../en-US/design/runtime-evolution/modules/reconciler.md`](../en-US/design/runtime-evolution/modules/reconciler.md) | 已对齐 |
| [`design/runtime-evolution/modules/policy-core-and-guard.md`](./design/runtime-evolution/modules/policy-core-and-guard.md) | [`../en-US/design/runtime-evolution/modules/policy-core-and-guard.md`](../en-US/design/runtime-evolution/modules/policy-core-and-guard.md) | 已对齐 |
| [`design/runtime-evolution/modules/capability-adapter.md`](./design/runtime-evolution/modules/capability-adapter.md) | [`../en-US/design/runtime-evolution/modules/capability-adapter.md`](../en-US/design/runtime-evolution/modules/capability-adapter.md) | 已对齐 |
| [`design/runtime-evolution/modules/context-projection.md`](./design/runtime-evolution/modules/context-projection.md) | [`../en-US/design/runtime-evolution/modules/context-projection.md`](../en-US/design/runtime-evolution/modules/context-projection.md) | 已对齐 |
| [`design/runtime-evolution/modules/node-executor.md`](./design/runtime-evolution/modules/node-executor.md) | [`../en-US/design/runtime-evolution/modules/node-executor.md`](../en-US/design/runtime-evolution/modules/node-executor.md) | 已对齐 |
| [`milestones/targets/runtime-kernel-roadmap-v0.1-v0.5.md`](./milestones/targets/runtime-kernel-roadmap-v0.1-v0.5.md) | [`../en-US/milestones/targets/runtime-kernel-roadmap-v0.1-v0.5.md`](../en-US/milestones/targets/runtime-kernel-roadmap-v0.1-v0.5.md) | 已对齐 |
| [`milestones/targets/runtime-kernel-task-breakdown.md`](./milestones/targets/runtime-kernel-task-breakdown.md) | [`../en-US/milestones/targets/runtime-kernel-task-breakdown.md`](../en-US/milestones/targets/runtime-kernel-task-breakdown.md) | 已对齐 |
| [`milestones/targets/v0.1-engineering-baseline.md`](./milestones/targets/v0.1-engineering-baseline.md) | [`../en-US/milestones/targets/v0.1-engineering-baseline.md`](../en-US/milestones/targets/v0.1-engineering-baseline.md) | 已对齐 |
| [`milestones/targets/v0.1-implementation-plan-review.md`](./milestones/targets/v0.1-implementation-plan-review.md) | [`../en-US/milestones/targets/v0.1-implementation-plan-review.md`](../en-US/milestones/targets/v0.1-implementation-plan-review.md) | 已对齐 |
| [`research/code-agent-survey.md`](./research/code-agent-survey.md) | 暂无 | 暂缓翻译：调研文档体量较大，后续如英文读者需要应补齐英文 `research/` 对应文档 |

## 维护约定

- `research/`：记录调研事实、横向对比和可复查观察结论，避免混入尚未验证的设计承诺。
- `design/modules/`：记录当前 / 近期模块级技术设计；模块文档可以引用后续 runtime 对象，但不得把长期目标表述为当前实现。
- `design/runtime-evolution/`：记录已接受的长期 runtime 演进目标；不得降级为普通未采纳提案，也不得暗示当前已经完整实现。
- `proposals/`：记录尚未纳入当前设计集的提案和想法；不得暗示已经实现。
- `milestones/targets/`：记录目标、计划、工程基线和任务拆分。
- `milestones/completed/`：记录已经完成的里程碑和验收证据。
- 新增或大幅更新正式中文文档时，必须同步新增或更新英文对应文档；若暂缓翻译，必须在中英文索引中标明原因和后续动作。

## 术语边界

- **调研事实**：来自横向调研和可复查观察，用于描述已有系统的能力和限制。
- **设计建议**：基于调研事实推导出的 code-agent operating model 与 harness-native runtime 演进路线，不表示既有系统已经具备该能力。
- **Data Plane**：从整个软件工程系统视角，Code Agent / Lattecode 的外部定位；Lattecode 执行任务并产出证据，但不取代外部治理系统。
- **Control Plane Authority**：仅指 Lattecode internal runtime authority，且只在逐步引入 runtime 结构后成立；不得理解为外部工程治理控制平面。
- **ActionGraph**：长期目标中的执行账本、调度表面、恢复入口、审计索引和 UX 可视化表面；runtime evolution 文档描述已接受的演进目标，不表示已经实现。
