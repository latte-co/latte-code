# Fluxcode 架构设计总览

## 文档状态与适用边界

本文是 Fluxcode 当前正式设计总览，用于统一团队对实现节奏的理解。它将早期目标从“一次性交付完整 harness-native runtime”调整为“先交付基础可工作的 local-first code agent，再逐步演进到 harness-native runtime”。

英文对应文档：[`docs/en-US/design/architecture-overview.md`](../../en-US/design/architecture-overview.md)。

本文描述的是设计目标、演进路径和模块边界，不表示当前 `src/` 已经实现这些能力。

## 1. 顶层定位

从整个软件工程系统视角看，Fluxcode 是 code agent `Data Plane`：它读取仓库、理解任务、调用工具、生成修改、运行验证、产出证据，并把结果交给人类和既有工程系统判断。Fluxcode 不取代 repo permissions、CI、code review、compliance、release 或 deployment gates。

`Control Plane Authority` 在本文中只表示 Fluxcode internal runtime authority。这个内部权威不是 v0.1 的起点要求，而是随着执行轨迹、事实、副作用、事务和恢复能力逐步结构化后形成的 runtime 权威边界。

因此，Fluxcode 的设计采用两层叙事：

| 层级 | 目标 | 团队需要先理解什么 |
| --- | --- | --- |
| 近期产品形态 | 一个基础、可工作的 local-first code agent | 能完成真实代码任务，能解释改了什么、验证了什么、还有什么风险 |
| 长期架构方向 | harness-native runtime | 把执行过程结构化为可审计、可恢复、可治理的 runtime 对象 |

## 2. 渐进式架构路线

Fluxcode 不再要求团队从完整 `ActionGraph`、`StateStore`、`Scheduler`、`EffectLedger`、`TransactionManager`、`Reconciler` 全套概念开始实现。更合适的路线是让抽象从可工作的 code agent 中长出来。

```text
Basic working code agent
  -> Structured task trace
  -> Evidence and fact discipline
  -> Controlled effects and transactions
  -> Scheduling and reconciliation
  -> Harness-native runtime
```

### 2.1 第一阶段：基础可工作 code agent

最早期的 Fluxcode 应先完成以下闭环：

- 接收用户任务和验收条件。
- 搜索、读取并理解仓库上下文。
- 生成小范围代码修改。
- 运行用户允许的验证命令。
- 汇报 diff、验证结果、风险和阻塞。
- 保留最小执行记录，便于复盘。

这一阶段不需要完整 `ActionGraph` 或完整事实系统。可以先使用简单的 `TaskRun`、`StepTrace`、`ToolCallRecord`、`PatchSummary` 和 `VerificationResult`。

### 2.2 第二阶段：把执行过程结构化

当基础 agent 能稳定完成任务后，再把执行记录结构化：

- `StepTrace` 演进为 `ActionNode`。
- 简单任务日志演进为 `ActionGraph`。
- 工具输出开始区分原始输出、摘要和可引用证据。
- 每次修改和验证都能追溯到具体步骤。

这一步的重点不是引入复杂调度，而是让任务过程不再只存在于聊天 transcript 中。

### 2.3 第三阶段：引入证据、事实和上下文投影

在执行轨迹稳定后，引入 `Observation`、`Evidence`、`Fact` 和 `ContextProjection`：

- 工具输出先成为 `Observation`。
- 可追溯、带边界的材料成为 `Evidence`。
- 经过规则确认后才成为 `Fact`。
- LLM 输入来自 `ContextProjection`，而不是直接裁剪 transcript。

这一步解决“agent 如何知道自己知道什么”。

### 2.4 第四阶段：引入副作用、事务和恢复

当 Fluxcode 开始处理更多文件修改、命令执行、Git 操作或外部 API 时，再引入 `EffectLedger`、`TransactionManager` 和 `Reconciler`：

