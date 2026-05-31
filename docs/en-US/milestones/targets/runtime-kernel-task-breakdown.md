# Code Agent Evolution Task Breakdown v0.1-v0.5

## Status

This is the independent task breakdown for evolving Fluxcode from a basic code agent to a harness-native runtime. The historical file name `runtime-kernel-task-breakdown` is retained to keep existing indexes stable; the content now follows an incremental implementation plan.

Chinese counterpart: [`docs/zh-CN/milestones/targets/runtime-kernel-task-breakdown.md`](../../../zh-CN/milestones/targets/runtime-kernel-task-breakdown.md).

## 1. Overall Dependency

```text
v0.1 Basic working code agent
  -> v0.2 Structured trace and tool discipline
  -> v0.3 Evidence, facts, and context projection
  -> v0.4 Effects, transactions, and recovery
  -> v0.5 Harness-native runtime hardening
```

Shared premise: Fluxcode is externally a code-agent `Data Plane`; internal `Control Plane Authority` only means Fluxcode internal runtime authority, and is a gradually formed target.

## 2. `v0.1`: Basic Working Code Agent

### Tasks

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.1-task-spec` | Define minimal `TaskSpec`: goal, scope, acceptance criteria, non-goals | None | User tasks can be saved as structured input |
| `v0.1-phase-gated-react` | Implement phase-gated ReAct on the existing query loop: tool loops are allowed inside phases, and phase completion validates structured artifacts | `v0.1-task-spec` | `Understand`, `Plan`, `Edit`, and `Verify` can use ReAct but must produce schema-valid objects |
| `v0.1-tool-contract` | Establish basic tool contract: schema, read-only / mutating status, permission requirements, risk level, result summary | `v0.1-phase-gated-react` | Tool calls can be validated before execution, tool results are recorded, dangerous tools do not execute naked |
| `v0.1-permission-pipeline` | Establish allow / deny / ask permission pipeline and write decisions into events and trace | `v0.1-tool-contract` | Unauthorized command or path access blocks / asks instead of continuing |
| `v0.1-repo-read` | Support file search, reading, and context summaries | `v0.1-task-spec` | Agent can locate relevant code, tests, and docs |
| `v0.1-edit-loop` | Support small scoped patch generation and application | `v0.1-repo-read` | Diff is explainable and limited to task-related files |
| `v0.1-verification` | Support running declared and allowed verification commands | `v0.1-edit-loop` | Command, exit code, and key output are recorded |
| `v0.1-step-trace` | Record key steps, tool calls, change summaries, and verification results | `v0.1-repo-read`, `v0.1-verification` | Final report can trace each key action |
| `v0.1-handoff` | Report change summary, verification result, risks, and blockers | `v0.1-step-trace` | User can decide whether to accept the change |

### Non-goals

- Full runtime kernel.
- Full `ActionGraph` / `StateStore` / `Scheduler`.
- Full OS sandbox.
- Multi-agent parallel collaboration.
- Automated PR, release pipeline, or remote server.
- Full MCP, IDE, TUI, telemetry, or git automation alignment.

## 3. `v0.2`: Structured Trace and Tool Discipline

### Tasks

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.2-trace-schema` | Extend `StepTrace`: id, parent, status, inputs, outputs, artifacts | `v0.1-step-trace` | Trace can map to future `ActionNode` |
| `v0.2-capability-descriptor` | Define basic capability descriptor | `v0.1-repo-read`, `v0.1-verification` | File, search, shell, Git, LSP, and model capabilities describe inputs, outputs, and risks |
| `v0.2-policy-guard` | Add minimal guard: path scope, command allowlist, write scope, dangerous operation confirmation | `v0.2-capability-descriptor` | Out-of-scope operations are rejected or ask the user |
| `v0.2-node-executor-lite` | Wrap linear step execution as early `NodeExecutor` | `v0.2-trace-schema` | Execution steps produce structured results |
| `v0.2-action-node-seed` | Introduce lightweight `ActionNode` as a structured alias for trace | `v0.2-node-executor-lite` | Step dependency and status can be expressed without complex graph machinery |

### Non-goals

