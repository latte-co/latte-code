# Runtime 演进目标：NodeExecutor

## 文档状态

本文定义 `NodeExecutor` 的渐进式设计。v0.1 可以是基础 agent loop 的执行器；随着 `StepTrace` 演进为 `ActionNode`，它再收敛为单节点执行组件。

英文对应文档：[`docs/en-US/design/runtime-evolution/modules/node-executor.md`](../../../../en-US/design/runtime-evolution/modules/node-executor.md)。

## 演进节奏

| 阶段 | 形态 | 目标 |
| --- | --- | --- |
| v0.1 | basic agent executor | 执行搜索、读取、编辑、验证和汇报步骤 |
| v0.2 | trace-aware executor | 每一步产出结构化 `StepTrace` |
| v0.5 | `NodeExecutor` | 执行 scheduler 派发的单个 `ActionNode` |

## 责任

- 在早期执行基础 code agent 的线性步骤。
- 在成熟阶段由 `Scheduler` 授权后执行单个 `ActionNode`。
- 根据 node profile 选择 deterministic、single-decision 或 exploratory 执行路径。
- 通过 `Capability Adapter` 调用能力，并输出 `Event`、`Observation`、`EvidenceRef`、`ActionResult`。
- 对 exploratory node 执行 node-level bounded ReAct mini-loop。

## 非目标

- 不拥有全局调度权。
- 不直接晋升 `Fact`。
- 不绕过 `EffectLedger` 声明副作用。
- 不直接 commit、rollback 或补偿 transaction。
- 不把 agent-level / global ReAct 作为最终 runtime 主控制器。

## Execution profiles

| Profile | 适用场景 | LLM 使用 | mini-loop |
| --- | --- | --- | --- |
| `deterministic` | 输入、能力和输出契约已确定，如固定验证、格式转换 | 不需要 | 无 |
| `single_decision` | 需要一次 LLM `PolicyDecision`，如选择补充上下文或生成候选 edit | 一次 | 无 |
| `exploratory` | 需要局部探索、召回、轻量验证或试探性阅读 | 可多步但受限 | bounded ReAct mini-loop |

## Bounded ReAct mini-loop contract

`exploratory` profile 可以使用局部 ReAct，但必须满足：

- 有固定 step budget 和 timeout。
- 有 capability allowlist。
- 每步必须绑定当前 step / `ActionNode`。
- 每步输出只能是 `Event`、`Observation`、`PolicyDecision`、`EvidenceRef` 或 loop-local hypothesis。
- mini-loop 结束条件必须明确：完成、预算耗尽、需要用户、需要 reconcile、失败。
- 任何候选事实必须交给 `TrustGate` / promotion rule，不能直接写入 active `Fact`。
- 任何 mutating effect 必须先进入 `EffectLedger`，不能由 loop 内工具调用裸执行。

## 最小数据契约

```ts
type NodeExecutionProfile = "deterministic" | "single_decision" | "exploratory";

type ActionResult = {
  stepId: string;
  status: "done" | "failed" | "blocked" | "needs_reconcile";
  observations: string[];
  evidenceRefs: string[];
  effectRefs: string[];
};
```

## 不变量

- executor 只能执行当前任务范围内的 step / node。
- executor 不得修改全局 ready queue。
- executor 不得直接写入 active `Fact`。
- executor 不得绕过 commit / rollback boundary。
- exploratory mini-loop 的每一步必须可审计。

## 验收方向

- v0.1 能完成端到端代码任务。
- v0.2 每一步都有结构化 trace。
- 三类 profile 路径可独立测试。
- mini-loop 产物不会直接进入 active fact store。
- mutating capability 缺少 effect declaration 时执行失败。

## 与其他模块关系

- `Scheduler` 决定何时运行 node 和采用哪个 profile。
- `Capability Adapter` 执行具体能力。
- `EffectLedger` 记录 mutating effect。
- `StateStore` 接收 observation / evidence，但 fact promotion 由 gate 控制。
- `Reconciler` 处理 node execution 返回的 `needs_reconcile`。