- mutating action 在执行前有 effect 声明。
- 文件修改绑定 overlay 或 transaction。
- 验证失败、外部文件变化、partial effect 和 stale fact 可以触发恢复或人工接管。

这一步解决“agent 做过什么、能否恢复、不能恢复时如何交给人”。

### 2.5 第五阶段：形成 harness-native runtime

当上述能力稳定后，Fluxcode 才进入完整 harness-native runtime 形态：

- `ActionGraph` 成为执行账本、调度表面、恢复入口和 UX 表面。
- `StateStore` 管理 facts、evidence 和生命周期。
- `Scheduler` 基于依赖、gate、预算和状态调度节点。
- `EffectLedger` 和 `TransactionManager` 管理副作用与提交边界。
- `Reconciler` 处理 graph、fact、effect、transaction 与现实之间的失配。

## 3. 基础 code agent 工作模型

早期 Fluxcode 的工作模型应保持直接、可解释、可测试。

这个阶段应对齐成熟 conversation-native code agent 的基础能力基线：保留 ReAct query loop、统一 tool contract、权限前置、文件和 shell 安全、session recovery、上下文预算和 CLI / headless 复用。Fluxcode 不应在 `v0.1` 为了未来 runtime 抽象牺牲这些基础交互能力。

Fluxcode 的差异点是给 ReAct 增加 phase artifact boundary：模型仍通过工具循环完成探索和修改，但每个阶段结束必须产出结构化对象。这样可以先获得 Claude Code 类系统的可用性，同时为后续 `ActionNode`、`Evidence`、`EffectRecord` 和 `ReconcileDecision` 留下可迁移数据。

| 阶段 | 行为 | 最小产物 |
| --- | --- | --- |
| 任务接收 | 记录目标、范围、验收条件和非目标 | `TaskSpec` |
| 仓库理解 | 搜索文件、读取文档、定位相关代码和测试 | `StepTrace`、上下文摘要 |
| 计划 | 给出短计划或内部步骤列表 | task steps |
| 修改 | 生成并应用小范围 patch | diff、修改理由 |
| 验证 | 运行声明存在且用户允许的测试或构建命令 | `VerificationResult` |
| 交付 | 汇报变更、证据、风险、阻塞和后续建议 | handoff summary |

这个模型允许内部先简单实现，但必须保留未来演进所需的记录点：每一步做了什么、为什么做、调用了什么工具、产生了什么输出、是否修改了文件、验证结果是什么。

## 4. Runtime 概念如何从基础实现演进

| 早期实现对象 | 演进后的 runtime 对象 | 何时引入 |
| --- | --- | --- |
| `TaskSpec` | `TaskSpec` + graph objective | 从 v0.1 保留 |
| `StepTrace` | `ActionNode` | 当需要恢复、阻塞、重试和审计时 |
| task log | `ActionGraph` | 当步骤之间出现依赖和验证关系时 |
| tool output | `Observation` | 当需要区分原始观察和结论时 |
| verified output | `Evidence` | 当需要引用验证、文件片段或用户确认时 |
| accepted claim | `Fact` | 当上下文需要生命周期和可信度时 |
| prompt context | `ContextProjection` | 当 transcript 裁剪开始不可靠时 |
| write summary | `EffectRecord` | 当副作用需要审计或恢复时 |
| patch batch | `OverlayRevision` / `Transaction` | 当修改需要 commit gate 或 rollback 时 |
| retry note | `ReconcileDecision` | 当失败不能靠重新 prompt 解决时 |

## 5. 与普通 `ReAct` agent 的关系

`ReAct` / transcript-driven loop 适合早期探索和局部工具调用。Fluxcode 可以在基础 code agent 阶段使用这种策略，但不应把它当成长期架构的唯一状态载体。

因此，Fluxcode 的 `v0.1` 推荐方案不是“去 ReAct”，而是 **phase-gated ReAct**：外层 phase runner 管理阶段边界、预算、权限和 artifact schema，内层 query loop 保留模型与工具的多轮交互。

