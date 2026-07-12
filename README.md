# Lattecode

Lattecode is a Rust code agent for scoped repository changes. It provides a Ratatui terminal interface and a scriptable headless CLI over the same durable runtime.

中文文档见 [docs/zh-CN/README.md](docs/zh-CN/README.md). Architecture details are in [docs/en-US/design/architecture-overview.md](docs/en-US/design/architecture-overview.md).

## Requirements

- Rust 1.93 or newer (the pinned toolchain is in `rust-toolchain.toml`)
- A terminal for the interactive TUI
- An OpenAI-compatible chat-completions endpoint for `run` and `resume`

## Build and install

```bash
cargo build --workspace
cargo install --path crates/lattecode
```

Run `cargo run -p lattecode -- --help` without installing.

## Use

```bash
lattecode tui
lattecode run --focus crates/latte-core "add the requested validation"
lattecode resume <run-id> --allow
lattecode resume <run-id> --deny
lattecode show <run-id>
lattecode list
lattecode --json show <run-id>
lattecode --json list
```

With no arguments Lattecode opens the TUI when stdin/stdout are terminals. `run` and `resume` require:

```bash
export LATTE_OPENAI_ENDPOINT="https://example.invalid/v1/chat/completions"
export LATTE_OPENAI_MODEL="model-name"
export LATTE_OPENAI_API_KEY="..."
export LATTE_VERIFY_ARGV='["cargo","test","--workspace"]'
```

`LATTE_VERIFY_ARGV` is a JSON argv array and is executed without a shell. State is stored in `.latte/lattecode.db`; do not commit that directory. Provider secrets stay in memory and are never written to the database.

## Safety and recovery

- `latte-engine` is the sole authority for filesystem and process effects.
- Mutations bind to the workspace, lease, revision, content digest, and a single-use approval.
- Effect state is persisted before execution. Ambiguous interruption becomes `Unknown` and requires explicit reconciliation; it is never silently retried.
- File writes use handle-relative replacement on supported platforms and fail before starting on unsupported platforms.
- Process execution is argv-first, drains bounded output, and terminates Unix process groups on timeout or cancellation. Process supervision fails closed on non-Unix targets; Windows is compile-checked in CI but execution is unsupported.
- TUI permission prompts default to deny and approval requires an explicit action.

## Configuration library

The engine exposes a JSONC loader for embedders. See [lattecode.config.example.jsonc](lattecode.config.example.jsonc). The CLI uses the `LATTE_*` variables above directly.

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
| `latte-core` | Typed IDs, commands, events, run state, and transitions |
| `latte-engine` | SQLite/WAL, leases, policy, tools, effects, and process supervision |
| `latte-headless` | Provider, context, agent loop, verification, and headless service |
| `latte-tui` | Ratatui projection/reducer and terminal lifecycle |
| `lattecode` | User-facing CLI and TUI binary |

Licensed under the [Apache License 2.0](LICENSE).
