# Lattecode Documentation (English)

This directory contains Lattecode research, accepted designs, proposals, and milestones. Formal English documents should remain structurally and semantically aligned with [`docs/zh-CN/`](../zh-CN/README.md).

The current design posture is evolutionary: Lattecode should first become a basic, working local-first code agent, then gradually structure its execution trace, facts, evidence, side effects, transactions, scheduling, and recovery into a harness-native runtime. Runtime terms describe target direction and interface constraints; they do not mean `src/` already implements the full runtime kernel.

## Directory Layers

| Directory | Purpose |
| --- | --- |
| [`design/`](./design/architecture-overview.md) | Current formal design entry; the top-level overview remains `design/architecture-overview.md` |
| [`design/modules/`](./design/modules/README.md) | Current / near-term module-level technical designs primarily aligned with the basic `v0.1` code agent implementation |
| [`design/runtime-evolution/`](./design/runtime-evolution/README.md) | Accepted long-term runtime evolution targets; not generic unaccepted proposals and not evidence of complete current implementation |
| [`proposals/`](./proposals/README.md) | Proposals and idea documents not yet incorporated into the current design set; they must not imply implementation |
| [`milestones/`](./milestones/README.md) | Milestone management, separated into targets/plans and completed records |
| [`research/`](../zh-CN/research/code-agent-survey.md) | Research facts, horizontal comparisons, and verifiable observations. English research counterparts are deferred until needed |

## Current Formal Design Documents

- [Architecture Overview](./design/architecture-overview.md)
  - Current top-level architecture entry. It defines the code-agent-first evolution path, module relationships, external boundaries, and non-goals.
- Current / near-term module-level technical designs:
  - [`Code Agent Loop`](./design/modules/code-agent-loop.md)
  - [`Context Management and Compression`](./design/modules/context-management-and-compression.md)
  - [`Provider Compatibility Layer`](./design/modules/provider-compatibility-layer.md)
- Accepted long-term runtime evolution targets:
  - [`Runtime Evolution`](./design/runtime-evolution/README.md)
  - [`ActionGraph`](./design/runtime-evolution/modules/action-graph.md)
  - [`StateStore`](./design/runtime-evolution/modules/state-store.md)
  - [`Scheduler`](./design/runtime-evolution/modules/scheduler.md)
  - [`EffectLedger`](./design/runtime-evolution/modules/effect-ledger.md)
  - [`TransactionManager`](./design/runtime-evolution/modules/transaction-manager.md)
  - [`Reconciler`](./design/runtime-evolution/modules/reconciler.md)
  - [`Policy Core and Guard`](./design/runtime-evolution/modules/policy-core-and-guard.md)
  - [`Capability Adapter`](./design/runtime-evolution/modules/capability-adapter.md)
  - [`ContextProjection`](./design/runtime-evolution/modules/context-projection.md)
  - [`NodeExecutor`](./design/runtime-evolution/modules/node-executor.md)
- Milestone targets:
  - [`Code Agent Evolution Roadmap v0.1-v0.5`](./milestones/targets/runtime-kernel-roadmap-v0.1-v0.5.md)
  - [`Code Agent Evolution Task Breakdown v0.1-v0.5`](./milestones/targets/runtime-kernel-task-breakdown.md)
  - [`v0.1 Engineering Baseline`](./milestones/targets/v0.1-engineering-baseline.md)
  - [`v0.1 Implementation Plan and Technical Review`](./milestones/targets/v0.1-implementation-plan-review.md)

## Translation Status

