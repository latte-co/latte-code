# AGENTS.md

## Project

Latte Code is a Rust 2024 workspace implementing a code agent with a Ratatui frontend and headless CLI. Rust is the only implementation; do not add Node.js, TypeScript, or compatibility layers.

## Structure

- `crates/latte-core`: serializable protocol and state machine; independent of storage, providers, and UI.
- `crates/latte-engine`: privileged authority for SQLite state, leases, policy, filesystem tools, effects, and supervised processes.
- `crates/latte-headless`: provider, repository context, agent loop, verification, handoff, and command service.
- `crates/latte-tui`: pure UI projection/reducer and Ratatui terminal lifecycle.
- `crates/latte-code`: binary composition and CLI contracts.
- `docs/`: architecture and operations docs (Chinese only).

Local `.latte/`, `.oh-my-code/`, `.tmp/`, `target/`, logs, and secrets are not project source.

## Required checks

Run the complete local gate with `make ci`. See `DEVELOPMENT.md` for setup, individual targets, and troubleshooting.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo doc --workspace --no-deps --locked
```

When installed, also run `make lint-ci`, `make coverage`, and `cargo deny --locked check`. Coverage is three independent gates: UT-only lines >= 95%, final-binary E2E lines >= 90%, and all-target lines >= 90%. Do not lower or merge these floors.

## PR delivery policy

Deliver repository changes through a topic branch and a pull request targeting `main`; never push changes directly to `main`. The repository ruleset or branch-protection configuration must require the single stable `PR Gate` check before merge. `PR Gate` is a fail-closed aggregate: it succeeds only when repository quality, Rust 1.93 MSRV, all three platforms' check/Clippy/UT/Contract/portable E2E/release build, Linux/macOS Unix PTY/process E2E, documentation, three independent coverage jobs, and dependency audit succeed. A skipped or cancelled dependency is a gate failure.

The workflow contract lives in `.github/workflows/ci.yml`, but GitHub rulesets and branch protection are external repository settings. Do not claim that required-check enforcement is active until the remote repository has actually been configured to require `PR Gate`. Release builds prove compilation only; they are required by the PR gate but do not replace release smoke.

## Feature test policy

Every change that adds or modifies product behavior must include both:

- UT at the lowest responsible module, covering success, boundary, typed failure, and relevant safety-negative paths.
- At least one final-binary E2E that enters through `CARGO_BIN_EXE_latte-code` and proves the primary user-observable journey. Permission, security, recovery, verification, process, and TUI changes also require the applicable negative journey.

The workspace must pass at least 95% line coverage from UT-only execution (`make coverage-unit`) and at least 90% line coverage from final-binary E2E execution (`make coverage-e2e`). UT, E2E, Contract, and doc-test hits are measured independently and must not be substituted or merged. New or modified functional code must keep both global gates passing and must directly cover its own success, boundary, typed-failure, and applicable safety-negative paths.

Required E2E must be isolated, deterministic, bounded, independent of the public network and real Provider credentials, and free of `#[ignore]` or conditional skips. Use observable events or durable projections instead of fixed sleeps, continuously drain PTY output, and clean up child process groups on failure.

Put cross-platform final-binary CLI, loopback Provider, and SQLite journeys in `e2e_portable`; they must execute on Linux, macOS, and Windows without skipping the target. Put PTY, Unix signals/process groups, symlink semantics, and executable Unix verification journeys in `e2e_unix`. Windows process execution remains fail-closed, so portable Provider journeys must terminate before verification (for example, an input request or typed Provider failure) instead of pretending Unix process supervision exists.

Follow the [E2E authoring guide](docs/testing/e2e-authoring-guide.md). A feature without the required UT, E2E, and coverage evidence is incomplete. Pure documentation, formatting, comments, or behavior-neutral build metadata may omit E2E only when the delivery note explains why behavior is unchanged.

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
