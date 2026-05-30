# Module Technical Design: EffectLedger

## Document Status

Current design placeholder for clarifying the `EffectLedger` side-effect ledger boundary before implementation. This belongs to Fluxcode internal runtime design; externally Fluxcode remains a code-agent `Data Plane`.

Chinese counterpart: [`docs/zh-CN/design/modules/effect-ledger.md`](../../../zh-CN/design/modules/effect-ledger.md).

## Responsibility

- Record effect declaration, execution result, and compensation state for every mutating action.
- Distinguish expected / observed / effective effect.
- Provide effect evidence for transaction, reconcile, audit, and human handoff.
- Prevent undeclared file, shell, network, Git, external API, and similar side effects from entering the runtime.

## Non-goals

- Does not execute capabilities directly.
- Does not decide whether business facts are true.
- Does not replace OS sandbox or external permission systems.
- Does not allow tool logs to replace effect records.

## Inputs / Outputs

| Direction | Content |
| --- | --- |
| Input | effect declaration, capability result, sandbox metadata, compensation record |
| Output | `EffectRecord`, effect status, effect mismatch event, audit summary |

## Core Data Contracts

```ts
type EffectRecord = {
  id: string;
  actionNodeId: string;
  kind: "file_write" | "command" | "network" | "external_api" | "git" | "approval";
  target: string;
  inputDigest: string;
  reversible: boolean;
  status: "planned" | "applied" | "failed" | "partial" | "compensated";
  transactionId?: string;
};
```

## Invariants

- Mutating action must have `planned` effect before execution.
- Observed effect must trace to action node and capability adapter.
- `reversible=false` effect must pass the corresponding gate before execution.
- Partial / failed effect must enter reconcile.

## Failure Modes

- Tool produces undeclared side effect.
- Output summary hides actual modification scope.
- Compensation is marked successful while external state is not restored.
- Effect lacks transaction binding.

## Testing / Acceptance Direction

- File write, shell command, and external API calls all produce effect records.
- Mismatch between declared and observed effect triggers effect reconcile.
- Non-compensable effect without approval is blocked.
- After rollback / compensation, effect status is explainable.

## Relation to Other Modules

- `NodeExecutor` can only trigger declared effects through capability adapters.
- `TransactionManager` binds mutating effects to overlay / transaction.
- `Reconciler` handles failed / partial / compensated effects.
- `ActionGraph` references effect ids as audit indexes.
