# Module Technical Design: ActionGraph

## Document Status

Current design placeholder for clarifying the `ActionGraph` boundary before implementation. This belongs to Fluxcode internal runtime design; from the external software-engineering-system perspective, Fluxcode remains a code-agent `Data Plane` and does not replace CI, review, permissions, or deployment control.

Chinese counterpart: [`docs/zh-CN/design/modules/action-graph.md`](../../../zh-CN/design/modules/action-graph.md).

## Responsibility

- Express how a task is decomposed into `ActionNode` objects.
- Store dependency, blocking, verification, and reconcile relations.
- Provide the ready / blocked / failed / completed scheduling surface for `Scheduler`.
- Act as an audit index linking `PolicyDecision`, `Evidence`, `EffectRecord`, `Transaction`, and `Fact`.
- Support UX display of plan, progress, and human handoff points.

## Non-goals

- Does not store all runtime state.
- Does not own `Fact` lifecycle.
- Does not execute capabilities.
- Does not directly commit, rollback, or compensate effects.

## Inputs / Outputs

| Direction | Content |
| --- | --- |
| Input | `TaskSpec`, `PolicyDecision`, module / capability metadata, reconcile updates |
| Output | `ActionNode` records, edge records, node status, audit references, scheduler view |

## Core Data Contracts

```ts
type ActionGraph = {
  graphId: string;
  objective: string;
  nodes: Record<string, ActionNode>;
  edges: Array<{ from: string; to: string; kind: "depends_on" | "blocks" | "verifies" | "reconciles" }>;
  status: "planning" | "running" | "blocked" | "completed" | "failed" | "reconciling";
};
```

`ActionNode` must at least carry capability, read/write set, policy reference, effect handle, rollback handle, and status.

## Invariants

- Every executed runtime action must trace to an `ActionNode`.
- `ActionGraph` stores references to facts, evidence, effects, and transactions; it does not copy their authority state.
- Edge changes must trigger scheduler-view recomputation.
- Failed / blocked nodes must not be silently ignored by downstream nodes.

## Failure Modes

- Node references missing capability, fact, or evidence.
- Edges create cycles or invalid dependencies.
- Node status diverges from `EffectLedger` / `TransactionManager` status.
- Recovery loses blocked reason.

## Testing / Acceptance Direction

- After graph create, update, and recovery, all node references resolve.
- A failed node blocks dependent nodes.
- Reconcile update marks affected nodes.
- UX view can explain execution progress and blocked reason from the graph.

## Relation to Other Modules

- `Scheduler` reads the graph scheduling surface.
- `StateStore` owns `Fact` / `Evidence`; graph stores only references.
- `EffectLedger` / `TransactionManager` write back effect and transaction references.
- `Reconciler` may update node status, edges, and blocked reason.
