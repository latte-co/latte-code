# Latte Code Capability Roadmap

Chinese counterpart: [Latte Code 能力 Roadmap](../zh-CN/roadmap.md).

This roadmap is a dependency-ordered completion checklist, not a release-date
promise. It records capabilities that the current repository actually supports
and the boundaries still to deliver. A design document, a partial foundation,
or a prototype does not make a capability complete.

## Status rules

- `[x]`: available in the current implementation with appropriate automated
  verification.
- `[ ]`: not yet delivered; design and partial infrastructure do not count.
- Before changing an item from `[ ]` to `[x]`, define its supported boundary,
  add lowest-responsibility UT and final-binary E2E, and pass the project
  quality gates.

Platform-specific items must name their limits. Unix process supervision is
implemented today; that does not mean Windows supports arbitrary external
process execution.

## Product foundation and delivery base

- [x] Layered Rust workspace: `latte-core`, `latte-engine`, `latte-headless`,
  `latte-tui`, and the final `latte-code` binary.
- [x] Typed state, commands, events, and stable Rust crate boundaries.
- [x] TUI terminal lifecycle: raw mode, panic/interruption restoration, TTY
  preflight, and constrained-viewport rendering.
- [x] Linux, macOS, and Windows CI gates for build, static checks, UT,
  contract, portable E2E, and release build; Unix additionally runs PTY/process
  E2E.
- [x] Independent UT, final-binary E2E, all-target coverage, and documentation
  link-check rules.
- [ ] Repeatable performance benchmarks, resource-limit baselines, and
  regression alerts.
- [ ] Post-install cross-platform release smoke and upgrade/downgrade
  compatibility verification.

## Engineering and quality assurance

- [ ] A machine-readable test-evidence registry: every capability links its
  owner, risk, UT, contract tests, replay, PTY E2E, live eval, platform scope,
  verification command, and known gaps. The roadmap records delivery scope;
  the registry records how that scope remains proven.
- [ ] A reusable composition-replay Harness that composes TUI, Headless,
  Engine, and a loopback Provider in-process and asserts public events,
  projections, and persistence boundaries. Final-binary PTY E2E then focuses
  on terminal, key input, child-process, and real composition-entry behavior.
- [ ] Redacted, versioned Provider/Harness fixtures for SSE, tool calls,
  failures, cancellation, context, model-visible tool schema, and the actual
  transport request. Every Harness Profile must pass its applicable shared
  conformance contracts.
- [ ] A fault-injection matrix for Effects, JSONL, and the Session Catalog:
  interrupted writes, torn lines, restart, lease loss, Provider interruption,
  and `Unknown` Effects. Recovery must never repeat a side effect.
- [ ] Failure evidence bundles that retain redacted ANSI/PTY output, event
  traces, loopback-Provider requests, state summaries, temporary workspaces,
  and reproduction commands with an explicit retention policy.
- [ ] Separate live evals with temporary repositories, explicit task oracles,
  allowed models/Harnesses, and auditable artifacts. They run only nightly,
  manually, or as release smoke and never replace deterministic PR gates.
- [ ] A release-artifact quality chain: isolated install/start/`--help`/minimal
  headless smoke, version/checksum/signature/manifest consistency, upgrade and
  downgrade checks, and published-package regression verification.
- [ ] Consumable CI results: shard long-running tests, emit JUnit/structured
  reports and failure artifacts, and provide stable rerun and timeout-diagnosis
  paths. Introduce more complex test archive/distribution only when scale
  warrants it.
- [ ] Conversion contracts for compatible configuration, Session, and extension
  migration, with fixtures for version upgrades, lost fields, warnings,
  permission downgrades, and rejection paths.
- [ ] Performance and resource-budget smoke for startup, first frame, Session
  load, context construction, replay, and cleanup, with comparable baselines
  and trend alerts. Monitor regressions before setting hard thresholds.

## Configuration, credentials, and model connectivity

- [x] Deterministic merge of built-in defaults, user-level, and workspace-level
  JSONC configuration.
