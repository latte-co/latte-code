# Runtime 演进目标：ActionGraph

## 文档状态

本文定义 `ActionGraph` 的渐进式设计。`ActionGraph` 是长期 runtime 对象，不是 v0.1 必须完整实现的起点。v0.1 可以先使用 `StepTrace`，再在 v0.2-v0.5 逐步演进为正式 graph。

英文对应文档：[`docs/en-US/design/runtime-evolution/modules/action-graph.md`](../../../../en-US/design/runtime-evolution/modules/action-graph.md)。

## 演进节奏

| 阶段 | 形态 | 目标 |
| --- | --- | --- |
| v0.1 | `StepTrace` | 记录关键步骤、工具调用、修改和验证结果 |
| v0.2 | lightweight `ActionNode` | 为步骤增加 id、status、输入输出和依赖 |
| v0.4 | recoverable action records | 标记失败、阻塞、partial effect 和恢复入口 |
| v0.5 | `ActionGraph` | 成为执行账本、调度表面、恢复入口和 UX 表面 |

## 责任

- 表达任务如何从简单步骤演进为 `ActionNode`。
- 保存节点依赖、阻塞、验证和恢复关系。
- 为 `Scheduler` 提供 ready / blocked / failed / completed 的调度表面。
- 作为审计索引连接 `PolicyDecision`、`Evidence`、`EffectRecord`、`Transaction` 和 `Fact`。

## 非目标

- v0.1 不要求完整 DAG 调度。
- 不保存全部 runtime state。
- 不拥有 `Fact` 生命周期。
- 不执行 capability。
- 不直接 commit、rollback 或补偿 effect。

## 最小数据契约

```ts
type StepTrace = {
  id: string;
  title: string;
  status: "pending" | "running" | "blocked" | "done" | "failed";
  inputs: string[];
  outputs: string[];
  toolCallIds: string[];
};

type ActionNode = StepTrace & {
  dependsOn: string[];
  evidenceIds: string[];
  effectIds: string[];
  transactionId?: string;
};
```

## 不变量

- v0.1 起，每个关键行动必须至少有 `StepTrace`。
- 引入 `ActionNode` 后，failed / blocked node 不得被下游节点静默忽略。
- `ActionGraph` 只保存事实、证据、effect、transaction 的引用，不复制其权威状态。
- edge 变化必须触发 scheduler view 重新计算。

## 验收方向

- 用户能从 trace 理解 agent 做了什么。
- 失败步骤能解释阻塞原因。
- v0.5 graph 能从 v0.1-v0.4 的 trace 数据自然迁移。

## 与其他模块关系

- `NodeExecutor` 产生 `StepTrace` / `ActionNode` 结果。
- `Scheduler` 在成熟阶段读取 graph 调度表面。
- `StateStore` 保存 `Fact` / `Evidence` 权威状态，graph 只引用。
- `EffectLedger` / `TransactionManager` 回写 effect 与 transaction reference。
- `Reconciler` 可修改 node status、edge 和阻塞原因。
