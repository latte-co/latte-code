# Module Technical Design: Context Management and Compression

## Status

This document defines the near-term Fluxcode design for context management and compression. It belongs to the current / near-term module design layer: it constrains the existing `v0.1` context budget behavior and describes how to evolve toward `ContextLedger`, lane-aware `ContextProjection`, `ToolOutputRef`, append-only revisions / `CompactionRecord`, and a cache-aware prompt rendering envelope.

References to long-term `StateStore`, `Fact` graph, and token-aware provider windows describe later runtime evolution only. They do not mean the current `src/` implementation already has those capabilities.

Chinese counterpart: [`docs/zh-CN/design/modules/context-management-and-compression.md`](../../../zh-CN/design/modules/context-management-and-compression.md).

## 1. Design Goal

Fluxcode context compression is not plain transcript summary. It is an auditable history transformation: every compaction must state its input range, retained material, discarded material, external tool-output references, budget decision, and prompt version.

The near-term target is:

```text
Session / Event Log / Evidence / Tool Outputs
  -> ContextLedger
  -> per-turn ContextProjection
  -> Stable Prefix / Append-only Ledger / Dynamic Suffix prompt rendering envelope
  -> PromptRegistry prompt messages + provider cache hints
  -> append-only CompactionRecord chain
```

Core invariants:

- Task, acceptance criteria, permission state, verification results, and key evidence must not be silently lost during compression.
- Large tool outputs are not inserted into prompts directly; the prompt keeps only a summary, `ToolOutputRef`, and truncation / omission markers.
- Compaction records form an append-only audit trail; resume must be able to explain what the model saw, what it did not see, and why.
- Stable Prefix / Append-only Ledger / Dynamic Suffix is the prompt-rendering and provider-cache envelope. It wraps the 10-lane `ContextLane` model; it does not replace the lanes themselves.
- Provider prompt cache is a performance optimization only. It is not a source of state, authority, facts, or recovery. Cache hit / miss / eviction must not change semantics, evidence, recovery, or budget decisions.
- Cached prefix still counts toward the provider context window. Cache eligibility must obey policy, data boundary, retention, and secret-redaction boundaries; not all stable material may be cached provider-side.
- `.tmp/codeagent/claude-code`, `CodeWhale`, `codex`, and `opencode` are comparative research inputs only; `.tmp/` code must not be treated as formal Fluxcode source, interface, or implementation evidence.

## 2. Three-layer Boundary

| Layer | Current state / target | Allowed wording | Disallowed wording |
| --- | --- | --- | --- |
| Current `v0.1` implementation | Uses byte estimates to enforce `context.maxPromptBytes`; records basic compaction text in `ContextSnapshot.compactedSummary`; performs basic transcript and tool-result trimming; uses `maxToolResultBytes` and `recentStepCount` for recent context | Existing minimal `context_budget_gate`; recoverable session snapshot; basic transcript/tool-result trimming | Do not claim `ContextLedger`, lane budgets, append-only `CompactionRecord`, or token-aware provider windows already exist |
| Near-term design | Introduce `ContextLedger`, lane-aware per-turn `ContextProjection`, lane budgets, `ToolOutputRef`, append-only revisions / `CompactionRecord`, and a cache-aware prompt envelope | Upgrade compression into a sourced, budgeted, omission-aware, cache-eligible, recoverable history transformation | Do not treat model summaries or provider cache as fact / state sources; do not pretend discarded material is still present in the prompt |
| Long-term evolution | Integrate with `StateStore`, `Fact` graph, formal `Evidence` freshness / invalidation, and provider token windows | Generate projection from facts/evidence/policy/action state and calibrate it against real provider token budgets | Do not require `v0.1` to have a full runtime kernel or full graph recovery |

## 3. Data Model

### 3.1 `ContextLedger`

`ContextLedger` is the session-level context ledger. It does not replace the event log or `Evidence`; it indexes prompt-eligible context material by lane, source, budget, and preservation policy.

Minimal shape:

