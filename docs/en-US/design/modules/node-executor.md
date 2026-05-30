# Module Technical Design: NodeExecutor

## Document Status

Current design placeholder for clarifying the execution strategy for a single `ActionNode` before implementation. This belongs to Fluxcode internal runtime design; externally Fluxcode remains a code-agent `Data Plane`.

Chinese counterpart: [`docs/zh-CN/design/modules/node-executor.md`](../../../zh-CN/design/modules/node-executor.md).

## Responsibility

- Execute one `ActionNode` after `Scheduler` authorization.
- Select deterministic, single-decision, or exploratory execution path based on node profile.
- Invoke capabilities through `Capability Adapter` and output `Event`, `Observation`, `EvidenceRef`, and `ActionResult`.
- Run node-level bounded ReAct mini-loop for exploratory nodes.

## Non-goals

- Does not own global scheduling authority.
- Does not directly promote `Fact`.
- Does not bypass `EffectLedger` side-effect declaration.
- Does not directly commit, rollback, or compensate transactions.
- Does not make agent-level / global ReAct the runtime main controller.

## Execution Profiles

| Profile | Use case | LLM usage | Mini-loop |
| --- | --- | --- | --- |
| `deterministic` | Inputs, capability, and output contract are known: formatting, fixed verification, deterministic transform | None | None |
| `single_decision` | Requires one LLM `PolicyDecision`, such as asking for missing context or generating a candidate edit node | Once | None |
| `exploratory` | Requires local exploration, recall, lightweight verification, or tentative reading | Multi-step but bounded | bounded ReAct mini-loop |

## Bounded ReAct Mini-loop Contract

`exploratory` profile may use local ReAct, but it must satisfy:

- Fixed step budget and timeout.
- Capability allowlist.
- Every step binds to the current `ActionNode`.
- Every step outputs only `Event`, `Observation`, `PolicyDecision`, `EvidenceRef`, or loop-local hypothesis.
- Explicit exit condition: done, budget exhausted, needs user, needs reconcile, failed.
- Any candidate fact must go through `TrustGate` / promotion rule; it cannot directly write active `Fact`.
- Any mutating effect must enter `EffectLedger` before execution; loop tools cannot execute bare effects.

## Inputs / Outputs

| Direction | Content |
| --- | --- |
| Input | dispatch decision, `ActionNode`, `ContextProjection`, capability descriptors, budget / guard policy |
| Output | `ActionResult`, events, observations, evidence refs, guard / failure signal, reconcile request |

## Core Data Contracts

```ts
type NodeExecutionProfile = "deterministic" | "single_decision" | "exploratory";

type ActionResult = {
  nodeId: string;
  status: "done" | "failed" | "blocked" | "needs_reconcile";
  observations: string[];
  evidenceRefs: string[];
  effectRefs: string[];
};
```

## Invariants

- Executor only runs scheduler-dispatched nodes.
- Executor does not modify the global ready queue.
- Executor does not directly write active `Fact`.
- Executor does not bypass commit / rollback boundary.
- Every exploratory mini-loop step is auditable.

## Failure Modes

- Mini-loop runs without bound.
- Loop-local hypothesis is promoted as fact by mistake.
- Executor directly calls tool and creates undeclared side effects.
- Executor replans global work after failure.

## Testing / Acceptance Direction

- Three profile paths are independently testable.
- Exploratory loop stops at budget and returns structured status.
- Mini-loop artifacts do not directly enter active fact store.
- Mutating capability fails when effect declaration is missing.

## Relation to Other Modules

- `Scheduler` decides when to run a node and which profile to use.
- `Capability Adapter` executes concrete capabilities.
- `EffectLedger` records mutating effects.
- `StateStore` receives observation / evidence, but fact promotion is gate-controlled.
- `Reconciler` handles `needs_reconcile` returned by node execution.
