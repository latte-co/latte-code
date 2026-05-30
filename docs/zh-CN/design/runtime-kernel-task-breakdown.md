# Runtime Kernel 任务拆分 v0.1-v0.5

## 文档状态

本文是 Fluxcode runtime kernel 的独立任务拆分文档，服务于实现排期、验收和依赖管理。它不表示当前实现已经完成。架构参考以 [`architecture-overview.md`](./architecture-overview.md) 为准，路线阶段以 [`runtime-kernel-roadmap-v0.1-v0.5.md`](./runtime-kernel-roadmap-v0.1-v0.5.md) 为准。

英文对应文档：[`docs/en-US/design/runtime-kernel-task-breakdown.md`](../../en-US/design/runtime-kernel-task-breakdown.md)。

## 1. 总体依赖

```text
v0.1 Reference frame + linear internal runtime authority
  -> v0.2 Capability / Effect / Transaction hardening
  -> v0.3 Fact / Evidence / Reconcile
  -> v0.4 Scheduler / UX / Multi-executor
  -> v0.5 Evaluation / Security / Adapter Boundary
```

全版本共同前提：Fluxcode externally 是 code agent `Data Plane`；内部 `Control Plane Authority` 仅表示 Fluxcode internal runtime authority。

## 2. `v0.1`: Linear Internal Runtime Authority

### 2.1 任务列表

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.1-doc-ref-frame` | 建立设计文档参考系：Code Agent externally 为 `Data Plane`，`Control Plane Authority` 限定为 internal runtime authority | 无 | README、overview、roadmap、module docs 均按 data-plane code agent + internal runtime authority 口径描述 Fluxcode |
| `v0.1-node-executor-profiles` | 定义 `deterministic`、`single_decision`、`exploratory` 三类 `NodeExecutor` profile | `v0.1-doc-ref-frame` | `node-executor.md` 说明 bounded ReAct mini-loop 只适用于 exploratory node |
| `v0.1-action-graph` | 建立最小 `ActionGraph` / `ActionNode` contract | `v0.1-doc-ref-frame` | 每个执行动作都有 `ActionNode`，graph 只保存引用，不持有全部事实和副作用 |
| `v0.1-policy-core-guard` | 建立最小 `PolicyDecision` sum type 与 `PolicyGuard` 失败语义 | `v0.1-action-graph` | LLM 只能输出受约束决策，不能直接 syscall、写文件、commit 或 rollback |
| `v0.1-state-store` | 建立 `Observation`、`Evidence`、versioned `Fact` 最小模型 | `v0.1-doc-ref-frame` | active `Fact` 必须有 `evidenceIds`、lifecycle、coverage、confidence |
| `v0.1-promotion` | 建立初始 promotion rule / `TrustGate` | `v0.1-state-store` | 工具输出和 mini-loop 步骤不会直接成为 `Fact` |
| `v0.1-effect-ledger` | 建立最小 `EffectRecord` 和 effect declaration path | `v0.1-action-graph` | mutating action 执行前必须先声明 effect |
| `v0.1-transaction` | 建立最小 `OverlayRevision` / `Transaction` 边界 | `v0.1-effect-ledger` | 文件写入绑定 overlay 或 transaction，提交前经过 `transaction_gate` |
| `v0.1-context-projection` | 用 `ContextProjection` 替代 transcript 裁剪作为 LLM 输入 | `v0.1-state-store` | stale / invalidated fact 不作为强事实进入 projection |
| `v0.1-light-reconciler` | 支持 graph / fact / effect / transaction 的轻量 reconcile | `v0.1-state-store`, `v0.1-effect-ledger`, `v0.1-transaction` | failed / partial / stale / invalidated 状态会阻塞或修复下游节点 |

### 2.2 非目标

- 完整并行 scheduler。
- 完整 OS sandbox。
- 大规模 multi-agent 或 executor fleet。
- 自动 PR、发布流水线或远程 server。
- 把 global ReAct loop 当作 runtime 主控制器。

## 3. `v0.2`: Capability / Effect / Transaction Hardening

### 3.1 任务列表

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.2-capability-contract` | 完善 primitive / semantic capability schema | `v0.1-policy-core-guard` | capability 声明 input/output、pre/post、evidence requirement、failure modes |
| `v0.2-capability-state` | 引入 `declared` / `observed` / `effective` 状态 | `v0.2-capability-contract` | degraded / blocked capability 会进入 `Scheduler` 阻塞或降级路径 |
| `v0.2-effect-model` | 区分 expected / observed / effective effect | `v0.1-effect-ledger` | declared 与 observed effect 不一致时进入 effect reconcile |
| `v0.2-sandbox-contract` | 定义 S0-S4 sandbox level 语义 | `v0.2-capability-contract` | 每次能力执行记录 sandbox boundary |
| `v0.2-permission-model` | 按 node / capability scope 授权 | `v0.2-capability-contract` | 无权限 action 由 `permission_gate` 阻断，不通过 prompt retry |
| `v0.2-transaction-hardening` | 强化 overlay diff、rollback handle、commit gate、compensation marker | `v0.1-transaction` | stale verification 或 invalid overlay 阻止 commit |

### 3.2 非目标

- 追求工具数量或 marketplace。
- 用工具适配绕过 `EffectLedger`。
- 把 sandbox level 误写成已经具备完整 OS 隔离。

## 4. `v0.3`: Fact / Evidence / Reconcile

