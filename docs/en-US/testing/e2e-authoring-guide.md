# Latte Code E2E Authoring Guide

> This guide defines how feature development must pair unit tests with final-binary E2E tests. See [UT / E2E testing gate design](../design/testing-gates.md) for the overall gate model and scenario IDs.
>
> 中文版本：[E2E 编写手册](../../zh-CN/testing/e2e-authoring-guide.md)。

## 1. Mandatory policy

Every change that adds or modifies product behavior must include all of the following:

1. **UT**: directly prove the rules, boundaries, and failures at the lowest responsible module.
2. **E2E**: enter through the final `latte-code` binary and prove the user-observable behavior.
3. **Independent coverage gates**: workspace UT-only line coverage must be `>= 95%`, and final-binary E2E line coverage must be `>= 80%`.
4. **Direct change coverage**: new or modified functional code must be reached by its corresponding UT and E2E, not merely inherit a passing workspace number from existing tests.
5. **Regression protection**: a user-visible bug fix must add a test that fails before the fix and passes afterward.

Pure documentation, comments, formatting, or build metadata changes with no runtime behavior change may omit E2E, but the delivery note must state why. A refactor qualifies only when it demonstrably preserves behavior; otherwise it follows the feature-change policy.

E2E does not replace UT, and Contract/Component tests must not be presented as E2E. A feature without its corresponding UT and E2E is incomplete.

## 2. Test locations

| Test type | Location | Entry point |
| --- | --- | --- |
| UT | `#[cfg(test)]` modules beside the owning `src/**/*.rs` code | `make test-unit` |
| Contract/Component | `crates/*/tests/*.rs` | `make test-contract` |
| Portable final-binary E2E | `crates/latte-code/tests/e2e/portable.rs` | `make test-e2e-portable` |
| Unix final-binary E2E | `crates/latte-code/tests/e2e/*.rs` registered by `e2e/mod.rs` | `make test-e2e-unix` |

Place E2E tests by user journey:

- `portable.rs`: cross-platform CLI, loopback Provider, and SQLite journeys that must execute on Linux, macOS, and Windows;
- `headless.rs`, `headless_matrix.rs`, and `runtime_convergence.rs`: CLI, configuration, JSON envelopes, multi-round read-only tools, cross-process convergence, and secret non-egress.
- `provider.rs`: HTTP statuses, retries, timeouts, SSE, stream fallback, and wire-compatibility failures.
- `tools.rs`, `permission_chain.rs`, and `runtime.rs`: final-binary tool matrices, aliases, permission chains, process supervision, and durable tool rounds.
- `recovery.rs`, `legacy.rs`, and `legacy_lifecycle_matrix.rs`: permission, cross-process resume, legacy-schema migration, verification, persistence, and recovery.
- `public_lifecycle_matrix.rs`, `public_boundary_matrix.rs`, and `v2_boundary_matrix.rs`: construct lifecycle or boundary fixtures through the public Engine authority, then accept the public projection through the final CLI/TUI.
- `tui.rs`, `interactive_matrix.rs`, `projection.rs`, and `ui.rs`: real PTY, keyboard protocol, blocking cards, interactive streams, projection, cancellation, and terminal restoration.
- `support.rs`: `Scenario`, `ScriptedProvider`, `PtySession`, bounded waits, and process cleanup.
- `mod.rs`: module registration only; do not accumulate scenarios here.

Add a scenario to the closest existing file. A scenario belongs in `portable.rs` only when its entire behavior is supported on all three platforms. PTY, Unix signals/process groups, symlink semantics, executable verification, and any other Unix-only assumption belong in the Unix suite. Create a new Unix module only for a stable new user-journey category, and register it in `e2e/mod.rs`.

Use a public Engine fixture only to create a valid lifecycle state that the current final binary cannot create through a command. The fixture must call public authority APIs and must not write private SQLite tables; a fresh final CLI/TUI process and its user-visible output must still provide the acceptance result. A legacy-schema migration fixture may create historical schema data only for compatibility scenarios and must not bypass current authority rules.

## 3. What qualifies as E2E

A Latte Code E2E test must:

