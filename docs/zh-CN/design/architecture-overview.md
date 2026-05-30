# Fluxcode 架构设计总览

## 文档状态

本文是 Fluxcode 当前正式设计总览草案，用于统一 `docs/zh-CN/design/` 下路线图、模块技术设计和任务拆分的顶层口径。本文不表示当前 `src/` 已经实现这些能力。

英文对应文档：[`docs/en-US/design/architecture-overview.md`](../../en-US/design/architecture-overview.md)。

## 1. 顶层参考系

从整个软件工程系统视角看，Code Agent 整体属于 `Data Plane`：它读取仓库、调用工具、生成修改、运行验证，并把结果交给人类和既有工程系统判断。Fluxcode 不取代 repo permissions、CI、code review、compliance、release 或 deployment gates。

Fluxcode 内部仍需要一个本地的 runtime authority，也可称为内部 runtime control plane。该内部控制平面只在 Fluxcode 进程和任务执行边界内拥有权威，负责把模型输出、工具观察、文件副作用和人工确认转化为可追踪的 runtime 对象。

因此本文中的 `Control Plane Authority` 一律指 **Fluxcode internal runtime authority**，不是外部工程治理 `Control Plane`。

## 2. 与飞书文档的顶层关系

飞书文档可承载需求说明、评审意见、外部设计稿和人工协作记录。它们属于外部协作与治理材料，不是 Fluxcode runtime 的内部事实源。

| 飞书文档层级 | 在 Fluxcode 中的角色 | 进入 runtime 的方式 |
| --- | --- | --- |
| 产品 / 需求说明 | 用户意图、约束、验收背景 | 解析为 `TaskSpec`、acceptance criteria、user-provided `Observation` |
| 架构 / 设计评审文档 | 外部设计依据或人工决策记录 | 作为带来源的 `Evidence` 或候选 `Fact` 输入，需显式 promotion |
| 任务拆分 / 项目管理文档 | 外部执行计划和协作状态 | 映射为候选 `ActionGraph` / `ActionNode`，由 `Scheduler` 校验依赖 |
| 评审评论 / 审批结论 | 人工信号与 gate 输入 | 进入 `human_approval_gate`、`trust_gate` 或 `validation_gate` |

换言之，飞书文档可以是 Fluxcode 的输入、证据和协作界面，但不能绕过 `StateStore`、`EffectLedger`、`TransactionManager` 或 `Reconciler` 直接改变内部事实和提交状态。

## 3. 顶层 planes / layers

| 层级 | 责任 | Fluxcode 是否拥有外部权威 |
| --- | --- | --- |
| 外部软件工程控制系统 | repo permissions、CI、review、compliance、deployment gates、组织流程 | 否。Fluxcode 只能适配、读取或请求这些系统的结果 |
| Fluxcode data-plane code agent boundary | 执行代码任务、提出修改、运行验证、产出证据和交接信息 | 否。它是工程系统中的执行型 data-plane 组件 |
| Fluxcode internal runtime authority services | `StateStore`、`Scheduler`、`EffectLedger`、`TransactionManager`、`Reconciler` | 仅在 Fluxcode runtime 内部拥有权威 |
| Executor / capability layer | 文件、shell、LSP、Git、MCP、测试运行器、模型调用等能力适配 | 无独立权威，必须通过 adapter 输出 runtime-native 对象 |
| UX / human handoff layer | 展示计划、阻塞、风险、证据、确认请求和恢复建议 | 不替代人工判断；提供可审计的交接面 |

## 4. 核心对象边界

### 4.1 `ActionGraph`

`ActionGraph` 是执行账本、调度表面和 UX 表面，不是全知状态容器。

它负责：

- 记录任务拆成哪些 `ActionNode`。
- 表达依赖、阻塞、验证和 reconcile 关系。
- 为 `Scheduler` 暴露 ready / blocked / failed / completed 节点。
- 作为审计索引，把 `PolicyDecision`、`Evidence`、`EffectRecord`、`Transaction` 和 `Fact` 引回具体行动。
- 让用户知道系统准备做什么、已经做了什么、哪里需要接管。

它不直接拥有 `Fact` 生命周期、effect 权威、提交 / 回滚状态或 reconcile 语义。

### 4.2 Internal runtime authority services

| 服务 | 内部权威 | 不应委托给 |
| --- | --- | --- |
| `StateStore` | `Observation`、`Evidence`、versioned `Fact`、fact lifecycle、`ContextProjection` | transcript、prompt、单个 graph blob |
| `Scheduler` | `ActionNode` 可执行性、依赖、预算、阻塞、恢复点 | LLM 自然语言推理 |
| `EffectLedger` | 文件、shell、network、Git、外部 API、用户确认等 effect 的声明、结果和补偿状态 | 工具日志或聊天记录 |
| `TransactionManager` | `OverlayRevision`、checkpoint、commit、rollback、compensation、transaction status | patch 文本或模型记忆 |
| `Reconciler` | graph / fact / effect / transaction 的失配检测和恢复语义 | 失败后的 prompt retry |

