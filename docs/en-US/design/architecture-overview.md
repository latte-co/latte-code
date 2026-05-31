# Fluxcode Architecture Overview

## Status and Scope

This is the current formal architecture overview for Fluxcode. It aligns the team on a slower implementation rhythm: first deliver a basic, working local-first code agent, then gradually evolve it into a harness-native runtime.

Chinese counterpart: [`docs/zh-CN/design/architecture-overview.md`](../../zh-CN/design/architecture-overview.md).

This document describes design goals, evolution path, and module boundaries. It does not mean `src/` already implements these capabilities.

## 1. Top-level Positioning

From the perspective of the broader software engineering system, Fluxcode is a code-agent `Data Plane`: it reads repositories, understands tasks, calls tools, generates changes, runs verification, produces evidence, and hands results to humans and existing engineering systems. Fluxcode does not replace repo permissions, CI, code review, compliance, release, or deployment gates.

`Control Plane Authority` in this document only means Fluxcode internal runtime authority. This internal authority is not a `v0.1` starting requirement. It forms gradually as execution trace, facts, side effects, transactions, and recovery become structured runtime objects.

Fluxcode therefore uses two layers of narrative:

| Layer | Goal | What the team should understand first |
| --- | --- | --- |
| Near-term product shape | A basic, working local-first code agent | Complete real coding tasks and explain changes, verification, and risks |
| Long-term architecture direction | Harness-native runtime | Structure execution into auditable, recoverable, governable runtime objects |

## 2. Evolutionary Architecture Path

Fluxcode no longer asks the team to start with the full set of `ActionGraph`, `StateStore`, `Scheduler`, `EffectLedger`, `TransactionManager`, and `Reconciler`. The abstractions should grow out of a working code agent.

```text
Basic working code agent
  -> Structured task trace
  -> Evidence and fact discipline
  -> Controlled effects and transactions
  -> Scheduling and reconciliation
  -> Harness-native runtime
```

### 2.1 Stage One: Basic Working Code Agent

The earliest Fluxcode should complete this loop:

- Accept a user task and acceptance criteria.
- Search, read, and understand repository context.
- Generate small, scoped code changes.
- Run user-approved verification commands.
- Report diff, verification result, risks, and blockers.
- Keep a minimal execution record for review.

This stage does not need a full `ActionGraph` or fact system. It may start with `TaskRun`, `StepTrace`, `ToolCallRecord`, `PatchSummary`, and `VerificationResult`.

### 2.2 Stage Two: Structure Execution

After the basic agent can complete tasks, structure its execution records:

- `StepTrace` evolves into `ActionNode`.
- A simple task log evolves into `ActionGraph`.
- Tool output starts to distinguish raw output, summaries, and citable evidence.
- Every change and verification can be traced back to a concrete step.

The goal is not complex scheduling. The goal is to stop carrying task state only in chat transcript.

### 2.3 Stage Three: Evidence, Facts, and Context Projection

After execution trace stabilizes, introduce `Observation`, `Evidence`, `Fact`, and `ContextProjection`:

- Tool output first becomes `Observation`.
- Sourced and scoped material becomes `Evidence`.
- Only checked claims become `Fact`.
- LLM input comes from `ContextProjection`, not direct transcript trimming.

This stage answers: how does the agent know what it knows?

### 2.4 Stage Four: Effects, Transactions, and Recovery

When Fluxcode handles more file edits, shell commands, Git operations, or external APIs, introduce `EffectLedger`, `TransactionManager`, and `Reconciler`:

- Mutating actions have effect declarations before execution.
- File changes bind to overlay or transaction boundaries.
- Verification failures, external file changes, partial effects, and stale facts can trigger recovery or human handoff.

This stage answers: what did the agent do, can it recover, and when must it hand off?

### 2.5 Stage Five: Harness-native Runtime

After the previous capabilities become stable, Fluxcode reaches the full harness-native runtime shape:

- `ActionGraph` becomes execution ledger, scheduling surface, recovery entry, and UX surface.
- `StateStore` manages facts, evidence, and lifecycle.
- `Scheduler` dispatches nodes based on dependencies, gates, budgets, and state.
- `EffectLedger` and `TransactionManager` manage side effects and commit boundaries.
- `Reconciler` handles mismatches across graph, fact, effect, transaction, and reality.

## 3. Basic Code Agent Operating Model

Early Fluxcode should stay direct, explainable, and testable.

This stage should align with the baseline capabilities of mature conversation-native code agents: keep the ReAct query loop, unified tool contract, permission-before-execution, file and shell safety, session recovery, context budget, and CLI / headless reuse. Fluxcode should not sacrifice these basic interaction capabilities in `v0.1` for future runtime abstractions.

