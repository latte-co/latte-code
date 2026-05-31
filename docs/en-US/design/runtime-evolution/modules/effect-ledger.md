# Runtime Evolution Target: EffectLedger

## Status

This document defines the evolutionary design for `EffectLedger`. Early implementation may record change and command summaries; when side effects need audit, recovery, or human approval, it evolves into a formal effect ledger.

Chinese counterpart: [`docs/zh-CN/design/runtime-evolution/modules/effect-ledger.md`](../../../../zh-CN/design/runtime-evolution/modules/effect-ledger.md).

## Evolution Rhythm

| Stage | Shape | Goal |
| --- | --- | --- |
| v0.1 | change / command summary | Record changed files and executed commands |
| v0.2 | capability effect metadata | Separate read-only and mutating capabilities |
| v0.4 | `EffectRecord` | Record mutating action declaration, result, and recovery state |
| v0.5 | `EffectLedger` | Support effect audit, compensation, and reconcile |

## Responsibilities

- Record effect declarations, execution results, and compensation state for all mutating actions.
- Separate expected / observed / effective effect.
- Provide effect evidence for transactions, reconcile, audit, and human handoff.
- Prevent undeclared file, shell, network, Git, or external API side effects from entering runtime.

## Non-goals

- `v0.1` does not require a full side-effect ledger.
- Do not directly execute capabilities.
- Do not decide whether business facts are true.
- Do not replace OS sandbox or external permission systems.
- Do not allow tool logs to replace effect records.

## Minimal Contract

```ts
type EffectRecord = {
  id: string;
  stepId: string;
  kind: "file_write" | "command" | "network" | "external_api" | "git" | "approval";
  target: string;
  reversible: boolean;
  status: "planned" | "applied" | "failed" | "partial" | "compensated";
  transactionId?: string;
};
```

## Invariants

- From `v0.1`, file changes and command execution must appear in final records.
- From `v0.4`, mutating actions must have a `planned` effect before execution.
- Observed effects must trace back to a step / action node and capability adapter.
- `reversible=false` effects must pass the relevant gate before execution.
- Partial / failed effects must enter reconcile.

## Acceptance Direction

- File writes, shell commands, and external APIs all have effect records.
- Declared and observed mismatches trigger effect reconcile.
- Non-compensable effects without approval are blocked.
- Effect status remains explainable after rollback / compensation.

## Relationships

- `NodeExecutor` may trigger effects only through capability adapters.
- `TransactionManager` binds mutating effects to overlay / transaction.
- `Reconciler` handles failed / partial / compensated effects.
- `ActionGraph` references effect ids as audit indexes.
