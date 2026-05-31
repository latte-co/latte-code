# Runtime 演进目标：Reconciler

## 文档状态

本文定义 `Reconciler` 的渐进式设计。早期只需要记录失败和阻塞原因；当事实、effect、transaction 开始相互影响时，再引入正式 reconcile 机制。

英文对应文档：[`docs/en-US/design/runtime-evolution/modules/reconciler.md`](../../../../en-US/design/runtime-evolution/modules/reconciler.md)。

## 演进节奏

| 阶段 | 形态 | 目标 |
| --- | --- | --- |
| v0.1 | failure / blocker record | 记录失败原因和人工接管点 |
| v0.3 | stale marking | 文件或上下文变化后标记相关 fact 不再可靠 |
| v0.4 | light reconciler | 处理 failed step、partial effect、invalidated patch |
| v0.5 | `Reconciler` | 覆盖 graph、fact、effect、transaction 四类失配 |

## 责任

- 检测 graph、fact、effect、transaction 与现实之间的失配。
- 决定哪些对象仍有效、哪些需要重算、哪些必须撤回、哪些需要人工接管。
- 阻止下游节点继续使用已失效前提。
- 生成可审计 reconcile record。

## 非目标

- v0.1 不要求复杂恢复系统。
- 不是失败后 prompt retry。
- 不替代 `Scheduler` 做正常调度。
- 不直接执行工具、写文件或 commit。
- 不把用户外部修改自动视为验证过的事实。

## 最小数据契约

```ts
type ReconcileDecision = {
  id: string;
  kind: "graph" | "fact" | "effect" | "transaction";
  affectedRefs: string[];
  action: "block" | "mark_stale" | "supersede" | "compensate" | "request_user" | "retry_after_repair";
  reason: string;
};
```

## 不变量

- failed step / node 必须阻塞依赖它的 pending work。
- repo / overlay revision 改变必须触发相关 fact stale 检查。
- partial effect 不得被当成普通失败忽略。
- invalidated transaction 不得继续 commit。

## 验收方向

- v0.1 最终汇报包含失败和阻塞原因。
- v0.4 起 partial effect 触发 compensation 或 human handoff。
- v0.5 graph / fact / effect / transaction 四类 reconcile 有独立用例。
- reconcile record 可解释为什么某个 node 被阻塞或 superseded。

## 与其他模块关系

- 修改 `ActionGraph` node status 和 edge 影响。
- 更新 `StateStore` fact lifecycle。
- 读取 `EffectLedger` failed / partial 状态。
- 阻止 `TransactionManager` 过期 commit。
- 通知 `Scheduler` 重算 ready queue。
