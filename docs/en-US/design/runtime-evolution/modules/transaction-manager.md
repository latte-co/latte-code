# Runtime Evolution Target: TransactionManager

## Status

This document defines the evolutionary design for `TransactionManager`. Early implementation may manage patch batches and verification results; mature stages manage `OverlayRevision`, checkpoint, commit, rollback, and compensation.

Chinese counterpart: [`docs/zh-CN/design/runtime-evolution/modules/transaction-manager.md`](../../../../zh-CN/design/runtime-evolution/modules/transaction-manager.md).

## Evolution Rhythm

| Stage | Shape | Goal |
| --- | --- | --- |
| v0.1 | patch summary | Summarize changes and verification results |
| v0.4 | overlay / transaction lite | Bind patch, effect, and verification freshness |
| v0.5 | `TransactionManager` | Manage commit gate, rollback, checkpoint, and compensation |

## Responsibilities

- Manage `OverlayRevision`, checkpoint, commit, rollback, compensation, and transaction status.
- Bind file writes and other mutating effects to transaction boundaries.
- Before commit, check verification freshness, overlay state, rollback handle, and gate status.
- Provide explainable transaction records for failure recovery and human handoff.

## Non-goals

- `v0.1` does not require a full transaction system.
- Do not replace Git branch, review, or CI.
- Do not directly decide fact trustworthiness.
- Do not execute file writes directly; writes are performed by capability adapters and recorded in `EffectLedger`.
- Do not bypass external repo permissions.

## Minimal Contract

```ts
type Transaction = {
  id: string;
  patchRefs: string[];
  effectIds: string[];
  verificationIds: string[];
  status: "open" | "committed" | "rolled_back" | "failed" | "needs_reconcile";
  rollbackHandle?: string;
};
```

## Invariants

- File writes must link to a patch summary or transaction.
- From `v0.4`, commit must pass `transaction_gate`.
- Stale verification cannot support commit.
- If rollback handle is unavailable, the transaction must enter `needs_reconcile` or human handoff.

## Acceptance Direction

- Stale overlay blocks commit.
- Effect and transaction status align after rollback.
- Non-compensable effects enter approval / compensation semantics.
- Transaction records explain each commit, rollback, or blocker.

## Relationships

- Reads effect ids and state from `EffectLedger`.
- Reads verification freshness from `StateStore` / gates.
- Reports invalidated overlay, failed rollback, or partial compensation to `Reconciler`.
- `Scheduler` blocks or resumes nodes based on transaction status.
