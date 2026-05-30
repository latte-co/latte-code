# Module Technical Design: StateStore

## Document Status

Current design placeholder for clarifying the `StateStore` fact and evidence boundary before implementation. This belongs to Fluxcode internal runtime design; externally Fluxcode remains a code-agent `Data Plane`.

Chinese counterpart: [`docs/zh-CN/design/modules/state-store.md`](../../../zh-CN/design/modules/state-store.md).

## Responsibility

- Manage `Observation`, `Evidence`, and versioned `Fact` records.
- Maintain `Fact` lifecycle: `candidate`, `active`, `stale`, `superseded`, `invalidated`, `retracted`.
- Provide trusted, versioned, coverage-aware context material for `ContextProjection`.
- Record how promotion rules / `TrustGate` promote evidence into facts.

## Non-goals

- Does not execute tools or capabilities.
- Does not schedule `ActionNode`.
- Does not treat transcript, prompt, or external documents as facts directly.
- Does not replace external repo, CI, or review authority.

## Inputs / Outputs

| Direction | Content |
| --- | --- |
| Input | `Observation`, `Evidence`, promotion record, revision changes, reconcile requests |
| Output | active / candidate / stale facts, fact history, projection material, fact invalidation events |

## Core Data Contracts

```ts
type Fact = {
  id: string;
  namespace: string;
  claim: string;
  repoRevision: string;
  overlayRevision?: string;
  lifecycle: "candidate" | "active" | "stale" | "superseded" | "invalidated" | "retracted";
  confidence: number;
  coverage: { scope: "local" | "module" | "repo" | "external"; paths?: string[]; symbols?: string[] };
  evidenceIds: string[];
};
```

## Invariants

- Active `Fact` must reference at least one `Evidence`.
- `Observation` must not automatically become `Fact`.
- stale / invalidated / retracted facts must not enter `ContextProjection` as strong facts.
- `Fact` must bind to repo / overlay revision or explicit external source.

## Failure Modes

- Evidence lacks source, time, or boundary.
- Missing promotion rule lets model inference become fact.
- Repo / overlay change does not mark facts stale.
- Conflicting facts silently overwrite each other.

## Testing / Acceptance Direction

- Promotion creates auditable records.
- Revision change updates related fact lifecycle.
- Projection query does not return stale fact as strong fact.
- Conflicting facts can coexist, downgrade, retract, or request verification.

## Relation to Other Modules

- `ContextProjection` reads facts and evidence from `StateStore`.
- `Reconciler` updates stale / invalidated state.
- `TrustGate` decides which evidence can be promoted.
- `ActionGraph` stores only fact / evidence ids.