- [x] Literal and environment-variable credentials; keys resolve only in memory
  and are excluded from ordinary logs and durable state.
- [x] OpenAI Chat Completions-compatible Provider, `base_url`, bounded HTTP
  timeouts, and retries.
- [x] Bounded SSE streaming, tool-call aggregation, and constrained fallback to
  non-streaming requests.
- [x] Provider binding plus tool aliases and internally derived credential/data
  scope checks for resume.
- [ ] Explicit model/Provider selection and per-Session model management.
- [ ] Versioned Harness Profiles that resolve context strategy, system/developer
  prompt, tool schema/names, Plan/Stop semantics, and capability switches as a
  unit for a model/Provider. A Harness Profile is not a Provider adapter and
  must not relax authority based on an opaque model-name heuristic.
- [ ] Harness-Profile capability negotiation, fail-closed fallback for unknown
  profiles, and profile-specific contract/E2E conformance suites.
- [ ] Model catalog, capability declarations, fallback chains, cost/budget, and
  rate-limit visibility.
- [ ] User accounts, login/logout, credential rotation, and controlled
  credential-storage policy.
- [ ] Additional Provider-protocol adapters; Responses API and Anthropic are
  not implemented.
- [ ] Observable, controllable Provider rate limits, backoff, quota, and local
  health diagnostics.

## Workspaces, Sessions, and history

- [x] Workspace discovery, path confinement, workspace-contained tool execution,
  and Git/file manifest reads.
- [x] v1 Run state, Thread v2 control state, and transcript cards in the current
  workspace SQLite database.
- [x] Compatible v1 `run`, `resume`, `show`, and `list` CLI paths; existing v1
  Runs are never backfilled into Threads.
- [x] Immutable Thread v2 follow-up children, paged projections, and bounded
  history-budget validation.
- [ ] User-global `LATTE_CODE_HOME`, Project/Workspace/Session catalog, and
  cross-workspace discovery.
- [ ] One append-only JSONL conversation file per Session; SQLite retains only
  metadata and the control plane.
- [ ] Recovery of torn JSONL final lines, catalog repair, and global
  Session-partitioned leases.
- [ ] `/new`, `/sessions`, and `/resume` flows for create, selection, and
  recovery.
- [ ] Session archival, search, titles, forks, and visible history governance.
- [ ] Session deletion, import/export, sharing, and handoff, each retaining
  authority and sensitive-content boundaries.
- [ ] Compatible import of external-Agent Session/config/Skill/Plugin material
  with source, version, and lossy-conversion warning; an unsupported permission
  or Hook must never be silently treated as active.

## Agent runtime and context

- [x] Headless Provider → tool → Provider continuation loop.
- [x] Bounded repository-context collection, including path-confined
  `AGENTS.md` content.
- [x] Model tool calls, non-secret input requests, verification commands, and
  handoff/evidence runtime paths.
- [x] Fail-closed validation of Provider tool-call IDs, history grammar, and
  request-byte budgets.
- [x] Base protocol for Thread v2 snapshots, event subscription, and transient
  streaming progress.
- [ ] Verified TUI base loop: the first prompt crosses the selected Provider,
  tools/permission, persistence, and transcript presentation end to end. This
  is the highest-priority product loop.
- [ ] One runner per Session, asynchronous turns, FIFO mailbox, and parallel
  independent Sessions.
- [ ] Safely accept a second user prompt or trusted reminder at a safe boundary
  without mutating an in-flight Effect.
- [ ] Context compaction, summaries, selective context, and explainable token
  budgeting.
- [ ] User-controlled Session/Workspace memory, provenance, expiry, and reset;
  an unconfirmed model conclusion must never be presented as fact.
- [ ] Cross-model/Provider handoff that validates safe transformation of
  messages, tool results, reasoning/attachments, and context; protected content
  that is valid only for the original model must not be replayed.
- [ ] Agent Runtime SDK for embedded frontends: typed message/event stream,
  controlled context transformation, and lifecycle, without exposing Engine
  authority.
