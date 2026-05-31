# Module Technical Design: Code Agent Loop

## Status

This document defines the basic code agent loop for Fluxcode `v0.1`. It is based on temporary source research of Claude Code, CodeWhale, Codex, and opencode, but `.tmp/` sources are not treated as Fluxcode formal source or build inputs.

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

For `fluxcode run "implement a snake game"` to be truly runnable, Fluxcode needs at least:

- A real model provider, not only the fake default response.
- Repository read, search, edit, and file creation.
- Gates for writes and shell execution.
- Declared verification command execution.
- `AgentHandoff` containing changed files, verification, risks, and blockers.

If the target repository lacks application framework, test framework, or dependency decisions, Fluxcode must block clearly and ask the user. It must not silently scaffold, install dependencies, or publish.

## 2. Basic Code Agent Minimum Feature Set

| Capability | `v0.1` minimum | Non-goals |
| --- | --- | --- |
| CLI entry | `fluxcode run <task>` starts the real agent loop and outputs JSON / text handoff | No TUI / IDE cockpit |
| Session / TaskRun | Create recoverable `TaskRunState` with phase, steps, artifacts, tool calls, events | No multi-session collaboration |
| Model loop | Real `ModelClient` plus `FakeModelClient` tests; ReAct inside phases | Do not leak provider SDK into core loop |
| Provider abstraction | Support `fake` and `openai-compatible` first | No full provider catalog |
| Tool contract | Every tool has schema, mutating flag, risk, permission, summary, references | No raw function tools |
| Read/search | Directory listing, file reading, text search with truncation and references | No large indexing system |
| Edit/write | Strict old/new `edit_file` and constrained `write_file` for file creation | Avoid broad whole-file rewrites |
| Shell verify | Only declared, allowlisted, or user-approved commands | No arbitrary shell |
| Prompt registry | System prompt and phase prompts have id/version/schema | No scattered prompt strings |
| Context budget | Build prompts from `TaskSpec`, artifacts, evidence summaries, recent steps | No unbounded transcript concat |
| Evidence/event | Tool calls, permission decisions, and phase artifacts enter events or evidence | No natural-language-only summary |
| Handoff | Output changed files, verification, risks, blockers, next steps | Do not hide failures as success |
| Recovery | Append-only event log plus session snapshot can recover run state | No complex graph recovery |

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

## 4. Data Contracts

```ts
type TaskRunState = {
  id: string;
  sessionId: string;
  currentPhase: AgentPhase;
  task?: TaskSpec;
  context?: ContextPack;
  plan?: ChangePlan;
  patch?: PatchSummary;
  verification: VerificationResult[];
  handoff?: AgentHandoff;
  steps: StepTrace[];
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
```

Key artifacts:

| Artifact | Use |
| --- | --- |
| `TaskSpec` | User goal, scope, acceptance, non-goals, constraints |
| `ContextPack` | Read files, relevant snippets, command sources, open questions |
| `ChangePlan` | Target files, steps, verification commands, risks |
| `PatchSummary` | Changed files, diff refs, rationale |
| `VerificationResult` | command, status, exit code, summary, output refs |
| `AgentHandoff` | Final summary, verification, risks, blockers, next steps |

## 5. Complete Gate List

| Gate | Trigger | Pass criteria | Failure semantics | Acceptance |
| --- | --- | --- | --- | --- |
| `provider_gate` | run start | default provider exists, API key resolves, model supports tools | fail fast | CLI reports missing env clearly |
| `config_gate` | config load | schemaVersion, models, tools, permissions, context valid | fail fast | invalid config unit tests |
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
| `verification_gate` | after `Verify` | declared verification ran and result recorded, or skipped reason explicit | failed / skipped handoff | test failure not marked success |
| `handoff_gate` | before output | changed files, verification, risks, blockers complete | internal fail | handoff snapshot tests |
| `recovery_gate` | before resume | event log and snapshot rebuild, no dangling tool call | repair / fail | interrupted tool call resume tests |

These gates are the `v0.1` minimum. They are not equivalent to full OS sandbox. If platform isolation is not implemented, CLI and docs must not claim sandbox support.

## 6. Test Strategy

### 6.1 Unit Tests

| Scope | Tests |
| --- | --- |
| Config | merge, env placeholders, invalid provider, shell allowlist |
| Provider | fake, openai-compatible request mapping, missing API key, tool call parse error |
| Prompt | phase prompt id/version/schema and required boundaries |
| Context | compaction preserves task / acceptance / blockers / verification |
| Tool schema | invalid tool name, invalid args, output truncation |
| Permission | allow / ask / deny, mutating, high risk, deny globs |
| Edit | unique replace, multi-match, stale file, line ending preservation |
| Shell | allowlist, timeout, output cap, blocked install/delete/network/git-write |
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

- `fluxcode run "implement a snake game"` produces real code changes in an existing web app fixture.
- At least one scripted fake-model integration test covers the full loop.
- At least one real-provider smoke test can be run manually.
- Every P0 gate has unit or integration tests.
- CLI output contains `AgentHandoff` and can locate event log / evidence / changed files.
- Failure, blockers, and skipped verification are never reported as success.
