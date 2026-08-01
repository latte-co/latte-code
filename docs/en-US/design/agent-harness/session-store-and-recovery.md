# Session Storage and Recovery

Status: **design proposal; not implemented.**

Chinese counterpart: [Session 存储与恢复](../../../zh-CN/design/agent-harness/session-store-and-recovery.md).

## 1. Decision

A Session is a user-visible conversation. Content is append-only JSONL; SQLite
holds searchable Session metadata and runtime control state that needs
transactions/CAS. Both live in a user-global Latte Code home, never a workspace.
Workspace configuration may affect behavior but cannot choose history/database
location.

This replaces the current workspace `.latte/latte-code.db` direction. During
migration, `ThreadId` is the Session ID; no second conversation identity exists.

## 2. Storage model

```text
$LATTE_CODE_HOME/
  state.db
  sessions/
    <canonical-workspace-key>/<session-id>.jsonl
```

SQLite holds at least Project, Workspace, Session, Run, Effect, Permission,
Lease, Checkpoint, Evidence, and deduplication keys. It holds title,
last-activity, non-secret binding fingerprint, archive state, and JSONL locator,
but never duplicates conversation transcript.

JSONL starts with a small self-describing header. Later lines append only bounded
`message`, complete tool-call/tool-result, checkpoint, or compaction records with
stable `entry_id`, monotonic `seq`, and optional `run_id`. It excludes credentials,
request headers, raw Provider errors, partial delta, cancellation tokens, and
engine-private Effect descriptors.

## 3. Materialization and recovery

New Sessions and follow-ups remain in-memory Drafts through local validation.
Once a prompt is accepted, its Session/Run and exact user content become durable
before credential resolution, Provider construction, or network I/O.

Accepted submission is materialization:

1. insert non-discoverable `materializing` Session metadata;
2. append and sync the JSONL header and consumed input;
3. create the child Run/control state and make the Session discoverable; and
4. append the complete Provider outcome or a bounded, redacted failure card.

Validation or storage failure before acceptance creates no Session/Run row or
JSONL and restores the exact Draft. Configuration, credential, model,
authentication, transport, or start failure after acceptance preserves the user
record and appends a sanitized failure record. Provider-construction failures
are retryable; raw Provider errors and credentials are never persisted.

Startup may trim one torn final JSONL line only and must not rewrite valid
history. It repairs catalog from the header or removes `materializing` metadata
without valid JSONL. An `Started` Effect with uncertain observation becomes
`Unknown` and ends only through reconciliation.

## 4. Concurrency, privacy, and acceptance

Canonical workspace root determines bucket key. Separate Git worktrees have
separate Workspace records even for one Project. Each Session has one lease, one
writer, and increasing fencing token; takeover advances it and stale owners
cannot start or observe Effects.

Session mailbox, Provider stream, and retry are process-local. Crash-surviving
unconsumed prompts need an explicit durable inbox, visible user semantics, and
cleanup policy; they must not be silently hidden in JSONL or Provider Attempts.

- UT deterministically covers workspace buckets, materialization crash points,
  torn final lines, lease takeover, and fencing.
- UT proves credentials, Provider errors, partial delta, and descriptors never
  enter JSONL.
- E2E proves a fresh process lists, opens, and replays a completed Session; a
  Provider start failure preserves the accepted input, appends one bounded
  failure, and permits retry; interruption after `Started` exposes
  reconciliation only.