- launch Cargo's final binary through `env!("CARGO_BIN_EXE_latte-code")`;
- use an isolated temporary Git workspace, HOME, configuration, and SQLite database;
- cross the real CLI/TUI composition root instead of calling an internal service or Provider trait;
- send Provider scenarios through loopback HTTP/SSE and the production adapter, serializer, and parser;
- run TUI scenarios in a real PTY and verify terminal lifecycle;
- judge results only through public output, durable projections, filesystem results, Provider requests, or process state;
- use explicit deadlines and terminate plus reap child process groups on failure and Drop paths.

The following are not product E2E tests:

- direct reducer, parser, runtime, or mock Provider trait calls;
- constructing a service without crossing the final binary;
- accessing a real Provider or public network, or consuming a developer API key;
- using `#[ignore]`, conditional skips, or manual-only execution;
- asserting only that the process exited successfully without proving a user-observable result.

## 4. Authoring workflow

### 4.1 Start with an acceptance matrix

Write a minimal matrix before implementation:

| Dimension | Required question |
| --- | --- |
| happy path | What does the user see, and what is the durable state? |
| rejection | On deny, invalid input, or bad configuration, what must never happen? |
| interruption | After timeout, cancellation, or process exit, how do state and children converge? |
| durability | Does a new process observe the same result after reopen? |
| security | Which output, request, and persistence surfaces could carry a secret? |
| exactness | Are approval, effect, tool result, and Provider re-entry each consumed exactly once? |

Every feature needs at least one E2E test for its primary user journey. Permission, security, recovery, and verification behavior also require corresponding negative paths.

### 4.2 Add the lowest-responsibility UT first

UT should directly cover:

- valid input;
- boundary values;
- invalid input and typed errors;
- state invariants;
- negative security assertions;
- the smallest bug reproduction.

Do not put SQLite, sockets, real child processes, or PTYs into UT merely to raise coverage. Those belong to Contract/Component or E2E.

### 4.3 Select the platform boundary

Prefer the portable suite when the journey can be proved through CLI JSON, SQLite, and loopback HTTP alone. Portable Provider scenarios must terminate before process verification on Windows—for example with a durable input request or a typed terminal Provider failure—because non-Unix process supervision deliberately fails closed. Never hide a portable target failure with `cfg`, an ignored test, or a runtime skip.

Use the Unix suite when the behavior itself requires PTY, signals, process groups, symlinks, or executable verification. The following completion example is therefore Unix-only because it executes `/usr/bin/true` as verification.

### 4.4 Add the final-binary E2E

A basic headless scenario looks like this:

```rust
use super::support::{ProviderReply, Scenario, ScriptedProvider, json};

#[test]
fn feature_name_describes_user_visible_outcome() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::completion("done"),
    ]);

    let output = scenario.output(&["--json", "run", "perform task"], |command| {
        scenario.configure_provider(
            command,
            provider.endpoint(),
            r#"["/usr/bin/true"]"#,
            "test-secret",
        );
    });

    assert!(output.status.success());
    assert_eq!(json(&output)["status"], "completed");
    provider.assert_consumed();
    assert_eq!(provider.requests().len(), 1);
}
```

This is only a starting point. A real scenario must also assert feature-specific durable state, side effects, negative outcomes, and security boundaries.

## 5. Harness rules

### 5.1 Scenario

Use `Scenario` for every isolated environment. It provides:

- a temporary workspace and `.git` root;
- an isolated HOME;
- removal of Provider and verification environment variables that could contaminate the test;
- `CARGO_BIN_EXE_latte-code` execution;
- bounded final-binary execution;
- a unique child `LLVM_PROFILE_FILE` during coverage runs.

Never depend on the developer's current directory, real HOME, existing `.latte` state, or global system configuration.

### 5.2 ScriptedProvider

Provider E2E uses `ScriptedProvider`:

- declare every expected response in order;
- use `wait_for_calls` as an explicit network-event barrier;
- inspect method, path, headers, model, messages, tools, and tool results through `requests()`;
- use `assert_consumed()` to catch missing responses and unexpected extra requests;
- verify that a tool result reaches the next production Provider request.

Do not replace wire-contract assertions with “the mock was called,” and never let fixtures silently succeed on an unexpected request.

### 5.3 PTY

TUI E2E uses `PtySession`:

- wait for explicit TUI readiness or rendered text before sending input;
- use real Crossterm key encodings;
- for permission and reconciliation, prove inert keys leave durable state unchanged before sending the confirmation key;
- verify paired restoration of alternate screen, keyboard enhancement, and related modes after exit;
- keep the reader draining through EOF instead of stopping while waiting for child exit;
- terminate the entire process group and reap children on Drop and timeout paths.

