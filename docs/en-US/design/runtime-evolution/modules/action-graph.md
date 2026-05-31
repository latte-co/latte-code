# Runtime Evolution Target: ActionGraph

## Status

This document defines the evolutionary design for `ActionGraph`. `ActionGraph` is a long-term runtime object, not a full `v0.1` starting requirement. `v0.1` may start with `StepTrace`, then evolve into the formal graph from `v0.2` through `v0.5`.

Chinese counterpart: [`docs/zh-CN/design/runtime-evolution/modules/action-graph.md`](../../../../zh-CN/design/runtime-evolution/modules/action-graph.md).

## Evolution Rhythm

| Stage | Shape | Goal |
| --- | --- | --- |
| v0.1 | `StepTrace` | Record key steps, tool calls, changes, and verification results |
| v0.2 | lightweight `ActionNode` | Add id, status, inputs, outputs, and dependencies to steps |
| v0.4 | recoverable action records | Mark failures, blockers, partial effects, and recovery entries |
| v0.5 | `ActionGraph` | Become execution ledger, scheduling surface, recovery entry, and UX surface |

## Responsibilities

- Express how a task evolves from simple steps into `ActionNode`s.
- Store node dependencies, blockers, verification, and recovery relations.
- Provide a ready / blocked / failed / completed scheduling surface for `Scheduler`.
- Act as an audit index connecting `PolicyDecision`, `Evidence`, `EffectRecord`, `Transaction`, and `Fact`.

## Non-goals

- `v0.1` does not require full DAG scheduling.
- Do not store all runtime state.
- Do not own `Fact` lifecycle.
- Do not execute capabilities.
- Do not directly commit, rollback, or compensate effects.

## Minimal Contract

```ts
type StepTrace = {
  id: string;
  title: string;
  status: "pending" | "running" | "blocked" | "done" | "failed";
  inputs: string[];
  outputs: string[];
  toolCallIds: string[];
};

type ActionNode = StepTrace & {
  dependsOn: string[];
  evidenceIds: string[];
  effectIds: string[];
  transactionId?: string;
};
```

## Invariants

- From `v0.1`, every key action must have at least `StepTrace`.
- After `ActionNode` is introduced, failed / blocked nodes must not be silently ignored by downstream nodes.
- `ActionGraph` stores references to facts, evidence, effects, and transactions; it does not copy their authority state.
- Edge changes must trigger scheduler view recomputation.

## Acceptance Direction

- Users can understand what the agent did from trace.
- Failed steps explain blocking reasons.
- The `v0.5` graph can naturally migrate from trace data produced in `v0.1-v0.4`.

## Relationships

- `NodeExecutor` produces `StepTrace` / `ActionNode` results.
- `Scheduler` reads the graph scheduling surface in mature stages.
- `StateStore` owns `Fact` / `Evidence`; graph only references them.
- `EffectLedger` / `TransactionManager` write back effect and transaction references.
- `Reconciler` may update node status, edges, and blocking reasons.
