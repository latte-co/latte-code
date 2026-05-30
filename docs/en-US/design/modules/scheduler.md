# Module Technical Design: Scheduler

## Document Status

Current design placeholder for clarifying `Scheduler` authority before implementation. This belongs to Fluxcode internal runtime design; externally Fluxcode remains a code-agent `Data Plane`.

Chinese counterpart: [`docs/zh-CN/design/modules/scheduler.md`](../../../zh-CN/design/modules/scheduler.md).

## Responsibility

- Decide which `ActionNode` can run, when it runs, and which executor profile runs it.
- Maintain ready queue, blocked reason, retry budget, cancellation, and resume cursor.
- Read gate, fact, effect, and transaction state to prevent unsafe or stale nodes from continuing.
- Dispatch nodes to `NodeExecutor` without giving executors global scheduling authority.

## Non-goals

- Does not let LLM natural-language reasoning decide global scheduling.
- Does not execute capability details.
- Does not directly promote facts, declare effects, or commit transactions.
- Does not implement unbounded multi-agent fan-out.

## Inputs / Outputs

| Direction | Content |
| --- | --- |
| Input | `ActionGraph` view, gate status, fact lifecycle, effect / transaction status, budget policy |
| Output | dispatch decision, blocked reason, retry / cancellation record, resume cursor |

## Core Data Contracts

```ts
type DispatchDecision =
  | { kind: "RunNode"; nodeId: string; executorProfile: string }
  | { kind: "BlockNode"; nodeId: string; reason: string }
  | { kind: "CancelNode"; nodeId: string; reason: string }
  | { kind: "RequestReconcile"; nodeIds: string[]; reason: string };
```

## Invariants

- Nodes whose guards failed cannot run.
- Nodes depending on stale / invalidated facts cannot run.
- When transaction or effect state is partial / needs_reconcile, downstream mutating nodes must block.
- Scheduler decisions must be auditable and reference their triggering conditions.

## Failure Modes

- Ready queue uses a stale graph view.
- Retry budget is bypassed through prompt retry.
- Cancellation does not put effect / transaction into reconcile.
- Executor creates global scheduling decisions.

## Testing / Acceptance Direction

- Dependency-aware ready queue blocks downstream nodes correctly.
- Guard failure enters reject / ask / escalate, not infinite retry.
- Resume does not re-run completed non-idempotent nodes.
- Multi-executor scheduling follows read/write conflict policy.

## Relation to Other Modules

- Reads nodes and dependencies from `ActionGraph`.
- Queries fact lifecycle in `StateStore`.
- Queries risk state in `EffectLedger` / `TransactionManager`.
- Calls `NodeExecutor` to run one node.
- Sends drift to `Reconciler`.