### 4.1 任务列表

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.3-promotion-rules` | 为 test、typecheck、LSP、tree-sitter、user confirmation 定义 promotion rule | `v0.1-promotion` | 每类 active `Fact` 可解释其证据来源和覆盖范围 |
| `v0.3-fact-lifecycle` | 完善 stale、superseded、invalidated、retracted 触发和历史 | `v0.1-state-store` | revision / overlay 变化会更新相关 fact lifecycle |
| `v0.3-conflict-handling` | 支持冲突 facts 并存、降级、撤回或请求验证 | `v0.3-fact-lifecycle` | 冲突事实不会静默覆盖 |
| `v0.3-evidence-store` | 支持 evidence summary、artifact ref、raw hash、revision binding | `v0.1-state-store` | evidence 可追溯到 producer、boundary 和 action node |
| `v0.3-reconcile-protocol` | 实现 graph / fact / effect / transaction reconcile 统一入口 | `v0.1-light-reconciler`, `v0.2-effect-model` | partial effect、stale fact、invalid overlay 都能进入 reconcile |
| `v0.3-entropy-control` | 清理 dead pending nodes、failed speculation、duplicate evidence、outdated assumptions | `v0.3-reconcile-protocol` | 过期中间状态不会污染新 projection |

### 4.2 非目标

- 把失败简化为 prompt retry。
- 把 recall 信号当成 verify 结果。
- 用高置信 `Fact` 表达未验证模型推断。

## 5. `v0.4`: Scheduler / UX / Multi-executor

### 5.1 任务列表

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.4-ready-queue` | 支持 dependency-aware ready queue、retry budget、blocked reason、cancellation、resume cursor | `v0.3-reconcile-protocol` | scheduler 不执行 guard 未通过或依赖 stale fact 的 node |
| `v0.4-conflict-policy` | 基于 `readSet` / `writeSet` 检测冲突，支持 fail / rebase / merge / ask | `v0.2-transaction-hardening` | 写冲突不被静默覆盖 |
| `v0.4-ux-cockpit` | 展示 Action Graph、Fact/Evidence、Effect/Transaction、Reconcile、Escalation 视图 | `v0.4-ready-queue` | 用户无需读原始日志即可理解 blocked 原因和下一步 |
| `v0.4-human-handoff` | 标准化 `ApprovalRequest`、pre-escalation duty、用户 override 后 reconcile | `v0.2-permission-model`, `v0.3-reconcile-protocol` | 用户确认成为可审计 evidence / gate record |
| `v0.4-executor-profile` | 落地 executor capability、permission、evidence policy、result schema | `v0.1-node-executor-profiles` | executor 通过 profile 受控执行，不共享未结构化上下文 |
| `v0.4-multi-executor` | 在 transaction 和 effect ledger 约束下支持受控并行或角色化 executor | `v0.4-executor-profile`, `v0.4-conflict-policy` | 多 executor 通过 `ActionGraph`、`Evidence`、`Reconciler` 协作 |

### 5.2 非目标

- 无界自治。
- 聊天式 multi-agent fan-out。
- 让 executor 直接拥有全局调度权。

## 6. `v0.5`: Evaluation / Security / Adapter Boundary

### 6.1 任务列表

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.5-invariant-eval` | 自动检查 action、effect、fact、transaction、guard、projection、adapter 不变量 | `v0.4-ready-queue` | benchmark 报告同时展示任务结果和 runtime invariant 结果 |
| `v0.5-external-benchmark` | 记录完成率、验证通过率、确认次数、回滚次数、stale fact 检测、harness cost / latency | `v0.5-invariant-eval` | 指标不替代 invariant，只作为外部表现 |
| `v0.5-architecture-demos` | 展示 overlay rollback、stale fact reconcile、不可补偿 effect escalation | `v0.3-reconcile-protocol`, `v0.2-transaction-hardening` | 每个 demo 可绑定 evidence 和 gate 记录 |
| `v0.5-agency-security` | 防 tool poisoning、prompt-in-tool-output、cross-tool data exfiltration、evidence injection | `v0.2-sandbox-contract`, `v0.2-permission-model` | agency security 失败可被 guard / gate 拦截并审计 |
| `v0.5-anti-corruption` | 稳定 MCP、OpenAI tool-call、LSP、Git、IDE、shell、test runner adapter 边界 | `v0.2-capability-contract` | 外部协议只能通过 adapter 输出 runtime-native 对象 |
| `v0.5-adapter-boundary-policy` | 明确外部协议字段不直接进入内部 store / ledger / transaction manager | `v0.5-anti-corruption` | 能解释失败归因于 model、capability、fact、effect、transaction 或 scheduler |

### 6.2 非目标

- 以 benchmark 通过率替代 runtime invariant。
- 把 Fluxcode 描述为外部 CI / review / deployment gate。
- 让外部协议直接写入内部 `StateStore`、`EffectLedger` 或 `TransactionManager`。

## 7. 跨版本验收清单

- 所有当前正式设计文档保持中英文结构对齐。
- 所有 `Control Plane Authority` 表述限定为 internal runtime authority。
- 所有 mutating action 先进入 `ActionNode`、`EffectLedger` 和 transaction / overlay boundary。
- bounded ReAct 仅作为 `exploratory` node 的局部执行策略。
- mini-loop 结果不能直接成为 `Fact`。
- README 只指向当前维护的正式设计文档或明确非设计类调研文档。
