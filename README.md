# Latte Code

Latte Code is a Rust code agent for scoped repository changes. Its Ratatui UI is a durable, transcript-first conversation surface; the scriptable v1 CLI remains available for existing runs.

中文文档见 [docs/zh-CN/README.md](docs/zh-CN/README.md). Architecture details are in [docs/en-US/design/architecture-overview.md](docs/en-US/design/architecture-overview.md).

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
latte-code resume <run-id> --allow
latte-code resume <run-id> --deny
latte-code show <run-id>
latte-code list
latte-code --json show <run-id>
latte-code --json list
```

With no arguments Latte Code opens the TUI when stdin/stdout are terminals. Configuration is optional: Latte Code starts from built-in defaults, recursively overlays `$HOME/.latte/latte-code.jsonc`, and then overlays `<workspace>/.latte/latte-code.jsonc`. For the same key, the workspace value wins; arrays and scalar values replace the earlier value.

To customize the current workspace:

```bash
mkdir -p .latte
cp latte-code.config.example.jsonc .latte/latte-code.jsonc
export OPENAI_API_KEY="..."
```

Edit `.latte/latte-code.jsonc` to select `default_provider`, configure its `providers.<name>` entry, and set `verification.argv`. Either user or workspace files may contain only the keys they override. The bundled defaults and example use an OpenAI Chat provider with `api_key: { source: "env", name: "OPENAI_API_KEY" }`; the named environment variable is resolved only in memory when a provider call is made. `verification.argv` is executed without a shell. Product state is global at `$HOME/.latte/latte-code/state.db`, or below an absolute `LATTE_CODE_HOME` override; it is never created in a workspace. The legacy `database.path` configuration field is accepted only for migration compatibility and does not redirect product state. Provider secrets are never written to the transcript or normal effect ledger.

The TUI creates v2 threads. A completed child is immutable; a follow-up creates a new linked child after its provider binding, data scope, credential reference/generation, checkpoint grammar, and exact request budget are validated. Configure `credential_ref_id`, `data_scope_id`, and `credential_generation` for every provider used by the TUI. Older v1 runs remain readable through `show`/`list` but are not upgraded into threads.

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
