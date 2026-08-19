# Latte Code

Latte Code is a Rust code agent for scoped repository changes. Its Ratatui UI is a durable, transcript-first conversation surface; the CLI drives the same session engine through an embedded HTTP+SSE server.

中文文档见 [docs/README.md](docs/README.md)。架构细节见 [docs/design/architecture-overview.md](docs/design/architecture-overview.md)。

## Requirements

- Rust 1.93 or newer (the pinned toolchain is in `rust-toolchain.toml`)
- A terminal for the interactive TUI
- An OpenAI-compatible chat-completions endpoint for `run` and `resume`

## Build and install

```bash
cargo build --workspace
cargo install --path crates/latte-code
```

Run `cargo run -p latte-code -- --help` without installing.

## Use

```bash
latte-code tui
latte-code run --focus crates/latte-core "add the requested validation"
latte-code resume <session-id> "continue with the next step"
latte-code show <session-id>
latte-code list
latte-code --json show <session-id>
latte-code --json list
latte-code serve --port 4096
```

`run`/`list`/`show`/`resume` are session commands served over HTTP+SSE: by default the server is embedded in the process (random loopback port, token kept in memory); `--server <url>` connects to a standalone server, reading its token from `$LATTE_CODE_HOME/server.token` or `--token`. `serve` starts a standalone server on 127.0.0.1 (default port 4096) and writes its Bearer token to `$LATTE_CODE_HOME/server.token` with owner-only permissions.

With no arguments Latte Code opens the TUI when stdin/stdout are terminals. Latte Code starts from built-in application defaults, recursively overlays `$HOME/.latte/latte-code.jsonc`, and then overlays `<workspace>/.latte/latte-code.jsonc`. Provider/model configuration is explicit: TUI and Provider-backed operations require the merged configuration to define `default_model` and at least one matching Provider model, while read-only state commands remain available with an empty Provider catalog. For the same key, the workspace value wins; arrays, scalar values, and each Provider's complete `models` catalog replace the earlier value.

To customize the current workspace:

```bash
mkdir -p .latte
cp latte-code.config.example.jsonc .latte/latte-code.jsonc
# Only needed when api_key uses an environment reference:
export OPENAI_API_KEY="..."
```

Edit `.latte/latte-code.jsonc` to select the single global `default_model` as a `provider/model` ID (for example, `primary/model-id`), configure each `providers.<name>` entry, and set `verification.argv`. Each provider's `models` object is the complete picker catalog: the map key is the actual model ID sent to that Provider, optional `name` is display/search text only, and nested `options` use that provider type's own strict schema. OpenAI Chat options currently accept `context_window`, `reasoning_effort`, `temperature`, and `max_tokens`; `context_window` is retained in the pinned model contract for the subsequent context-management and compaction implementation. Other provider types may define different keys. A string array remains shorthand for models without names or overrides. Providers do not define independent defaults. Selected model options are pinned into the Session binding fingerprint, while display names are not; request options are applied only by the selected Provider implementation. User or workspace files may contain only the keys they override, except that a Provider's `models` catalog always replaces the earlier layer's complete catalog. Latte Code does not ship a built-in Provider or model. OpenAI Chat `api_key` accepts either a literal string such as `"sk-..."` or an environment reference such as `{ source: "env", name: "OPENAI_API_KEY" }`; references are resolved only when a provider call is made. Literal values remain in the user-owned JSONC source but are excluded from Debug output, Session bindings, fingerprints, transcripts, SQLite state, and normal effect records. Restrict files containing literal credentials to the current user. `verification.argv` is executed without a shell. Durable state lives in `$LATTE_CODE_HOME/state.db` with per-Session files below `$LATTE_CODE_HOME/sessions`; the default home is `$HOME/.latte/latte-code`. `database.path` remains parseable only to locate and idempotently import an older workspace database; it cannot redirect new state.

The TUI creates v2 threads. A completed child is immutable; a follow-up creates a new linked child after its provider binding, internally derived credential/data scope, checkpoint grammar, and exact request budget are validated. Credential binding metadata is derived from the Provider name and `api_key` source; it is not part of the public configuration. `run`/`list`/`show`/`resume` operate on these v2 sessions; the v1 run-id contract is removed.

OpenAI Chat Completions can use bounded SSE when `streaming` is true. Only actual deltas are rendered; a valid inline JSON response renders only its final answer. A zero-body unsupported streaming response may make one non-streaming retry; any body byte, malformed SSE, or cancellation does not fall back. Responses API and other provider protocols are not implemented. Provider-issued v2 tool calls are executed only by the engine through fenced durable `Prepare -> Started -> Observe` transitions. A restart or lost authority after `Started` records `Unknown` and requires explicit reconciliation; neither the TUI nor provider has direct effect authority.

## Safety and recovery

- `latte-engine` is the sole authority for filesystem and process effects.
- Mutations bind to the workspace, lease, revision, content digest, and a single-use approval.
- Effect state is persisted before execution. Ambiguous interruption becomes `Unknown` and requires explicit reconciliation; it is never silently retried.
- File writes use handle-relative replacement on supported platforms and fail before starting on unsupported platforms.
- Process execution is argv-first, drains bounded output, and terminates Unix process groups on timeout or cancellation. Process supervision fails closed on non-Unix targets; CI still runs Windows check, Clippy, UT, Contract, portable final-binary E2E, and release build gates without claiming Windows process execution support.
- TUI Enter sends a nonblank composer or pending-input value, while Shift+Enter inserts a newline. Permission and reconciliation prompts consume both keys without approving or acknowledging anything; those protected actions require their explicit chords.

## Configuration library

The CLI/TUI configuration layers are [`$HOME/.latte/latte-code.jsonc` and `<workspace>/.latte/latte-code.jsonc`](latte-code.config.example.jsonc) as described above. `latte-engine::config` is a smaller JSONC/environment-placeholder loader retained for embedding integrations; `Config::load` reads `.latte/latte-engine.jsonc`, and its `${NAME}` placeholders are not a CLI/TUI setup interface.

## Development

The complete local setup and workflow are documented in [DEVELOPMENT.md](DEVELOPMENT.md). The shortest path is:

```bash
make setup
make ci
```

Individual Cargo commands remain available:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo doc --workspace --no-deps
```

Coverage is checked with `cargo llvm-cov --workspace --all-features --all-targets --fail-under-lines 90`; dependencies with `cargo deny check`.

## Workspace

| Crate | Responsibility |
| --- | --- |
| `latte-core` | Typed v1/v2 IDs, commands, events, run state, thread snapshots, and redaction |
| `latte-engine` | SQLite/WAL, v1/v2 leases, thread commits, policy, tools, effects, and process supervision |
| `latte-headless` | Provider/SSE, context, v1 agent loop, and transcript-thread coordination |
| `latte-tui` | Ratatui transcript projection/reducer and terminal lifecycle |
| `latte-code` | Composition crate for the CLI and TUI binary |

Licensed under the [Apache License 2.0](LICENSE).
