# Rust architecture

Completion uses a stable A/B workspace snapshot while holding the engine operation gate. Snapshot B is the verification linearization point immediately before the atomic database commit. The handoff stores B's manifest digest and verification time; changes after B are not claimed as verified and can be detected by comparing a later manifest with that digest. Symbolic links are hashed as topology records and unsafe or unstable links fail closed.

Workspace manifest paths and symbolic-link targets must be valid UTF-8. Non-UTF-8 operating-system path bytes fail closed and are never replaced or folded into lossy manifest keys.
Manifest keys serialize the exact UTF-8 path component array as JSON; they never normalize separators. A literal backslash in a Unix filename therefore remains distinct from a nested path.

Chinese counterpart: [中文架构说明](../../zh-CN/design/architecture-overview.md).

## Composition

```text
lattecode CLI/TUI
  |-- latte-tui -------- projection + typed UI actions
  |-- latte-headless --- provider + context + agent loop + verification
        |-- latte-engine -- storage + policy + effects + tools + processes
              |-- latte-core -- IDs + commands + events + state transitions
```

`latte-core` has no storage, provider, or UI dependency. `latte-engine` owns all privileged effects and exposes a restricted handle. Both frontends consume durable projections and issue typed commands; they do not mutate repository or runtime state directly.

## Durable state

Each workspace uses `.latte/lattecode.db`. SQLite runs with WAL, foreign keys, a busy timeout, and full synchronous durability. The single writer commits an event, revision, and projection atomically. Command IDs are deduplicated across restart.

Run writes require a live owner lease and monotonically fenced token. Reacquiring an expired lease advances the fencing epoch, including for the same owner. A stale owner cannot append events after takeover.

## Effects and permissions

Effects follow:

```text
Declared -> Prepared -> Started -> ObservedSuccess | ObservedFailed | Unknown
```

The runtime persists intent and a pre-effect hash before execution. Approval binds the effect ID, run revision, lease, and exact request digest and is single-use. Permission consumption and the `Prepared -> Started` transition are atomic. A crash or ambiguous observation produces `Unknown`; retry requires reconciliation instead of guessing success.

Filesystem tools enforce workspace containment, denied globs, stale-content checks, and exact edit/create intent. Mutating writes use a held directory handle and same-directory atomic rename where safe primitives exist. Unsupported targets fail before permission consumption or `Started`.

## Process supervision

Commands are argv-first. Shell syntax is classified separately and treated as high-risk. Output is drained concurrently with byte caps; timeout and cancellation use a bounded grace period. Unix execution creates and supervises a process group, sends `TERM`, then `KILL`, and certifies descendant shutdown. Non-Unix process execution currently fails closed before creating an effect. CI compile-checks Windows but does not claim Windows process execution support.

## Agent runtime

The headless runtime assembles repository context, including `AGENTS.md`, talks to an OpenAI-compatible chat-completions provider, executes typed tool requests through the engine, runs the configured verification argv, and persists evidence plus a handoff. Completion policies prevent a run requiring verification from completing when verification failed or was not run.

The CLI supports `run`, `resume`, `show`, and `list`, plus versioned JSON output. The TUI is a Ratatui projection over the same engine state, handles lag through snapshot refresh, shows permission/input/unknown states explicitly, and restores terminal state through normal exit, error, panic, and interrupt paths.

## Trust boundaries

- Provider credentials are resolved in memory and redacted from debug output.
- Repository text, model output, and tool output are untrusted inputs.
- The model cannot directly invoke filesystem or process APIs.
- Approval defaults to denial and cannot be implied by a generic Enter key.
- Recovery never converts missing evidence into success.
