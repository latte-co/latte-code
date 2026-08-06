# Global session and data storage design

Status: **Implemented for local Session storage and current-Workspace management.**

The current implementation uses `$LATTE_CODE_HOME`, defaulting to
`$HOME/.latte/latte-code`, for global SQLite control state and per-Session
JSONL. Schema 11 registers stable Project/Workspace identities, Session leases
remain partitioned, JSONL is the transcript read authority with torn-tail
repair, and a workspace-local legacy database can be imported idempotently
without modifying its bytes. SQLite uses a transactional conversation outbox
only until accepted entries are synced to JSONL. Current-Workspace discovery,
search, rename, and fork are implemented; cross-Workspace discovery and catalog
reconstruction from orphan JSONL remain target work.

## 1. Decisions

| Data class | Authoritative location | Contract |
| --- | --- | --- |
| Session conversation content | Global per-session JSONL | Append-only user, assistant, tool, and context records. |
| Project, workspace, and session metadata | Global SQLite | Current-Workspace discovery and search, provider binding, and lineage. |
| Run and effect control state | Global SQLite | Transactional run state, effects, permissions, leases, checkpoints, evidence, and deduplication. |
| Drafts and Provider runtime | Process memory | Unaccepted prompts, HTTP streams, retries, cancellation, deltas, and raw Provider diagnostics. |
| Credentials | No persistent store | Only non-secret credential references and generations may be durable. |

Additional decisions:

- Session and database state never live below the workspace.
- Session files are grouped by workspace, not by date.
- JSONL is the sole replay source for conversation content; SQLite does not
  duplicate the transcript.
- There is no transcript outbox and no durable Provider-attempt table.
- Once a prompt is accepted, Provider startup failures are Session facts.
  Persist only a bounded, redacted failure card; never persist credentials or
  raw Provider diagnostics.
- Provider streaming deltas are transient. Only complete Provider outcomes are
  eligible for persistence.

## 2. Terminology

- **Project** is the logical repository identity. Git worktrees may share one
  Project.
- **Workspace** is one physical checkout or non-Git working directory.
- **Session** is one user-visible conversation. During the migration, it maps
  one-to-one to the existing `ThreadId`; a second identity is not introduced.
- **Run** is one submitted prompt and its Provider/tool continuation loop.
- **Effect** is an operation that may change or observe external state and is
  owned by `latte-engine`.
- **Draft** is an in-memory new Session or follow-up that has not passed local
  validation and reached the durable submission commit point.

## 3. Global storage home

`LATTE_CODE_HOME` overrides the product data home. The default is:

```text
~/.latte/latte-code/
```

The target layout is:

```text
~/.latte/
|-- latte-code.jsonc
`-- latte-code/
    |-- state.db
    `-- sessions/
        |-- Users-bytedance-projects-latte-co-latte-code-a13f8c2d/
        |   |-- <session-id>.jsonl
        |   `-- <session-id>.jsonl
        `-- Users-bytedance-projects-codeagent-92b11d04/
            `-- <session-id>.jsonl
```

The same home-relative contract applies on Windows using the resolved user
home. The application must not create a database or Session directory in a
workspace.

The workspace configuration may continue to control project behavior, but it
must not control the global storage location. A workspace-layer
`database.path` is accepted only for migration compatibility and is ignored;
it never redirects user history. The storage home may be selected only by the
process environment or trusted user configuration.

## 4. Workspace storage key

The Session bucket name is derived from the canonical workspace root:

1. Resolve and normalize the canonical root and path separators.
2. Normalize Unicode consistently across platforms.
3. Replace unsafe and separator characters with `-`.
4. Bound the readable prefix length.
5. Append a short SHA-256 digest of the canonical root.

The digest prevents collisions such as `/a/b-c` and `/a-b/c` after textual
sanitization. The resulting `storage_key` is stored in SQLite; the directory
name is never accepted directly from workspace configuration.

Each Git worktree is a distinct Workspace bucket but may point to the same
Project. Moving or deleting a Workspace does not move or delete its existing
Session files. A new location is attached as another Workspace record, and an
old Session can later be rebound explicitly.

## 5. Global SQLite responsibilities

The global database contains two logical groups. Neither group contains the
replayable conversation transcript.

### 5.1 Catalog

The catalog contains the equivalent of:

```text
projects
  project_id
  vcs_identity
  display_name
  created_at_ms
  updated_at_ms

