# Runtime 模块演进目标

本目录存放已接受的长期 runtime module targets。它们定义 `v0.2-v0.5` 期间从基础 code agent 向完整内部 runtime 结构演进时需要保持的模块边界、数据契约和验收方向。

当前 / 近期模块设计请见 [`../../modules/`](../../modules/README.md)。本目录中的文档不得被解读为当前 `v0.1` 已经实现完整 runtime 能力。

英文对应目录：[`docs/en-US/design/runtime-evolution/modules/`](../../../../en-US/design/runtime-evolution/modules/README.md)。

## Accepted runtime targets

- [`ActionGraph`](./action-graph.md)
- [`StateStore`](./state-store.md)
- [`Scheduler`](./scheduler.md)
- [`EffectLedger`](./effect-ledger.md)
- [`TransactionManager`](./transaction-manager.md)
- [`Reconciler`](./reconciler.md)
- [`Policy Core and Guard`](./policy-core-and-guard.md)
- [`Capability Adapter`](./capability-adapter.md)
- [`ContextProjection`](./context-projection.md)
- [`NodeExecutor`](./node-executor.md)
