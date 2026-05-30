# Runtime Kernel Task Breakdown v0.1-v0.5

## Document Status

This document is the independent task breakdown for the Fluxcode runtime kernel. It supports implementation planning, acceptance tracking, and dependency management. It does not claim that the current implementation is complete. [`architecture-overview.md`](./architecture-overview.md) is the architecture reference; [`runtime-kernel-roadmap-v0.1-v0.5.md`](./runtime-kernel-roadmap-v0.1-v0.5.md) owns version-level goals.

Chinese counterpart: [`docs/zh-CN/design/runtime-kernel-task-breakdown.md`](../../zh-CN/design/runtime-kernel-task-breakdown.md).

## 1. Overall Dependency

```text
v0.1 Reference frame + linear internal runtime authority
  -> v0.2 Capability / Effect / Transaction hardening
  -> v0.3 Fact / Evidence / Reconcile
  -> v0.4 Scheduler / UX / Multi-executor
  -> v0.5 Evaluation / Security / Adapter Boundary
```

Shared premise across all versions: Fluxcode is externally a code-agent `Data Plane`; internal `Control Plane Authority` only means Fluxcode internal runtime authority.

## 2. `v0.1`: Linear Internal Runtime Authority

### 2.1 Task List

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.1-doc-ref-frame` | Establish the design reference frame: Code Agent is externally `Data Plane`; `Control Plane Authority` is scoped to internal runtime authority | None | README, overview, roadmap, and module docs describe Fluxcode as a data-plane code agent with internal runtime authority |
| `v0.1-node-executor-profiles` | Define `deterministic`, `single_decision`, and `exploratory` `NodeExecutor` profiles | `v0.1-doc-ref-frame` | `node-executor.md` states that bounded ReAct mini-loop applies only to exploratory nodes |
| `v0.1-action-graph` | Establish minimal `ActionGraph` / `ActionNode` contracts | `v0.1-doc-ref-frame` | Every executed action has an `ActionNode`; graph stores references only, not all facts and effects |
| `v0.1-policy-core-guard` | Establish minimal `PolicyDecision` sum type and `PolicyGuard` failure semantics | `v0.1-action-graph` | LLM can only emit constrained decisions; it cannot syscall, write files, commit, or rollback directly |
| `v0.1-state-store` | Establish minimal `Observation`, `Evidence`, and versioned `Fact` model | `v0.1-doc-ref-frame` | Active `Fact` records have `evidenceIds`, lifecycle, coverage, and confidence |
| `v0.1-promotion` | Establish initial promotion rule / `TrustGate` | `v0.1-state-store` | Tool output and mini-loop steps do not directly become `Fact` |
| `v0.1-effect-ledger` | Establish minimal `EffectRecord` and effect declaration path | `v0.1-action-graph` | Mutating action declares effect before execution |
| `v0.1-transaction` | Establish minimal `OverlayRevision` / `Transaction` boundary | `v0.1-effect-ledger` | File writes bind to overlay or transaction; commit passes `transaction_gate` |
| `v0.1-context-projection` | Use `ContextProjection` instead of transcript trimming for LLM input | `v0.1-state-store` | Stale / invalidated facts do not enter projection as strong facts |
| `v0.1-light-reconciler` | Support lightweight graph / fact / effect / transaction reconcile | `v0.1-state-store`, `v0.1-effect-ledger`, `v0.1-transaction` | failed / partial / stale / invalidated state blocks or repairs downstream nodes |

### 2.2 Non-goals

- Full parallel scheduler.
- Full OS sandbox.
- Large-scale multi-agent or executor fleet.
- Automatic PR, release pipeline, or remote server.
- Global ReAct loop as the runtime main controller.

## 3. `v0.2`: Capability / Effect / Transaction Hardening

### 3.1 Task List

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.2-capability-contract` | Complete primitive / semantic capability schemas | `v0.1-policy-core-guard` | Capability declares input/output, pre/post, evidence requirements, and failure modes |
| `v0.2-capability-state` | Add `declared` / `observed` / `effective` states | `v0.2-capability-contract` | Degraded / blocked capability enters `Scheduler` block or downgrade path |
| `v0.2-effect-model` | Distinguish expected / observed / effective effects | `v0.1-effect-ledger` | Mismatch between declared and observed effects enters effect reconcile |
| `v0.2-sandbox-contract` | Define S0-S4 sandbox-level semantics | `v0.2-capability-contract` | Every capability execution records sandbox boundary |
| `v0.2-permission-model` | Authorize by node / capability scope | `v0.2-capability-contract` | Unauthorized action is blocked by `permission_gate`, not retried through prompt |
| `v0.2-transaction-hardening` | Harden overlay diff, rollback handle, commit gate, compensation marker | `v0.1-transaction` | Stale verification or invalid overlay blocks commit |

### 3.2 Non-goals

- Tool count or marketplace expansion.
- Bypassing `EffectLedger` through tool adapters.
- Claiming that sandbox levels already provide full OS isolation.

## 4. `v0.3`: Fact / Evidence / Reconcile