Fluxcode's difference is adding a phase artifact boundary around ReAct: the model still explores and edits through tool loops, but each phase must end with a structured object. This preserves the usability of Claude Code style systems while leaving migratable data for later `ActionNode`, `Evidence`, `EffectRecord`, and `ReconcileDecision`.

| Phase | Behavior | Minimal artifact |
| --- | --- | --- |
| Task intake | Record goal, scope, acceptance criteria, and non-goals | `TaskSpec` |
| Repository understanding | Search files, read docs, locate relevant code and tests | `StepTrace`, context summary |
| Planning | Produce a short plan or internal step list | task steps |
| Editing | Generate and apply scoped patches | diff, change rationale |
| Verification | Run declared and allowed test or build commands | `VerificationResult` |
| Handoff | Report changes, evidence, risks, blockers, and next steps | handoff summary |

The early implementation may stay simple, but it should keep records that support later evolution: what each step did, why it did it, what tools it called, what output it produced, whether it modified files, and how verification ended.

## 4. How Runtime Concepts Evolve

| Early object | Mature runtime object | When to introduce |
| --- | --- | --- |
| `TaskSpec` | `TaskSpec` + graph objective | Keep from v0.1 |
| `StepTrace` | `ActionNode` | When recovery, blocking, retry, and audit are needed |
| task log | `ActionGraph` | When steps have dependencies and verification relations |
| tool output | `Observation` | When raw observations and conclusions must be separated |
| verified output | `Evidence` | When verification, file snippets, or user confirmations must be cited |
| accepted claim | `Fact` | When context needs lifecycle and confidence |
| prompt context | `ContextProjection` | When transcript trimming becomes unreliable |
| write summary | `EffectRecord` | When side effects need audit or recovery |
| patch batch | `OverlayRevision` / `Transaction` | When changes need commit gates or rollback |
| retry note | `ReconcileDecision` | When failure cannot be solved by another prompt retry |

## 5. Relationship with Plain `ReAct` Agents

`ReAct` / transcript-driven loops are useful for early exploration and local tool use. Fluxcode may use that strategy in the basic code agent stage, but it should not be the only long-term state carrier.

The recommended `v0.1` design is therefore not removing ReAct, but **phase-gated ReAct**: the outer phase runner manages phase boundaries, budgets, permissions, and artifact schemas, while the inner query loop keeps multi-turn model-tool interaction.

Evolution constraints:

- `v0.1` may be a linear agent loop, but it must leave structured step trace.
- From `v0.2`, important steps should map to `ActionNode`.
- From `v0.3`, tool output and model inference cannot directly become `Fact`.
- From `v0.4`, mutating effects, transactions, and recovery cannot rely only on natural-language notes.
- In the full runtime, node-level bounded ReAct is only a local execution strategy for `NodeExecutor`.

## 6. Module Relationships

Fluxcode modules do not need to be fully implemented at the same time, but their evolution direction should stay consistent. Current / near-term module design lives in [`modules/`](./modules/README.md), while accepted long-term runtime targets live in [`runtime-evolution/`](./runtime-evolution/README.md).

| Module / Object | Early role | Mature role |
| --- | --- | --- |
| `NodeExecutor` | Run linear task steps and call file, search, edit, and verification capabilities | Execute a single `ActionNode` with deterministic, single-decision, or exploratory profile |
| `Capability Adapter` | Wrap basic local tools | Anti-corruption layer that emits runtime-native objects |
| `Policy Core and Guard` | Prevent LLM from writing, running commands, or expanding scope without guardrails | Produce and validate structured `PolicyDecision` |
| `ActionGraph` | Start as `StepTrace` | Execution ledger, scheduling surface, recovery entry, and UX surface |
| `StateStore` | Store task summaries, verification results, and evidence references | Manage `Observation`, `Evidence`, and versioned `Fact` |
| `ContextProjection` | Organize minimal task context | Generate sourced, budgeted, trust-scoped LLM input |
| `EffectLedger` | Record file changes and command summaries | Manage effect declarations, observed effects, and compensation state |
| `TransactionManager` | Manage patch batches and pre-handoff verification | Manage `OverlayRevision`, checkpoint, commit, and rollback |
| `Scheduler` | Start as a linear next-step runner | Dispatch nodes based on dependencies, gates, budgets, and recovery state |
| `Reconciler` | Record failures and blockers | Handle mismatches across graph, fact, effect, and transaction |

## 7. External Collaboration and Governance Boundaries

Fluxcode works with existing engineering systems but does not replace them.

