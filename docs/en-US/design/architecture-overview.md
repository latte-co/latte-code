# Rust architecture

Completion uses a stable A/B workspace snapshot while holding the engine operation gate. Snapshot B is the verification linearization point immediately before the atomic database commit. The handoff stores B's manifest digest and verification time; changes after B are not claimed as verified and can be detected by comparing a later manifest with that digest. Symbolic links are hashed as topology records and unsafe or unstable links fail closed.

Workspace manifest paths and symbolic-link targets must be valid UTF-8. Non-UTF-8 operating-system path bytes fail closed and are never replaced or folded into lossy manifest keys.
Manifest keys serialize the exact UTF-8 path component array as JSON; they never normalize separators. A literal backslash in a Unix filename therefore remains distinct from a nested path.

Chinese counterpart: [中文架构说明](../../zh-CN/design/architecture-overview.md).

## Composition

```text
latte-code CLI/TUI
  |-- latte-tui -------- projection + typed UI actions
  |-- latte-headless --- provider + context + agent loop + verification
        |-- latte-engine -- storage + policy + effects + tools + processes
              |-- latte-core -- IDs + commands + events + state transitions
```

`latte-core` has no storage, provider, or UI dependency. `latte-engine` owns all privileged effects and exposes a restricted handle. Both frontends consume durable projections and issue typed commands; they do not mutate repository or runtime state directly.

## Durable state

The CLI and TUI share one product-wide SQLite database at `$HOME/.latte/latte-code/state.db`, or at `LATTE_CODE_HOME/state.db` when that environment variable is an absolute path. A workspace supplies provider and verification configuration, but never owns product state. SQLite runs with WAL, foreign keys, a busy timeout, and full synchronous durability. The single writer commits an event, revision, and projection atomically. Command IDs are deduplicated across restart. The legacy `database.path` field is accepted only for migration compatibility and does not select the state database.

Thread v2 is additive: v1 `RunState`, commands, events, and protocol version remain byte compatible. Migration 7 adds separate `threads_v2`, linked child runs, the sole `thread_active_runs_v2` authority, paged typed transcript cards, a distinct thread event stream, and redacted command/source deduplication; migration 8 adds the engine-private canonical effect-descriptor table; migration 9 adds bounded Session catalog metadata (`title` and canonical `workspace_root`). A linked child cannot use a legacy transition, checkpoint, process, or tool mutation API. Its state/event/transcript updates are fenced through `CommitThreadRunUpdate`; old runs stay CLI-readable and are never backfilled.

Run writes require a live owner lease and monotonically fenced token. Reacquiring an expired lease advances the fencing epoch, including for the same owner. A stale owner cannot append events after takeover.

## Effects and permissions

Effects follow:

```text
Declared -> Prepared -> Started -> ObservedSuccess | ObservedFailed | Unknown
```

The runtime persists intent and a pre-effect hash before execution. Approval binds the effect ID, run revision, lease, and exact request digest and is single-use. Permission consumption and the `Prepared -> Started` transition are atomic. The exact descriptor is retained only in an engine-private table; the effects ledger, checkpoint, transcript, events, provider-history rebuild, and TUI receive a separate redacted projection. A crash or ambiguous observation produces `Unknown`; retry requires reconciliation instead of guessing success.

Filesystem tools enforce workspace containment, denied globs, stale-content checks, and exact edit/create intent. Mutating writes use a held directory handle and same-directory atomic rename where safe primitives exist. Unsupported targets fail before permission consumption or `Started`.

## Process supervision

Commands are argv-first. Shell syntax is classified separately and treated as high-risk. Output is drained concurrently with byte caps; timeout and cancellation use a bounded grace period. Unix execution creates and supervises a process group, sends `TERM`, then `KILL`, and certifies descendant shutdown. Non-Unix process execution currently fails closed before creating an effect. CI runs Windows check, Clippy, UT, Contract, portable final-binary E2E, and release-build gates without claiming Windows process execution support.

## Transcript runtime

Constrained terminals continue rendering the product layout instead of replacing it with a resize gate: decorative welcome content and secondary metadata collapse first, while the composer, blocking actions, and available transcript rows remain visible.

