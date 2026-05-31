# Runtime Evolution Target: Reconciler

## Status

This document defines the evolutionary design for `Reconciler`. Early implementation only needs to record failures and blockers; formal reconcile becomes necessary when facts, effects, and transactions start to affect each other.

Chinese counterpart: [`docs/zh-CN/design/runtime-evolution/modules/reconciler.md`](../../../../zh-CN/design/runtime-evolution/modules/reconciler.md).

## Evolution Rhythm

| Stage | Shape | Goal |
| --- | --- | --- |
| v0.1 | failure / blocker record | Record failure reasons and human handoff points |
| v0.3 | stale marking | Mark related facts unreliable after file or context changes |
| v0.4 | light reconciler | Handle failed step, partial effect, and invalidated patch |
| v0.5 | `Reconciler` | Cover graph, fact, effect, and transaction mismatch classes |

## Responsibilities

- Detect mismatches between graph, fact, effect, transaction, and reality.
- Decide which objects remain valid, which need recomputation, which must be withdrawn, and which require human handoff.
- Prevent downstream nodes from using invalidated assumptions.
- Produce auditable reconcile records.

## Non-goals

- `v0.1` does not require a complex recovery system.
- Not prompt retry after failure.
- Do not replace `Scheduler` for normal scheduling.
- Do not directly execute tools, write files, or commit.
- Do not treat user external edits as automatically verified facts.

## Minimal Contract

```ts
type ReconcileDecision = {
  id: string;
  kind: "graph" | "fact" | "effect" | "transaction";
  affectedRefs: string[];
  action: "block" | "mark_stale" | "supersede" | "compensate" | "request_user" | "retry_after_repair";
  reason: string;
};
```

## Invariants

- Failed steps / nodes must block dependent pending work.
- Repo / overlay revision changes must trigger stale checks for related facts.
- Partial effects must not be ignored as ordinary failures.
- Invalidated transactions must not continue to commit.

## Acceptance Direction

- `v0.1` final reports include failure and blocking reasons.
- From `v0.4`, partial effects trigger compensation or human handoff.
- `v0.5` has independent tests for graph / fact / effect / transaction reconcile.
- Reconcile records explain why a node was blocked or superseded.

## Relationships

- Updates `ActionGraph` node status and edge effects.
- Updates `StateStore` fact lifecycle.
- Reads failed / partial state from `EffectLedger`.
- Blocks stale commit in `TransactionManager`.
- Notifies `Scheduler` to recompute ready queue.