```ts
type ContextLedgerEntry = {
  id: string;
  sessionId: string;
  runId: string;
  lane: ContextLane;
  sourceType: "task" | "event" | "artifact" | "evidence" | "tool_output" | "snapshot" | "compaction" | "resume";
  sourceRef: string;
  summary: string;
  promptText?: string;
  toolOutputRef?: ToolOutputRef;
  hardPreserve: boolean;
  redaction: "none" | "redacted" | "omitted";
  createdAt: string;
};
```

An entry `summary` is prompt-usable context, not a fact. Fact promotion remains the responsibility of the later `StateStore` / `Fact` graph.

### 3.2 Per-turn `ContextProjection`

Before each model call, Fluxcode builds a per-turn `ContextProjection`. It is input to `PromptRegistry`, not a persistent fact source.

```ts
type TurnContextProjection = {
  id: string;
  sessionId: string;
  runId: string;
  stepId: string;
  promptId: string;
  promptVersion: string;
  lanes: ProjectedLane[];
  segments: PromptRenderSegment[];
  stablePrefixCacheKey?: StablePrefixCacheKey;
  omittedEntryIds: string[];
  redactedEntryIds: string[];
  compactionRecordIds: string[];
  budget: ContextBudgetDecision;
  createdAt: string;
};
```

Projection must record omitted / redacted entries instead of returning only final prompt text.

### 3.3 Append-only Revisions and Active Pointers

Dynamic `task` and `phase_artifact` content must not be rendered as mutable prefix blocks. They must be represented as append-only revisions, and projection selects the currently visible version only through an active pointer.

```ts
type TaskSpecRevision = {
  id: string;
  parentId?: string;
  sessionId: string;
  sourceEventId: string;
  taskText: string;
  acceptance: string[];
  constraints: string[];
  nonGoals: string[];
  createdAt: string;
};

type PhaseArtifactRevision = {
  id: string;
  parentId?: string;
  sessionId: string;
  phase: "context" | "plan" | "patch" | "verify" | "handoff";
  artifactKind: "ContextPack" | "ChangePlan" | "PatchSummary" | "VerificationResult" | "AgentHandoff";
  sourceEventId: string;
  contentRef: string;
  createdAt: string;
};

type ArtifactRevision = TaskSpecRevision | PhaseArtifactRevision;

type ActiveArtifactPointer = {
  id: string;
  lane: "task" | "phase_artifact";
  artifactKind: string;
  activeRevisionId: string;
  updatedByEventId: string;
  previousRevisionId?: string;
  createdAt: string;
};
```

Revision invariants:

- `TaskSpecRevision`, `PhaseArtifactRevision`, and `ArtifactRevision` are append-only. A change creates a new revision; it never overwrites old revision content.
- `parentId` points to the previous version in the same artifact lineage. A root revision must trace back to the initial task / phase event.
- Every revision must bind to `sourceEventId` so audit can explain whether it came from user input, an agent artifact, verification, or handoff.
- The active `TaskSpecRevision` is hard-preserved input for the `task` lane. Older versions may enter audit / compaction but must not impersonate the current task.
- The active `PhaseArtifactRevision` enters the `phase_artifact` lane according to the current phase hard / soft policy. Older versions participate only as historical artifact / evidence summaries.

`ActiveArtifactPointer` validity and repair rules:

- A pointer must reference an existing, not-redacted-to-unavailable revision whose lineage matches `artifactKind`.
- Pointer updates must record `previousRevisionId` and `updatedByEventId`; missing event provenance enters repair instead of being silently accepted.
- Pointer references a missing revision: block the current hard lane and try event-log replay; if replay fails, request repair / pending input.
- Pointer references a non-latest revision without conflict: continue by the audited pointer order but record a stale-pointer marker.
- Two active pointers reference different revisions of the same artifact: handle by the conflict matrix below; do not merge the two versions into one task or phase artifact.

Active pointer conflict matrix:

| Conflict | Handling | Result |
| --- | --- | --- |
| `task` pointer vs newer user task revision | Newer user revision wins | Update pointer; old pointer enters audit; if the user revision lacks acceptance criteria, request completion instead of inheriting old acceptance |
| `task` pointer vs agent-generated task rewrite | User source wins | Agent rewrite is only a proposal / phase artifact; it must not overwrite active task |
| `phase_artifact` pointer vs failed verification revision | Verification result constrains later phases | Pointer may reference the failed result; later patch / plan must see the failure reason |
| `phase_artifact` pointer vs handoff draft conflict | Latest audited event wins | Keep a conflict marker; block handoff if needed until an active draft is selected |
| pointer revision missing / hash mismatch | Event-log replay wins | Repair pointer if replay succeeds; otherwise enter repair blocker |

### 3.4 Minimum `CompactionRecord` Invariants

`CompactionRecord` is the append-only compaction-chain record. At minimum it must contain:

```ts
type CompactionRecord = {
  id: string;
  parentId?: string;
  sessionId: string;
  runId: string;
  inputRange: {
    messageRefStart?: string;
    messageRefEnd?: string;
    evidenceRefStart?: string;
    evidenceRefEnd?: string;
    ledgerEntryIds: string[];
  };
  outputSummary: string;
  retainedLanes: ContextLane[];
  discardedLanes: ContextLane[];
  redactedMarkers: string[];
  omittedMarkers: string[];
  toolOutputRefs: ToolOutputRef[];
  budgetDecision: ContextBudgetDecision;
  promptId: string;
  promptVersion: string;
  createdAt: string;
};
```

Invariants:

- `id` is globally unique; `parentId` points to the previous compaction record and forms a verifiable chain.
- `inputRange` must locate the message / evidence / ledger range being compacted.
- `outputSummary` must declare its source range and must not introduce facts outside that range.
- `retainedLanes` and `discardedLanes` must be explicit; discarding a hard lane must produce a blocker or repair state.
- Redacted / omitted content must be represented with markers and must not be disguised as known content in the summary.
- `toolOutputRefs` must cover large tool outputs externalized during compaction.
- `budgetDecision` plus prompt id/version must explain why compaction happened.

### 3.5 `ToolOutputRef`, `EvidenceRef`, and Degradation Semantics

`ToolOutputRef` represents an externalized reference to large or sensitive tool output. `Evidence` represents material with proof, verification, or audit meaning. They may reference each other, but they have different responsibilities.

```ts
type ToolOutputRef = {
  id: string;
  toolCallId: string;
  evidenceId?: string;
  storage: "event_log" | "local_blob" | "artifact_file" | "external_store";
  uri: string;
  sha256?: string;
  byteLength?: number;
  mimeType?: string;
  summary: string;
  truncated: boolean;
  redaction: "none" | "redacted" | "permission_required" | "unavailable";
  createdAt: string;
};

type EvidenceRef = {
  id: string;
  evidenceId: string;
  sourceRef: string;
  summary: string;
  freshness: "active" | "stale" | "invalidated" | "unavailable";
  degradation: "none" | "summary_only" | "ref_only" | "blocked";
  createdAt: string;
};
```

Boundary rules:

- Storage choice: `v0.1` may continue to keep event-log / session-snapshot summaries; near-term implementation may write large outputs to local blobs or artifact files. Regardless of storage, the prompt keeps only the summary, ref id, truncated marker, and redaction marker.
- Missing / invalid ref: Fluxcode must not infer the original output from an old summary. Projection marks it `unavailable` and either blocks, degrades, or requests a safe re-run depending on the lane policy.
- Permission / read failure: if the ref exists but cannot be read, the prompt may show only the ref, failure reason, and `permission_required` marker. It must not leak out-of-bound content or treat unread content as verified evidence.
- Redaction recovery: redacted output cannot be reconstructed from compaction summaries. Resume may use only the redaction marker, allowed public summary, and re-fetchable non-sensitive evidence. If the task depends on the original content, enter pending input or repair.
- `Evidence` boundary: `Evidence` records proof, verification, and audit meaning; `ToolOutputRef` records where the raw or large output lives. `Evidence.summary` may reference `ToolOutputRef.id`, but it must not assume the reference is always readable.
- `EvidenceRef` degradation: active evidence may be used as hard context; stale evidence may enter the prompt only as `summary_only` or `ref_only`; invalidated / unavailable evidence that supports active acceptance or verification must be `blocked`, otherwise it may degrade with a marker.
- Block / degrade behavior: an unreadable ref required by a hard lane blocks; an unreadable ref required only by a soft lane may degrade to summary / marker. Every degradation must appear in projection metadata and `CompactionRecord` / recovery output.

