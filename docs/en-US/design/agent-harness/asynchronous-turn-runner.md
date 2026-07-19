# Asynchronous Turn Runner and Session Mailbox

Status: **design proposal; not implemented.**

Chinese counterpart: [异步 Turn Runner 与 Session Mailbox](../../../zh-CN/design/agent-harness/asynchronous-turn-runner.md).

## 1. Decision

A Session has at most one agent loop, but the loop runs asynchronously. The TUI,
CLI, and trusted runtime sources may submit user input or a reminder while it is
running. Inputs enter one bounded mailbox per Session and the runner consumes
them at safe boundaries.

Asynchronous does not mean concurrent Provider requests for a Session or
mutating an HTTP stream already sent. Sessions may run concurrently; Provider
context, Run revision, tool round, and JSONL order remain serial per Session.
This evolves the v2 `ThreadRuntimeService` process-local active map and the
TUI's one-follow-up slot without changing protocol v1.

## 2. Ownership and input

`latte-headless` owns `TurnSupervisor`: it builds Provider history, drives the
stream, coordinates tool continuation, and consumes the mailbox. It has no
direct filesystem, process, SQLite-write, or approval-consumption capability;
those remain restricted `latte-engine` handles. `latte-engine` is authoritative
for leases, Run revisions, Effects, Permissions, and durable projections.
`latte-tui` owns composer/queued presentation only and submits typed commands.

```rust
enum RuntimeInput {
    UserPrompt { input_id: InputId, text: String },
    TrustedReminder {
        input_id: InputId,
        source: ReminderSource,
        text: String,
        dedupe_key: Option<String>,
        expires_at_ms: Option<u64>,
    },
}

enum ControlInput {
    Cancel,
    PermissionDecision { request_id: RequestId, decision: Decision },
    RequestedInputAnswer { request_id: RequestId, value: String },
}
```

Only a composition-root registered source creates `TrustedReminder`. It has
provenance, size limit, optional deduplication key, and expiry. TUI, Provider,
tool output, and extension text cannot relabel arbitrary text as a privileged
reminder.

## 3. Ordering and safe injection

Each mailbox entry has monotonic `input_seq`. User input is strict FIFO; valid
reminders use the same arrival-ordered data queue. Only control input bypasses
it: cancellation, permission decision, and explicit input-request answer wake
the runner promptly.

Data input enters Provider history only after the current Provider outcome is
complete, all tool results in the round are observed and in context, no Effect
has `Started` (or it reached a recoverable observed state), and there is no
pending permission/input/reconciliation. An input received during a tool Effect
therefore cannot change descriptor, approval, revision, or execution order and
remains `Queued`.

Ordinary Enter uses `Queue`. A future explicit `Steer` may cancel a Provider
stream only when no Effect has `Started`, discard partial delta, and continue
with confirmed context plus mailbox head. `Steer` never cancels an external
Effect implicitly; that needs explicit `Cancel` and normal `Unknown`/
reconciliation behavior.

## 4. Lifecycle, capacity, and recovery

Mailbox, partial delta, stream handle, timer, and cancellation token are
process-local. Acceptance may display `InputQueued` progress but writes no
JSONL, SQLite, or telemetry. Once an entry enters a Provider request and gets a
first complete valid outcome, session-store materialization appends exact input
and outcome to JSONL and creates needed Run/control state. Start failure, a full
mailbox, expired reminder, or process exit creates no Conversation Record.

Capacity and entry bytes are bounded and configurable. `MailboxFull` is
secret-safe, preserves composer text, and never drops older input. Only the
current Session lease owner runs the supervisor; lease loss stops consumption,
cancels cancellable Provider work, closes the writer, and leaves started Effects
to engine `Unknown`/reconciliation. An unmaterialized mailbox can be lost in a
crash: this deliberately follows the no Provider-attempt/Draft persistence rule.

`ThreadSnapshot` remains durable lifecycle authority. `ThreadTransientProgress`
may display `queued`, `consumed`, and `expired`, but the TUI discards these on an
event gap or reconnect and reloads its snapshot.

## 5. Acceptance

- UT proves one Provider request per Session and concurrent independent Sessions.
- UT uses fake clock and scripted Provider for FIFO, reminder deduplication/
  expiry, capacity rejection, cancellation precedence, and safe injection.
- UT proves input during tool/effect work cannot alter prepared descriptor,
  approval digest, or Run revision.
- E2E proves the final binary accepts a second prompt during a Provider stream
  and sends it exactly once in the next safe request.
- E2E proves a reminder during tool execution is injected only after tool result;
  cancellation and Provider-start failure create no fake transcript entry.