| English document | Chinese counterpart | Status |
| --- | --- | --- |
| [`design/architecture-overview.md`](./design/architecture-overview.md) | [`../zh-CN/design/architecture-overview.md`](../zh-CN/design/architecture-overview.md) | Aligned |
| [`design/modules/code-agent-loop.md`](./design/modules/code-agent-loop.md) | [`../zh-CN/design/modules/code-agent-loop.md`](../zh-CN/design/modules/code-agent-loop.md) | Aligned |
| [`design/modules/context-management-and-compression.md`](./design/modules/context-management-and-compression.md) | [`../zh-CN/design/modules/context-management-and-compression.md`](../zh-CN/design/modules/context-management-and-compression.md) | Aligned |
| [`design/modules/provider-compatibility-layer.md`](./design/modules/provider-compatibility-layer.md) | [`../zh-CN/design/modules/provider-compatibility-layer.md`](../zh-CN/design/modules/provider-compatibility-layer.md) | Aligned |
| [`design/runtime-evolution/README.md`](./design/runtime-evolution/README.md) | [`../zh-CN/design/runtime-evolution/README.md`](../zh-CN/design/runtime-evolution/README.md) | Aligned |
| [`design/runtime-evolution/modules/README.md`](./design/runtime-evolution/modules/README.md) | [`../zh-CN/design/runtime-evolution/modules/README.md`](../zh-CN/design/runtime-evolution/modules/README.md) | Aligned |
| [`design/runtime-evolution/modules/action-graph.md`](./design/runtime-evolution/modules/action-graph.md) | [`../zh-CN/design/runtime-evolution/modules/action-graph.md`](../zh-CN/design/runtime-evolution/modules/action-graph.md) | Aligned |
| [`design/runtime-evolution/modules/state-store.md`](./design/runtime-evolution/modules/state-store.md) | [`../zh-CN/design/runtime-evolution/modules/state-store.md`](../zh-CN/design/runtime-evolution/modules/state-store.md) | Aligned |
| [`design/runtime-evolution/modules/scheduler.md`](./design/runtime-evolution/modules/scheduler.md) | [`../zh-CN/design/runtime-evolution/modules/scheduler.md`](../zh-CN/design/runtime-evolution/modules/scheduler.md) | Aligned |
| [`design/runtime-evolution/modules/effect-ledger.md`](./design/runtime-evolution/modules/effect-ledger.md) | [`../zh-CN/design/runtime-evolution/modules/effect-ledger.md`](../zh-CN/design/runtime-evolution/modules/effect-ledger.md) | Aligned |
| [`design/runtime-evolution/modules/transaction-manager.md`](./design/runtime-evolution/modules/transaction-manager.md) | [`../zh-CN/design/runtime-evolution/modules/transaction-manager.md`](../zh-CN/design/runtime-evolution/modules/transaction-manager.md) | Aligned |
| [`design/runtime-evolution/modules/reconciler.md`](./design/runtime-evolution/modules/reconciler.md) | [`../zh-CN/design/runtime-evolution/modules/reconciler.md`](../zh-CN/design/runtime-evolution/modules/reconciler.md) | Aligned |
| [`design/runtime-evolution/modules/policy-core-and-guard.md`](./design/runtime-evolution/modules/policy-core-and-guard.md) | [`../zh-CN/design/runtime-evolution/modules/policy-core-and-guard.md`](../zh-CN/design/runtime-evolution/modules/policy-core-and-guard.md) | Aligned |
| [`design/runtime-evolution/modules/capability-adapter.md`](./design/runtime-evolution/modules/capability-adapter.md) | [`../zh-CN/design/runtime-evolution/modules/capability-adapter.md`](../zh-CN/design/runtime-evolution/modules/capability-adapter.md) | Aligned |
| [`design/runtime-evolution/modules/context-projection.md`](./design/runtime-evolution/modules/context-projection.md) | [`../zh-CN/design/runtime-evolution/modules/context-projection.md`](../zh-CN/design/runtime-evolution/modules/context-projection.md) | Aligned |
| [`design/runtime-evolution/modules/node-executor.md`](./design/runtime-evolution/modules/node-executor.md) | [`../zh-CN/design/runtime-evolution/modules/node-executor.md`](../zh-CN/design/runtime-evolution/modules/node-executor.md) | Aligned |
| [`milestones/targets/runtime-kernel-roadmap-v0.1-v0.5.md`](./milestones/targets/runtime-kernel-roadmap-v0.1-v0.5.md) | [`../zh-CN/milestones/targets/runtime-kernel-roadmap-v0.1-v0.5.md`](../zh-CN/milestones/targets/runtime-kernel-roadmap-v0.1-v0.5.md) | Aligned |
| [`milestones/targets/runtime-kernel-task-breakdown.md`](./milestones/targets/runtime-kernel-task-breakdown.md) | [`../zh-CN/milestones/targets/runtime-kernel-task-breakdown.md`](../zh-CN/milestones/targets/runtime-kernel-task-breakdown.md) | Aligned |
| [`milestones/targets/v0.1-engineering-baseline.md`](./milestones/targets/v0.1-engineering-baseline.md) | [`../zh-CN/milestones/targets/v0.1-engineering-baseline.md`](../zh-CN/milestones/targets/v0.1-engineering-baseline.md) | Aligned |
| [`milestones/targets/v0.1-implementation-plan-review.md`](./milestones/targets/v0.1-implementation-plan-review.md) | [`../zh-CN/milestones/targets/v0.1-implementation-plan-review.md`](../zh-CN/milestones/targets/v0.1-implementation-plan-review.md) | Aligned |
| Not yet translated | [`../zh-CN/research/code-agent-survey.md`](../zh-CN/research/code-agent-survey.md) | Deferred: large research document; add an English `research/` counterpart when English readers need it |

## Maintenance Conventions

- `research/`: Research facts, horizontal comparisons, and verifiable observations.
- `design/modules/`: Current / near-term module-level technical designs; module documents may reference later runtime objects but must not present long-term targets as current implementation.
- `design/runtime-evolution/`: Accepted long-term runtime evolution targets; do not demote them to generic unaccepted proposals, and do not imply they are already fully implemented.
- `proposals/`: Proposals and ideas not yet incorporated into the current design set; they must not imply implementation.
- `milestones/targets/`: Targets, plans, engineering baselines, and task breakdowns.
- `milestones/completed/`: Completed milestones and acceptance evidence.
- When adding or substantially updating a formal English document, add or update the corresponding Chinese document path. If translation is deferred, mark the reason and follow-up in both language indexes.

## Terminology Boundaries

- **Research Facts**: Findings from horizontal research and verifiable observations, used to describe existing systems' capabilities and limits.
- **Design Proposals**: The code-agent operating model and harness-native runtime evolution path derived from research facts; they do not mean existing systems already have these capabilities.
- **Data Plane**: The external positioning of Code Agent / Lattecode from the perspective of the whole software engineering system; Lattecode executes tasks and produces evidence, but does not replace external governance systems.
- **Control Plane Authority**: Only Lattecode internal runtime authority, and only after runtime structure is introduced incrementally; it must not be understood as an external engineering governance control plane.
- **ActionGraph**: The long-term execution ledger, scheduling surface, recovery entry point, audit index, and UX visualization surface; runtime evolution documents describe accepted evolution targets and do not imply implementation.
