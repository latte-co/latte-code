# 模块技术设计：ActionGraph

## 文档状态

当前设计占位，用于实现前明确 `ActionGraph` 的边界。本文属于 Fluxcode internal runtime 设计；从外部软件工程系统视角看，Fluxcode 仍是 code agent `Data Plane`，不取代 CI、review、权限或部署控制。

英文对应文档：[`docs/en-US/design/modules/action-graph.md`](../../../en-US/design/modules/action-graph.md)。

## 责任

- 表达任务如何拆成 `ActionNode`。
- 保存节点依赖、阻塞、验证和 reconcile 关系。
- 为 `Scheduler` 提供 ready / blocked / failed / completed 的调度表面。
- 作为审计索引连接 `PolicyDecision`、`Evidence`、`EffectRecord`、`Transaction` 和 `Fact`。
- 为 UX 展示执行计划、进度和人工接管点。

## 非目标

- 不保存全部 runtime state。
- 不拥有 `Fact` 生命周期。
- 不执行 capability。
- 不直接 commit、rollback 或补偿 effect。

## 输入 / 输出

| 方向 | 内容 |
| --- | --- |
| 输入 | `TaskSpec`、`PolicyDecision`、module / capability metadata、reconcile updates |
| 输出 | `ActionNode` records、edge records、node status、audit references、scheduler view |

## 核心数据契约

```ts
type ActionGraph = {
  graphId: string;
  objective: string;
  nodes: Record<string, ActionNode>;
  edges: Array<{ from: string; to: string; kind: "depends_on" | "blocks" | "verifies" | "reconciles" }>;
  status: "planning" | "running" | "blocked" | "completed" | "failed" | "reconciling";
};
```

`ActionNode` 必须至少携带 capability、read/write set、policy reference、effect handle、rollback handle 和 status。

## 不变量

- 每个执行过的 runtime action 必须能追溯到一个 `ActionNode`。
- `ActionGraph` 只保存事实、证据、effect、transaction 的引用，不复制其权威状态。
- edge 变化必须触发 scheduler view 重新计算。
- failed / blocked node 不得被下游节点静默忽略。

## 失败模式

- node 引用不存在的 capability、fact 或 evidence。
- edge 形成循环或失效依赖。
- node status 与 `EffectLedger` / `TransactionManager` 状态不一致。
- graph 恢复时丢失阻塞原因。

## 测试 / 验收方向

- 创建、更新、恢复 graph 后，所有 node reference 可解析。
- failed node 会阻塞依赖节点。
- reconcile update 能标记 affected nodes。
- UX view 能从 graph 中解释当前执行进度和 blocked reason。

## 与其他模块关系

- `Scheduler` 读取 graph 调度表面。
- `StateStore` 保存 `Fact` / `Evidence` 权威状态，graph 只引用。
- `EffectLedger` / `TransactionManager` 回写 effect 与 transaction reference。
- `Reconciler` 可修改 node status、edge 和阻塞原因。