| External object / system | Role in Fluxcode | What it cannot do |
| --- | --- | --- |
| Docs, requirements, designs | User intent, constraints, acceptance background | Directly become internal `Fact` |
| Issues / project work items | External task and collaboration state | Replace Fluxcode execution records |
| PR / code review | External review comments and merge context | Bypass local verification and human judgment |
| CI / tests / static checks | Verification signal and failure evidence | Prove every inference automatically |
| Approval / compliance / release flows | External governance gate | Become Fluxcode internal `Control Plane Authority` |
| Comments / chat / human confirmations | Human feedback and confirmation | Bypass evidence and record boundaries |

## 8. Current Design Invariants

- Fluxcode externally is a code-agent `Data Plane`, not an external engineering `Control Plane`.
- `Control Plane Authority` must be scoped to Fluxcode internal runtime authority, and is introduced incrementally.
- The `v0.1` priority is a basic working code agent, not a full runtime kernel.
- `ActionGraph`, `StateStore`, `Scheduler`, `EffectLedger`, `TransactionManager`, and `Reconciler` are evolution targets; early implementation should not create unnecessary complexity.
- Execution records, tool calls, file changes, and verification results should have traceable entry points from the first stage.
- Design documents must not imply the runtime capabilities are fully implemented.

## 9. Non-goals

- Do not design Fluxcode as an external engineering governance `Control Plane`.
- Do not replace repo permissions, CI, code review, compliance, release, or deployment gates.
- Do not require `v0.1` to deliver the full harness-native runtime.
- Do not make `ActionGraph` an omniscient state database.
- Do not use prompt transcript, model memory, or tool logs as the long-term fact lifecycle system.
- Do not use agent-level global ReAct as the final runtime controller.

## 10. Drill-down Documents

| Document | Role |
| --- | --- |
| This document | Formal architecture overview for the code-agent-first evolution path, module relationships, external boundaries, and non-goals |
| [`../milestones/targets/runtime-kernel-roadmap-v0.1-v0.5.md`](../milestones/targets/runtime-kernel-roadmap-v0.1-v0.5.md) | Stage goals from a basic code agent to harness-native runtime |
| [`../milestones/targets/runtime-kernel-task-breakdown.md`](../milestones/targets/runtime-kernel-task-breakdown.md) | Independent task breakdown with tasks, dependencies, acceptance criteria, and non-goals |
| [`../milestones/targets/v0.1-implementation-plan-review.md`](../milestones/targets/v0.1-implementation-plan-review.md) | `v0.1` implementation plan and technical review covering choices, dependencies, risks, and tests |
| [`modules/code-agent-loop.md`](./modules/code-agent-loop.md) | `Code Agent Loop` module design, defining `Intake -> Understand -> Plan -> Edit -> Verify -> Handoff` |
| [`../milestones/targets/v0.1-engineering-baseline.md`](../milestones/targets/v0.1-engineering-baseline.md) | `v0.1` runnable engineering baseline target for providers, config, tools, prompts, and context compaction |
| [`runtime-evolution/README.md`](./runtime-evolution/README.md) | Accepted long-term runtime evolution entry, separated from current / near-term module design and generic unaccepted proposals |
| [`runtime-evolution/modules/action-graph.md`](./runtime-evolution/modules/action-graph.md) | How `ActionGraph` / `ActionNode` evolve from `StepTrace` |
| [`runtime-evolution/modules/state-store.md`](./runtime-evolution/modules/state-store.md) | Gradual design of `StateStore`, `Observation`, `Evidence`, and `Fact` |
| [`runtime-evolution/modules/scheduler.md`](./runtime-evolution/modules/scheduler.md) | How `Scheduler` evolves from a linear runner |
| [`runtime-evolution/modules/effect-ledger.md`](./runtime-evolution/modules/effect-ledger.md) | How `EffectLedger` evolves from change records |
| [`runtime-evolution/modules/transaction-manager.md`](./runtime-evolution/modules/transaction-manager.md) | How `TransactionManager` evolves from patch batches |
| [`runtime-evolution/modules/reconciler.md`](./runtime-evolution/modules/reconciler.md) | How `Reconciler` evolves from failure records |
| [`runtime-evolution/modules/policy-core-and-guard.md`](./runtime-evolution/modules/policy-core-and-guard.md) | Gradual design of `PolicyDecision`, policy core, guard, and gates |
| [`runtime-evolution/modules/capability-adapter.md`](./runtime-evolution/modules/capability-adapter.md) | Capability adapter, tool boundaries, and runtime-native outputs |
| [`runtime-evolution/modules/context-projection.md`](./runtime-evolution/modules/context-projection.md) | `ContextProjection` design |
| [`runtime-evolution/modules/node-executor.md`](./runtime-evolution/modules/node-executor.md) | `NodeExecutor` profiles and bounded ReAct evolution |
