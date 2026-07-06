# Code Agent Evolution Roadmap v0.1-v0.5

## Status

This document defines Lattecode's evolution path from `v0.1` through `v0.5`. The historical file name `runtime-kernel-roadmap` is retained to keep existing indexes stable; the content now follows a code-agent-first approach: first build a basic working code agent for local repository workflows, then let runtime structure grow from traces, evidence, permissions, effects, and recovery needs.

Chinese counterpart: [`docs/zh-CN/milestones/targets/runtime-kernel-roadmap-v0.1-v0.5.md`](../../../zh-CN/milestones/targets/runtime-kernel-roadmap-v0.1-v0.5.md).

## 1. Roadmap Overview

```text
v0.1 Basic Working Code Agent
  -> v0.2 Structured Trace and Tool Discipline
  -> v0.3 Evidence, Facts, and Context Projection
  -> v0.4 Controlled Effects, Transactions, and Recovery
  -> v0.5 Internal Runtime Hardening
```

Reference frame: Lattecode is externally a code-agent `Data Plane`; `Control Plane Authority` only means Lattecode internal runtime authority, and only after runtime structure forms incrementally.

## 2. Core Principles

- First prove Lattecode can complete real coding tasks as a code agent, then extract runtime abstractions.
- `v0.1` does not require a full `ActionGraph`, `StateStore`, `Scheduler`, `EffectLedger`, `TransactionManager`, or `Reconciler`.
- Keep traceable execution records from day one, so later evolution is possible.
- Tool calls, file changes, and verification results must be reviewable.
- LLMs may help with understanding, planning, and editing, but should not become long-term fact sources or unconstrained tool executors.
- Full internal runtime structure is the long-term direction, not the MVP complexity starting point.

## 3. Version Table

| Version | Theme | Main goals | Non-goals |
| --- | --- | --- | --- |
| `v0.1` | Basic Working Code Agent | Complete the minimal contract-first code agent loop: CLI, config, `AGENTS.md`, session, agent-loop, tools, minimal MCP, local skills, local commands, permission, evidence / trace, handoff | Full runtime kernel, parallel scheduling, complex fact system, or full ecosystem platform |
| `v0.2` | Structured Trace and Tool Discipline | Structure execution steps as traceable task trace and establish basic capability boundaries | Over-building trace into a full graph platform |
| `v0.3` | Evidence, Facts, and Context Projection | Separate observation, evidence, and fact; introduce minimal context projection | Treating model inference as fact |
| `v0.4` | Controlled Effects, Transactions, and Recovery | Harden extension / effect / transaction boundaries; introduce `EffectRecord`, overlay / transaction lite, transaction gate, and recovery / reconcile semantics so mutating actions are no longer only natural-language records | Replacing the runtime mainline with ecosystem, MCP, plugins, skills, hooks, LSP, or TUI work; MCP / skills are not first introduced here |
| `v0.5` | Internal Runtime Hardening | Consolidate runtime invariants for `ActionGraph`, `StateStore`, `Scheduler`, `EffectLedger`, `TransactionManager`, and `Reconciler` | Replacing architecture invariants with benchmark scores |

## 3.1 TUI / renderer evolution lane

This document re-establishes TUI / output decisions as the roadmap's renderer evolution lane. This lane does not expand `v0.1` release acceptance and does not imply the current `src/` already implements these capabilities.

| Version | TUI / renderer posture |
| --- | --- |
| `v0.1` | The release-critical path remains headless JSON / text `AgentHandoff`; no formal TUI / IDE cockpit is delivered; `--output json` / `--output text` remain available; if `--ui tui` or an experimental command exists, it is opt-in only and must not change the handoff schema. |
| `v0.2` | Define renderer-neutral `TuiViewModel` and `PlainTextRenderer` fallback; an Ink experimental PoC may start, but UI only consumes runtime events / handoff and does not drive permissions, sessions, or runtime mutation. |
| `v0.3` | After structured trace, evidence refs, and context projection stabilize, extend the TUI view model for trace / evidence display; add PoC acceptance for streaming, backpressure, 1k / 10k events, resize, stdout / stderr mixed output, Ctrl+C / crash terminal restore, and snapshot fallback. |
| `v0.4+` | Set only an `OpenTUI` adapter evaluation gate for cockpit density, install burden, native build reliability, and fallback behavior; do not make `OpenTUI` a required `v0.4` deliverable, default dependency, or release deliverable. |
| `v0.5+` | If `ActionGraph` becomes a real UX surface, reconsider cockpit hardening candidates; still preserve headless JSON / text and the `PlainTextRenderer` recovery path. |

