# Module Technical Design: Reconciler

## Document Status

Current design placeholder for clarifying `Reconciler` drift detection and recovery boundaries before implementation. This belongs to Fluxcode internal runtime design; externally Fluxcode remains a code-agent `Data Plane`.

Chinese counterpart: [`docs/zh-CN/design/modules/reconciler.md`](../../../zh-CN/design/modules/reconciler.md).

## Responsibility

- Detect drift between graph, fact, effect, transaction, and reality.
- Decide which objects remain valid, which need recomputation, which must be retracted, and which require human handoff.
- Prevent downstream nodes from continuing with invalid assumptions.
- Generate auditable reconcile records.

## Non-goals

- Is not prompt retry after failure.
- Does not replace `Scheduler` for normal scheduling.
- Does not directly execute tools, write files, or commit.
- Does not treat user changes outside the runtime as automatically verified facts.

## Inputs / Outputs

| Direction | Content |
| --- | --- |
| Input | failed / partial effect, stale fact signal, invalid overlay, graph status mismatch, user override |
| Output | reconcile decision, affected nodes, fact lifecycle update, transaction block / repair request |

## Core Data Contracts

```ts
type ReconcileDecision = {
  id: string;
  kind: "graph" | "fact" | "effect" | "transaction";
  affectedRefs: string[];
  action: "block" | "supersede" | "mark_stale" | "compensate" | "request_user" | "retry_after_repair";
  reason: string;
};
```

## Invariants

- Failed node must block pending nodes that depend on it.
- Repo / overlay revision change must trigger related fact stale checks.
- Partial effect must not be ignored as a plain failure.
- Invalidated transaction must not continue to commit.

## Failure Modes

- Reconcile updates only graph but not fact / effect / transaction.
- User modification outside runtime is treated as active fact.
- Ready queue is not recomputed after repair.
- Stale assumptions continue to enter `ContextProjection`.

## Testing / Acceptance Direction

- Graph / fact / effect / transaction reconcile each have dedicated cases.
- External file changes mark related facts stale.
- Partial effect triggers compensation or human handoff.
- Reconcile record explains why a node was blocked or superseded.

## Relation to Other Modules

- Updates `ActionGraph` node status and edge impact.
- Updates `StateStore` fact lifecycle.
- Reads failed / partial status from `EffectLedger`.
- Blocks stale commit in `TransactionManager`.
- Notifies `Scheduler` to recompute ready queue.
