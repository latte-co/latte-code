# 模块技术设计：Reconciler

## 文档状态

当前设计占位，用于实现前明确 `Reconciler` 的失配检测和恢复边界。本文属于 Fluxcode internal runtime 设计；Fluxcode externally 仍是 code agent `Data Plane`。

英文对应文档：[`docs/en-US/design/modules/reconciler.md`](../../../en-US/design/modules/reconciler.md)。

## 责任

- 检测 graph、fact、effect、transaction 与现实之间的失配。
- 决定哪些对象仍有效、哪些需要重算、哪些必须撤回、哪些需要人工接管。
- 阻止下游节点继续使用已失效前提。
- 生成可审计 reconcile record。

## 非目标

- 不是失败后 prompt retry。
- 不替代 `Scheduler` 做正常调度。
- 不直接执行工具、写文件或 commit。
- 不把用户外部修改自动视为验证过的事实。

## 输入 / 输出

| 方向 | 内容 |
| --- | --- |
| 输入 | failed / partial effect、stale fact signal、invalid overlay、graph status mismatch、user override |
| 输出 | reconcile decision、affected nodes、fact lifecycle update、transaction block / repair request |

## 核心数据契约

```ts
type ReconcileDecision = {
  id: string;
  kind: "graph" | "fact" | "effect" | "transaction";
  affectedRefs: string[];
  action: "block" | "supersede" | "mark_stale" | "compensate" | "request_user" | "retry_after_repair";
  reason: string;
};
```

## 不变量

- failed node 必须阻塞依赖它的 pending nodes。
- repo / overlay revision 改变必须触发相关 fact stale 检查。
- partial effect 不得被当成普通失败忽略。
- invalidated transaction 不得继续 commit。

## 失败模式

- reconcile 只更新 graph，不更新 fact / effect / transaction。
- 用户绕过 runtime 的修改被误当成 active fact。
- repair 后未重新计算 ready queue。
- stale assumptions 继续进入 `ContextProjection`。

## 测试 / 验收方向

- graph / fact / effect / transaction 四类 reconcile 有独立用例。
- 外部文件变化会标记相关 facts stale。
- partial effect 触发 compensation 或 human handoff。
- reconcile record 可解释为什么某个 node 被阻塞或 superseded。

## 与其他模块关系

- 修改 `ActionGraph` node status 和 edge 影响。
- 更新 `StateStore` fact lifecycle。
- 读取 `EffectLedger` failed / partial 状态。
- 阻止 `TransactionManager` 过期 commit。
- 通知 `Scheduler` 重算 ready queue。
