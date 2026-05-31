# Code Agent Evolution Roadmap v0.1-v0.5

## Status

This document defines Fluxcode's evolution path from `v0.1` through `v0.5`. The historical file name `runtime-kernel-roadmap` is retained to keep existing indexes stable; the content now follows a code-agent-first approach: first build a basic working local-first code agent, then gradually evolve it into a harness-native runtime.

Chinese counterpart: [`docs/zh-CN/milestones/targets/runtime-kernel-roadmap-v0.1-v0.5.md`](../../../zh-CN/milestones/targets/runtime-kernel-roadmap-v0.1-v0.5.md).

## 1. Roadmap Overview

```text
v0.1 Basic Working Code Agent
  -> v0.2 Structured Trace and Tool Discipline
  -> v0.3 Evidence, Facts, and Context Projection
  -> v0.4 Effects, Transactions, and Recovery
  -> v0.5 Harness-native Runtime Hardening
```

Reference frame: Fluxcode is externally a code-agent `Data Plane`; `Control Plane Authority` only means Fluxcode internal runtime authority, and only after runtime structure forms incrementally.

## 2. Core Principles

- First prove Fluxcode can complete real coding tasks as a code agent, then extract runtime abstractions.
- `v0.1` does not require a full `ActionGraph`, `StateStore`, `Scheduler`, `EffectLedger`, `TransactionManager`, or `Reconciler`.
- Keep traceable execution records from day one, so later evolution is possible.
- Tool calls, file changes, and verification results must be reviewable.
- LLMs may help with understanding, planning, and editing, but should not become long-term fact sources or unconstrained tool executors.
- Harness-native runtime is the direction, not the MVP complexity starting point.

## 3. Version Table

| Version | Theme | Main goals | Non-goals |
| --- | --- | --- | --- |
| `v0.1` | Basic Working Code Agent | Complete the minimal loop of task intake, repository understanding, editing, verification, and handoff | Full runtime kernel, parallel scheduling, complex fact system |
| `v0.2` | Structured Trace and Tool Discipline | Structure execution steps as traceable task trace and establish basic capability boundaries | Over-building trace into a full graph platform |
| `v0.3` | Evidence, Facts, and Context Projection | Separate observation, evidence, and fact; introduce minimal context projection | Treating model inference as fact |
| `v0.4` | Effects, Transactions, and Recovery | Manage mutating effects, patch transactions, verification freshness, and failure recovery | Unbounded autonomy or complex multi-agent |
| `v0.5` | Harness-native Runtime Hardening | Consolidate runtime invariants for `ActionGraph`, `StateStore`, `Scheduler`, `EffectLedger`, `TransactionManager`, and `Reconciler` | Replacing architecture invariants with benchmark scores |

## 4. `v0.1`: Basic Working Code Agent

### Goal

`v0.1` should prove Fluxcode can complete a real coding task in a local repository: understand context, modify files, run verification, and report results.

The implementation should build on a Claude Code style conversation-native query loop, but add Fluxcode's own phase artifact boundary. The model still interacts with tools through ReAct; phase completion requires structured objects such as `TaskSpec`, `ContextPack`, `ChangePlan`, `PatchSummary`, `VerificationResult`, or `AgentHandoff`.

### Required Capabilities

- `TaskSpec`: record user goal, scope, acceptance criteria, and non-goals.
- Phase-gated ReAct: each phase keeps model / tool / observation loops, constrained by budget, tool allowlist, and artifact schema.
- Tool contract: tools declare schema, read-only / mutating status, permission requirements, risk level, and result summary.
- Permission pipeline: tools pass allow / deny / ask decisions before execution, and decisions are recorded.
- Repository search and read: locate relevant files, tests, and docs.
- Editing: generate and apply small scoped patches.
- Verification: run declared and user-approved commands such as `npm test`.
- `StepTrace`: record key steps, tool calls, change summaries, and verification results.
- Handoff: report change summary, verification evidence, risks, and blockers.

### Acceptance Direction

- Complete at least one end-to-end coding task.
- Commands such as `fluxcode run "implement a snake game"` enter the real agent loop; if the target repository has the application and test foundation, they produce code, tests, and verification results.
- If the repository lacks required framework, test, or dependency decisions, Fluxcode asks for confirmation instead of silently scaffolding or installing dependencies.
- Every tool call and file change has a readable record.
- Verification command, result, and failure information are recorded.
- The final report lets users judge what changed, why, and whether it was verified.

## 5. `v0.2`: Structured Trace and Tool Discipline

`v0.2` structures the `v0.1` execution log so it can naturally evolve into `ActionGraph` and `ActionNode`.

### Required Capabilities

- Add step id, parent, status, inputs, and outputs to `StepTrace`.
- Basic `CapabilityDescriptor` for file, search, shell, Git, LSP, and model calls.
- Simple `PolicyGuard` to block out-of-scope paths, undeclared commands, and broad unrelated edits.
- Early `NodeExecutor` shape as a linear task-step executor.

### Acceptance Direction

- Every important step can map to a future `ActionNode`.
- Tool calls are no longer only free-text logs.
- Failed steps have explicit status and reasons.

## 6. `v0.3`: Evidence, Facts, and Context Projection

`v0.3` addresses context trustworthiness so the agent does not treat all transcript content as fact.

### Required Capabilities

- `Observation`: raw observations from tools, users, or environment.
- `Evidence`: sourced, timed, scoped, and artifact-linked evidence.
- Minimal `Fact`: claims that are verified or user-confirmed.
- `ContextProjection`: select minimal context for the current step and mark sources and uncertainty.

### Acceptance Direction

- Tool output does not automatically become `Fact`.
- LLM hypothesis and verified fact are distinguishable.
- Stale or uncertain material does not enter prompts as strong fact.

## 7. `v0.4`: Effects, Transactions, and Recovery

`v0.4` handles side effects and recovery in real engineering changes.

### Required Capabilities

- `EffectRecord`: record file writes, shell, Git, external API, and other mutating actions.
- `OverlayRevision` / transaction: bind a patch set and verification results.
- Transaction gate: block commit on stale verification, invalid overlay, or non-compensable effect.
- Lightweight `Reconciler`: handle failed step, partial effect, stale fact, and invalidated patch.

### Acceptance Direction

- Mutating actions have effect records.
- Before handoff or commit, Fluxcode can explain whether verification is still fresh.
- Failure is not hidden behind prompt retry.

## 8. `v0.5`: Harness-native Runtime Hardening

`v0.5` consolidates objects from previous stages into a harness-native runtime.

### Required Capabilities

- `ActionGraph` becomes execution ledger, scheduling surface, recovery entry, and UX surface.
- `StateStore` manages `Observation`, `Evidence`, and versioned `Fact`.
- `Scheduler` runs nodes based on dependencies, gates, budgets, and recovery state.
- `EffectLedger` and `TransactionManager` manage side effects and transactions.
- `Reconciler` covers graph, fact, effect, and transaction mismatch classes.

### Acceptance Direction

- Runtime invariants can be tested and explained.
- Users can understand blocked reasons, risks, and recovery options.
- External system signals can only enter runtime through adapters, evidence, or gates.

## 9. Cross-version Invariants

- Fluxcode externally remains a code-agent `Data Plane`.
- `Control Plane Authority` must be scoped to internal runtime authority.
- `v0.1` prioritizes a basic working agent, not a full runtime.
- Each stage introduces only the minimal abstraction needed for current problems.
- Execution records, tool calls, file changes, and verification results must be traceable.
- Runtime concept implementation must update the corresponding design documents; formal Chinese and English documents must remain structurally and semantically aligned.
