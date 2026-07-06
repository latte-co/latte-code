# Module Technical Design: Code Agent Loop

## Status

This document defines the basic code agent loop for Lattecode `v0.1`. It is based on temporary source research of Claude Code, CodeWhale, Codex, and opencode, but `.tmp/` sources are not treated as Lattecode formal source or build inputs.

Chinese counterpart: [`docs/zh-CN/design/modules/code-agent-loop.md`](../../../zh-CN/design/modules/code-agent-loop.md).

## 1. Conclusion

The minimum production-usable basic code agent is neither a pure state machine nor naked ReAct. It should be:

```text
Persistent TaskRun
  + phase-gated ReAct
  + typed tool contract
  + permission / path / edit / shell / verification gates
  + append-only event log
  + structured handoff
```

The outer phase runner owns engineering boundaries: phase order, budgets, permissions, structured artifact validation, persistence, and recovery. The inner loop still keeps ReAct query behavior: the model can call tools, observe results, adjust the plan, and continue.

For `lattecode run "implement a snake game"` to be truly runnable, Lattecode needs at least:

- A real model provider, not only the fake default response.
- A unified path for CLI, local command, local skill, minimal MCP bridge, and built-in tools to produce `TaskSpec`.
- Config, `AGENTS.md`, session snapshot, and pinned constraints loaded into reusable `ContextPack`.
- Repository read, search, edit, and file creation.
- Gates for writes and shell execution.
- Declared verification command execution.
- `AgentHandoff` containing changed files, verification, risks, and blockers.

If the target repository lacks application framework, test framework, or dependency decisions, Lattecode must block clearly and ask the user. It must not silently scaffold, install dependencies, or publish.

## 2. Basic Code Agent Minimum Feature Set

| Capability | `v0.1` minimum | Non-goals |
| --- | --- | --- |
| CLI entry | `lattecode run <task>` starts the real agent loop and outputs JSON / text handoff | No TUI / IDE cockpit |
| Config | Load project-local JSONC config for models, runtime, tools, permissions, session, commands, skills, and MCP | No secrets storage or global policy platform |
| `AGENTS.md` loader | Read `AGENTS.md` within repo root / cwd boundaries, record snapshot/hash, and inject it into context | No untracked text concatenated directly into prompts |
| Session / TaskRun | Create recoverable `TaskRunState` with phase, status, steps, artifacts, tool calls, events, and context snapshot | No cloud sync, multi-device, or multi-user collaboration |
| Model loop | Real `ModelClient` plus `FakeModelClient` tests; ReAct inside phases | Do not leak provider SDK into core loop |
| Provider abstraction | Support `fake` and `openai-compatible` first | No full provider catalog |
| Tool contract | Every tool has schema, mutating flag, risk, permission, summary, references | No raw function tools |
| Built-in tools | Minimal read/search/edit/write/shell/manifest/diff capabilities | No large tool ecosystem or plugin marketplace |
| Minimal MCP bridge | List / call tools from config-defined servers through permission, evidence, trace, and session | Disabled by default or explicitly enabled; no marketplace, resource / prompt platform, or server-management UI |
| Local skills | Load local instruction / workflow / command bundles into context / prompt registry | No hub, install, publish, or marketplace; skills cannot directly execute side effects |
| Local commands | Read built-in / local command specs and convert commands into `TaskSpec` / phase events | Commands do not bypass agent-loop, permission, or session |
| Read/search | Directory listing, file reading, text search with truncation and references | No large indexing system |
| Edit/write | Strict old/new `edit_file` and constrained `write_file` for file creation | Avoid broad whole-file rewrites |
| Shell verify | Only declared, allowlisted, or user-approved commands | No arbitrary shell |
| Prompt registry | System prompt and phase prompts have id/version/schema | No scattered prompt strings |
| Context budget | Build prompts from `TaskSpec`, artifacts, evidence summaries, recent steps | No unbounded transcript concat |
| Evidence/event | Tool calls, permission decisions, and phase artifacts enter events or evidence | No natural-language-only summary |
| Handoff | Output changed files, verification, risks, blockers, next steps | Do not hide failures as success |
| Recovery | Append-only event log plus session snapshot can recover run state | No complex graph recovery |

