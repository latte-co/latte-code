# AGENTS.md

## Project

Latte Code is a Rust 2024 workspace implementing a code agent with a Ratatui frontend and headless CLI. Rust is the only implementation; do not add Node.js, TypeScript, or compatibility layers.

## Structure

- `crates/latte-core`: serializable protocol and state machine; independent of storage, providers, and UI.
- `crates/latte-engine`: privileged authority for SQLite state, leases, policy, filesystem tools, effects, and supervised processes.
- `crates/latte-headless`: provider, repository context, agent loop, verification, handoff, and command service.
- `crates/latte-tui`: pure UI projection/reducer and Ratatui terminal lifecycle.
- `crates/latte-code`: binary composition and CLI contracts.
- `docs/en-US` and `docs/zh-CN`: maintained architecture and operations docs.

Local `.latte/`, `.oh-my-code/`, `.tmp/`, `target/`, logs, and secrets are not project source.

## Required checks

Run the complete local gate with `make ci`. See `DEVELOPMENT.md` for setup, individual targets, and troubleshooting.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo doc --workspace --no-deps
```

When installed, also run `cargo llvm-cov --workspace --all-features --all-targets --fail-under-lines 90` and `cargo deny check`. Do not lower the coverage floor.

## Invariants

- `latte-engine` is the sole privileged effect authority; frontends use typed commands and projections.
- Persist declared/prepared effects before execution. Never infer success after interruption; record `Unknown` and require reconciliation.
- Bind mutations to exact run revision, lease fencing token, input digest, and single-use approval.
- Keep filesystem mutations workspace-contained and handle-relative. Unsupported safety primitives fail before `Started`.
- Execute commands argv-first. Explicit shell execution is high-risk and must pass policy.
- Bound output, timeout, cancellation, and shutdown. On Unix, supervise the whole process group.
- Never log or persist provider keys.
- Required verification cannot complete when failed, missing, or not run.
- TUI actions reduce to typed runtime commands; Enter never implicitly approves permission.

Keep English and Chinese formal docs aligned. Document only implemented behavior and name platform limits. Do not commit, push, tag, publish, or create a PR without an explicit user request.