workspaces
  workspace_id
  project_id
  canonical_root
  storage_key
  git_common_dir
  branch
  first_seen_at_ms
  last_seen_at_ms

sessions
  session_id
  project_id
  workspace_id
  content_path
  content_format
  title
  preview
  lifecycle
  provider_binding_json
  latest_run_id
  forked_from_session_id
  forked_from_seq
  last_content_seq
  last_content_bytes
  created_at_ms
  updated_at_ms
```

`last_content_seq` and `last_content_bytes` are repairable caches. JSONL wins
when those fields disagree with the file.

The Provider binding retains the complete non-secret semantic binding needed
for resume validation: Provider name/type, protocol, model, configuration and
tool fingerprints, aliases, credential reference, data scope, and credential
generation. It never contains a credential value.

### 5.2 Engine control plane

SQLite remains authoritative for:

- Run state and revision.
- The active Run for a Session.
- Effect declarations, exact engine-private descriptors, attempts, and
  observations.
- Pending permissions and non-secret input requests.
- Runtime checkpoints and verification evidence.
- Command and source-key deduplication.
- Lease ownership and fencing tokens.

These records are not conversation content. Moving them to JSONL would remove
the transactions and compare-and-swap behavior that prevent duplicated or
misreported external effects.

## 6. Per-Session ownership

The current configured database uses scoped runtime leases:

```text
runtime_lease
  scope (primary key: runtime or thread:<session-id>)
  owner
  fencing_token
  expires_at_ms