Shared boundary: runtime core must not import `react`, `ink`, or `@opentui/*`; the main runtime must not introduce Bun / Zig / native build chains; TUI must not become the only output channel or bypass schema, permission, evidence, trace, or handoff.

## 4. `v0.1`: Basic Working Code Agent

### Goal

`v0.1` should prove Lattecode can complete a real coding task in a local repository: understand context, modify files, run verification, and report results.

The implementation should build on a Claude Code style conversation-native query loop, but add Lattecode's own phase artifact boundary. The model still interacts with tools through ReAct; phase completion requires structured objects such as `TaskSpec`, `ContextPack`, `ChangePlan`, `PatchSummary`, `VerificationResult`, or `AgentHandoff`.

### Required Capabilities

- CLI: minimal headless entries such as `lattecode run`, resume, show, and list.
- Config: project-local JSONC config for models, runtime, tools, permissions, session, commands, skills, and MCP, with no secrets storage.
- `AGENTS.md` loader: read constraints within repo root / cwd boundaries, record snapshot/hash, and enter context snapshot.
- Session lifecycle: create / list / show / resume; stable session id; fixed cwd / repo root; `TaskRunState.status` only uses `queued`, `running`, `waiting_permission`, `blocked`, `failed`, `completed`.
- `TaskSpec`: record user goal, scope, acceptance criteria, and non-goals.
- Phase-gated ReAct: each phase keeps model / tool / observation loops, constrained by budget, tool allowlist, and artifact schema.
- Agent loop / phase runner: the main path is `CLI / local command / local skill / minimal MCP bridge / built-in tools -> TaskSpec -> Session / TaskRunState -> ContextPack -> AgentLoop / PhaseRunner -> PermissionDecision -> Evidence / StepTrace -> AgentHandoff`.
- Tool contract: tools declare schema, read-only / mutating status, permission requirements, risk level, and result summary.
- Built-in tools: minimal usable read/search/edit/write/shell/manifest/minimal diff summary set; `v0.1` diff only covers changed files / diff summary for `AgentHandoff` and safety review.
- Minimal MCP bridge: config-defined servers; list/call tools; unified permission, evidence, trace, and session; disabled by default or explicitly enabled; no marketplace, resource / prompt platform, or server-management UI; no permission bypass.
- Local skill loader: local instruction / workflow / command bundles; inject into context / prompt registry; no hub, install, publish, or marketplace; no direct side effects.
- Local command specs: built-in / local commands route through `TaskSpec`, phase event, and session, without bypassing agent-loop / permission / session.
- Permission pipeline: tools pass allow / deny / ask decisions before execution, and decisions are recorded.
- Repository search and read: locate relevant files, tests, and docs.
- Editing: generate and apply small scoped patches.
- Verification: run declared and user-approved commands such as `npm test`.
- `StepTrace`: record key steps, tool calls, change summaries, and verification results.
- Handoff: report change summary, verification evidence, risks, and blockers.
- Output contract: keep `AgentHandoff` stable through JSON / text headless output; TUI is not part of `v0.1` acceptance.

### Acceptance Direction

- Complete at least one end-to-end coding task.
- Commands such as `lattecode run "implement a snake game"` enter the real agent loop; if the target repository has the application and test foundation, they produce code, tests, and verification results.
- If the repository lacks required framework, test, or dependency decisions, Lattecode asks for confirmation instead of silently scaffolding or installing dependencies.
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
- Renderer-neutral `TuiViewModel` / `PlainTextRenderer` contract: provide read-only projection and fallback for later TUI PoC without changing runtime authority.

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
- TUI PoC acceptance: if an Ink experimental path exists, it must cover Node20 / Node22 matrix, non-TTY fallback, streaming / backpressure, 1k / 10k events, resize, stdout / stderr mixed output, Ctrl+C / crash terminal restore, and snapshot tests.

### Acceptance Direction

- Tool output does not automatically become `Fact`.
- LLM hypothesis and verified fact are distinguishable.
- Stale or uncertain material does not enter prompts as strong fact.

## 7. `v0.4`: Controlled Effects, Transactions, and Recovery

The `v0.4` runtime mainline stays aligned with the formal architecture and runtime-evolution modules: after the `v0.1` through `v0.3` foundations for tools, trace, evidence, facts, context, and permissions are stable, Lattecode introduces effect declarations, overlay / transaction lite, transaction gates, and basic recovery / reconcile semantics. The goal is to make mutating actions auditable, blockable, recoverable, or handoff-ready; it is not to make ecosystem extensibility the main delivery theme for this stage.

