# Module Technical Design: ContextProjection

## Document Status

Current design placeholder for clarifying the projection boundary for LLM input context before implementation. This belongs to Fluxcode internal runtime design; externally Fluxcode remains a code-agent `Data Plane`.

Chinese counterpart: [`docs/zh-CN/design/modules/context-projection.md`](../../../zh-CN/design/modules/context-projection.md).

## Responsibility

- Generate LLM-usable context from `StateStore`, `ActionGraph`, task acceptance, and policy constraints.
- Make explicit which facts, evidence, and hypotheses are included, and which stale / redacted / over-budget information is excluded.
- Prevent transcript trimming from becoming the fact source.
- Provide auditable input for `PolicyDecision`.

## Non-goals

- Does not automatically read the whole repository or full history.
- Does not promote facts.
- Does not execute tools.
- Does not inject external document content into prompt unconditionally.

## Inputs / Outputs

| Direction | Content |
| --- | --- |
| Input | fact ids, evidence ids, action node context, acceptance criteria, token budget, trust policy |
| Output | `ContextProjection`, omitted list, redaction list, trust scope, projection audit record |

## Core Data Contracts

```ts
type ContextProjection = {
  id: string;
  actionNodeId: string;
  factIds: string[];
  evidenceIds: string[];
  hypotheses: string[];
  omittedDueToBudget: string[];
  redactions: string[];
  trustScope: string[];
  tokenBudget: number;
};
```

## Invariants

- stale / invalidated fact cannot enter projection as strong fact.
- Projection must record omitted / redacted information.
- Key facts in prompt must trace to fact or evidence id.
- Untrusted external content must mark trust scope.

## Failure Modes

- Transcript trimming brings in stale facts.
- Budget trimming removes key constraints without record.
- External document is treated as runtime fact.
- Hypothesis and fact are indistinguishable in prompt.

## Testing / Acceptance Direction

- Stale fact is excluded or explicitly marked.
- Projection audit explains what the model saw and did not see.
- Redaction does not remove required acceptance criteria.
- `PolicyDecision` traces to projection id.

## Relation to Other Modules

- Reads fact / evidence from `StateStore`.
- Gets current node goal and dependencies from `ActionGraph`.
- Provides input to `Policy Core`.
- Receives stale / invalidation results from `Reconciler`.
