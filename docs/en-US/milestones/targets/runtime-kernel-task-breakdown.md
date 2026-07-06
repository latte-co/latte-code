# Code Agent Evolution Task Breakdown v0.1-v0.5

## Status

This is the independent task breakdown for evolving Lattecode from a basic code agent to a harness-native runtime. The historical file name `runtime-kernel-task-breakdown` is retained to keep existing indexes stable; the content now follows an incremental implementation plan.

Chinese counterpart: [`docs/zh-CN/milestones/targets/runtime-kernel-task-breakdown.md`](../../../zh-CN/milestones/targets/runtime-kernel-task-breakdown.md).

## 1. Overall Dependency

```text
v0.1 Basic working code agent
  -> v0.2 Structured trace and tool discipline
  -> v0.3 Evidence, facts, and context projection
  -> v0.4 Controlled effects, transactions, and recovery
  -> v0.5 Harness-native runtime hardening
```

Shared premise: Lattecode is externally a code-agent `Data Plane`; internal `Control Plane Authority` only means Lattecode internal runtime authority, and is a gradually formed target.

## 2. `v0.1`: Basic Working Code Agent

### Tasks

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.1-cli-config-contract` | Lock CLI, config, and `TaskSpec` contracts: minimal `run` / resume / show / list entries, project-local JSONC config, command args become `TaskSpec` | None | CLI / config produce structured task input without TUI or global secrets |
| `v0.1-agents-loader` | Implement minimal `AGENTS.md` loader: repo root / cwd boundaries, snapshot/hash, pinned constraints | `v0.1-cli-config-contract` | `AGENTS.md` constraints enter context snapshot and are traceable; unreadable files have explicit semantics |
| `v0.1-session-management` | Add local session management: create / list / show / resume, stable session id, fixed cwd / repo root, `TaskRunState.status` | `v0.1-cli-config-contract` | Supports `queued` / `running` / `waiting_permission` / `blocked` / `failed` / `completed` with clear resume semantics |
| `v0.1-phase-gated-react` | Implement phase-gated ReAct on the existing query loop: tool loops are allowed inside phases, and phase completion validates structured artifacts | `v0.1-cli-config-contract`, `v0.1-session-management` | `Understand`, `Plan`, `Edit`, and `Verify` can use ReAct but must produce schema-valid objects |
| `v0.1-built-in-tools` | Establish minimal built-in tools: read/search/edit/write/shell/manifest/minimal diff summary, retaining tool contract | `v0.1-phase-gated-react` | Tools declare schema, read-only / mutating status, permission requirements, risk level, and result summary; P0 diff only emits changed files / diff summary, and dangerous tools do not execute naked |
| `v0.1-permission-pipeline` | Establish allow / deny / ask permission pipeline and write decisions into events and trace | `v0.1-built-in-tools` | Unauthorized command or path access blocks / asks instead of continuing |
| `v0.1-minimal-mcp-bridge` | Implement minimal MCP bridge: config-defined servers, list/call tools, disabled by default or explicitly enabled | `v0.1-built-in-tools`, `v0.1-permission-pipeline` | MCP tools map into Lattecode tool contract and enter permission / evidence / trace / session; no permission bypass |
| `v0.1-local-skill-loader` | Implement local skill loader: load local instruction / workflow / command bundles into context / prompt registry | `v0.1-agents-loader`, `v0.1-session-management` | Skills cannot directly execute side effects; no hub, install, publish, or marketplace |
| `v0.1-local-command-specs` | Implement built-in / local command specs: command -> `TaskSpec` / phase event / session | `v0.1-cli-config-contract`, `v0.1-session-management` | Commands do not bypass agent-loop, permission, or session |
| `v0.1-evidence-trace-binding` | Bind `StepTrace`, `Evidence`, tool invocation, shell output summary, file edit summary, and verification result | `v0.1-permission-pipeline`, `v0.1-minimal-mcp-bridge`, `v0.1-local-skill-loader`, `v0.1-local-command-specs` | Final report can trace each key action; tools, file changes, and verification results have evidence refs |
| `v0.1-output-contract` | Re-establish the output contract: keep `--output json` / `--output text`, with headless `AgentHandoff` as the release-critical path | `v0.1-evidence-trace-binding` | TUI is not part of `v0.1` release acceptance; non-TTY / CI / redirected output can reliably receive JSON / text handoff |
| `v0.1-agent-handoff` | Emit structured `AgentHandoff`: change summary, verification result, risks, blockers, required user decisions, trace / evidence refs | `v0.1-output-contract`, `v0.1-evidence-trace-binding` | User can decide whether to accept the change; failures, blockers, and skipped verification are not reported as success |

### Non-goals

- Full runtime kernel.
- Full `ActionGraph` / `StateStore` / `Scheduler`.
- Full OS sandbox.
- Multi-agent parallel collaboration.
- Automated PR, release pipeline, or remote server.
- Full MCP platform, marketplace, resource / prompt ecosystem, skill hub, command marketplace, IDE, formal TUI / cockpit, telemetry, or git automation alignment.
- Cloud sync, multi-device, multi-user sessions, full branch / fork graph, long-term memory, or full `ActionGraph` persistence.
- TUI as the only output channel, or TUI bypassing schema, permission, evidence, trace, or handoff.

## 3. `v0.2`: Structured Trace and Tool Discipline

### Tasks

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.2-trace-schema` | Extend `StepTrace`: id, parent, status, inputs, outputs, artifacts | `v0.1-evidence-trace-binding` | Trace can map to future `ActionNode` |
| `v0.2-capability-descriptor` | Define basic capability descriptor | `v0.1-built-in-tools`, `v0.1-evidence-trace-binding` | File, search, shell, Git, LSP, and model capabilities describe inputs, outputs, and risks |
| `v0.2-policy-guard` | Add minimal guard: path scope, command allowlist, write scope, dangerous operation confirmation | `v0.2-capability-descriptor` | Out-of-scope operations are rejected or ask the user |
| `v0.2-node-executor-lite` | Wrap linear step execution as early `NodeExecutor` | `v0.2-trace-schema` | Execution steps produce structured results |
| `v0.2-action-node-seed` | Introduce lightweight `ActionNode` as a structured alias for trace | `v0.2-node-executor-lite` | Step dependency and status can be expressed without complex graph machinery |
| `v0.2-tui-view-model-contract` | Define the `runtime event stream -> stable TuiViewModel -> renderer adapters` contract | `v0.1-output-contract`, `v0.2-trace-schema` | TUI view model only reads runtime events / handoff; it does not drive permissions, sessions, or runtime mutation |
| `v0.2-plaintext-renderer` | Implement or specify the `PlainTextRenderer` fallback | `v0.2-tui-view-model-contract` | Non-TTY, CI, snapshots, and crash fallback do not depend on TUI dependencies |
| `v0.2-ink-experimental-poc` | Establish the Ink experimental PoC gate: optional / lazy / experimental package, with Node gate when required | `v0.2-tui-view-model-contract`, `v0.2-plaintext-renderer` | Only call the PoC usable after Node20 / Node22 matrix, non-TTY fallback, streaming / backpressure, 1k / 10k events, resize, stdout / stderr mixed output, Ctrl+C / crash restore, and snapshot tests pass |

