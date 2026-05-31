# Runtime Evolution Target: Policy Core and Guard

## Status

This document defines the evolutionary design for `Policy Core and Guard`. The early goal is not a complex policy engine; it is preventing the basic code agent from overstepping scope, expanding work, or treating uncertain inference as fact.

Chinese counterpart: [`docs/zh-CN/design/runtime-evolution/modules/policy-core-and-guard.md`](../../../../zh-CN/design/runtime-evolution/modules/policy-core-and-guard.md).

## Evolution Rhythm

| Stage | Shape | Goal |
| --- | --- | --- |
| v0.1 | basic task boundary | Respect user scope, non-goals, and allowed commands |
| v0.2 | `PolicyGuard` lite | Validate paths, commands, write scope, and dangerous actions |
| v0.3 | evidence-aware policy | Separate hypothesis, evidence, and fact |
| v0.5 | `PolicyDecision` | Use structured decisions and gate semantics |

## Responsibilities

- `Policy Core` turns task context into constrained `PolicyDecision`.
- `PolicyGuard` validates schema, references, permissions, safety, trust boundaries, and evidence requirements.
- Separate retryable missing-context failures from errors that must reject / ask / escalate.
- Prevent LLMs from directly triggering syscall, file writes, commands, commit, or rollback.

## Non-goals

- `v0.1` does not need a full policy engine.
- LLM does not own global scheduling.
- LLM does not directly call tools.
- LLM inference does not automatically become `Fact`.
- Guard does not replace external compliance or code review.

## Minimal Contract

```ts
type PolicyDecision =
  | { kind: "Proceed"; reason: string }
  | { kind: "AskUser"; question: string; blockingStepId?: string }
  | { kind: "Reject"; reason: string }
  | { kind: "Abstain"; reason: string; missingContext?: string[] };
```

## Invariants

- Writes and command execution must be constrained by task scope and user authorization.
- Patch-generation decisions can only produce candidate patches or edit steps; they cannot bypass write boundaries.
- `AskUser` must bind to a blocking reason or explicit missing context.
- `PermissionInvalid`, `PolicyUnsafe`, and `TrustBoundaryBroken` cannot be bypassed through prompt retry.

## Acceptance Direction

- Schema invalidity is recognized and produces structured errors.
- Permission / trust failures go to reject / ask / escalate.
- Every valid decision traces back to task boundary, projection, or evidence refs.
- From `v0.2`, dangerous commands and out-of-scope paths are blocked by default.

## Relationships

- Receives input from `ContextProjection`.
- Submits candidate node or policy references to `ActionGraph`.
- `Scheduler` decides when related nodes run.
- Shares failure semantics with `TrustGate` / `PermissionGate`.