演进约束如下：

- v0.1 可以是线性 agent loop，但必须留下结构化 step trace。
- v0.2 开始，重要步骤应能映射到 `ActionNode`。
- v0.3 开始，工具输出和模型推断不能直接成为 `Fact`。
- v0.4 以后，mutating effect、transaction 和恢复不能只靠自然语言描述。
- 完整 runtime 中，node-level bounded ReAct 只能作为 `NodeExecutor` 的局部执行策略。

## 6. 模块关系

Fluxcode 的模块不要求同时完整实现，但它们的演进方向应保持一致。当前 / 近期模块设计放在 [`modules/`](./modules/README.md)，已接受的长期 runtime 目标放在 [`runtime-evolution/`](./runtime-evolution/README.md)。

| 模块 / 对象 | 早期角色 | 成熟角色 |
| --- | --- | --- |
| `NodeExecutor` | 线性执行任务步骤，调用文件、搜索、编辑和验证能力 | 执行单个 `ActionNode`，支持 deterministic、single-decision、exploratory profile |
| `Capability Adapter` | 包装本地文件、搜索、shell、Git、LSP 等基础工具 | 外部能力 anti-corruption layer，输出 runtime-native 对象 |
| `Policy Core and Guard` | 约束 LLM 不直接越权写文件、跑命令或扩大范围 | 生成并校验结构化 `PolicyDecision` |
| `ActionGraph` | 从 `StepTrace` 开始，记录关键步骤 | 执行账本、调度表面、恢复入口和 UX 表面 |
| `StateStore` | 保存任务摘要、验证结果和少量证据引用 | 管理 `Observation`、`Evidence`、versioned `Fact` |
| `ContextProjection` | 组织当前任务最小上下文 | 生成带来源、预算和信任边界的 LLM 输入 |
| `EffectLedger` | 记录文件修改和命令执行摘要 | 管理 effect declaration、observed effect 和补偿状态 |
| `TransactionManager` | 管理 patch 批次和提交前验证 | 管理 `OverlayRevision`、checkpoint、commit、rollback |
| `Scheduler` | 早期可以是线性 next-step runner | 基于依赖、gate、预算和恢复状态调度节点 |
| `Reconciler` | 记录失败和阻塞原因 | 处理 graph、fact、effect、transaction 失配 |

## 7. 外部协作与治理边界

Fluxcode 与既有工程系统协作，但不替代它们。

| 外部对象 / 系统 | 在 Fluxcode 中的角色 | 不能做什么 |
| --- | --- | --- |
| 文档、需求说明、设计稿 | 用户意图、约束、验收背景 | 不能直接成为内部 `Fact` |
| Issue / 项目管理项 | 外部任务和协作状态 | 不能替代 Fluxcode 的执行记录 |
| PR / code review | 外部审查意见和合入上下文 | 不能绕过本地验证和人工判断 |
| CI / 测试 / 静态检查 | 验证信号和失败证据 | 不能自动证明所有推断成立 |
| 审批 / 合规 / 发布流程 | 外部治理 gate | 不能成为 Fluxcode 内部 `Control Plane Authority` |
| 评论 / 聊天 / 人工确认 | 人工反馈和确认 | 不能绕过证据和记录边界 |

## 8. 当前设计不变量

- Fluxcode externally 是 code agent `Data Plane`，不是外部工程治理 `Control Plane`。
- `Control Plane Authority` 必须限定为 Fluxcode internal runtime authority，并且是渐进式引入的目标。
- v0.1 的优先级是基础可工作 code agent，而不是完整 runtime kernel。
- `ActionGraph`、`StateStore`、`Scheduler`、`EffectLedger`、`TransactionManager`、`Reconciler` 是演进方向，不应在早期实现中制造不必要复杂度。
- 执行记录、工具调用、文件修改和验证结果从第一阶段起就应保留可追溯入口。
- 设计文档不得暗示 runtime 能力已经全部实现。

