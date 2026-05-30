# Runtime Kernel Roadmap v0.1-v0.5

## Document Status

This document defines Fluxcode runtime kernel goals from `v0.1` through `v0.5`. It is a roadmap / architecture design document and does not claim that the current implementation is complete. Detailed task breakdown lives in [`runtime-kernel-task-breakdown.md`](./runtime-kernel-task-breakdown.md).

Chinese counterpart: [`docs/zh-CN/design/runtime-kernel-roadmap-v0.1-v0.5.md`](../../zh-CN/design/runtime-kernel-roadmap-v0.1-v0.5.md).

## 1. Roadmap Overview

Fluxcode should establish minimal internal runtime authority from `v0.1`. Linear execution is acceptable; missing authority boundaries are not.

```text
v0.1 Linear Internal Runtime Authority
  -> v0.2 Capability / Effect / Transaction Hardening
  -> v0.3 Fact / Evidence / Reconcile
  -> v0.4 Scheduler / UX / Multi-executor
  -> v0.5 Evaluation / Security / Adapter Boundary
```

Reference frame: from the perspective of the whole software-engineering system, Fluxcode is a code-agent `Data Plane`; `Control Plane Authority` in this document means only Fluxcode internal runtime authority, not external repo / CI / review / deployment governance.

## 2. Core Principles

- Fluxcode is externally an execution-oriented data-plane component and does not replace external engineering control systems.
- Internal runtime authority owns `StateStore`, `Scheduler`, `EffectLedger`, `TransactionManager`, and `Reconciler`.
- `ActionGraph` is an execution ledger, scheduling surface, and UX surface, not an omniscient state container.
- `transcript` / session is not a source of truth.
- Tool output first becomes `Observation` or `Evidence`; it does not directly become `Fact`.
- `Fact` can only be promoted by `TrustGate` / promotion rule.
- ReAct is an execution strategy, not the runtime architecture; only node-level bounded ReAct is allowed for exploratory `ActionNode` execution.

## 3. Version Table

| Version | Theme | Main goal | Non-goal |
| --- | --- | --- | --- |
| `v0.1` | Linear Internal Runtime Authority | Establish the top-level reference frame, minimal graph / state / effect / transaction / reconcile loop, and `NodeExecutor` profiles | Full parallelism, full OS sandbox, multi-agent platform, global ReAct main controller |
| `v0.2` | Capability / Effect / Transaction Hardening | Harden capability contracts, effect ledger, overlay / transaction, permission, and sandbox contracts | Tool count or marketplace expansion |
| `v0.3` | Fact / Evidence / Reconcile | Complete fact lifecycle, promotion protocol, stale detection, and four reconcile classes | Treating failure as prompt retry |
| `v0.4` | Scheduler / UX / Multi-executor | Add controlled scheduling, graph cockpit, executor profiles, and human handoff UX | Unbounded autonomy or chat-style multi-agent fan-out |
| `v0.5` | Evaluation / Security / Adapter Boundary | Architecture-differentiating evaluation, agency security, anti-corruption layer, adapter boundary policy | Replacing runtime invariants with benchmark pass rate |

## 4. `v0.1`: Linear Internal Runtime Authority

### 4.1 Goal

`v0.1` should prove that even with linear scheduling, few capabilities, and light UI, Fluxcode maintains facts, scheduling, effects, transactions, and recovery boundaries through internal runtime authority instead of transcript or a global ReAct loop.

### 4.2 Required Capabilities

- Reference frame: documents and type names preserve the code-agent `Data Plane` and internal runtime authority boundary.
- `ActionGraph` / `ActionNode`: every executed action is ledgered, auditable, and recoverable.
- `PolicyDecision` / `PolicyGuard`: LLM output is constrained by closed sum type and guard.
- `StateStore`: records `Observation`, `Evidence`, and versioned `Fact`.
- Promotion: mini-loop steps, tool output, and model inference do not directly become `Fact`.
- `EffectLedger`: mutating action declares effect before execution.
- `TransactionManager`: file writes bind to overlay / transaction, and commit passes gates.
- `ContextProjection`: LLM input comes from projection, not transcript trimming.
- `Reconciler`: handles minimal graph, fact, effect, and transaction drift.
- `NodeExecutor` profiles: `deterministic`, `single_decision`, `exploratory`; bounded ReAct belongs only to `exploratory` nodes.

### 4.3 Acceptance Direction

- every executed action has an `ActionNode`.
- every mutating action has an `EffectRecord` before execution.
- every active `Fact` has evidence refs and promotion record.
- stale facts do not enter projection as strong facts.
- bounded ReAct mini-loop cannot directly promote `Fact`, commit, rollback, or modify global scheduling.

## 5. `v0.2`: Capability / Effect / Transaction Hardening

`v0.2` hardens capability, effect, and transaction boundaries so the runtime can handle more real files, commands, and external side effects.

Acceptance direction: capability declares full inputs, outputs, and failure modes; mutating capability cannot bypass `EffectLedger`; `reversible=false` effect requires approval; stale verification or invalid overlay blocks commit.

## 6. `v0.3`: Fact / Evidence / Reconcile

`v0.3` extends `StateStore` from minimal fact recording into a recoverable, explainable, downgrade-aware fact system, and makes `Reconciler` cover graph, fact, effect, and transaction drift.

Acceptance direction: revision changes mark facts stale; conflicting facts are not silently overwritten; partial effects enter reconcile; prompt retry no longer substitutes for reconcile.

## 7. `v0.4`: Scheduler / UX / Multi-executor

`v0.4` strengthens scheduling, graph cockpit, and controlled multi-executor support after fact and effect semantics stabilize. The goal is not “multi-agent chat fan-out”, but scheduler-selected executors under explicit dependencies, permissions, budgets, read/write sets, and transaction boundaries.

Acceptance direction: users can understand blocked reason; scheduler does not execute nodes whose guards failed or whose dependencies rely on stale facts; multiple executors collaborate through `ActionGraph`, `Evidence`, and `Reconciler`.

## 8. `v0.5`: Evaluation / Security / Adapter Boundary

`v0.5` proves that Fluxcode's value comes from runtime invariants and recoverable execution semantics, not only model capability or tool count.

Acceptance direction: benchmarks report both task results and runtime invariants; agency-security failures are intercepted by gates / guards; external protocols only enter through adapters that emit runtime-native objects; failures can be attributed to model, capability, fact, effect, transaction, or scheduler causes.

## 9. Cross-version Invariants

- Fluxcode externally remains a code-agent `Data Plane`.
- `Control Plane Authority` must be scoped to internal runtime authority.
- `ActionGraph` does not become an omniscient state container.
- `Observation`, `Evidence`, and `Fact` remain separate.
- LLM cannot directly syscall, write files, run commands, commit, rollback, or call external APIs.
- Mutating actions must first have `ActionNode`, `EffectRecord`, and transaction / overlay boundary.
- Node-level bounded ReAct may only be a local execution strategy for exploratory `ActionNode`.
- Code changes implementing runtime concepts must update corresponding design docs; formal Chinese and English docs must stay structurally and semantically aligned.
