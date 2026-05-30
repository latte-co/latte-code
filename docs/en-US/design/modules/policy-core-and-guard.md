# Module Technical Design: Policy Core and Guard

## Document Status

Current design placeholder for clarifying LLM policy output and guard boundaries before implementation. This belongs to Fluxcode internal runtime design; externally Fluxcode remains a code-agent `Data Plane`.

Chinese counterpart: [`docs/zh-CN/design/modules/policy-core-and-guard.md`](../../../zh-CN/design/modules/policy-core-and-guard.md).

## Responsibility

- `Policy Core` converts task context into constrained `PolicyDecision` values.
- `PolicyGuard` validates schema, references, permission, safety, trust boundary, and evidence requirements.
- Distinguish errors that can be retried with more context from errors that must reject / ask / escalate.
- Prevent the LLM from directly triggering syscall, file writes, command execution, commit, or rollback.

## Non-goals

- LLM does not own global scheduling authority.
- LLM does not directly call tools.
- LLM inference does not automatically become `Fact`.
- Guard does not replace external compliance or code review.

## Inputs / Outputs

| Direction | Content |
| --- | --- |
| Input | `ContextProjection`, candidate graph, capability metadata, policy constraints |
| Output | valid / rejected `PolicyDecision`, guard failure, missing context request |

## Core Data Contracts

```ts
type PolicyDecision =
  | { kind: "GeneratePatch"; targetNodes: string[]; assumptions: string[]; requiredCapabilities: string[] }
  | { kind: "ExplainFailure"; failedNodeId: string; evidenceIds: string[] }
  | { kind: "AskUser"; question: string; options?: string[]; blockingNodeIds: string[] }
  | { kind: "Abstain"; reason: string; missingContext?: string[] };
```

## Invariants

- `PolicyDecision` must be a closed sum type.
- `GeneratePatch` may only create candidate patches or edit nodes; it cannot write files.
- `AskUser` must bind to blocking nodes or explicit missing context.
- `PermissionInvalid`, `PolicyUnsafe`, and `TrustBoundaryBroken` are not bypassed by prompt retry.

## Failure Modes

- Model output references missing node / evidence / capability.
- Model writes hypothesis as fact.
- Guard failure is wrapped as plain retry.
- Policy output attempts to bypass `EffectLedger` or transaction boundary.

## Testing / Acceptance Direction

- Schema invalidity is detected and returned as structured error.
- Permission / trust failure enters reject / ask / escalate.
- `GeneratePatch` does not create direct write effects.
- Every valid decision traces to projection and evidence refs.

## Relation to Other Modules

- Receives input from `ContextProjection`.
- Submits candidate node or policy reference to `ActionGraph`.
- `Scheduler` decides when related nodes run.
- Shares failure semantics with `TrustGate` / `PermissionGate`.
