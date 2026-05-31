# Runtime 演进目标

本目录存放已经纳入正式设计集的长期 runtime 演进目标。它们不是普通未采纳 `proposals/`，但也不是当前 `v0.1` / 近期实现必须完整交付的模块设计。

目录边界：

- [`../modules/`](../modules/README.md)：当前 / 近期模块设计，目前只收录基础 `Code Agent Loop`。
- [`./modules/`](./modules/README.md)：从 `v0.2` 到 `v0.5` 逐步引入的 accepted runtime module targets。
- [`../../milestones/targets/`](../../milestones/targets/README.md)：版本目标、任务拆分、工程基线和实现计划。

英文对应目录：[`docs/en-US/design/runtime-evolution/`](../../../en-US/design/runtime-evolution/README.md)。

## 子目录

- [`modules/`](./modules/README.md)：长期 runtime 模块目标与 staged migration 约束。

## 维护约定

- 可以描述 `ActionGraph`、`StateStore`、`Scheduler`、`EffectLedger`、`TransactionManager`、`Reconciler`、`ContextProjection`、`NodeExecutor` 等长期对象。
- 必须明确这些对象是演进目标或阶段性约束，不得暗示当前 `src/` 已经具备完整 runtime-kernel 能力。
- 不把这些文档降级为 `proposals/`；它们属于已接受的长期方向。