### Non-goals

- Complex DAG scheduling.
- Full policy engine.
- Capability marketplace.
- Splitting services for abstraction's sake.
- Importing `react`, `ink`, or `@opentui/*` from runtime core.

## 4. `v0.3`: Evidence, Facts, and Context Projection

### Tasks

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.3-observation-evidence` | Define minimal `Observation` and `Evidence` models | `v0.2-trace-schema` | Tool output has source, time, boundary, and artifact refs |
| `v0.3-fact-lite` | Define minimal `Fact` and lifecycle | `v0.3-observation-evidence` | Only verified or user-confirmed claims become facts |
| `v0.3-promotion-rule` | Add basic promotion rules | `v0.3-fact-lite` | LLM hypothesis cannot directly become active fact |
| `v0.3-context-projection` | Organize LLM input with `ContextProjection` | `v0.3-fact-lite` | Prompt-critical material can be traced to fact / evidence / hypothesis |
| `v0.3-stale-marking` | Mark related facts stale when files, verification results, or local edit records change | `v0.3-context-projection` | Stale fact does not enter projection as strong fact |

### Non-goals

- Full knowledge graph.
- Treating recall as verification.
- Creating strong facts for every text fragment.

## 5. `v0.4`: Controlled Effects, Transactions, and Recovery

### Tasks

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.4-effect-record` | Define `EffectRecord`: planned / observed / effective effect, status, compensation possibility, and action linkage | `v0.2-capability-descriptor`, `v0.3-observation-evidence` | Mutating actions for files, shell, Git, external APIs, and similar effects have planned effect before execution and observed effect afterward |
| `v0.4-transaction-lite` | Define `OverlayRevision` / transaction lite: patch refs, effect ids, verification ids, rollback handle, transaction status | `v0.4-effect-record`, `v0.3-stale-marking` | Patch, effect, and verification freshness bind to the same transaction boundary |
| `v0.4-transaction-gate` | Add pre-commit gate: verification freshness, overlay status, approval for non-compensable effects, rollback conditions | `v0.4-transaction-lite`, `v0.2-policy-guard` | Stale verification, invalid overlays, or non-compensable effects without approval block commit |
| `v0.4-recovery-handoff` | Define blocked / human handoff semantics for partial / failed effects, missing rollback handles, and stale facts | `v0.4-transaction-gate` | Failures are not only natural-language logs; users can see recovery options and handoff reasons |
| `v0.4-reconcile-lite` | Add minimal reconcile entry points for effect, transaction, and stale-fact cases | `v0.4-recovery-handoff` | Partial effects, invalid overlays, and stale facts can enter needs_reconcile instead of continuing automatically |
| `v0.4-extension-boundary-side-lane` | Keep MCP / plugins / skills / hooks / LSP as compatibility / extension boundaries, convert them into `CapabilityDescriptor` records, and route them through the same validation, permission, evidence, trace, effect, and transaction pipelines | `v0.4-effect-record`, `v0.4-transaction-gate` | External capabilities cannot bypass the runtime `v0.4` effects / transactions / recovery mainline; read-only LSP may remain, while code-action writes are deferred |
| `v0.4-opentui-adapter-evaluation-gate` | Use a `v0.4+` side gate to evaluate `OpenTUI` as a future cockpit / `ActionGraph` surface adapter candidate | `v0.2-tui-view-model-contract` | Produce only an evaluation conclusion: cockpit density, install burden, native build reliability, fallback behavior; do not make `OpenTUI` a required `v0.4` main-runtime deliverable, default dependency, or release deliverable |