## 5. `Observation → Evidence → Fact` promotion

Fluxcode 不把工具输出、模型推断或飞书文档内容直接当成事实。

| 层级 | 含义 | 默认状态 |
| --- | --- | --- |
| `Observation` | 工具、用户、环境或外部系统产生的原始观察 | 未审查，可能局部、噪声或过期 |
| `Evidence` | 带来源、时间、边界、摘要和 artifact 引用的证据载体 | 可追溯，但仍不等于事实 |
| `Fact` | 经 promotion rule / `TrustGate` 晋升进 `StateStore` 的版本化事实 | 必须有 lifecycle、coverage、confidence、evidenceIds |

LLM 的自然语言推断默认只能成为 `Hypothesis`。mini-loop 中的每一步默认产生 `Event`、`Observation`、`PolicyDecision` 或 `EvidenceRef`；只有通过 `TrustGate` 或明确 promotion rule 后，才可成为 `Fact`。

## 6. Gate taxonomy

| Gate kind | Owner | 典型输入 | 失败语义 |
| --- | --- | --- | --- |
| `validation_gate` | verifier capability / `StateStore` | test、typecheck、LSP、tree-sitter evidence | 补充验证、降级 fact、阻塞节点 |
| `trust_gate` | trust policy / `StateStore` | trust zone、来源、外部 effect scope | escalate 或 abort，不靠 prompt retry |
| `permission_gate` | capability resolver / permission store | capability grant、node scope、用户授权 | reject、ask user 或阻塞 |
| `human_approval_gate` | Human | risk summary、diff、不可补偿 effect | 等待用户选择或中止 |
| `transaction_gate` | `TransactionManager` | overlay status、rollback handle、verification freshness | 阻止 commit / rollback / rebase |
| `reconcile_gate` | `Reconciler` | stale facts、partial effects、invalidated overlay、affected nodes | 先修复状态再继续调度 |

## 7. Node-level bounded ReAct

Fluxcode 反对的是 agent-level / global ReAct 作为 runtime 主控制器；接受的是 node-level bounded ReAct，作为 `NodeExecutor` 执行探索型 `ActionNode` 的局部 execution strategy。

ReAct 是执行策略，不是 runtime architecture。全局调度、事实晋升、副作用声明和提交 / 回滚仍由内部 runtime authority services 负责。

`NodeExecutor` 支持三类 execution profile：

| Profile | 用途 | mini-loop |
| --- | --- | --- |
| `deterministic` | 已知输入、能力和输出契约的确定性节点 | 无 |
| `single_decision` | 需要一次 LLM `PolicyDecision` 的节点 | 无 |
| `exploratory` | 需要局部探索、召回、试探验证的节点 | 有界 ReAct mini-loop |

有界 mini-loop 必须有 step budget、capability allowlist、read/write/effect boundary、evidence policy 和 exit condition。它不能直接晋升 `Fact`，不能绕过 `EffectLedger`，不能直接 commit / rollback，也不能修改全局调度。

## 8. 文档结构关系

| 文档 | 角色 |
| --- | --- |
| 本文 | 当前正式架构总览，定义参考系、层级、核心边界和文档关系 |
| [`modules/`](./modules/action-graph.md) | 各模块实现前的详细技术设计占位，按模块维护输入输出、契约、不变量和验收方向 |
| [`runtime-kernel-roadmap-v0.1-v0.5.md`](./runtime-kernel-roadmap-v0.1-v0.5.md) | v0.1-v0.5 的阶段目标和跨版本不变量 |
| [`runtime-kernel-task-breakdown.md`](./runtime-kernel-task-breakdown.md) | 独立任务拆分，记录每个版本的任务、依赖、验收和非目标 |

## 9. 当前不变量

- Fluxcode externally 是 code agent `Data Plane`，不是外部工程治理 `Control Plane`。
- 所有 `Control Plane Authority` 表述都必须限定为 Fluxcode internal runtime authority。
- `ActionGraph` 是账本、调度表面和 UX 表面，不是全知状态容器。
- `Observation`、`Evidence`、`Fact` 必须分层，`Fact` 只能由 promotion rule / gate 晋升。
- `NodeExecutor` 可使用 node-level bounded ReAct，但 global runtime architecture 不能退化为 agent-level ReAct loop。
- 设计文档是 design draft，不得暗示这些能力已经全部实现。
