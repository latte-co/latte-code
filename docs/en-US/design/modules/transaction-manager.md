# Module Technical Design: TransactionManager

## Document Status

Current design placeholder for clarifying `TransactionManager` overlay, commit, and rollback boundaries before implementation. This belongs to Fluxcode internal runtime design; externally Fluxcode remains a code-agent `Data Plane`.

Chinese counterpart: [`docs/zh-CN/design/modules/transaction-manager.md`](../../../zh-CN/design/modules/transaction-manager.md).

## Responsibility

- Manage `OverlayRevision`, checkpoint, commit, rollback, compensation, and transaction status.
- Bind file writes and other mutating effects to transaction boundaries.
- Before commit, check verification freshness, overlay state, rollback handle, and gate status.
- Provide explainable transaction records for failure recovery and human handoff.

## Non-goals

- Does not replace Git branch, review, or CI.
- Does not directly decide fact trust.
- Does not execute file writes; writes are executed by capability adapters and recorded in `EffectLedger`.
- Does not bypass external repo permissions.

## Inputs / Outputs

| Direction | Content |
| --- | --- |
| Input | overlay diff ref, effect ids, verification evidence, commit policy, rollback request |
| Output | transaction status, commit / rollback decision, checkpoint record, transaction gate result |

## Core Data Contracts

```ts
type Transaction = {
  id: string;
  overlayRevision: string;
  actionNodeIds: string[];
  effectIds: string[];
  rollbackHandle: string;
  commitPolicy: "manual" | "auto_after_verify" | "never";
  status: "open" | "committed" | "rolled_back" | "compensating" | "failed" | "needs_reconcile";
};
```

## Invariants

- File writes must bind to overlay or transaction.
- Commit must pass `transaction_gate`.
- Stale verification must not support commit.
- If rollback handle is unavailable, transaction must enter `needs_reconcile` or human handoff.

## Failure Modes

- Overlay base changes and makes diff stale.
- Rollback handle points to missing or unrecoverable state.
- Effect is applied but transaction is not recorded.
- Commit conflicts with external repo state.

## Testing / Acceptance Direction

- Stale overlay blocks commit.
- After rollback, effect and transaction status align.
- Non-compensable effect enters approval / compensation semantics.
- Transaction record explains each commit, rollback, or block reason.

## Relation to Other Modules

- Reads effect ids and status from `EffectLedger`.
- Gets verification freshness from `StateStore` / gates.
- Reports invalidated overlay, failed rollback, or partial compensation to `Reconciler`.
- `Scheduler` blocks or resumes nodes based on transaction status.
