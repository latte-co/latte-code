# Runtime Module Evolution Targets

This directory stores accepted long-term runtime module targets. They define module boundaries, data contracts, and acceptance direction for evolving from the basic code agent toward the harness-native runtime during `v0.2-v0.5`.

For current / near-term module design, see [`../../modules/`](../../modules/README.md). Documents in this directory must not be read as evidence that the current `v0.1` implementation already has full runtime capability.

Chinese counterpart: [`docs/zh-CN/design/runtime-evolution/modules/`](../../../../zh-CN/design/runtime-evolution/modules/README.md).

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