- Complex DAG scheduling.
- Full policy engine.
- Capability marketplace.
- Splitting services for abstraction's sake.

## 4. `v0.3`: Evidence, Facts, and Context Projection

### Tasks

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.3-observation-evidence` | Define minimal `Observation` and `Evidence` models | `v0.2-trace-schema` | Tool output has source, time, boundary, and artifact refs |
| `v0.3-fact-lite` | Define minimal `Fact` and lifecycle | `v0.3-observation-evidence` | Only verified or user-confirmed claims become facts |
| `v0.3-promotion-rule` | Add basic promotion rules | `v0.3-fact-lite` | LLM hypothesis cannot directly become active fact |
| `v0.3-context-projection` | Organize LLM input with `ContextProjection` | `v0.3-fact-lite` | Prompt-critical material can be traced to fact / evidence / hypothesis |
| `v0.3-stale-marking` | Mark related facts stale when files or overlay change | `v0.3-context-projection` | Stale fact does not enter projection as strong fact |

### Non-goals

- Full knowledge graph.
- Treating recall as verification.
- Creating strong facts for every text fragment.

## 5. `v0.4`: Effects, Transactions, and Recovery

### Tasks

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.4-effect-record` | Define `EffectRecord` and record mutating actions | `v0.2-capability-descriptor` | File writes, shell, Git, and external API have effect records |
| `v0.4-overlay-transaction` | Establish `OverlayRevision` / transaction boundary | `v0.4-effect-record` | Patch batch, verification result, and commit status are linked |
| `v0.4-transaction-gate` | Check verification freshness, overlay state, and non-compensable effects before commit | `v0.4-overlay-transaction`, `v0.3-stale-marking` | Stale verification or invalid overlay blocks commit |
| `v0.4-light-reconciler` | Handle failed step, partial effect, stale fact, and invalidated patch | `v0.4-transaction-gate` | Failure enters block / repair / ask, not silent retry |
| `v0.4-human-handoff` | Standardize risk, irreversible effect, and user confirmation requests | `v0.4-light-reconciler` | User confirmation becomes auditable evidence / gate record |

### Non-goals

- Unbounded autonomy.
- Chat-style multi-agent fan-out.
- Replacing Git, CI, or code review.

## 6. `v0.5`: Harness-native Runtime Hardening

### Tasks

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.5-action-graph` | Consolidate trace / action node into formal `ActionGraph` | `v0.2-action-node-seed`, `v0.4-light-reconciler` | Graph can express dependency, blocking, verification, and recovery relations |
| `v0.5-state-store` | Consolidate evidence / fact lite into formal `StateStore` | `v0.3-fact-lite` | Fact lifecycle, coverage, confidence, and evidence refs are auditable |
| `v0.5-scheduler` | Evolve linear runner into dependency-aware `Scheduler` | `v0.5-action-graph` | Nodes with failed guards or stale fact dependencies do not run |
| `v0.5-effect-transaction-hardening` | Harden `EffectLedger` and `TransactionManager` invariants | `v0.4-overlay-transaction` | Mutating actions cannot bypass effect / transaction boundary |
| `v0.5-reconciler` | Cover graph, fact, effect, and transaction reconcile classes | `v0.5-state-store`, `v0.5-effect-transaction-hardening` | Partial effect, stale fact, and invalid overlay enter reconcile |
| `v0.5-invariant-eval` | Add runtime invariant tests and architecture demos | `v0.5-reconciler` | Benchmark report shows both task result and runtime invariant result |

### Non-goals

- Replacing runtime invariants with benchmark scores.
- Describing Fluxcode as external CI / review / deployment gate.
- Letting external protocols write directly into internal `StateStore`, `EffectLedger`, or `TransactionManager`.

## 7. Cross-version Acceptance Checklist

- All current formal design documents remain structurally aligned in Chinese and English.
- All `Control Plane Authority` wording is scoped to internal runtime authority.
- `v0.1` tasks run end to end instead of only defining abstract types.
- Each stage introduces only the minimal runtime concept needed for current problems.
- README only points to currently maintained formal design documents or explicitly non-design research documents.