```

Legacy headless runs share the `runtime` scope. Different Thread v2 Sessions
use different scopes and can run concurrently. A Session has at most one
active engine owner and one JSONL writer. Reacquiring an expired lease advances
the globally monotonic fencing token. A stale owner cannot begin or observe an
Effect and must close
its Session writer when ownership is lost.

Returning from a durable `WaitingPermission` or `WaitingInput` operation
atomically changes the linked Run and active-row lease token to zero before the
lease row is removed. Token zero is a clean quiescence marker: startup
preserves the waiting child, while a later coordinator must acquire a fresh
global fencing epoch before writing. A missing lease with a nonzero token still
means an unclean owner loss and follows conservative interruption/`Unknown`
recovery. When the user explicitly allows a prepared Effect in a new epoch,
the engine revalidates the private canonical descriptor and atomically rebinds
both the single-use permission capability and operation digest to that epoch
before `Started`.

## 7. JSONL contract

The first line is a small self-identifying header, not a copy of the Session
catalog:

```json
{"record":"session","format_version":1,"session_id":"019...","workspace_id":"019...","created_at_ms":1780000000000}
```

Conversation records follow in append order:

```json
{"record":"message","entry_id":"019...","seq":1,"run_id":"019...","created_at_ms":1780000000001,"role":"user","content":"Fix the failing test"}
{"record":"message","entry_id":"019...","seq":2,"run_id":"019...","created_at_ms":1780000000100,"role":"assistant","content":"I will inspect it.","finish_reason":"stop","usage":{"input_tokens":100,"output_tokens":20,"cache_read_tokens":0}}
```

Supported replay records are deliberately small:

- `message` with `system`, `user`, `assistant`, or `tool` role.
- Complete assistant tool-call envelopes.
- Tool results linked by `tool_call_id`.
- `context_checkpoint` and `compaction_summary`.

Every entry has a stable `entry_id`, monotonically increasing `seq`, optional
`run_id`, and bounded content. Provider-issued IDs must already satisfy the
existing safe opaque-ID grammar before they are written.

JSONL does not contain:

- Provider configuration, credential, model-start, HTTP, transport, timeout,
  or retry-exhaustion errors.
- Authorization headers, API keys, or raw credential values.
- Provider stream handles, cancellation tokens, timers, or partial deltas.
- An engine-private executable Effect descriptor.
- A partially assembled assistant message.

Normal writes are append-only. At semantic boundaries, the single writer
writes complete lines and calls `sync_data`; it does not sync every streaming
delta. Crash repair may truncate one torn final line back to the last complete
newline. No other in-place history rewrite is allowed.

## 8. Draft and materialization lifecycle

### 8.1 New Session

A new prompt starts as an in-memory Draft:

```text
prompt
-> validate the non-secret Provider binding
-> create the durable Session, child Run, and user card
-> start the child under its Session lease
-> resolve the credential reference in memory
-> construct the Provider
-> make the first Provider request
```

Validation or storage failure before durable creation leaves no Session or Run
and restores the exact draft. After creation, configuration, credential, model,
authentication, transport, timeout, or other Provider startup failure
terminalizes that child with a bounded, sanitized failure card. The user card
is not removed or copied back into the composer. Provider-construction
failures are retryable: the Session returns to `Ready`, and a later submission
creates a new immutable child. Raw Provider diagnostics and credential values
remain process-local.

The persistence commit point is acceptance of the validated user submission,
before Provider construction or network I/O. The application:

1. Inserts a non-listable `materializing` Session metadata row.
2. Writes and syncs the JSONL header and user message.
3. Creates the durable child Run/control state.
4. Marks the Session listable as `Running`.

A complete assistant message, tool-call envelope, input request, or sanitized
failure is appended only after that durable submission boundary.

Startup removes a `materializing` row with no valid file, or repairs catalog
metadata from a valid self-identifying file. It never removes an accepted
Session merely because Provider startup failed.

### 8.2 Follow-up

A follow-up uses the same boundary: validate first, then atomically append its
user card and create a child before Provider construction. A
Provider-construction failure appends a retryable failure card; completed prior
children remain immutable, and the Session returns to `Ready` for another
follow-up. If a later Provider request fails after durable tool work, the
existing `Failed`, `Interrupted`, or `ReconciliationRequired` control state is
retained. Only bounded redacted presentation text may be durable.

## 9. Effect ordering

A complete Provider tool-call outcome uses this order:

```text
append and sync assistant tool-call message
-> SQLite Prepare Effect
-> resolve permission when required
-> SQLite Started
-> execute the Effect
-> SQLite Observed or Unknown
-> append and sync tool result
```

The ordering has the following recovery semantics:

| Crash point | Required result |
| --- | --- |
| Before the tool-call message | No Session content and no Effect. |
| After tool-call append, before `Prepared` | The call is known but definitely unexecuted. |
| After `Prepared`, before `Started` | The Effect is terminalized as not started. |
| After `Started`, before observation | The Effect becomes `Unknown`; reconciliation is required. |
| After observation, before tool-result append | Reconstruct and append the provider-safe result from the SQLite observation. |
| After tool-result append | Continue from JSONL normally. |

The exact executable descriptor remains engine-private in SQLite. JSONL holds
only the bounded, redacted Provider-history representation. A failed JSONL
append before `Started` aborts execution. A failed append after an Effect
observation stops the Run without retrying the Effect; recovery repairs the
missing tool result from the authoritative observation.

## 10. Reads and projections

- Current-Workspace Session discovery and search query SQLite only. Project and
  Workspace registration remains global storage metadata, not a TUI discovery scope.
- Opening a Session loads the bounded JSONL tail and the current SQLite control
  projection.
- Provider history is rebuilt only from JSONL conversation records, beginning
  at the latest usable context checkpoint.
- The TUI projection combines JSONL messages, SQLite Effect/permission state,
  and transient in-memory Provider progress.
- Event gaps or reconnects reload that combined projection; a second durable
  transcript event table is not required for new JSONL Sessions.

Compaction appends a `context_checkpoint` or `compaction_summary`; it does not
delete or rewrite earlier lines.

## 11. Session operations

- **Fork** creates a new independent file in the target Workspace bucket,
  copies content through the selected sequence, and records
  `forked_from_session_id` plus `forked_from_seq` in SQLite.
- **Workspace loss or movement** never deletes history. Resume requires an
  explicit future rebinding flow when the original path is unavailable; the
  current TUI does not discover Sessions from another Workspace.

## 12. Security and limits

- Create the product home and Workspace buckets with user-only directory
  permissions and Session files with user-only file permissions.
- Redact and validate data before it crosses into JSONL or public SQLite
  projections.
- Keep exact executable inputs behind the engine-private boundary.
- Store large binary attachments outside JSONL by content digest if attachment
  support is added; JSONL keeps only a bounded reference.
- Bound line size, tool results, context checkpoints, scans, and tail repair.
- A Workspace cannot redirect global storage or provide a storage bucket name.

## 13. Migration

Migration from the current workspace database is additive and idempotent:

1. Resolve the global product home and initialize the global schema.
2. When a Workspace is opened, detect its legacy `.latte/latte-code.db`.
3. Import Project, Workspace, Session, Run, and Effect metadata into the global
   database.
4. Export `thread_transcript_v2` content into the Workspace JSONL bucket,
   preserving order, IDs, redaction, and lineage.
5. Record an import fingerprint so retries do not duplicate Sessions or
   effects.
6. Keep the legacy database unchanged until an explicit cleanup operation.

New Sessions use `jsonl_v1`. Existing SQLite-backed Sessions remain readable
during rollout or are explicitly migrated; tables are not silently dropped.
Workspace-layer `database.path` remains parseable for migration compatibility
but is ignored after the global storage contract is enabled.

## 14. Required verification

Implementation is incomplete until UT and final-binary E2E prove at least:

- Provider startup failure preserves the accepted user card and appends one
  bounded failure card without duplicating or restoring the prompt.
- Credential values and raw Provider diagnostics are absent from SQLite,
  JSONL, and persistent application logs.
- A workspace contains no Latte Code database or Session files.
- Two workspaces share the global database but use distinct Session buckets.
- Two different Sessions run concurrently; two writers for one Session are
  fenced.
- JSONL tail repair removes only a torn final line.
- A failed follow-up appends one immutable failed child while preserving every
  earlier child; a retryable configuration failure permits another follow-up.
- A `Started` Effect becomes `Unknown` after crash or lease loss.
- An observed Effect with a missing JSONL tool result is repaired without
  executing the Effect again.
- Fork and idempotent legacy import preserve history and lineage.

These scenarios follow the repository's independent UT 95%, final-binary E2E
90%, and all-target 90% coverage gates.

## 15. Current implementation status

Latte Code resolves an absolute `LATTE_CODE_HOME`, defaulting to
`$HOME/.latte/latte-code`, and stores global control state in `state.db` plus
conversation files under `sessions/<workspace-storage-key>/`. A parsed
`database.path` identifies only a legacy workspace database for idempotent
import and cannot redirect new state.

Migration 9 adds a bounded, redacted title and canonical `workspace_root` to
the v2 Session metadata. Catalog reads do not deserialize transcript rows. The
TUI filters Sessions by the current canonical workspace, resumes the newest
matching Session on startup, starts a transient draft for `/new`, and exposes
explicit selection through `/sessions` and `/resume`.
When upgrading a v8 database, legacy rows can be adopted only when the database
is physically below the current canonical workspace; the migration backfills
their workspace and title in the schema transaction. An external or shared v8
database with unscoped Session rows fails with an explicit migration error
instead of silently attributing them to the caller's workspace.

Migration 10 replaces the singleton runtime lease with scoped lease rows.
Legacy headless runs retain the `runtime` scope, while Thread v2 uses one
`thread:<session-id>` scope per Session. Each acquisition has a distinct
coordinator owner, so a second coordinator for the same Session is rejected
while different Sessions remain concurrent. The runtime releases its lease
when an operation returns; durable input and permission waits become
writer-free clean quiescence rather than orphaned runs. Fencing tokens remain
globally monotonic so concurrent Sessions do not weaken restart recovery.
Releasing a Thread lease while its child is still active is an unclean
coordinator exit: the release transaction immediately interrupts the child, or
marks every `Started` Effect `Unknown` and requires reconciliation, before it
removes the lease. The TUI therefore never waits for process restart to escape
a lease-less `Running` projection.

A new conversation remains process-local only through local prompt and
non-secret binding validation. Once accepted, one transaction persists the
`threads_v2` row, linked Run, user transcript entry, exact lease token, and
durable `Start` events before credential resolution or Provider construction.
It cannot commit a token-zero Running Session between creation and Start.
Missing credentials or Provider
construction failure adds a secret-safe retryable failure card and returns the
Session to `Ready`; the composer stays empty and usable. A syntactically invalid
binding or a storage failure before that boundary still restores the draft.
An HTTP, authentication, transport, timeout, or model-selection failure from an
attempted Provider request follows the same retryable child-failure path: the
accepted user card and sanitized failure remain durable, while a later
follow-up creates a new child. Invalid successful responses and unsafe
Provider-issued IDs remain terminal protocol failures.
The TUI reconciles an accepted composer submission only against a redacted user
card whose source is the new-Session or follow-up commit path; an input-request
answer with identical text cannot acknowledge it. Input answers use a separate
submission identity bound to Session, Run, and request ID. Shift+Enter remains
a newline while that request owns the editor, and a failed command restores the
value only after an authoritative snapshot proves that its exact input card was
not committed. Terminal Sessions reject ordinary composer submission without
consuming the draft; a queued follow-up is restored if the active child ends
before the follow-up is committed.

A model selection is a Session binding transition, not an editor preference.
Only a `Ready` Session with no active child may change it, under the exact
`thread:<session-id>` lease and expected revision. The transaction replaces the
complete non-secret provider binding, appends a bounded System card, and emits
`BindingChanged`. The TUI blocks a competing follow-up until a refreshed
snapshot contains the selected provider and model. Provider credentials are
not resolved by this transition; construction and any resulting sanitized
failure belong to the next durably accepted child.

Migration 11 registers Project and Workspace rows, including a stable Git
common-directory Project identity so linked worktrees group together while
retaining distinct Workspace storage keys. Session JSONL uses a self-describing
header and bounded append-only entries, validates monotonic identities, rejects
symlink targets, syncs accepted records, and repairs only a torn final line.
Public transcript reads use JSONL; SQLite `conversation_outbox` rows are
deleted after the corresponding JSONL fsync. Legacy import fingerprints the source, rejects
foreign-workspace rows and ID collisions, leaves the source unchanged, skips
live leases, recovers control state, and materializes imported conversations as
JSONL. TUI discovery and search are limited to the current Workspace; catalog
reconstruction from orphan JSONL is not implemented.

Unit tests cover global-home resolution, migrations through schema 11,
worktree-aware catalog identity, scoped authority, JSONL tail repair and read
authority, idempotent legacy import, and durable retryable Provider failures.
Final-binary E2E covers global state and JSONL creation, unchanged legacy
source import, `/resume`, `/new`, long-tail follow-up, and queued multiline TUI
turns.

## 16. Delivery phases

1. Add the global product home, Workspace storage keys, global catalog, and
   per-Session leases.
2. Introduce Draft validation plus durable new-Session/follow-up acceptance so
   Provider startup failures become visible retryable child failures.
3. Add the bounded JSONL writer, reader, tail repair, checkpoints, and combined
   projection.
4. Integrate the existing fenced Effect lifecycle with JSONL tool-call and
   tool-result ordering.
5. Add legacy import, current-Workspace Session discovery, rename, and fork.