## 4. Lane-aware Context Model

The near-term context model has 10 lanes. Each lane has a hard / soft preservation policy, budget, and degradation behavior.

| Lane | Content | Default policy |
| --- | --- | --- |
| `control` | system / developer / policy / permission gate constraints and prompt contract | hard-preserve; block when over budget |
| `task` | user goal, scope, acceptance, non-goals, constraints, blockers | hard-preserve; not removed by ordinary compaction |
| `runtime_baseline` | `AGENTS.md` snapshot/hash, config summary, workspace boundary, declared commands | hard-preserve summary; raw text may be externalized |
| `phase_artifact` | `ContextPack`, `ChangePlan`, `PatchSummary`, `VerificationResult`, draft `AgentHandoff` | preserve according to current phase priority |
| `evidence` | evidence summaries, verification refs, diff refs, file snapshot refs | hard for active acceptance / verification; otherwise summarizable |
| `tool_output` | raw or large output from read/search/shell/MCP tools | externalize as `ToolOutputRef` by default; prompt keeps summary and markers |
| `working_set` | current modified files, relevant snippets, open questions, active hypotheses | soft-preserve; degrade when proven irrelevant |
| `recent_tail` | last user/assistant/tool turns | soft-preserve; deterministically prune old tail first |
| `compaction` | compaction-chain summaries plus omitted/redacted markers | hard metadata; summaries may be hierarchically compacted |
| `resume` | pending input, resume marker, unresolved permission/question, recovery state | hard-preserve; resolve conflicts by recovery precedence |

### 4.1 Mapping to Current Formal Baseline Lanes

The current formal `Code Agent Loop` baseline can be summarized as Task / Artifact / Evidence / Recent loop. The 10-lane model maps to it as follows:

| Near-term lane | Current baseline lane | Explanation |
| --- | --- | --- |
| `control` | Task | Control information constrains task execution; permission state is separately hard-preserved in `resume` |
| `task` | Task | Directly maps to goal, scope, acceptance, and non-goals |
| `runtime_baseline` | Task | Injected as execution constraints and workspace baseline, not ordinary recent transcript |
| `phase_artifact` | Artifact | Maps to `ContextPack`, `ChangePlan`, `PatchSummary`, `VerificationResult`, `AgentHandoff` |
| `evidence` | Evidence | Maps to tool invocation, diff, verification, file snapshot, and handoff refs |
| `tool_output` | Evidence | Raw output is externalized and may be referenced by evidence; prompt keeps summary / ref |
| `working_set` | Artifact | Active files, snippets, and hypotheses are a working subset of phase artifacts |
| `recent_tail` | Recent loop | Maps to recent interactions kept by `recentStepCount` |
| `compaction` | Evidence | Records audit evidence of context transformation, not task facts |
| `resume` | Task | Resume entry and pending input constrain the next step and outrank ordinary summaries |

## 5. Prompt Rendering / Cache Envelope

Stable Prefix / Append-only Ledger / Dynamic Suffix is the prompt rendering envelope used to describe provider prompt-cache stability boundaries and incremental rendering boundaries. It does not change lane semantics: projection still selects, prunes, and degrades material by the 10-lane `ContextLane` model first, then renders lane content into the three segments.