### 4.1 Task List

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.3-promotion-rules` | Define promotion rules for test, typecheck, LSP, tree-sitter, and user confirmation | `v0.1-promotion` | Each active `Fact` class can explain its evidence source and coverage |
| `v0.3-fact-lifecycle` | Complete triggers and history for stale, superseded, invalidated, and retracted facts | `v0.1-state-store` | Revision / overlay changes update related fact lifecycle |
| `v0.3-conflict-handling` | Support conflicting facts through coexistence, downgrade, retraction, or verification request | `v0.3-fact-lifecycle` | Conflicting facts are not silently overwritten |
| `v0.3-evidence-store` | Support evidence summary, artifact ref, raw hash, and revision binding | `v0.1-state-store` | Evidence traces to producer, boundary, and action node |
| `v0.3-reconcile-protocol` | Implement a unified entry for graph / fact / effect / transaction reconcile | `v0.1-light-reconciler`, `v0.2-effect-model` | Partial effects, stale facts, and invalid overlays enter reconcile |
| `v0.3-entropy-control` | Clean dead pending nodes, failed speculation, duplicate evidence, outdated assumptions | `v0.3-reconcile-protocol` | Stale intermediate state does not pollute new projections |

### 4.2 Non-goals

- Treating failure as prompt retry.
- Treating recall signals as verification results.
- Representing unverified model inference as high-confidence `Fact`.

## 5. `v0.4`: Scheduler / UX / Multi-executor

### 5.1 Task List

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.4-ready-queue` | Support dependency-aware ready queue, retry budget, blocked reason, cancellation, resume cursor | `v0.3-reconcile-protocol` | Scheduler does not run nodes whose guards failed or whose dependencies rely on stale facts |
| `v0.4-conflict-policy` | Detect conflicts with `readSet` / `writeSet`; support fail / rebase / merge / ask | `v0.2-transaction-hardening` | Write conflicts are not silently overwritten |
| `v0.4-ux-cockpit` | Show Action Graph, Fact/Evidence, Effect/Transaction, Reconcile, and Escalation views | `v0.4-ready-queue` | Users understand blockers and next steps without reading raw logs |
| `v0.4-human-handoff` | Standardize `ApprovalRequest`, pre-escalation duty, and reconcile after user override | `v0.2-permission-model`, `v0.3-reconcile-protocol` | User confirmation becomes auditable evidence / gate record |
| `v0.4-executor-profile` | Implement executor capability, permission, evidence policy, and result schema | `v0.1-node-executor-profiles` | Executors run under profiles and do not share unstructured context |
| `v0.4-multi-executor` | Support controlled parallelism or role-specific executors under transaction and effect ledger constraints | `v0.4-executor-profile`, `v0.4-conflict-policy` | Multiple executors collaborate through `ActionGraph`, `Evidence`, and `Reconciler` |

### 5.2 Non-goals

- Unbounded autonomy.
- Chat-style multi-agent fan-out.
- Executors directly owning global scheduling authority.

## 6. `v0.5`: Evaluation / Security / Adapter Boundary

### 6.1 Task List

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.5-invariant-eval` | Automatically check action, effect, fact, transaction, guard, projection, and adapter invariants | `v0.4-ready-queue` | Benchmark reports show both task results and runtime invariant results |
| `v0.5-external-benchmark` | Record completion, verification pass rate, approval count, rollback count, stale fact detection, harness cost / latency | `v0.5-invariant-eval` | Metrics do not replace invariants; they report external behavior only |
| `v0.5-architecture-demos` | Demonstrate overlay rollback, stale fact reconcile, and non-compensable effect escalation | `v0.3-reconcile-protocol`, `v0.2-transaction-hardening` | Each demo binds to evidence and gate records |
| `v0.5-agency-security` | Mitigate tool poisoning, prompt-in-tool-output, cross-tool data exfiltration, evidence injection | `v0.2-sandbox-contract`, `v0.2-permission-model` | Agency-security failures are intercepted and audited by guard / gate |
| `v0.5-anti-corruption` | Stabilize MCP, OpenAI tool-call, LSP, Git, IDE, shell, and test-runner adapter boundaries | `v0.2-capability-contract` | External protocols only enter through adapters that emit runtime-native objects |
| `v0.5-adapter-boundary-policy` | Ensure external protocol fields do not directly enter internal stores / ledgers / transaction manager | `v0.5-anti-corruption` | Failures can be attributed to model, capability, fact, effect, transaction, or scheduler causes |

### 6.2 Non-goals

- Replacing runtime invariants with benchmark pass rate.
- Describing Fluxcode as an external CI / review / deployment gate.
- Letting external protocols write directly into internal `StateStore`, `EffectLedger`, or `TransactionManager`.

## 7. Cross-version Acceptance Checklist

- Current formal design documents stay structurally aligned across Chinese and English.
- Every `Control Plane Authority` reference is scoped to internal runtime authority.
- Every mutating action first enters `ActionNode`, `EffectLedger`, and transaction / overlay boundary.
- Bounded ReAct is only a local execution strategy for `exploratory` nodes.
- Mini-loop results cannot directly become `Fact`.
- README files only point to current maintained formal design documents or clearly non-design research documents.
