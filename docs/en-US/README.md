# Fluxcode Documentation (English)

This directory contains English Fluxcode documentation. Formal English documents should remain structurally and semantically aligned with [`docs/zh-CN/`](../zh-CN/README.md).

## Index

### Current Formal Design Documents

- [Architecture Overview](./design/architecture-overview.md)
  - Current top-level architecture entry. It first gives Fluxcode's top-level positioning as an external code-agent `Data Plane` and clarifies that `Control Plane Authority` only means Fluxcode internal runtime authority; then it expands the code agent operating model and its difference from a plain `ReAct` / transcript-driven agent; after that, it explains how the harness-native runtime supports code-agent behavior as a governance substrate, and covers the runtime loop, key module authority ownership, external collaboration and governance boundaries, non-goals, and drill-down document indexes.
- [Runtime Kernel Roadmap v0.1-v0.5](./design/runtime-kernel-roadmap-v0.1-v0.5.md)
  - Current version goals and cross-version invariants from `v0.1` through `v0.5`.
- [Runtime Kernel Task Breakdown v0.1-v0.5](./design/runtime-kernel-task-breakdown.md)
  - Independent task breakdown with tasks, dependencies, acceptance criteria, and non-goals per version.
- Module technical designs:
  - [`ActionGraph`](./design/modules/action-graph.md)
  - [`StateStore`](./design/modules/state-store.md)
  - [`Scheduler`](./design/modules/scheduler.md)
  - [`EffectLedger`](./design/modules/effect-ledger.md)
  - [`TransactionManager`](./design/modules/transaction-manager.md)
  - [`Reconciler`](./design/modules/reconciler.md)
  - [`Policy Core and Guard`](./design/modules/policy-core-and-guard.md)
  - [`Capability Adapter`](./design/modules/capability-adapter.md)
  - [`ContextProjection`](./design/modules/context-projection.md)
  - [`NodeExecutor`](./design/modules/node-executor.md)

## Translation Status

| English document | Chinese counterpart | Status |
| --- | --- | --- |
| [`design/architecture-overview.md`](./design/architecture-overview.md) | [`../zh-CN/design/architecture-overview.md`](../zh-CN/design/architecture-overview.md) | Aligned |
| [`design/runtime-kernel-roadmap-v0.1-v0.5.md`](./design/runtime-kernel-roadmap-v0.1-v0.5.md) | [`../zh-CN/design/runtime-kernel-roadmap-v0.1-v0.5.md`](../zh-CN/design/runtime-kernel-roadmap-v0.1-v0.5.md) | Aligned |
| [`design/runtime-kernel-task-breakdown.md`](./design/runtime-kernel-task-breakdown.md) | [`../zh-CN/design/runtime-kernel-task-breakdown.md`](../zh-CN/design/runtime-kernel-task-breakdown.md) | Aligned |
| [`design/modules/action-graph.md`](./design/modules/action-graph.md) | [`../zh-CN/design/modules/action-graph.md`](../zh-CN/design/modules/action-graph.md) | Aligned |
| [`design/modules/state-store.md`](./design/modules/state-store.md) | [`../zh-CN/design/modules/state-store.md`](../zh-CN/design/modules/state-store.md) | Aligned |
| [`design/modules/scheduler.md`](./design/modules/scheduler.md) | [`../zh-CN/design/modules/scheduler.md`](../zh-CN/design/modules/scheduler.md) | Aligned |
| [`design/modules/effect-ledger.md`](./design/modules/effect-ledger.md) | [`../zh-CN/design/modules/effect-ledger.md`](../zh-CN/design/modules/effect-ledger.md) | Aligned |
| [`design/modules/transaction-manager.md`](./design/modules/transaction-manager.md) | [`../zh-CN/design/modules/transaction-manager.md`](../zh-CN/design/modules/transaction-manager.md) | Aligned |
| [`design/modules/reconciler.md`](./design/modules/reconciler.md) | [`../zh-CN/design/modules/reconciler.md`](../zh-CN/design/modules/reconciler.md) | Aligned |
| [`design/modules/policy-core-and-guard.md`](./design/modules/policy-core-and-guard.md) | [`../zh-CN/design/modules/policy-core-and-guard.md`](../zh-CN/design/modules/policy-core-and-guard.md) | Aligned |
| [`design/modules/capability-adapter.md`](./design/modules/capability-adapter.md) | [`../zh-CN/design/modules/capability-adapter.md`](../zh-CN/design/modules/capability-adapter.md) | Aligned |
| [`design/modules/context-projection.md`](./design/modules/context-projection.md) | [`../zh-CN/design/modules/context-projection.md`](../zh-CN/design/modules/context-projection.md) | Aligned |
| [`design/modules/node-executor.md`](./design/modules/node-executor.md) | [`../zh-CN/design/modules/node-executor.md`](../zh-CN/design/modules/node-executor.md) | Aligned |
| Not yet translated | [`../zh-CN/research/code-agent-survey.md`](../zh-CN/research/code-agent-survey.md) | Deferred: large research document; add an English `research/` counterpart when English readers need it |

## Maintenance Conventions

- `design/`: Architecture design proposals, layering, interface models, module technical designs, and phased roadmaps.
- `research/`: Research facts, horizontal comparisons, and verifiable observations. The English `research/` directory should be added only when English research counterparts are maintained.
- When adding or substantially updating a formal English document, add or update the corresponding Chinese document path.
- If a translation is intentionally deferred, mark the deferral in both language indexes with the reason and expected follow-up.
- English documents should use the same core terms as Chinese documents, including `Data Plane`, `Control Plane Authority` scoped to internal runtime authority, `ActionGraph`, `StateStore`, `Scheduler`, `EffectLedger`, `TransactionManager`, `Reconciler`, `PolicyDecision`, `Observation`, `Evidence`, `Fact`, `OverlayRevision`, `ContextProjection`, and `NodeExecutor`.

## Terminology Boundaries

- **Research Facts**: Findings from horizontal research and verifiable observations, used to describe existing systems' capabilities and limits.
- **Design Proposals**: The code-agent operating model and harness-native runtime design derived from research facts; they do not mean existing systems already have these capabilities.
- **Data Plane**: The external positioning of Code Agent / Fluxcode from the perspective of the whole software engineering system; Fluxcode executes tasks and produces evidence, but does not replace external governance systems.
- **Control Plane Authority**: Only Fluxcode internal runtime authority, responsible for internal facts, scheduling, side effects, transactions, and recovery semantics; it must not be understood as an external engineering governance control plane.
- **ActionGraph**: The execution ledger, scheduling surface, recovery entry point, audit index, and UX visualization surface; it is not an omniscient state container.