- [ ] Recovery semantics and visible control plane for background/long-running
  work.
- [ ] Pause, background, queue, scheduling, and notification task system; task
  results may return to a Session only through an explicit provenance-bearing
  input path.

## Tools, Effects, and verification

- [x] Built-in read-only tools: read file, list directory, search, read project
  manifest, and Git diff.
- [x] Built-in mutation tools: exact edit, constrained write/create, and
  stale-content checks.
- [x] Argv-first verification/process execution, output limits, timeout,
  cancellation, and Unix process-group supervision.
- [x] Post-change verification evidence and the rule that failed, missing, or
  unrun verification cannot complete a Run.
- [ ] Effect-aware tool scheduler: independent read-only tools may run in
  parallel, while mutation, approval, Effect ledger, and external process work
  retain a provable serial/isolated order.
- [ ] Safe supervised external-process execution on Windows; it deliberately
  fails closed today.
- [ ] Richer built-in developer tools, structured code modification, and a
  previewable patch workflow.
- [ ] Input/output model for attachments, images, and tool-produced artifacts,
  including size limits, provenance, and persistence boundaries.
- [ ] Pluggable external-tool integration bounded by schemas and resources.
- [ ] Runtime sandboxing, network-access control, and configurable isolation
  profiles.

## Planning, tasks, and change management

- [ ] User-visible Goals, Plans, Todos, and task dependencies: inspectable
  execution commitments, not hidden prompts.
- [ ] Plan-first/Review modes, where planning, execution, verification, and
  delivery use different permission and UI states without creating a bypass
  around Engine Effects.
- [ ] Git change lifecycle: status, selective staging, commits, branches,
  PRs/Issues, and code-host integration; every external write still requires
  explicit user authorization.
- [ ] Auditable snapshots/rollback separate from a user's repository `.git`,
  plus safe worktree create/enter/exit/cleanup.
- [ ] Repeatable code review, security review, test advice, and structured
  findings bound to file/line, evidence, and severity.
- [ ] Human handoff package: current goal, plan, changes, verification, risks,
  next steps, and a recoverable position.

## Safety, permission, and recovery

- [x] `latte-engine` is the only authority for filesystem/process effects,
  SQLite control state, and privileged Effects.
- [x] Durable `Declared → Prepared → Started → Observed/Unknown` Effect
  lifecycle.
- [x] Single-use approval bound exactly to revision, lease, fencing, and request
  digest.
- [x] An interruption or uncertain observation becomes `Unknown`, requires
  explicit reconciliation, and is never guessed successful.
- [x] Workspace confinement, handle-relative safe writes, deny globs, and
  fail-closed unsupported safety primitives.
- [x] Providers, TUI, and projections can read only redacted public data, not a
  private descriptor or credential.
- [ ] Workspace trust for workspace-provided commands, MCP, Skills, Hooks, and
  remote configuration, with source confirmation and least-privilege grants.
- [ ] User-facing policy editing, policy explanation, audit export, and
  organization-level policy distribution.
- [ ] Fine-grained authorization for network, credentials, data scopes, and
  external services.

## TUI, CLI, and interaction

- [x] Transcript-first Ratatui TUI, composer, Unicode editing, navigation, and
  constrained-viewport degradation.
- [x] Separate permission, input-request, and Unknown-reconciliation paths;
  Enter never approves implicitly.
- [x] Snapshot reload after event gaps; local progress is not durable authority.
- [x] `Ctrl+P` local command palette for help, navigation, refresh, and quit.
- [ ] Slash-command catalog, composer suggestions, argument validation, and
  availability recheck at dispatch.
- [ ] `/new`, Session picker, `/sessions`/`/resume`, and safe switching with the
  current composer draft.
- [ ] Browseable details and file jumps for change diffs, verification results,
  and Effect history.
- [ ] Accessibility, themes, keymaps, localization, and configurable terminal
  experience.

## Events, replay, and observability

- [x] Transactional Engine events, Thread event stream, snapshot reload, and
  bounded transient progress.