MCP, skills, and commands already appear in `v0.1` as minimal entries or bridge capabilities. `v0.4` is not their first delivery stage; it hardens extension / effect / transaction boundaries. Any MCP, plugin, skills, hooks, LSP, or similar capability must enter the same capability schema, permission, evidence, trace, effect, and transaction-gate pipeline; it must not bypass the runtime mainline or replace the evolution of `EffectLedger`, `TransactionManager`, or `Reconciler`.

### Required Capabilities

- `EffectRecord`: mutating actions have a planned effect before execution and record observed effect, status, and compensation possibility after execution.
- `OverlayRevision` / transaction lite: bind patch batches, effect ids, verification freshness, and rollback handles into one transaction boundary.
- `transaction_gate`: before commit, check verification freshness, overlay status, approval status for non-compensable effects, and rollback conditions.
- Recovery / reconcile boundary: partial effects, failed effects, stale facts, invalid overlays, or missing rollback handles enter blocked / needs_reconcile / human handoff instead of continuing automatically.
- Compatibility / extension boundary: if MCP, plugins, skills, hooks, LSP, or similar external capabilities are introduced, they must be converted into Lattecode `CapabilityDescriptor` records and routed through validation, permission, evidence, trace, effect, and transaction pipelines; read-only LSP may remain a low-risk compatibility lane, while code-action writes are deferred.
- `OpenTUI` adapter evaluation gate: only a `v0.4+` side gate to evaluate future cockpit / `ActionGraph` surface needs, install burden, native build reliability, and fallback behavior; it is not a required `v0.4` release deliverable, default dependency, or renderer choice.

### Acceptance Direction

- Mutating actions for files, shell, Git, external APIs, and similar effects have effect declarations before execution and auditable status afterward.
- Stale verification, invalid overlays, and non-compensable effects without approval block commit.
- Partial or failed effects can enter recover, reconcile, or human handoff instead of only being written into natural-language logs.
- External capabilities cannot bypass schema, permission, evidence, trace, effect, or transaction gates; disabled external tools are invisible to the model and cannot be invoked by name.
- External results have truncation, references, and evidence records; they do not directly become `Fact`.

## 8. `v0.5`: Internal Runtime Hardening

`v0.5` consolidates trace, evidence, context, effects, transactions, recovery, and controlled extension boundaries from previous stages into a full internal runtime. Side effects, transactions, and recovery enter the mainline in `v0.4`; this stage hardens them with `ActionGraph`, `StateStore`, `Scheduler`, and `Reconciler` into runtime invariants.

### Required Capabilities

- `ActionGraph` becomes execution ledger, scheduling surface, recovery entry, and UX surface.
- `StateStore` manages `Observation`, `Evidence`, and versioned `Fact`.
- `Scheduler` runs nodes based on dependencies, gates, budgets, and recovery state.
- `EffectRecord`, `OverlayRevision`, `EffectLedger`, and `TransactionManager` manage side effects and transactions for file writes, shell, Git, and external capabilities.
- `Reconciler` covers graph, fact, effect, and transaction mismatch classes; extension adapter issues are classified through existing graph / effect / transaction reconciliation or treated as `v0.4+` extension hardening input, without adding a new `ReconcileDecision.kind`.
- If `ActionGraph` becomes a real UX surface, evaluate cockpit hardening candidates; `OpenTUI` is still not a default dependency or release-critical output path.

### Acceptance Direction

- Runtime invariants can be tested and explained.
- Users can understand blocked reasons, risks, and recovery options.
- External system signals can only enter runtime through adapters, evidence, or gates.
- Mutating effects cannot bypass the effect ledger, transaction gate, or permission record.

## 9. Cross-version Invariants

- Lattecode externally remains a code-agent `Data Plane`.
- `Control Plane Authority` must be scoped to internal runtime authority.
- `v0.1` prioritizes a basic working agent, not a full runtime.
- Each stage introduces only the minimal abstraction needed for current problems.
- Execution records, tool calls, file changes, and verification results must be traceable.
- `v0.1` MCP, skills, and commands only deliver minimal bridge / local loader / local specs; full MCP platform, marketplace, resource / prompt ecosystem, skill hub, and command marketplace are not part of `v0.1`.
- Ecosystem, MCP, plugins, skills, hooks, LSP, and TUI / cockpit work may only act later as compatibility / extension / renderer side lanes; they must not replace the runtime `v0.4` effects / transactions / recovery mainline.
- Runtime concept implementation must update the corresponding design documents; formal Chinese and English documents must remain structurally and semantically aligned.
