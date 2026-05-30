# 模块技术设计：Scheduler

## 文档状态

当前设计占位，用于实现前明确 `Scheduler` 的调度权威。本文属于 Fluxcode internal runtime 设计；Fluxcode externally 仍是 code agent `Data Plane`。

英文对应文档：[`docs/en-US/design/modules/scheduler.md`](../../../en-US/design/modules/scheduler.md)。

## 责任

- 判断哪些 `ActionNode` 可执行、何时执行、由哪个 executor profile 执行。
- 维护 ready queue、blocked reason、retry budget、cancellation 和 resume cursor。
- 读取 gate、fact、effect、transaction 状态，阻止不安全或过期节点继续。
- 把节点交给 `NodeExecutor`，但不让 executor 反向拥有全局调度权。

## 非目标

- 不让 LLM 自然语言推理决定全局调度。
- 不执行 capability 细节。
- 不直接晋升 fact、声明 effect 或提交 transaction。
- 不做无界 multi-agent fan-out。

## 输入 / 输出

| 方向 | 内容 |
| --- | --- |
| 输入 | `ActionGraph` view、gate status、fact lifecycle、effect / transaction status、budget policy |
| 输出 | dispatch decision、blocked reason、retry / cancellation record、resume cursor |

## 核心数据契约

```ts
type DispatchDecision =
  | { kind: "RunNode"; nodeId: string; executorProfile: string }
  | { kind: "BlockNode"; nodeId: string; reason: string }
  | { kind: "CancelNode"; nodeId: string; reason: string }
  | { kind: "RequestReconcile"; nodeIds: string[]; reason: string };
```

## 不变量

- guard 未通过的 node 不可执行。
- 依赖 stale / invalidated fact 的 node 不可执行。
- transaction 或 effect 处于 partial / needs_reconcile 时，下游 mutating node 必须阻塞。
- scheduler 决策必须可审计，并引用触发条件。

## 失败模式

- ready queue 使用过期 graph view。
- retry budget 被 prompt retry 绕过。
- cancellation 后 effect / transaction 未进入 reconcile。
- executor 越权创建全局调度决策。

## 测试 / 验收方向

- dependency-aware ready queue 正确阻塞下游。
- guard failure 进入 reject / ask / escalate，而不是无限 retry。
- resume 后不重复执行已完成且不可幂等节点。
- 多 executor 调度遵守 read/write conflict policy。

## 与其他模块关系

- 从 `ActionGraph` 读取节点与依赖。
- 查询 `StateStore` 中 fact lifecycle。
- 查询 `EffectLedger` / `TransactionManager` 的风险状态。
- 调用 `NodeExecutor` 执行单个 node。
- 将失配交给 `Reconciler`。
