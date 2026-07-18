# Global session and data storage design

Status: **Proposed; not yet implemented.**

The current implementation stores runtime state and thread transcripts in the
configured workspace-relative SQLite database, whose default is
`.latte/latte-code.db`. This document defines the target storage contract. The
migration must preserve the existing engine invariants around effects,
permissions, leases, fencing, deduplication, and `Unknown` reconciliation.

## 1. Decisions

| Data class | Authoritative location | Contract |
| --- | --- | --- |
| Session conversation content | Global per-session JSONL | Append-only user, assistant, tool, and context records. |
| Project, workspace, and session metadata | Global SQLite | Discovery, search, lifecycle, provider binding, lineage, and archive state. |
| Run and effect control state | Global SQLite | Transactional run state, effects, permissions, leases, checkpoints, evidence, and deduplication. |
| Drafts and Provider runtime | Process memory | Draft prompts, HTTP streams, retries, cancellation, deltas, and startup errors. |
| Credentials | No persistent store | Only non-secret credential references and generations may be durable. |

Additional decisions:

- Session and database state never live below the workspace.
- Session files are grouped by workspace, not by date.
- JSONL is the sole replay source for conversation content; SQLite does not
  duplicate the transcript.
- There is no transcript outbox and no durable Provider-attempt table.
- Provider startup failures are not Session facts and are never persisted.
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
- **Draft** is an in-memory new Session or follow-up that has not reached the
  persistence commit point.

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
`database.path` is rejected rather than silently redirecting user history. The
storage home may be selected only by the process environment or trusted user
configuration.

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
  archived_at_ms
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

The current database-wide singleton lease cannot be used after all workspaces
share one global database. The target lease is scoped to a Session:

```text
session_leases
  session_id (primary key)
  owner
  fencing_token
  expires_at_ms
```

Different Sessions can run concurrently. A Session has at most one active
engine owner and one JSONL writer. Reacquiring an expired lease advances the
fencing token. A stale owner cannot begin or observe an Effect and must close
its Session writer when ownership is lost.

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
-> resolve the credential reference in memory
-> construct the Provider
-> make the first Provider request
```

Configuration, credential, model, authentication, transport, timeout, or other
startup failure leaves no Session row, Run row, JSONL file, persistent log
record, or telemetry payload. The sanitized presentation error remains in the
current UI state, and the prompt returns to the composer for retry.

The persistence commit point is the first complete valid Provider outcome:

- A complete assistant message.
- A complete assistant tool-call envelope.
- A valid input request.

After that point the application:

1. Inserts a non-listable `materializing` Session metadata row.
2. Writes and syncs the JSONL header, user message, and complete Provider
   outcome.
3. Creates the durable Run/control state required by the outcome.
4. Marks the Session listable with its actual lifecycle.

Startup removes a `materializing` row with no valid file, or repairs catalog
metadata from a valid self-identifying file. Empty failed Sessions never appear
in discovery.

### 8.2 Follow-up

A follow-up is also an in-memory Draft until its first complete Provider
outcome. A startup failure does not create a child Run or append the user
prompt. The existing Session remains byte-for-byte unchanged and the prompt is
returned to the composer.

Once a complete outcome arrives, the Run is materialized and the user/outcome
records are appended together. If a later Provider request fails after durable
tool work, only the minimum generic Run state such as `Interrupted` or
`ReconciliationRequired` is retained. The Provider error text is still not
persisted.

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

- Global Session discovery, search, archive filtering, Project grouping, and
  Workspace grouping query SQLite only.
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

- **Archive** updates `archived_at_ms`; the JSONL file is not moved.
- **Fork** creates a new independent file in the target Workspace bucket,
  copies content through the selected sequence, and records
  `forked_from_session_id` plus `forked_from_seq` in SQLite.
- **Hard delete** is explicit and removes the Session catalog row, control
  state, JSONL, and owned attachments. Archive remains the default UI action.
- **Workspace loss or movement** never deletes history. Resume requires an
  explicit valid Workspace binding when the original path is unavailable.

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
Workspace-layer `database.path` becomes invalid after the global storage
contract is enabled.

## 14. Required verification

Implementation is incomplete until UT and final-binary E2E prove at least:

- Provider startup failure changes neither the Session count nor the JSONL
  tree.
- Provider startup error text is absent from SQLite, JSONL, and persistent
  application logs.
- A workspace contains no Latte Code database or Session files.
- Two workspaces share the global database but use distinct Session buckets.
- Two different Sessions run concurrently; two writers for one Session are
  fenced.
- JSONL tail repair removes only a torn final line.
- A failed follow-up leaves the original Session unchanged.
- A `Started` Effect becomes `Unknown` after crash or lease loss.
- An observed Effect with a missing JSONL tool result is repaired without
  executing the Effect again.
- Archive, fork, Workspace rebinding, and idempotent legacy import preserve
  history and lineage.

These scenarios follow the repository's independent UT 95%, final-binary E2E
80%, and all-target 90% coverage gates.

## 15. Delivery phases

1. Add the global product home, Workspace storage keys, global catalog, and
   per-Session leases.
2. Introduce Draft new-Session and follow-up lifecycles so Provider startup
   failures remain transient.
3. Add the bounded JSONL writer, reader, tail repair, checkpoints, and combined
   projection.
4. Integrate the existing fenced Effect lifecycle with JSONL tool-call and
   tool-result ordering.
5. Add legacy import, global Session discovery, archive, fork, delete, and
   Workspace rebinding.