The fixed `v0.1` main flow is:

```text
CLI / local command / local skill / minimal MCP bridge / built-in tools
  -> TaskSpec
  -> Session / TaskRunState
  -> ContextPack
  -> AgentLoop / PhaseRunner
  -> PermissionDecision
  -> Evidence / StepTrace
  -> AgentHandoff
```

The minimum MCP, skill, and command boundaries are:

- MCP: only config-defined servers, list tools, and call tool. MCP tools must be converted into the Lattecode tool contract and routed through permission, evidence, trace, and session. MCP is disabled by default or explicitly enabled; it cannot bypass permissions; no marketplace, resource / prompt platform, or server-management UI.
- Skill: only a local skill loader. A skill is an instruction / workflow / command bundle that can inject into context / prompt registry; it cannot directly execute side effects; no hub, install, publish, or marketplace.
- Command: only built-in / local command specs. Commands must route through `TaskSpec`, phase events, and the session system; they cannot call tools directly to bypass the agent loop, permission, or session.

### 2.1 TUI / output boundary

This document re-establishes the `v0.1` TUI / output decisions. The `v0.1` release-critical path remains headless JSON / text `AgentHandoff`; a formal TUI, IDE cockpit, or full-screen cockpit is not part of `v0.1` release acceptance.

The output boundary is:

- `--output json` and `--output text` must remain available, and automation, CI, non-TTY, and redirected-output scenarios must not depend on TUI.
- `--ui tui` or a standalone experimental command may only be opt-in; without explicit selection, the CLI should keep headless handoff behavior.
- TUI only consumes runtime events and `AgentHandoff` / view model. It must not drive permissions, sessions, runtime mutation, schema, evidence, trace, or handoff semantics.
- Runtime core must not import `react`, `ink`, or `@opentui/*`; UI dependencies must stay behind renderer adapters or experimental package boundaries.
- `PlainTextRenderer` is the default available path for non-TTY, CI, snapshots, and crash fallback; Lattecode does not build a complete terminal renderer in-house.

Renderer boundaries use a stable view model:

```text
runtime event stream
  -> stable TuiViewModel
  -> InkRenderer | OpenTuiRenderer | PlainTextRenderer
```

`InkRenderer` may be an experimental PoC direction for `v0.2` / `v0.3`; if the selected Ink path requires Node `>=22`, it must have a Node gate. Acceptable options include pinning `Ink v6`, optional / lazy import, or a standalone experimental package. `OpenTuiRenderer` is only a `v0.4+` adapter evaluation gate; only if `ActionGraph` becomes a real UX surface in `v0.5+` should it be reconsidered as a cockpit hardening candidate. The main runtime must not introduce Bun / Zig / native build chains.

## 3. Loop Shape

`v0.1` uses phase-gated ReAct:

```text
Intake
  -> Understand
  -> Plan
  -> Edit
  -> Verify
  -> Handoff
```

Each phase is a contract:

```ts
type AgentPhase = "intake" | "understand" | "plan" | "edit" | "verify" | "handoff";

type PhaseContract<Output> = {
  phase: AgentPhase;
  allowedTools: string[];
  maxReactSteps: number;
  outputSchemaName: string;
  validateOutput(value: unknown): Output;
  next(output: Output, run: TaskRunState): AgentPhase | "completed" | "blocked";
};
```

Inside a phase, the loop is still ReAct:

```text
build phase prompt
  -> model.generate
  -> tool calls
  -> permission / schema / path / shell gates
  -> tool results
  -> next model turn
  -> structured phase output
```

A phase is complete only when the corresponding artifact passes schema validation.

| Phase | Allowed tools | Required artifact | Failure behavior |
| --- | --- | --- | --- |
| `Intake` | none or read-only config | `TaskSpec` | ask user when goal / scope is unclear |
| `Understand` | `list_directory`, `read_file`, `search`, `read_project_manifest` | `ContextPack` | block / ask when context is missing |
| `Plan` | read-only tools | `ChangePlan` | ask for scaffold / dependency decisions |
| `Edit` | `read_file`, `edit_file`, `write_file`, optional `apply_patch` | `PatchSummary` | block when edit gates fail |
| `Verify` | `shell_exec`, read-only tools | `VerificationResult[]` | failed verification enters failed handoff |
| `Handoff` | none or `git_diff` | `AgentHandoff` | must list unverified checks and blockers |

