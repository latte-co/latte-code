# 模块技术设计：TransactionManager

## 文档状态

当前设计占位，用于实现前明确 `TransactionManager` 的 overlay、提交和回滚边界。本文属于 Fluxcode internal runtime 设计；Fluxcode externally 仍是 code agent `Data Plane`。

英文对应文档：[`docs/en-US/design/modules/transaction-manager.md`](../../../en-US/design/modules/transaction-manager.md)。

## 责任

- 管理 `OverlayRevision`、checkpoint、commit、rollback、compensation 和 transaction status。
- 将文件写入和其他 mutating effect 绑定到 transaction boundary。
- 在 commit 前检查验证新鲜度、overlay 状态、rollback handle 和 gate 状态。
- 为失败恢复和人工接管提供可解释 transaction record。

## 非目标

- 不取代 Git branch、review 或 CI。
- 不直接决定事实可信度。
- 不执行具体文件写入；写入由 capability adapter 执行并进入 `EffectLedger`。
- 不绕过外部 repo permissions。

## 输入 / 输出

| 方向 | 内容 |
| --- | --- |
| 输入 | overlay diff ref、effect ids、verification evidence、commit policy、rollback request |
| 输出 | transaction status、commit / rollback decision、checkpoint record、transaction gate result |

## 核心数据契约

```ts
type Transaction = {
  id: string;
  overlayRevision: string;
  actionNodeIds: string[];
  effectIds: string[];
  rollbackHandle: string;
  commitPolicy: "manual" | "auto_after_verify" | "never";
  status: "open" | "committed" | "rolled_back" | "compensating" | "failed" | "needs_reconcile";
};
```

## 不变量

- 文件写入必须绑定 overlay 或 transaction。
- commit 前必须通过 `transaction_gate`。
- stale verification 不得支持 commit。
- rollback handle 不可用时 transaction 必须进入 `needs_reconcile` 或 human handoff。

## 失败模式

- overlay base 变化导致 diff 过期。
- rollback handle 指向不存在或不可恢复状态。
- effect 已 applied 但 transaction 未记录。
- commit 与外部 repo 状态冲突。

## 测试 / 验收方向

- stale overlay 阻止 commit。
- rollback 后 effect 和 transaction status 对齐。
- 不可补偿 effect 进入 approval / compensation 语义。
- transaction record 能解释每次提交、回滚或阻塞原因。

## 与其他模块关系

- 从 `EffectLedger` 读取 effect ids 和状态。
- 从 `StateStore` / gates 获取验证新鲜度。
- 向 `Reconciler` 报告 invalidated overlay、failed rollback 或 partial compensation。
- `Scheduler` 根据 transaction status 阻塞或恢复 node。
