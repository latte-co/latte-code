# Runtime Kernel Roadmap v0.1-v0.5

## 文档状态

本文定义 Fluxcode `v0.1` 至 `v0.5` 的 runtime kernel 阶段目标和跨版本不变量。它是 roadmap / architecture design，不表示当前实现已经完成。详细任务拆分见 [`runtime-kernel-task-breakdown.md`](./runtime-kernel-task-breakdown.md)。

英文对应文档：[`docs/en-US/design/runtime-kernel-roadmap-v0.1-v0.5.md`](../../en-US/design/runtime-kernel-roadmap-v0.1-v0.5.md)。

## 1. 路线总览

Fluxcode 的路线从 `v0.1` 起建立最小 internal runtime authority。线性执行可以接受，但权威边界不能缺失。

```text
v0.1 Linear Internal Runtime Authority
  -> v0.2 Capability / Effect / Transaction Hardening
  -> v0.3 Fact / Evidence / Reconcile
  -> v0.4 Scheduler / UX / Multi-executor
  -> v0.5 Evaluation / Security / Adapter Boundary
```

参考系：从整个软件工程系统视角，Fluxcode 是 code agent `Data Plane`；本文中的 `Control Plane Authority` 仅指 Fluxcode internal runtime authority，不指外部 repo / CI / review / deployment 治理平面。

## 2. 核心原则

- Fluxcode externally 是执行型 data-plane 组件，不取代外部工程控制系统。
- Internal runtime authority 负责 `StateStore`、`Scheduler`、`EffectLedger`、`TransactionManager`、`Reconciler`。
- `ActionGraph` 是执行账本、调度表面和 UX 表面，不是全知状态容器。
- `transcript` / session 不是 source of truth。
- 工具输出先成为 `Observation` 或 `Evidence`，不能直接成为 `Fact`。
- `Fact` 只能通过 `TrustGate` / promotion rule 晋升。
- ReAct 是执行策略，不是 runtime architecture；仅允许 node-level bounded ReAct 用于 exploratory `ActionNode`。

## 3. 版本表

| Version | Theme | 主要目标 | 不做什么 |
| --- | --- | --- | --- |
| `v0.1` | Linear Internal Runtime Authority | 建立顶层参考系、最小 graph / state / effect / transaction / reconcile 闭环，以及 `NodeExecutor` profiles | 不做完整并行、完整 OS sandbox、multi-agent 平台、global ReAct 主控制器 |
| `v0.2` | Capability / Effect / Transaction Hardening | 强化 capability contract、effect ledger、overlay / transaction、permission 和 sandbox contract | 不追求工具数量或 marketplace |
| `v0.3` | Fact / Evidence / Reconcile | 完善 fact lifecycle、promotion protocol、stale detection 和四类 reconcile | 不把失败简化为 prompt retry |
| `v0.4` | Scheduler / UX / Multi-executor | 引入受控调度、graph cockpit、executor profile、human handoff UX | 不做无界自治或聊天式 multi-agent fan-out |
| `v0.5` | Evaluation / Security / Adapter Boundary | 架构差异化评测、agency security、anti-corruption layer、adapter boundary policy | 不以 benchmark 通过率替代 runtime invariant |

## 4. `v0.1`: Linear Internal Runtime Authority

### 4.1 目标

`v0.1` 应证明：即使调度是线性的、能力很少、UI 很轻，Fluxcode 仍由 internal runtime authority 维护事实、调度、副作用、事务和恢复边界，而不是由 transcript 或 global ReAct loop 驱动。

### 4.2 必须建立的能力

- 参考系：文档和类型命名保持 code agent `Data Plane` 与 internal runtime authority 边界。
- `ActionGraph` / `ActionNode`：每个执行动作可账本化、可审计、可恢复。
- `PolicyDecision` / `PolicyGuard`：LLM 输出受 closed sum type 与 guard 约束。
- `StateStore`：记录 `Observation`、`Evidence`、versioned `Fact`。
- Promotion：mini-loop、工具输出和模型推断不能直接成为 `Fact`。
- `EffectLedger`：mutating action 执行前声明 effect。
- `TransactionManager`：文件写入绑定 overlay / transaction，提交经过 gate。
- `ContextProjection`：LLM 输入来自 projection，不来自 transcript 裁剪。
- `Reconciler`：处理 graph、fact、effect、transaction 的最小失配。
- `NodeExecutor` profiles：`deterministic`、`single_decision`、`exploratory`，其中 bounded ReAct 只属于 `exploratory` node。

### 4.3 验收方向

- every executed action has an `ActionNode`。
- every mutating action has an `EffectRecord` before execution。
- every active `Fact` has evidence refs and promotion record。
- stale facts do not enter projection as strong facts。
- bounded ReAct mini-loop cannot directly promote `Fact`、commit、rollback 或修改全局调度。

## 5. `v0.2`: Capability / Effect / Transaction Hardening

`v0.2` 强化能力、效果和事务边界，让 runtime 能稳定处理更多真实文件、命令和外部副作用。

验收方向：capability 声明完整输入输出和 failure modes；mutating capability 无法绕过 `EffectLedger`；`reversible=false` effect 需要 approval；stale verification 或 invalid overlay 阻止 commit。

## 6. `v0.3`: Fact / Evidence / Reconcile

`v0.3` 把 `StateStore` 从最小事实记录扩展为可恢复、可解释、可降级的事实系统，并让 `Reconciler` 覆盖 graph、fact、effect、transaction 四类失配。

验收方向：revision 变化标记 facts stale；conflicting facts 不静默覆盖；partial effect 进入 reconcile；prompt retry 不再替代 reconcile。

## 7. `v0.4`: Scheduler / UX / Multi-executor

`v0.4` 在事实和副作用语义稳定后强化调度、graph cockpit 和受控 multi-executor。重点不是“多 agent 聊天分叉”，而是 scheduler 在明确依赖、权限、预算、read/write set 和 transaction 边界下选择执行者。

验收方向：用户能理解 blocked reason；scheduler 不执行 guard 未通过或依赖 stale fact 的 node；多 executor 通过 `ActionGraph`、`Evidence`、`Reconciler` 协作。

## 8. `v0.5`: Evaluation / Security / Adapter Boundary

`v0.5` 证明 Fluxcode 的价值来自 runtime invariant 与可恢复执行语义，而不是只来自模型能力或工具数量。

验收方向：benchmark 同时报告任务结果和 runtime invariant；agency security failure 可被 gate / guard 拦截；外部协议只能通过 adapter 输出 runtime-native 对象；失败可归因到 model、capability、fact、effect、transaction 或 scheduler。

## 9. 跨版本不变量

- Fluxcode externally remains a code-agent `Data Plane`。
- `Control Plane Authority` 必须限定为 internal runtime authority。
- `ActionGraph` 不成为全知状态容器。
- `Observation`、`Evidence`、`Fact` 保持分层。
- LLM 不能直接 syscall、写文件、跑命令、commit、rollback 或访问外部 API。
- mutating action 必须先有 `ActionNode`、`EffectRecord` 和 transaction / overlay boundary。
- Node-level bounded ReAct 只可作为 exploratory `ActionNode` 的局部 execution strategy。
- 代码实现 runtime 概念时必须同步更新相应设计文档；正式中英文文档必须保持结构和语义对齐。