The terminal presents one focused conversation as a single transcript viewport with a fixed multiline composer; there is no session sidebar or session overlay. The welcome state keeps product identity and the resolved environment side by side in a wide/tall viewport and intentionally stacks them at smaller widths. After the first prompt, the compact header presents the resolved workspace path rather than inventing branch or repository metrics. Its pure presentation projection groups durable cards by child run and pairs tool-call/tool-result cards by their public `tool_call_id` when available. Activity uses at most three semantic levels: a truthful run/status heading, a tool action, and an optional result detail with bounded structured target/query/command metadata. Permission presentation derives separate operation, target, and scope rows from the redacted durable descriptor and summary. Completion cards carry the redacted handoff payload so changed files and verification evidence survive restart and remain available to the TUI. The projection never renders private checkpoint data or raw payload JSON. Thread projections carry the newest bounded 500 cards, not the oldest page; when earlier cards are omitted, the transcript renders that fact explicitly.

Composer mode owns every printable character, including `q`, `s`, `j`, `k`, and `?`. Enter sends a nonblank composer or pending-input value, while Shift+Enter inserts a newline; the idle composer also advertises Ctrl+Enter as a compatibility submit chord, while F5 remains unadvertised. Unicode grapheme clusters are edited atomically, while wrapping, alignment, and cursor placement use terminal display width for CJK and emoji. Ctrl+P opens a local command palette whose help, navigation, refresh, and quit entries reduce to the same safe UI state or typed actions as their keyboard equivalents. The terminal session enables progressive keyboard disambiguation and bracketed paste where supported, without enabling unused mouse capture, and restores the prior mode on every exit path. Esc explicitly enters transcript Navigation mode, where `j`/`k` select actions, PageUp/PageDown scroll, Enter/Space expands or collapses the selected action, and printable `q` quits; F10 is the explicit quit key in either mode. The first Ctrl+C interrupts active work and arms an exit confirmation; a second Ctrl+C within two seconds exits, while timeout or another key disarms it. A pending permission is an inline card with a bounded, control-safe redacted operation summary (write target/content intent, process argv/cwd, or a read/invocation target). Permission and reconciliation branches consume the complete key event: decisions require the exact focused `d` or Ctrl+A action, and Enter and Shift+Enter cannot approve, acknowledge, or mutate a text buffer. Event gaps and reconnects clear transient progress then reload an authoritative snapshot inside the projection adapter. The event loop redraws only after projected or local state changes. A single local queued follow-up is dispatched only from a freshly read `Ready` snapshot.

The current thread coordinator supports provider conversation, typed user/assistant/input/failure/completion cards, immutable follow-up children, and exact bounded history. Provider-issued tool-call and input-request IDs must use the small opaque grammar `[A-Za-z0-9_-]{1,256}` before they can become durable source, request, or deduplication keys. It does not grant direct effect authority to the provider or TUI. Provider-requested v2 tools are executed only by the engine as fenced durable effects: `Prepare -> Started -> Observe`. The engine persists the descriptor before execution, renews the lease while a provider call or started effect is active, and records `Unknown` on restart or lease loss after `Started`; only an explicit reconciliation can resolve that state. The TUI exposes that acknowledgement as a separate confirmation: Ctrl+R opens the redacted unknown-effect card and Ctrl+A confirms; Enter never acknowledges it. Confirmation records the effect as failed and terminalizes the exact linked child.

## Agent runtime

The headless runtime assembles repository context, including `AGENTS.md`, talks to an OpenAI-compatible chat-completions provider, executes typed tool requests through the engine, runs the configured verification argv, and persists evidence plus a handoff. Completion policies prevent a run requiring verification from completing when verification failed or was not run.

The CLI supports `run`, `resume`, `show`, and `list`, plus versioned JSON output. The TUI is a Ratatui projection over the same engine state, handles lag through snapshot refresh, shows permission/input/unknown states explicitly, and restores terminal state through normal exit, error, panic, and interrupt paths.

## Trust boundaries

- Provider credentials are resolved in memory and redacted from debug output.
- `latte-code` recursively merges built-in defaults, `$HOME/.latte/latte-code.jsonc`, and workspace `.latte/latte-code.jsonc` in that order; missing files are valid and workspace keys win. It selects the named default provider for new runs and shares one headless registry between CLI and TUI. Thread bindings structurally pin every v1 semantic binding field (including aliases), plus a non-secret credential reference, credential generation, and data scope before any secret resolution or history egress. Resume fails closed when a pinned semantic binding no longer matches.
- OpenAI Chat Completions supports bounded SSE when configured. It handles CRLF, comments, multi-data events, UTF-8 chunk splits, tool aggregation, `[DONE]`, cancellation, and a single zero-body unsupported-stream fallback. Inline JSON is rendered as a final outcome, never fabricated deltas. Responses API remains out of scope.
- Repository text, model output, and tool output are untrusted inputs.
- The model cannot directly invoke filesystem or process APIs.
- Approval defaults to denial and cannot be implied by a generic Enter key.
- Recovery never converts missing evidence into success.