- [x] Durable Run/Effect/permission/checkpoint/verification control information.
- [ ] Offline replay whose authoritative conversation is JSONL; replay never
  calls a Provider or executes an Effect.
- [ ] Searchable Session/Run/Effect timeline, structured diagnostics, and
  redacted audit export.
- [ ] Token/cost/latency reports attributed to Provider, model, task, tool, and
  context.
- [ ] Opt-in telemetry, privacy boundaries, crash reporting, and runtime
  metrics.
- [ ] Event retention, pagination, version evolution, and migration policy.

## Extensions, delegation, and multi-agent work

- [ ] One capability registry with stable IDs, versions, provenance, schemas,
  resource limits, and availability.
- [ ] Trusted text-only prompt commands; no shell interpolation or arbitrary
  callback.
- [ ] Explicit Provider adapters and permission/schema boundaries for external
  Tool/MCP integration.
- [ ] MCP tools, resources, prompts, authorization/elicitation, and connection
  lifecycle; each remote capability must enter policy/approval independently.
- [ ] Discovery, versions, signatures, dependencies, lifecycle, and revocable
  permissions for installable plugins/skills.
- [ ] Declarative lifecycle Hooks with input/output schemas, timeout, and
  failure policy; Hooks cannot bypass Engine permission, sandbox, or audit.
- [ ] Restricted delegated child Runs with budgets, deadline, tool allowlist,
  cancellation, and result summary.
- [ ] Multi-agent parent/child visualization, approval isolation, resource
  governance, and recovery.
- [ ] Agent-to-agent messages, task assignment, shared bounded context, and a
  user-visible collaboration record.

## Code intelligence, IDE, and remote capability

- [ ] LSP/semantic indexing, symbol navigation, code search, and structured
  edits.
- [ ] IDE bridge for authorized interoperability among editor selection,
  diagnostics, diffs, terminals, and Sessions.
- [ ] Local app-server/API: connection authentication, versioned RPC, event
  backpressure, snapshot reload, and concurrency/authority contracts for
  multiple frontends sharing Sessions.
- [ ] Web, desktop, mobile, and CLI surfaces reuse one public protocol without
  duplicating Engine authority.
- [ ] Remote execution, remote workspaces, queues, reconnect, and credential
  isolation.
- [ ] Optional browser, computer-use, and multimodal experience capabilities.

## Related roadmap designs

The following documents define architecture boundaries for incomplete work;
they do not independently change this checklist's status:

- [Global session and data storage](design/data-storage.md)
- [Slash commands](design/slash-commands.md)
- [Asynchronous turn runner](design/agent-harness/asynchronous-turn-runner.md)
- [Session storage and recovery](design/agent-harness/session-store-and-recovery.md)
- [Effect authority and policy](design/agent-harness/effect-authority-and-policy.md)
- [Extensions and delegation](design/agent-harness/extensions-and-delegation.md)
- [Events, projections, and replay](design/agent-harness/event-projection-and-replay.md)
- [TUI runtime contract](design/agent-harness/tui-runtime-contract.md)
- [Verification harness](design/agent-harness/verification-harness.md)

## Scope calibration after the reference sweep

This checklist was updated after scanning CodeWhale, Codex, OpenCode, and
Claude Code in `references/`, then expanded with Pi and Trae-X. Their product
choices differ, but together they show that a complete Code Agent also needs
model operations, Harness Profiles, Session governance, context/memory,
planning/tasks, change management, controlled extensibility, code intelligence,
multi-surface protocol, and observability. Pi demonstrates the boundary for an
embeddable event-driven Agent Runtime and Provider handoff, but it has no
built-in permission system and is not a security-design reference. Trae-X shows
why a Provider adapter and per-model Harness Profile must be separate: the
latter selects prompt/context, tool schema, and runtime semantics rather than
directly running another Agent binary. Ink and OpenTUI provide terminal-UI
infrastructure reference only; their widgets and rendering implementation are
not Latte Code product capabilities.
