# Runtime Evolution Target: NodeExecutor

## Status

This document defines the evolutionary design for `NodeExecutor`. `v0.1` may be the executor for a basic agent loop; as `StepTrace` evolves into `ActionNode`, it converges into a single-node execution component.

Chinese counterpart: [`docs/zh-CN/design/runtime-evolution/modules/node-executor.md`](../../../../zh-CN/design/runtime-evolution/modules/node-executor.md).

## Evolution Rhythm

| Stage | Shape | Goal |
| --- | --- | --- |
| v0.1 | basic agent executor | Run search, read, edit, verification, and handoff steps |
| v0.2 | trace-aware executor | Each step produces structured `StepTrace` |
| v0.5 | `NodeExecutor` | Execute one `ActionNode` dispatched by scheduler |

## Responsibilities

- Execute linear steps for the basic code agent in early stages.
- In mature stages, execute a single `ActionNode` after `Scheduler` authorization.
- Select deterministic, single-decision, or exploratory execution path based on node profile.
- Call capabilities through `Capability Adapter` and emit `Event`, `Observation`, `EvidenceRef`, and `ActionResult`.
- Run node-level bounded ReAct mini-loop for exploratory nodes.

## Non-goals

- Does not own global scheduling authority.
- Does not directly promote `Fact`.
- Does not bypass `EffectLedger` declarations.
- Does not directly commit, rollback, or compensate transactions.
- Does not use agent-level / global ReAct as the final runtime controller.

## Execution Profiles

| Profile | Use case | LLM usage | mini-loop |
| --- | --- | --- | --- |
| `deterministic` | Known input, capability, and output contract, such as fixed verification or format conversion | None | No |
| `single_decision` | One LLM `PolicyDecision`, such as choosing extra context or producing a candidate edit | Once | No |
| `exploratory` | Local exploration, retrieval, lightweight verification, or tentative reading | Multi-step but bounded | bounded ReAct mini-loop |

## Bounded ReAct Mini-loop Contract

`exploratory` profile may use local ReAct, but it must satisfy:

- Fixed step budget and timeout.
- Capability allowlist.
- Every step binds to the current step / `ActionNode`.
- Each step only emits `Event`, `Observation`, `PolicyDecision`, `EvidenceRef`, or loop-local hypothesis.
- Explicit exit condition: done, budget exhausted, needs user, needs reconcile, or failed.
- Candidate facts must go through `TrustGate` / promotion rule and cannot directly write active `Fact`.
- Mutating effects must enter `EffectLedger` first and cannot be executed naked inside the loop.

## Minimal Contract

```ts
type NodeExecutionProfile = "deterministic" | "single_decision" | "exploratory";

type ActionResult = {
  stepId: string;
  status: "done" | "failed" | "blocked" | "needs_reconcile";
  observations: string[];
  evidenceRefs: string[];
  effectRefs: string[];
};
```

## Invariants

- Executor can only run steps / nodes within current task scope.
- Executor must not modify the global ready queue.
- Executor must not directly write active `Fact`.
- Executor must not bypass commit / rollback boundaries.
- Every exploratory mini-loop step must be auditable.

## Acceptance Direction

- `v0.1` can complete end-to-end coding tasks.
- `v0.2` gives every step structured trace.
- The three profiles can be tested independently.
- Mini-loop outputs do not directly enter the active fact store.
- Mutating capability without effect declaration fails.

## Relationships

- `Scheduler` decides when to run a node and which profile to use.
- `Capability Adapter` executes concrete capabilities.
- `EffectLedger` records mutating effects.
- `StateStore` receives observations / evidence; fact promotion is controlled by gates.
- `Reconciler` handles `needs_reconcile` from node execution.