Fixed sleeps must not prove readiness or ordering. A documented fixed wait is allowed only when the product protocol itself defines a time window, such as the double-`Ctrl+C` debounce window.

## 6. Required assertions

Choose and combine the applicable assertions below; an exit-code assertion alone is insufficient:

- CLI exit code plus JSON version, status, and error code;
- thread/run lifecycle and pending request;
- exact effect state and single-use approval;
- exactly-once file mutation, with no mutation on deny or failure;
- verification evidence and handoff;
- Provider request count, order, and tool result;
- absence of the raw secret value from stdout, stderr, Provider bodies, transcripts, and SQLite;
- absence of the process group after timeout or cancellation;
- uncertain execution becomes `Unknown` and is never guessed successful or retried automatically;
- inert protected TUI keys, exact confirmation keys, and terminal-mode restoration.

Failure messages should include redacted stdout, stderr, or PTY transcripts so local and CI failures remain diagnosable.

## 7. The UT 95% / E2E 80% coverage policy

### 7.1 Measurement

UT coverage runs crate-local lib/bin tests only and excludes Contract, E2E, and documentation tests. E2E coverage runs only the portable and Unix final-binary targets. Each profile is cleaned and collected independently:

```bash
make coverage-unit # --lib --bins --fail-under-lines 95
make coverage-e2e  # --test e2e_portable --test e2e_unix --fail-under-lines 80
```

Generate HTML to inspect uncovered lines:

```bash
cargo llvm-cov clean --workspace
cargo llvm-cov --workspace --all-features --lib --bins --html
```

Lines reached only by E2E do not count toward the 95% UT requirement. UT, Contract, or tests that directly call internal APIs do not count toward the 80% E2E requirement. Do not manufacture compliance by expanding exclusions, deleting assertions, moving tests between layers, or measuring only convenient packages.

### 7.2 Required gates

The three coverage gates are independent, and completion requires all three:

1. `make coverage-unit`: workspace UT-only lines `>= 95%`.
2. `make coverage-e2e`: final-binary E2E lines `>= 80%`.
3. `make coverage-total`: all-target lines `>= 90%`.

`make coverage` runs the three gates serially and cleans the profile before each one so hits from one layer cannot contaminate another. In addition to keeping the workspace gates green, new or modified behavior must directly cover its success, boundary, typed-failure, and applicable safety-negative paths.

Review evidence must include the UT-only summary and an HTML inspection of new or changed executable lines. If tooling does not calculate diff coverage automatically, inspect touched functional lines individually; missing data is not a pass.

## 8. Naming and stability

- Name tests after the user journey and result, for example `write_file_deny_never_mutates_and_never_reenters_the_provider`.
- Avoid names such as `test_1` or `happy_path` that omit behavior.
- Give every test its own Scenario; do not share ports, HOME, SQLite, or mutable global state.
- Use events, public projections, file appearance, or Provider calls as barriers.
- Bound every wait and emit sufficient evidence on failure.
- Required E2E must not use `#[ignore]`, conditional skips, real networks, or real credentials.
- Before promotion to required, a portable E2E should pass ten consecutive runs on Linux, macOS, and Windows without a flake; a Unix E2E should do so on Linux and macOS.

## 9. Completion checklist

A feature is complete only when every applicable item is satisfied:

- [ ] The acceptance matrix states the happy path and negative paths.
- [ ] The lowest responsible module has corresponding UT.
- [ ] Workspace UT-only line coverage is at least 95%.
- [ ] Workspace final-binary E2E line coverage is at least 80%.
- [ ] New or modified functional code is reached directly by corresponding UT and E2E.
- [ ] At least one final-binary E2E covers the primary user journey.
- [ ] Permission, security, recovery, and verification behavior has explicit negative assertions.
- [ ] E2E does not depend on public networking, real keys, fixed sleeps, or `#[ignore]`.
- [ ] Child/process-group, PTY, and Provider fixtures clean up on failure.
- [ ] `make test-unit`, `make test-contract`, and the applicable portable/Unix E2E targets pass.
- [ ] `make coverage` and `make ci` pass.
- [ ] English and Chinese behavior documentation is synchronized.