| Segment | Source lanes | Cache semantics | Constraints |
| --- | --- | --- | --- |
| Stable Prefix | Stable system / developer policy, fixed prompt contract, static instructions eligible for cache | May produce `StablePrefixCacheKey` and provider cache hints | May contain only material that passes policy/dataBoundary/secret-redaction checks and whose retention policy allows provider-side cache |
| Append-only Ledger | Active `task` revision, `runtime_baseline` revision, `compaction` metadata, active `evidence` refs, phase artifact audit trail | Not overwritten as a mutable prefix; rendered in append-only order and may form cacheable chunks | Revision / pointer / compaction records must be auditable; new content appends instead of rewriting history |
| Dynamic Suffix | Current-turn instruction, recent tail, active working set, tool-call schema, provider-specific suffix | Not cached by default, or cached only briefly | May change per turn; must not carry the only source of state |

```ts
type PromptRenderSegment = {
  kind: "stable_prefix" | "append_only_ledger" | "dynamic_suffix";
  laneIds: ContextLane[];
  contentRef: string;
  tokenEstimate?: number;
  byteEstimate?: number;
  cacheEligibility: "eligible" | "ineligible" | "redacted" | "boundary_restricted";
};

type StablePrefixCacheKey = {
  promptId: string;
  promptVersion: string;
  modelProvider: string;
  modelId: string;
  stablePrefixHash: string;
  policyRevisionId: string;
  dataBoundaryRevisionId: string;
  secretRedactionRevisionId: string;
  baselineRevisionIds: string[];
};
```

Cache contract:

- Provider prompt cache is a performance optimization only. Cache hit / miss / eviction may affect only cache-control metadata, latency, and cost metadata; it must not affect semantic `ContextProjection` content, recovery path, evidence visibility, or budget-gate decisions.
- Cached prefix still counts toward the provider context window. Budgeting must estimate the full prompt and must not remove prefix tokens / bytes just because a provider cache hit is expected.
- Any `StablePrefixCacheKey` component change invalidates the old key: prompt version, provider/model, stable prefix hash, policy revision, data boundary revision, secret redaction revision, or baseline revision changes all trigger re-render.
- Cache eligibility is jointly decided by policy, data boundary, retention, and secret redaction. Material can be stable but still ineligible for provider-side cache because it contains user data, secrets, path-boundary material, or provider retention restrictions.
- Cache invalidation records must be auditable: record the invalidating event, old key, new key, re-render decision, and whether provider cache hints are allowed.

### 5.1 True Stable Prefix vs Conditional Baseline

True stable prefix contains only material that does not change with workspace observation within a session / run and is eligible for provider-side cache. `AGENTS.md`, config, workspace boundary, declared commands, skills, and MCP server state are conditional baselines: they may be invalidated by file, environment, permission, or tool-registration changes, so they must not be treated as unconditionally stable prefix.

```ts
type BaselineRevisionRecord = {
  id: string;
  source: "AGENTS" | "config" | "workspace" | "commands" | "skills" | "mcp";
  observedHash: string;
  revisionId: string;
  invalidatingEventId?: string;
  reRenderDecision: "reuse" | "rerender" | "block_for_recheck";
  createdAt: string;
};
```

Baseline revision / audit rules:

- Every conditional baseline must record `source`, `observedHash`, `revisionId`, optional `invalidatingEventId`, and `reRenderDecision`.
- If the baseline hash and policy/data boundary are unchanged, `reuse` is allowed. A changed hash must `rerender`; missing read permission or uncertain boundaries must `block_for_recheck`.
- Conditional baseline may enter the Append-only Ledger or a cacheable chunk, but only cache-eligible content may receive Stable Prefix provider cache hints.
- Baseline audit records are the source for recovery and explanation. Provider cache is not a baseline state source.

## 6. Budget / Reserve Algorithm

The budget algorithm must prune deterministically first, and use model summary / checkpoint only when needed. Deterministic pruning is reproducible; model summary adds an interpretive layer.

Recommended flow:

1. Read provider / config prompt budget. Current `v0.1` uses byte estimates; near-term design keeps byte fallback; long-term design adds provider token-aware windows. Even when stable prefix cache hits, budget against the full prefix + ledger + suffix provider context window.
2. Reserve budget for response, tool-call schema, permission / resume state, and emergency markers.
3. Assemble hard lanes: `control`, `task`, active `resume`, current-phase required artifact, active acceptance evidence, and compaction metadata.
4. If hard lanes already exceed budget, do not drop hard lanes. Return a `context_budget_gate` blocker with the over-budget lane and suggested repair.
5. Deterministically prune soft lanes: old `recent_tail`, raw `tool_output`, low-relevance `working_set`, and stale phase artifacts.
6. Externalize large `tool_output` into `ToolOutputRef`; prompt keeps summary, ref id, byte/token estimate, and truncated marker.
7. If still over budget, create model summary / checkpoint and write a `CompactionRecord`; its input range and output must be auditable.
8. Estimate again. If byte/token mismatch or provider cache-policy rejection makes the provider reject the prompt, enter provider-aware fallback: record provider error metadata, disable cache hints for that attempt, shrink soft lanes, increase reserve, and block if necessary instead of deleting hard lanes.

`ContextBudgetDecision` should at least record budget source, max estimate, reserved estimate, used estimate, hard-lane estimate, pruning steps, whether model summary was used, fallback reason, and provider token rejection / cache-control rejection metadata. Cache state must never justify exceeding budget.

## 7. Resume / Recovery Reconstruction

Resume reconstructs context from:

```text
session metadata
  + append-only event log
  + latest context snapshot
  + ContextLedger entries
  + CompactionRecord chain
  + retained recent tail
  + pending input / resume marker
  -> reconstructed ContextLedger
  -> next-turn ContextProjection
```

Conflict precedence:

| Conflict | Precedence | Handling |
| --- | --- | --- |
| event log vs snapshot | event log wins | Snapshot is a checkpoint; event log is the source of state transitions. Replay missing snapshot events from the log; if snapshot claims a state unsupported by the log, enter repair |
| ledger vs retained tail | ledger wins | Retained tail is prompt convenience. If it disagrees with ledger entries / compaction chain, discard the tail and regenerate it from the ledger |
| pending permission vs compacted decision | pending permission wins | Compaction summary cannot replace user approval / denial. Unresolved permission/question stays hard-preserved in the `resume` lane |
| `ToolOutputRef` summary vs ref read result | readable ref wins; marker wins when unreadable | If the ref is readable and hash matches, re-summarize it; if missing or hash-mismatched, mark unavailable and do not infer raw output from summary |
| `EvidenceRef` active vs stale / invalidated | Freshness wins | Active evidence may enter hard context; stale evidence degrades to summary/ref marker; invalidated evidence that supports active acceptance blocks |
| active pointer vs revision chain | Pointer audit integrity wins | Pointer must reference a traceable revision; missing, hash-mismatched, or conflicting multi-pointer states use the active pointer conflict matrix for repair |
| compaction parent chain vs latest record | parent-chain integrity wins | Missing parent or damaged input range enters damaged-chain repair instead of continuing unaudited compaction |

Resume output must include reconstructed / degraded / repair status, unavailable refs, damaged compaction records, and retained pending input.

## 8. Integration with Existing Modules

| Module | Near-term integration |
| --- | --- |
| `AgentLoop` | Requests `ContextProjection` before every model call; appends ledger entries for tool results, phase artifacts, permissions, and handoff; returns canonical `PendingInput` or blocker on context overrun |
| `PromptRegistry` | Receives projection lanes, render segments, budget decision, prompt id/version, and cache eligibility; renders summary/ref/truncation/redaction markers explicitly and attaches provider cache hints only to eligible stable prefix |
| `Evidence` | Continues binding tool invocation, verification, diff, and handoff; adds references to `ToolOutputRef` and `CompactionRecord` |
| `Session` | Stores context snapshot, event log, pending input, and compaction-chain head; rebuilds through conflict precedence during resume |
| future `StateStore` | Later promotes `Fact` from `Evidence` / artifacts; projection reads fact/evidence freshness instead of transcript summary |
| future `ContextProjection` runtime module | Converges this near-term lane model into the runtime-evolution projection with source, budget, omission, and trust boundaries |

