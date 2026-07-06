# Module Technical Design: Code Agent Loop

## Status

This document defines the minimal ReAct loop for Lattecode `v0.1` local repository tasks. The design starts with a local code agent: it reads repository context, makes controlled file changes, runs verification, and produces a reviewable handoff.

Chinese counterpart: [`docs/zh-CN/design/modules/code-agent-loop.md`](../../../zh-CN/design/modules/code-agent-loop.md).

## 1. Goal And Non-Goals

The `v0.1` goal is a minimal, resumable, verifiable code agent loop that can complete small development, repair, and documentation tasks inside a local repository.

| Category | Content |
| --- | --- |
| Goal | Accept task input, load repository instructions and relevant files, run a ReAct tool loop, modify files, run verification, and output a handoff |
| Goal | Keep explicit permission boundaries for writes, shell commands, and external path access |
| Goal | Preserve a recoverable run record: messages, tool calls, tool results, changed files, verification results, blockers |
| Non-goal | Do not front-stage a full `ActionGraph`, advanced `Scheduler`, `Reconciler`, or complete `runtime kernel` in `v0.1` |
| Non-goal | No multi-agent workflow, plugin marketplace, TUI / IDE cockpit, transaction rollback, cloud collaboration, or long-term memory system |

If a task requires project scaffolding, dependency installation, file deletion, git state mutation, or publishing, the agent must ask for user confirmation. It must not perform those actions silently.

## 2. Minimal Loop Shape

The main flow is fixed as:

```text
TaskInput
  -> ContextPack
  -> ReAct turn loop
  -> Tool results / Changed files
  -> Verification
  -> Handoff
```

| Stage | Responsibility | Minimum artifact |
| --- | --- | --- |
| `TaskInput` | Record the user goal, scope, constraints, and acceptance criteria | `taskId`, raw input, optional acceptance |
| `ContextPack` | Summarize repository instructions, relevant files, recent tool results, and open questions | file refs, summary, token budget state |
| `ReAct turn loop` | Let the model iterate through thought, tool call, and observation | messages, tool calls, tool results |
| `Tool results / Changed files` | Persist read/write results and change summaries | changed files, diff refs, permission refs |
| `Verification` | Run declared verification commands or record why verification cannot run | command, status, summary, evidence refs |
| `Handoff` | Produce a reviewable deliverable | summary, changed files, verification, risks, blockers |

The minimum ReAct turn is:

1. Build model input from `TaskInput`, `ContextPack`, and recent tool results.
2. The model returns a normal message or tool calls.
3. The runtime validates tool schema, path boundaries, permissions, and budget.
4. The tool runs, and the runtime records result, summary, and evidence reference.
5. Loop state is updated; if permission or user input is needed, the run enters a waiting state.
6. When the model considers the edit complete, the run must enter verification and handoff; natural language alone cannot end the task.

## 3. Loop State And Records

`v0.1` does not need a complex graph, but it does need a recoverable `TaskRunState`.

```ts
type TaskRunStatus =
  | "running"
  | "waiting_permission"
  | "blocked"
  | "failed"
  | "completed";

type TaskRunState = {
  taskId: string;
  status: TaskRunStatus;
  taskInput: string;
  messages: MessageRecord[];
  context: ContextPack;
  toolCalls: ToolCallRecord[];
  toolResults: ToolResultRecord[];
  changedFiles: ChangedFileRecord[];
  permissions: PermissionRecord[];
  verification: VerificationRecord[];
  handoffStatus?: "not_ready" | "ready" | "delivered";
  compactedSummary?: string;
  resumeReason?: string;
};
```

| Record | Required fields |
| --- | --- |
| `MessageRecord` | role, content summary, token estimate, createdAt |
| `ToolCallRecord` | id, tool name, input summary, mutating, permission id, createdAt |
| `ToolResultRecord` | tool call id, status, output summary, evidence refs, error |
| `ChangedFileRecord` | path, operation, read revision, write revision, diff ref |
| `PermissionRecord` | id, action, path / command, reason, decision, decidedAt |
| `VerificationRecord` | command, status, exit code, summary, evidence refs |

These records serve three purposes: feed the next model turn, resume interrupted work, and generate the final handoff.

## 4. Minimal Tool Set

| Tool | Mutates state | Purpose | Minimum constraints |
| --- | --- | --- | --- |
| `list_directory` | No | Inspect directory structure | Only repository paths |
| `read_file` | No | Read file contents | Truncate large files and keep references |
| `search_text` | No | Search files and text | Ignore dependency directories, build artifacts, and sensitive files by default |
| `edit_file` | Yes | Apply local replacements against already-read content | Must be read-before-write; old text matches uniquely by default |
| `write_file` | Yes | Create a new file or perform controlled whole-file write | Create intent must be explicit; overwriting existing files needs confirmation |
| `shell_exec` | Maybe | Run verification commands | Only verification commands are allowed by default; other commands need confirmation |
| `git_diff` | No | Read current diff as handoff evidence | Read-only; no `git add`, `commit`, or `push` |

`v0.1` can avoid a large tool ecosystem. Built-in tools only need to cover the local repository loop: read, search, edit, write, verify, and inspect diff.

## 5. Tool Contract And Permissions

Every tool must declare:

| Field | Meaning |
| --- | --- |
| `name` | Stable tool name |
| `inputSchema` | Validatable input shape |
| `mutating` | Whether the tool may change files, processes, network, or external state |
| `risk` | `low`, `medium`, or `high` |
| `permission` | `allow`, `ask`, `deny`, or allowlist rule |
| `resultSummary` | Short summary for the next model turn and handoff |
| `evidenceRefs` | References to files, diffs, command output, or tool results |