TUI must not be a completion condition for any phase. Phase artifacts remain governed by schema validation; renderers may only display `StepTrace`, runtime events, permission prompt state, and final `AgentHandoff`.

## 4. Data Contracts

```ts
type TaskRunState = {
  id: string;
  sessionId: string;
  status: "queued" | "running" | "waiting_permission" | "blocked" | "failed" | "completed";
  currentPhase: AgentPhase;
  task?: TaskSpec;
  context?: ContextPack;
  plan?: ChangePlan;
  patch?: PatchSummary;
  verification: VerificationResult[];
  handoff?: AgentHandoff;
  steps: StepTrace[];
  resume?: ResumeMarker;
  contextSnapshot: ContextSnapshot;
};

type StepTrace = {
  id: string;
  phase: AgentPhase;
  status: "pending" | "running" | "done" | "blocked" | "failed";
  promptId: string;
  promptVersion: string;
  summary: string;
  toolCallIds: string[];
  evidenceIds: string[];
  reactBudget: { maxSteps: number; usedSteps: number };
  error?: string;
};

type ResumeMarker =
  | { type: "permission"; permissionId: string }
  | { type: "blocked"; questionId: string }
  | { type: "failed"; failedStepId: string }
  | { type: "completed"; handoffId: string };

type ContextSnapshot = {
  taskInput: string;
  messageRefs: string[];
  decisionRefs: string[];
  compactedSummary: string;
  pinnedConstraints: string[];
  agentsMd?: { path: string; hash: string; summary: string };
  skills?: { name: string; path: string; hash: string; summary: string }[];
  commands?: { name: string; path: string; hash: string; description: string }[];
  mcpTools?: { server: string; tool: string; toolName: string }[];
};

type PendingInput =
  | {
      kind: "permission";
      permissionId: string;
      toolCallId: string;
      phase: AgentPhase;
      action: "write_file" | "edit_file" | "shell_exec" | "mcp_call" | "external_path";
      reason: string;
      command?: string;
      path?: string;
      options: ["approve", "deny"];
    }
  | {
      kind: "question";
      questionId: string;
      phase: AgentPhase;
      prompt: string;
      expectedAnswer: "text" | "json";
      schemaName?: string;
    };

type ResumeInput =
  | { kind: "permission"; permissionId: string; decision: "approve" | "deny"; reason?: string }
  | { kind: "question"; questionId: string; answerText?: string; answerJson?: unknown };

type HeadlessRunEnvelope = {
  runId: string;
  sessionId: string;
  status: TaskRunState["status"];
  pendingInput?: PendingInput;
  handoff?: AgentHandoff;
};
```

`PendingInput`, `ResumeInput`, and `HeadlessRunEnvelope` are the canonical `v0.1` headless run / resume contracts. `waiting_permission` must include `pendingInput.kind = "permission"`; `blocked` must include `pendingInput.kind = "question"`. `AgentHandoff.blockers`, `requiredDecisions`, `TaskRunState.resume`, and the event log must reuse the same `permissionId` / `questionId` so CLI resume and handoff do not grow separate id contracts.

Legacy `AgentResult.status` / `SessionState.status` values are compatibility-only. Legacy `denied` is no longer a canonical `TaskRunState.status`; headless envelopes, the `graph-ready` wrapper, and later run-state code must map it to `blocked` and explain the permission denial or external decision gap through blockers / required decisions.

### 4.1 Minimum Session Management Scope

`v0.1` session management is local-first run recovery, not a long-term collaboration or memory system.

