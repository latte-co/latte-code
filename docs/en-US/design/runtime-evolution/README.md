# Runtime Evolution Targets

This directory stores long-term runtime evolution targets that are already part of the accepted formal design set. They are not generic unaccepted `proposals/`, but they also are not module designs that the current `v0.1` / near-term implementation must fully deliver.

Directory boundary:

- [`../modules/`](../modules/README.md): current / near-term module design, currently limited to the basic `Code Agent Loop`.
- [`./modules/`](./modules/README.md): accepted runtime module targets introduced gradually from `v0.2` through `v0.5`.
- [`../../milestones/targets/`](../../milestones/targets/README.md): version targets, task breakdowns, engineering baselines, and implementation plans.

Chinese counterpart: [`docs/zh-CN/design/runtime-evolution/`](../../../zh-CN/design/runtime-evolution/README.md).

## Subdirectories

- [`modules/`](./modules/README.md): long-term runtime module targets and staged migration constraints.

## Maintenance Conventions

- These documents may describe long-term objects such as `ActionGraph`, `StateStore`, `Scheduler`, `EffectLedger`, `TransactionManager`, `Reconciler`, `ContextProjection`, and `NodeExecutor`.
- They must state these objects as evolution targets or staged constraints, and must not imply `src/` already implements the full runtime kernel.
- Do not demote these documents to `proposals/`; they are accepted long-term direction.
