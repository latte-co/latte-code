# Runtime Evolution Target: StateStore

## Status

This document defines the evolutionary design for `StateStore`. Early Fluxcode does not need a full fact database, but it must avoid treating transcript, tool output, or model inference as long-term fact.

Chinese counterpart: [`docs/zh-CN/design/runtime-evolution/modules/state-store.md`](../../../../zh-CN/design/runtime-evolution/modules/state-store.md).

## Evolution Rhythm

| Stage | Shape | Goal |
| --- | --- | --- |
| v0.1 | verification summary | Store task summary, verification results, and key output references |
| v0.3 | evidence / fact lite | Separate `Observation`, `Evidence`, and `Fact` |
| v0.5 | `StateStore` | Manage versioned facts, lifecycle, coverage, and confidence |

## Responsibilities

- Manage `Observation`, `Evidence`, and versioned `Fact`.
- Maintain `Fact` lifecycle: `candidate`, `active`, `stale`, `superseded`, `invalidated`, `retracted`.
- Provide trusted, versioned, and scoped context material for `ContextProjection`.
- Record how promotion rules / `TrustGate` promote evidence into facts.

## Non-goals

- `v0.1` does not require a full fact system.
- Do not execute tools or capabilities.
- Do not schedule `ActionNode`s.
- Do not treat transcript, prompt, or external docs directly as facts.
- Do not replace external repo, CI, or review authority.

## Minimal Contract

```ts
type Observation = {
  id: string;
  source: "tool" | "user" | "environment" | "external";
  summary: string;
  rawRef?: string;
};

type Evidence = {
  id: string;
  observationIds: string[];
  scope: string[];
  producedByStepId: string;
};

type Fact = {
  id: string;
  claim: string;
  lifecycle: "candidate" | "active" | "stale" | "superseded" | "invalidated" | "retracted";
  evidenceIds: string[];
};
```

## Invariants

- Active `Fact` must reference at least one `Evidence`.
- `Observation` must not automatically become `Fact`.
- Stale / invalidated / retracted facts must not enter `ContextProjection` as strong facts.
- `Fact` must bind to repo / overlay revision or explicit external source.

## Acceptance Direction

- From `v0.3`, LLM hypothesis and verified fact are distinguishable.
- Promotion must produce an auditable record.
- Related facts update lifecycle after revision changes.
- Projection queries do not return stale facts as strong facts.

## Relationships

- `ContextProjection` reads facts and evidence from `StateStore`.
- `Reconciler` updates stale / invalidated state.
- `TrustGate` decides which evidence can be promoted.
- `ActionGraph` only references fact / evidence ids.