| Scope | Minimum requirement |
| --- | --- |
| Lifecycle | Support create / list / show / resume; stable session id; cwd and repo root fixed when the session is created |
| `TaskRunState.status` | Only use `queued`, `running`, `waiting_permission`, `blocked`, `failed`, `completed` |
| Resume semantics | `waiting_permission -> approve/deny -> continue`; `blocked -> user input -> continue`; `failed -> repair/retry`; `completed -> follow-up/fork later` |
| Context snapshot | Store task input, messages, decisions, compacted summary, pinned constraints, and `AGENTS.md` snapshot/hash |
| Trace/evidence binding | Bind `StepTrace`, `Evidence`, tool invocation, shell output summary, file edit summary, and verification result |

Explicit non-goals: no cloud sync, multi-device, multi-user collaboration, full branch / fork graph, long-term memory, or full `ActionGraph` persistence.

Key artifacts:

| Artifact | Use |
| --- | --- |
| `TaskSpec` | User goal, scope, acceptance, non-goals, constraints |
| `ContextPack` | Read files, relevant snippets, command sources, open questions |
| `ChangePlan` | Target files, steps, verification commands, risks |
| `PatchSummary` | Changed files, diff refs, rationale |
| `VerificationResult` | command, status, exit code, summary, output refs |
| `AgentHandoff` | Final summary, verification, risks, blockers, next steps |

The current `v0.1` implementation slice uses the following minimum field set. Later versions may extend it without redefining the headless contract:

| Artifact | Minimum fields |
| --- | --- |
| `TaskSpec` | `objective`, `scope`, `acceptance`, `nonGoals`, `constraints`, `blockers` |
| `ContextPack` | `summary`, `filesRead`, `relevantSnippets`, `commandSources`, `openQuestions` |
| `ChangePlan` | `summary`, `targetFiles`, `steps`, `verificationCommands`, `risks` |
| `PatchSummary` | `changedFiles`, `diffRefs`, `rationale`, `evidenceRefs` |
| `VerificationResult` | `command`, `status`, `summary`, `evidenceRefs`, with optional `exitCode`, `outputRefs` |
| `AgentHandoff` | `id`, `status`, `summary`, `changedFiles`, `verification`, `risks`, `blockers`, `requiredDecisions`, `traceRefs`, `evidenceRefs` |

## 5. Complete Gate List

| Gate | Trigger | Pass criteria | Failure semantics | Acceptance |
| --- | --- | --- | --- | --- |
| `provider_gate` | run start | default provider exists, API key resolves, model supports tools | fail fast | CLI reports missing env clearly |
| `config_gate` | config load | schemaVersion, models, tools, permissions, context valid | fail fast | invalid config unit tests |
| `agents_gate` | before context construction | `AGENTS.md` snapshot/hash recorded and path is within repo root / cwd boundaries | block / ignore with reason | external or unreadable `AGENTS.md` is not silently added to prompt |
| `task_gate` | after `Intake` | `TaskSpec.objective` non-empty, scope and non-goals clear | ask user | vague task triggers ask |
| `context_budget_gate` | before every model call | prompt within budget, preserved lanes retained | block / compact | long tool output summarized, acceptance preserved |
| `tool_schema_gate` | before tool execution | tool exists, input matches schema | tool error, continue or block | invalid JSON / args tests |
| `permission_gate` | before mutating / shell / external path | allow / ask / deny decision exists | ask / deny / block | write and shell ask by default |
| `path_boundary_gate` | before file tool | path is inside workspace or trusted external path | deny / ask | `../`, `.git`, `.env` blocked |
| `read_before_write_gate` | before edit/write | file was read or create intent is explicit | block / ask | direct unread edit rejected |
| `stale_write_gate` | before edit/write | file hash / mtime unchanged since read | block / reread | concurrent modification triggers reread |
| `edit_match_gate` | before `edit_file` | oldText matches exactly once unless replaceAll explicit | block | multiple matches need more context |
| `diff_review_gate` | after mutating edit/write before or after apply | diff summary reviewable, high risk asks | ask / deny | diff enters permission metadata |
| `shell_command_gate` | before `shell_exec` | command from allowlist, manifest scripts, or user approval | ask / deny | install/delete/network/git-write blocked by default |
| `mcp_gate` | before MCP tool list / call | server explicitly enabled, tool mapped to Lattecode contract, and permission checked | ask / deny / block | disabled MCP servers are invisible to the model |
| `skill_gate` | before skill load / injection | skill comes from allowed local path and only injects instruction / workflow / command spec | deny / block | skill cannot directly execute side effects |
| `command_gate` | before local command execution | command becomes `TaskSpec` / phase event and enters session | ask / deny / block | command cannot bypass the loop and call tools directly |
| `verification_gate` | after `Verify` | declared verification ran and result recorded, or skipped reason explicit | failed / skipped handoff | test failure not marked success |
| `handoff_gate` | before output | changed files, verification, risks, blockers complete | internal fail | handoff snapshot tests |
| `recovery_gate` | before resume | event log and snapshot rebuild, no dangling tool call | repair / fail | interrupted tool call resume tests |