## 9. Testing Strategy

Testing must go beyond overflow. Minimum matrix:

| Scenario | Assertion |
| --- | --- |
| hard-lane preservation | task / acceptance / control / pending input / active evidence still exist after compaction; over budget blocks instead of deleting them |
| missing `ToolOutputRef` | projection marks unavailable; does not infer raw output from old summary; blocks or degrades according to lane policy |
| invalid / unreadable `ToolOutputRef` | hash mismatch, permission failure, and unreadable path produce markers; sensitive content does not enter prompt |
| damaged compaction chain | missing parent, damaged input range, or broken record order enters repair; no unaudited summary is generated |
| hard lane over budget | returns `context_budget_gate` blocker with over-budget lane and reserve details; hard lane is not dropped |
| reproducible projection after resume | event log + snapshot + ledger + compaction chain rebuild a projection with the same critical lanes as before interruption |
| byte/token mismatch fallback | byte estimate passes but provider token rejection triggers fallback prune / reserve; hard lanes remain preserved |
| provider cache rejection fallback | provider rejects cache-control / retention hints; the attempt disables cache hints and re-renders; projection semantics remain unchanged |
| cache semantic equivalence | cache disabled, miss, evicted, and hit paths produce the same semantic `ContextProjection`; only cache-control, cost, and latency metadata may differ |
| cache eligibility boundary | when policy, dataBoundary, secret redaction, or retention disallows caching, stable material does not receive provider-side cache hints; sensitive content does not enter cached prefix |
| conditional baseline audit | `AGENTS` / config / workspace / commands / skills / MCP changes record source, observed hash, revision id, invalidating event id, and re-render decision |
| active pointer conflict | `task` / `phase_artifact` pointer conflicts repair or block by matrix; conflicting revisions are not merged |
| deterministic pruning order | same ledger and budget produce the same pruning order, omitted ids, and budget decision |
| model summary audit | when model summary is used, `CompactionRecord` contains input range, prompt version, retained/discarded lanes |
| redaction recovery | redaction marker is recoverable, raw text is not reconstructed from summary; tasks depending on raw text enter pending input / repair |

## 10. Phased Sub-roadmap

| Phase | Goal | Acceptance |
| --- | --- | --- |
| v0.1 hardening | Keep existing byte-estimate compaction; make hard lanes explicit; include truncation markers in tool-result summaries; test hard-lane preservation and overflow blockers | No public source contract change; context budget unit / integration tests are added |
| v0.2 ledger slice | Introduce minimal `ContextLedger` entry, `ToolOutputRef`, `EvidenceRef`, task / phase artifact revisions, and active pointers; externalize large outputs; render refs / markers in `PromptRegistry` | missing ref, permission failure, deterministic pruning, and active pointer conflict tests pass |
| v0.3 audit compaction | Introduce append-only `CompactionRecord` chain and conditional baseline audit records; compaction summaries carry input range, retained/discarded lanes, prompt version | damaged chain, model summary audit, baseline invalidation, and resume projection reproducibility tests pass |
| v0.4 cache-aware rendering | Introduce Stable Prefix / Append-only Ledger / Dynamic Suffix envelope, `StablePrefixCacheKey`, cache eligibility, and cache semantic equivalence tests | cache disabled/miss/evicted/hit are semantically equivalent; provider cache rejection fallback does not change projection |
| v0.5 runtime alignment | Align with `StateStore` / `Fact` graph / token-aware provider windows and converge with the long-term `ContextProjection` module | provider token fallback, fact/evidence provenance, and policy trace are auditable |

## 11. Non-goals

- Do not implement long-term memory or cross-device sync in this design.
- Do not promote model-generated summaries into `Fact`.
- Do not treat provider prompt cache, cache keys, or cache-hit results as Fluxcode state, facts, authority, or recovery sources.
- Do not use context compaction to bypass permission, path boundary, redaction, or evidence freshness.
- Do not require `v0.1` to immediately introduce a full `StateStore`, `ActionGraph`, or token-aware provider SDK adapter.