### Non-goals

- Marketplace.
- Remote execution.
- Chat-style multi-agent fan-out.
- Unconstrained arbitrary plugin code execution.
- Complete IDE/TUI product surface.
- Bun / Zig / native build chain in the main runtime.
- Replacing the effects / transactions / recovery mainline with ecosystem / MCP / plugin / skills / hooks / LSP work.

## 6. `v0.5`: Harness-native Runtime Hardening

### Tasks

| ID | Task | Depends on | Acceptance |
| --- | --- | --- | --- |
| `v0.5-action-graph` | Consolidate trace / action node into formal `ActionGraph` | `v0.2-action-node-seed`, `v0.4-reconcile-lite` | Graph can express dependency, blocking, verification, external capability, and recovery relations |
| `v0.5-state-store` | Consolidate evidence / fact lite into formal `StateStore` | `v0.3-fact-lite` | Fact lifecycle, coverage, confidence, and evidence refs are auditable |
| `v0.5-scheduler` | Evolve linear runner into dependency-aware `Scheduler` | `v0.5-action-graph` | Nodes with failed guards or stale fact dependencies do not run |
| `v0.5-effect-transaction-hardening` | Harden the `v0.4` `EffectRecord` / `OverlayRevision` / transaction gate into `EffectLedger` and `TransactionManager` invariants | `v0.4-effect-record`, `v0.4-transaction-lite`, `v0.4-transaction-gate`, `v0.4-extension-boundary-side-lane` | Mutating actions from files, shell, Git, and external capabilities cannot bypass effect / transaction boundary |
| `v0.5-reconciler` | Cover graph, fact, effect, and transaction reconcile classes; classify extension adapter issues through existing graph / effect / transaction categories or treat them as extension hardening input | `v0.5-state-store`, `v0.5-effect-transaction-hardening`, `v0.4-reconcile-lite` | Partial effect, stale fact, and invalid overlay enter reconcile; extension failure does not add a new `ReconcileDecision.kind` |
| `v0.5-invariant-eval` | Add runtime invariant tests and architecture demos | `v0.5-reconciler` | Benchmark report shows both task result and runtime invariant result |

### Non-goals

- Replacing runtime invariants with benchmark scores.
- Describing Lattecode as external CI / review / deployment gate.
- Letting external protocols write directly into internal `StateStore`, `EffectLedger`, or `TransactionManager`.
- Making cockpit hardening or `OpenTUI` a default `v0.5` dependency before `ActionGraph` becomes a real UX surface.

## 7. Cross-version Acceptance Checklist

- All current formal design documents remain structurally aligned in Chinese and English.
- All `Control Plane Authority` wording is scoped to internal runtime authority.
- `v0.1` tasks run end to end instead of only defining abstract types.
- Each stage introduces only the minimal runtime concept needed for current problems.
- README only points to currently maintained formal design documents or explicitly non-design research documents.
