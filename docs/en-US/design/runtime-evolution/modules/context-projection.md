# Runtime Evolution Target: ContextProjection

## Status

This document defines the evolutionary design for `ContextProjection`. Early implementation may be a task context summary; as facts, evidence, and uncertainty grow, it upgrades into formal context projection.

Chinese counterpart: [`docs/zh-CN/design/runtime-evolution/modules/context-projection.md`](../../../../zh-CN/design/runtime-evolution/modules/context-projection.md).

## Evolution Rhythm

| Stage | Shape | Goal |
| --- | --- | --- |
| v0.1 | task context summary | Provide minimal relevant context for editing and verification |
| v0.3 | evidence-aware projection | Separate fact, evidence, hypothesis, and stale material |
| v0.5 | `ContextProjection` | Generate sourced, budgeted, omission-aware, trust-scoped LLM input |

## Responsibilities

- Generate LLM-usable context from `StateStore`, `ActionGraph`, task acceptance, and policy constraints.
- Make explicit which facts, evidence, and hypotheses are included, and which stale / redacted / over-budget material is excluded.
- Prevent transcript trimming from becoming a fact source.
- Provide auditable input for `PolicyDecision`.

## Non-goals

- `v0.1` does not require a full projection engine.
- Do not automatically read the entire repository or full history.
- Do not promote facts.
- Do not execute tools.
- Do not inject external document content into prompts unconditionally.

## Minimal Contract

```ts
type ContextProjection = {
  id: string;
  stepId: string;
  factIds: string[];
  evidenceIds: string[];
  hypotheses: string[];
  omittedDueToBudget: string[];
  trustScope: string[];
};
```

## Invariants

- Stale / invalidated facts cannot enter projection as strong facts.
- Projection must record omitted / redacted information.
- Key prompt facts must trace back to fact or evidence ids.
- Untrusted external content must carry trust scope.

## Acceptance Direction

- `v0.1` context summary covers task-relevant files and acceptance criteria.
- From `v0.3`, stale facts are excluded or explicitly marked.
- Projection audit can explain what the model saw and did not see.
- `PolicyDecision` can trace back to projection id.

## Relationships

- Reads facts / evidence from `StateStore`.
- Gets current node goal and dependencies from `ActionGraph`.
- Provides input to `Policy Core`.
- Receives stale / invalidation results from `Reconciler`.
