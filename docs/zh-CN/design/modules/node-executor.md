# 模块技术设计：NodeExecutor

## 文档状态

当前设计占位，用于实现前明确单个 `ActionNode` 的执行策略。本文属于 Fluxcode internal runtime 设计；Fluxcode externally 仍是 code agent `Data Plane`。

英文对应文档：[`docs/en-US/design/modules/node-executor.md`](../../../en-US/design/modules/node-executor.md)。

## 责任

- 在 `Scheduler` 授权后执行单个 `ActionNode`。
- 根据 node profile 选择 deterministic、single-decision 或 exploratory 执行路径。
- 通过 `Capability Adapter` 调用能力，并输出 `Event`、`Observation`、`EvidenceRef`、`ActionResult`。
- 对 exploratory node 执行 node-level bounded ReAct mini-loop。

## 非目标

- 不拥有全局调度权。
- 不直接晋升 `Fact`。
- 不绕过 `EffectLedger` 声明副作用。
- 不直接 commit、rollback 或补偿 transaction。
- 不把 agent-level / global ReAct 作为 runtime 主控制器。

## Execution profiles

| Profile | 适用场景 | LLM 使用 | mini-loop |
| --- | --- | --- | --- |
| `deterministic` | 输入、能力和输出契约已确定，如格式化、固定验证、确定性转换 | 不需要 | 无 |
| `single_decision` | 需要一次 LLM `PolicyDecision`，如选择补充上下文或生成候选 edit node | 一次 | 无 |
| `exploratory` | 需要局部探索、召回、轻量验证或试探性阅读 | 可多步但受限 | bounded ReAct mini-loop |

## Bounded ReAct mini-loop contract

`exploratory` profile 可以使用局部 ReAct，但必须满足：

- 有固定 step budget 和 timeout。
- 有 capability allowlist。
- 每步必须绑定当前 `ActionNode`。
- 每步输出只能是 `Event`、`Observation`、`PolicyDecision`、`EvidenceRef` 或 loop-local hypothesis。
- mini-loop 结束条件必须明确：完成、预算耗尽、需要用户、需要 reconcile、失败。
- 任何候选事实必须交给 `TrustGate` / promotion rule，不能直接写入 active `Fact`。
- 任何 mutating effect 必须先进入 `EffectLedger`，不能由 loop 内工具调用裸执行。

## 输入 / 输出

| 方向 | 内容 |
| --- | --- |
| 输入 | dispatch decision、`ActionNode`、`ContextProjection`、capability descriptors、budget / guard policy |
| 输出 | `ActionResult`、events、observations、evidence refs、guard / failure signal、reconcile request |

## 核心数据契约

```ts
type NodeExecutionProfile = "deterministic" | "single_decision" | "exploratory";

type ActionResult = {
  nodeId: string;
  status: "done" | "failed" | "blocked" | "needs_reconcile";
  observations: string[];
  evidenceRefs: string[];
  effectRefs: string[];
};
```

## 不变量

- executor 只能执行 scheduler 派发的 node。
- executor 不得修改全局 ready queue。
- executor 不得直接写入 active `Fact`。
- executor 不得绕过 commit / rollback boundary。
- exploratory mini-loop 的每一步必须可审计。

## 失败模式

- mini-loop 无界运行。
- loop-local hypothesis 被误晋升为 fact。
- executor 直接调用工具产生未声明副作用。
- executor 在失败后自行重排全局任务。

## 测试 / 验收方向

- 三类 profile 路径可独立测试。
- exploratory loop 超过预算会停止并返回结构化状态。
- mini-loop 产物不会直接进入 active fact store。
- mutating capability 缺少 effect declaration 时执行失败。

## 与其他模块关系

- `Scheduler` 决定何时运行 node 和采用哪个 profile。
- `Capability Adapter` 执行具体能力。
- `EffectLedger` 记录 mutating effect。
- `StateStore` 接收 observation / evidence，但 fact promotion 由 gate 控制。
- `Reconciler` 处理 node execution 返回的 `needs_reconcile`。