These gates are the `v0.1` minimum. They are not equivalent to full OS sandbox. If platform isolation is not implemented, CLI and docs must not claim sandbox support.

## 6. Test Strategy

### 6.1 Unit Tests

| Scope | Tests |
| --- | --- |
| Config | merge, env placeholders, invalid provider, shell allowlist |
| `AGENTS.md` | snapshot/hash, repo boundary, missing / unreadable behavior |
| Provider | fake, openai-compatible request mapping, missing API key, tool call parse error |
| Session | create / list / show / resume, status transitions, fixed cwd / repo root |
| Prompt | phase prompt id/version/schema and required boundaries |
| Context | compaction preserves task / acceptance / blockers / verification |
| Tool schema | invalid tool name, invalid args, output truncation |
| Permission | allow / ask / deny, mutating, high risk, deny globs |
| Edit | unique replace, multi-match, stale file, line ending preservation |
| Shell | allowlist, timeout, output cap, blocked install/delete/network/git-write |
| MCP / Skill / Command | disabled-by-default MCP, skill no-side-effect, command routes through `TaskSpec` |
| Handoff | failed verification is never marked successful |

### 6.2 Integration Tests

Integration tests must use real temporary filesystem repositories, not only pure functions.

| Scenario | Fixture | Assertions |
| --- | --- | --- |
| happy path | small TypeScript / web app fixture | source changed, tests added, `npm test` passes, handoff complete |
| snake game | existing Vite / vanilla app fixture | game files generated, test or build passes |
| permission ask | mutating edit / shell | returns waiting_permission or blocked, no file write |
| verification fail | intentionally failing test | handoff marks failed and keeps failure summary |
| context overflow | large tool output | output summarized, critical constraints retained |
| interrupted resume | interrupt after tool call | resume has no dangling tool call and continues or safely fails |
| stale write | external file change after read | edit blocks or rereads |
| local command / skill / MCP | command, skill, or MCP tool triggers a task | all enter `TaskSpec -> Session -> PhaseRunner` and produce permission / evidence / trace |

### 6.3 Provider Integration Tests

`v0.1` should not require real model tests in normal CI.

- Required in CI: scripted `FakeModelClient` integration.
- Optional local smoke: run one small fixture after configuring a real provider.
- External API keys are forbidden in CI unless explicitly opt-in through secrets.

## 7. Evolution Toward Runtime

| `v0.1` object | Later object | Evolution |
| --- | --- | --- |
| `TaskRunState` | `ActionGraph` seed | phase / step become node / edge |
| `StepTrace` | `ActionNode` | add dependsOn, readSet, writeSet, effect refs |
| tool evidence | `Evidence` | add coverage, freshness, artifact refs |
| permission decisions | `PolicyDecision` / gate records | structure into policy layer |
| edit / shell records | `EffectRecord` | add expected / observed / compensation |
| failed handoff | `ReconcileDecision` | add affected refs and repair action |

## 8. `v0.1` Done Definition

- `lattecode run "implement a snake game"` produces real code changes in an existing web app fixture.
- At least one scripted fake-model integration test covers the full loop.
- At least one real-provider smoke test can be run manually.
- Every P0 gate has unit or integration tests.
- CLI output contains `AgentHandoff` and can locate event log / evidence / changed files.
- Session lifecycle, `AGENTS.md` snapshot, minimal MCP bridge, local skill loader, and local command specs enter the minimal contract-first loop.
- Failure, blockers, and skipped verification are never reported as success.