## 9. 非目标

- 不把 Fluxcode 设计为外部工程治理 `Control Plane`。
- 不替代 repo permissions、CI、code review、compliance、release 或 deployment gates。
- 不要求 v0.1 一次性交付完整 harness-native runtime。
- 不把 `ActionGraph` 设计成全知状态数据库。
- 不把 prompt transcript、模型记忆或工具日志作为长期事实生命周期系统。
- 不把 agent-level global ReAct loop 作为最终 runtime 主控制器。

## 10. 下钻文档索引

| 文档 | 角色 |
| --- | --- |
| 本文 | 当前正式架构总览，定义 code-agent-first 的演进路线、模块渐进关系、外部边界和非目标 |
| [`../milestones/targets/runtime-kernel-roadmap-v0.1-v0.5.md`](../milestones/targets/runtime-kernel-roadmap-v0.1-v0.5.md) | v0.1-v0.5 从基础 code agent 到 harness-native runtime 的阶段目标 |
| [`../milestones/targets/runtime-kernel-task-breakdown.md`](../milestones/targets/runtime-kernel-task-breakdown.md) | 独立任务拆分，记录每个版本的任务、依赖、验收和非目标 |
| [`../milestones/targets/v0.1-implementation-plan-review.md`](../milestones/targets/v0.1-implementation-plan-review.md) | v0.1 实现计划与技术评审，覆盖选型、依赖、风险和测试计划 |
| [`modules/code-agent-loop.md`](./modules/code-agent-loop.md) | `Code Agent Loop` 模块设计，定义 `Intake -> Understand -> Plan -> Edit -> Verify -> Handoff` |
| [`../milestones/targets/v0.1-engineering-baseline.md`](../milestones/targets/v0.1-engineering-baseline.md) | v0.1 可运行工程基线目标，定义 provider、config、tools、prompt 和 context 压缩方案 |
| [`runtime-evolution/README.md`](./runtime-evolution/README.md) | 已接受的长期 runtime 演进目标入口，区分于当前 / 近期模块设计和普通未采纳提案 |
| [`runtime-evolution/modules/action-graph.md`](./runtime-evolution/modules/action-graph.md) | `ActionGraph` / `ActionNode` 如何从 `StepTrace` 演进 |
| [`runtime-evolution/modules/state-store.md`](./runtime-evolution/modules/state-store.md) | `StateStore`、`Observation`、`Evidence`、`Fact` 的渐进设计 |
| [`runtime-evolution/modules/scheduler.md`](./runtime-evolution/modules/scheduler.md) | `Scheduler` 如何从线性 runner 演进为调度器 |
| [`runtime-evolution/modules/effect-ledger.md`](./runtime-evolution/modules/effect-ledger.md) | `EffectLedger` 如何从修改记录演进为副作用账本 |
| [`runtime-evolution/modules/transaction-manager.md`](./runtime-evolution/modules/transaction-manager.md) | `TransactionManager` 如何从 patch 批次演进为事务边界 |
| [`runtime-evolution/modules/reconciler.md`](./runtime-evolution/modules/reconciler.md) | `Reconciler` 如何从失败记录演进为恢复机制 |
| [`runtime-evolution/modules/policy-core-and-guard.md`](./runtime-evolution/modules/policy-core-and-guard.md) | `PolicyDecision`、policy core、guard 和 gate 的渐进设计 |
| [`runtime-evolution/modules/capability-adapter.md`](./runtime-evolution/modules/capability-adapter.md) | capability adapter、工具调用边界和 runtime-native 输出设计 |
| [`runtime-evolution/modules/context-projection.md`](./runtime-evolution/modules/context-projection.md) | `ContextProjection` 上下文投影设计 |
| [`runtime-evolution/modules/node-executor.md`](./runtime-evolution/modules/node-executor.md) | `NodeExecutor` 执行 profile 和 bounded ReAct 演进设计 |
