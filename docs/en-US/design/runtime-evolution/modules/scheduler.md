# Runtime Evolution Target: Scheduler

## Status

This document defines the evolutionary design for `Scheduler`. `v0.1` may be a linear runner; a full `Scheduler` is needed only after real needs for dependencies, blockers, recovery, or controlled concurrency appear.

Chinese counterpart: [`docs/zh-CN/design/runtime-evolution/modules/scheduler.md`](../../../../zh-CN/design/runtime-evolution/modules/scheduler.md).

## Evolution Rhythm

| Stage | Shape | Goal |
| --- | --- | --- |
| v0.1 | linear next-step runner | Execute task steps in order |
| v0.2 | guarded runner | Check path, command, and capability boundaries before execution |
| v0.4 | recovery-aware runner | Block or request recovery on failed / partial / stale state |
| v0.5 | `Scheduler` | Dispatch nodes based on dependencies, gates, budgets, state, and recovery results |

## Responsibilities

- Decide which `ActionNode`s can run, when they run, and which executor profile runs them.
- Maintain ready queue, blocked reason, retry budget, cancellation, and resume cursor.
- Read gate, fact, effect, and transaction state to block unsafe or stale nodes.
- Dispatch nodes to `NodeExecutor` without giving executors global scheduling authority.

## Non-goals

- `v0.1` does not need a complex scheduler.
- Do not let LLM natural-language reasoning decide global scheduling.
- Do not execute capability internals.
- Do not directly promote facts, declare effects, or commit transactions.
- Do not implement unbounded multi-agent fan-out.

## Minimal Contract

```ts
type DispatchDecision =
  | { kind: "RunStep"; stepId: string }
  | { kind: "BlockStep"; stepId: string; reason: string }
  | { kind: "AskUser"; stepId: string; question: string }
  | { kind: "RequestRecovery"; stepIds: string[]; reason: string };
```

## Invariants

- Steps / nodes with failed guards cannot run.
- Nodes depending on stale / invalidated facts cannot run.
- When transaction or effect is partial / needs_reconcile, downstream mutating nodes must block.
- Scheduler decisions must be auditable and cite trigger conditions.

## Acceptance Direction

- `v0.1` linear execution order is reviewable.
- `v0.2` guard failures go to reject / ask / escalate, not infinite retry.
- `v0.5` dependency-aware ready queue blocks downstream correctly.
- Resume does not repeat completed non-idempotent nodes.

## Relationships

- Reads nodes and dependencies from `ActionGraph`.
- Queries fact lifecycle in `StateStore`.
- Queries risk state from `EffectLedger` / `TransactionManager`.
- Calls `NodeExecutor` to execute one node.
- Sends mismatches to `Reconciler`.
