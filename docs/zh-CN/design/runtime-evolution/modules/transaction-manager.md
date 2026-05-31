# Runtime 演进目标：TransactionManager

## 文档状态

本文定义 `TransactionManager` 的渐进式设计。早期可以先管理 patch 批次和验证结果；成熟阶段再管理 `OverlayRevision`、checkpoint、commit、rollback 和 compensation。

英文对应文档：[`docs/en-US/design/runtime-evolution/modules/transaction-manager.md`](../../../../en-US/design/runtime-evolution/modules/transaction-manager.md)。

## 演进节奏

| 阶段 | 形态 | 目标 |
| --- | --- | --- |
| v0.1 | patch summary | 汇总本次修改和验证结果 |
| v0.4 | overlay / transaction lite | 将 patch、effect 和验证新鲜度绑定 |
| v0.5 | `TransactionManager` | 管理 commit gate、rollback、checkpoint 和 compensation |

## 责任

- 管理 `OverlayRevision`、checkpoint、commit、rollback、compensation 和 transaction status。
- 将文件写入和其他 mutating effect 绑定到 transaction boundary。
- 在 commit 前检查验证新鲜度、overlay 状态、rollback handle 和 gate 状态。
- 为失败恢复和人工接管提供可解释 transaction record。

## 非目标

- v0.1 不要求完整事务系统。
- 不取代 Git branch、review 或 CI。
- 不直接决定事实可信度。
- 不执行具体文件写入；写入由 capability adapter 执行并进入 `EffectLedger`。
- 不绕过外部 repo permissions。

## 最小数据契约

```ts
type Transaction = {
  id: string;
  patchRefs: string[];
  effectIds: string[];
  verificationIds: string[];
  status: "open" | "committed" | "rolled_back" | "failed" | "needs_reconcile";
  rollbackHandle?: string;
};
```

## 不变量

- 文件写入必须能关联到 patch summary 或 transaction。
- v0.4 起，commit 前必须通过 `transaction_gate`。
- stale verification 不得支持 commit。
- rollback handle 不可用时 transaction 必须进入 `needs_reconcile` 或 human handoff。

## 验收方向

- stale overlay 阻止 commit。
- rollback 后 effect 和 transaction status 对齐。
- 不可补偿 effect 进入 approval / compensation 语义。
- transaction record 能解释每次提交、回滚或阻塞原因。

## 与其他模块关系

- 从 `EffectLedger` 读取 effect ids 和状态。
- 从 `StateStore` / gates 获取验证新鲜度。
- 向 `Reconciler` 报告 invalidated overlay、failed rollback 或 partial compensation。
- `Scheduler` 根据 transaction status 阻塞或恢复 node。