Permission rules:

- Read-only tools are allowed by default, but still obey repository path boundaries and sensitive-file rules.
- `edit_file` and `write_file` require the target file to be read first; new files require explicit create intent.
- Before writing, the runtime checks path boundaries, old-content match, and stale file state; high-risk diffs require user confirmation.
- `shell_exec` only allows project-declared, configured allowlist, or user-confirmed verification commands by default.
- The agent must not silently install dependencies, delete files, start network requests, mutate git state, or run publishing commands.
- `git_diff` is a read-only evidence tool; `git add`, `git commit`, and `git push` are not part of the default tool set.

When permission is denied, the run should enter `blocked` or output a handoff with a blocker. Denial must not be wrapped as success.

## 6. Context Policy

`ContextPack` is the engineering boundary for each model input. It should not concatenate the full transcript without bounds.

| Context lane | Content |
| --- | --- |
| Repo instructions | `AGENTS.md`, project README, explicit user constraints |
| Task input | Original task, scope, acceptance criteria, non-goals |
| Relevant files | Read-file summaries, key snippets, path references |
| Recent tool results | Recent results, errors, permission decisions, diff summary |
| Verification plan | Runnable commands, already-run commands, skipped reasons |
| Open questions | User decisions and blockers |

Token budget behavior:

- Always preserve task objective, acceptance criteria, permission decisions, changed files, and blockers.
- Long tool output must be summarized; summaries keep evidence refs so raw files can be reread when needed.
- Older messages may be compacted into `compactedSummary`, but explicit user constraints must not be lost.
- If context is insufficient for a safe edit, the agent should read more files or enter `blocked`.

## 7. Verification And Handoff

Verification command priority:

1. Commands explicitly provided by the user.
2. Test / build / check commands declared in project config or package manifests.
3. Commands proposed by the agent from repository facts, after allowlist or user confirmation.

If verification cannot run, the skipped reason must be recorded. If verification fails, the final status cannot be successful.

Minimum `AgentHandoff` fields:

| Field | Meaning |
| --- | --- |
| `status` | `completed`, `failed`, or `blocked` |
| `summary` | What was completed |
| `changedFiles` | file paths, operation types, diff refs |
| `commandsRun` | commands, exit codes, summaries, evidence refs |
| `risks` | uncovered risks, behavior changes, compatibility concerns |
| `blockers` | permission denial, missing information, verification failure |
| `evidenceRefs` | file snippets, diffs, tool results, command output references |

The handoff is for code review and follow-up work. It should not only say "done".

## 8. Failure And Resume States

| State | Meaning | Resume behavior |
| --- | --- | --- |
| `waiting_permission` | A tool call needs user approval | Continue after approve; enter `blocked` or handoff after deny |
| `blocked` | Missing user decision, context, or permission | Continue after user input; otherwise keep blocker |
| `failed` | Tool execution or verification failed and cannot be auto-repaired | Retry from the failed point or output failed handoff |
| `completed` | Handoff has been generated and the loop is closed | Later requests should create a new task or follow-up task |

Continuing `waiting_permission`, `blocked`, or `failed` runs must reuse the same `taskId` and read existing messages, tool results, changed files, permissions, and verification records. It must avoid duplicate writes or repeated high-risk shell commands. A `completed` run can only be read or reviewed as completed state; further work should create a follow-up task.

## 9. Deferred Work

The following items are later evolution and are not part of the `v0.1` minimal ReAct loop:

- A full graph execution model, such as promoting every step into an `ActionGraph` node.
- Advanced `Scheduler`, concurrent execution, priority queues, and cross-task scheduling.
- `Reconciler`-driven automatic repair, complex fact systems, and long-term state reasoning.
- Complete `runtime kernel`, transaction / rollback, effect ledger, or overlay revision.
- Multi-agent collaboration, role orchestration, plugin marketplace, remote tool ecosystem.
- TUI / IDE cockpit as the core interaction surface.

These directions can evolve from `v0.1` messages, tool records, permissions, verification, and handoff evidence. They should not precede a working local code agent loop.

## 10. `v0.1` Implementation Slice And Acceptance

Implementation slice:

- `lattecode run <task>` creates `TaskRunState` with `taskId` and raw task input.
- Load repository instructions, relevant files, and recent tool results into `ContextPack`.
- Connect a model client that supports multi-turn ReAct tool calls.
- Provide the minimal built-in tools: list, read, search, edit, write, shell verification, git diff.
- Run permission gates for mutating tools, shell commands, and external paths.
- Enforce read-before-write, path boundaries, stale-write checks, and diff evidence.
- Run verification commands or record why verification could not run.
- Output `AgentHandoff` with changed files, commands run, results, risks, blockers, and evidence refs.
- Support local resume for `waiting_permission`, `blocked`, and `failed`; a `completed` run can only be read, reviewed, or used to derive a follow-up task, not continue the original mutating loop.

Acceptance:

- In an existing small repository fixture, the agent can complete a simple code or documentation change and produce a diff.
- A scripted fake-model integration test covers the full `TaskInput -> ContextPack -> ReAct -> Verification -> Handoff` flow.
- Writing a target file before reading it is blocked.
- Unconfirmed install, delete, network, and git-write commands are blocked or enter `waiting_permission`.
- When verification fails, handoff status is `failed` or `blocked`; it is not reported as success.
- Handoff lists changed files, verification commands, result summaries, risks, and evidence refs.
