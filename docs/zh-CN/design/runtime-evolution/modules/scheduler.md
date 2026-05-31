# Runtime 演进目标：Scheduler

## 文档状态

本文定义 `Scheduler` 的渐进式设计。v0.1 可以是线性 runner；只有当任务步骤、依赖、阻塞、恢复和并发需求真实出现后，才需要完整 `Scheduler`。

英文对应文档：[`docs/en-US/design/runtime-evolution/modules/scheduler.md`](../../../../en-US/design/runtime-evolution/modules/scheduler.md)。

## 演进节奏

| 阶段 | 形态 | 目标 |
| --- | --- | --- |
| v0.1 | linear next-step runner | 按任务步骤顺序执行 |
| v0.2 | guarded runner | 执行前检查路径、命令和能力边界 |
| v0.4 | recovery-aware runner | 遇到 failed / partial / stale 状态时阻塞或请求恢复 |
| v0.5 | `Scheduler` | 基于依赖、gate、预算、状态和恢复结果调度节点 |

## 责任

- 判断哪些 `ActionNode` 可执行、何时执行、由哪个 executor profile 执行。
- 维护 ready queue、blocked reason、retry budget、cancellation 和 resume cursor。
- 读取 gate、fact、effect、transaction 状态，阻止不安全或过期节点继续。
- 把节点交给 `NodeExecutor`，但不让 executor 反向拥有全局调度权。

## 非目标

- v0.1 不做复杂调度器。
- 不让 LLM 自然语言推理决定全局调度。
- 不执行 capability 细节。
- 不直接晋升 fact、声明 effect 或提交 transaction。
- 不做无界 multi-agent fan-out。

## 最小数据契约

```ts
type DispatchDecision =
  | { kind: "RunStep"; stepId: string }
  | { kind: "BlockStep"; stepId: string; reason: string }
  | { kind: "AskUser"; stepId: string; question: string }
  | { kind: "RequestRecovery"; stepIds: string[]; reason: string };
```

## 不变量

- guard 未通过的 step / node 不可执行。
- 依赖 stale / invalidated fact 的 node 不可执行。
- transaction 或 effect 处于 partial / needs_reconcile 时，下游 mutating node 必须阻塞。
- scheduler 决策必须可审计，并引用触发条件。

## 验收方向

- v0.1 线性执行顺序可复盘。
- v0.2 guard failure 进入 reject / ask / escalate，而不是无限 retry。
- v0.5 dependency-aware ready queue 正确阻塞下游。
- resume 后不重复执行已完成且不可幂等节点。

## 与其他模块关系

- 从 `ActionGraph` 读取节点与依赖。
- 查询 `StateStore` 中 fact lifecycle。
- 查询 `EffectLedger` / `TransactionManager` 的风险状态。
- 调用 `NodeExecutor` 执行单个 node。
- 将失配交给 `Reconciler`。
