# Latte Code UT / E2E Testing Gate Design

> Status: the layered framework, three independent coverage jobs, and a fail-closed `PR Gate` workflow are implemented. Required-check enforcement is not active until the GitHub branch-protection/ruleset setting is actually configured. E2E-H-001 through H-008, H-010/H-011, and E2E-T-001 through T-008 are present. H-009 covers the public recovery semantics but still lacks a real kill barrier; later gates remain planned here.
>
> Baseline: 2026-07-15, current working tree, measured by `make ci` and three independent llvm-cov profiles.
>
> 中文版本：[UT / E2E 测试卡点](../../zh-CN/design/testing-gates.md)。

## 1. Decision

Latte Code's merge gate must be more precise than “run `cargo test` once.” The target model has three layers:

1. **UT**: prove rules inside one module quickly and deterministically;
2. **Contract / Component**: prove crate boundaries plus SQLite, filesystem, process, and Provider adapter contracts;
3. **E2E**: enter through the final `latte-code` binary and verify user-observable results through real configuration, persistence, a mock Provider, and a PTY.

UT and E2E are the two primary gates. The Contract layer does not broaden product scope; it correctly classifies existing database, HTTP, and process tests that are currently described as UT. Without this layer, UT becomes progressively slower and E2E failures do not identify the responsible boundary.

Final rules:

- Every PR must pass UT, Contract, P0 E2E, and three independent line-coverage gates: UT-only >= 95%, final-binary E2E >= 90%, and all-target >= 90%. New or modified functional code must also be reached directly by its corresponding UT and E2E.
- Blocking CI must not contact a real Provider, depend on the public network, or consume real API keys.
- Linux, macOS, and Windows run check, Clippy, UT, Contract, portable final-binary E2E, and release-build gates on every PR. Linux and macOS additionally run the Unix PTY/process E2E suite. This validates every applicable surface without claiming unsupported Windows process supervision.
- Safety, permission, recovery, and verification behavior require explicit positive and negative assertions; coverage alone is insufficient.
- New gates move through `draft -> shadow -> required`; planned protection must never be reported as active protection.

## 2. Current baseline

### 2.1 Existing capabilities

The repository and CI already provide:

- `make ci`;
- `cargo fmt`, Clippy, Rustdoc, and `cargo deny`;
- `cargo test --workspace --all-targets --all-features`;
- `cargo llvm-cov ... --fail-under-lines 90`;
- native Linux, macOS, and Windows check, Clippy, UT, Contract, portable E2E, and release builds, plus Linux/macOS Unix PTY/process E2E;
- actionlint, ShellCheck, Rust 1.93 MSRV, dependency audit, locked dependency resolution, three independent coverage jobs, and a `PR Gate` aggregating every required job;
- final-binary CLI tests, loopback mock HTTP, and real PTY tests;
- real SQLite/temp workspaces, process groups, permission, recovery, and TUI reducer tests.

Measured baseline on 2026-07-15:

| Metric | 2026-07-15 baseline | Notes |
| --- | ---: | --- |
| Cargo-discovered tests | 353 | 245 crate-local, 15 contract, 3 portable E2E, 75 Unix E2E, and 15 documentation tests |
| Crate-local tests | 245 | Independent `--lib --bins` profile; existing inline tests still contain some component behavior to purify |
| Contract / component | 15 | Five contract targets protected by the inventory check |
| Final-binary E2E | 78 | Three portable CLI/Provider/SQLite scenarios on all platforms plus 75 Unix headless, Provider, tool/recovery, public-boundary, and real-PTY scenarios |
| UT-only line coverage | 95.05% | Current macOS working tree measured `26837 / 28234` from `make coverage-unit` |
| Final-binary E2E line coverage | 80.78% | Current macOS working tree measured `10687 / 13230` across both E2E targets |
| Total line coverage | 96.64% | Current macOS working tree measured `27286 / 28234` from `make coverage-total` |

### 2.2 Current remaining gaps

1. **UT still needs purification**: `test-unit` is independently visible, but existing inline tests still include component behavior using SQLite, sockets, child processes, and real signals.
2. **The real crash barrier still has a gap**: public Engine authority plus final CLI/TUI covers `Started -> Unknown -> reconcile`, but H-009 must still terminate the final binary after a real effect reaches Started through an external barrier.
3. **The Provider fidelity layer is not implemented**: the scripted Provider proves product behavior, while cassette replay and a live canary remain planned.
4. **Release jobs build without starting artifacts**: file existence does not prove that the artifact starts, resolves configuration, or emits stable JSON.
5. **Failure evidence is not packaged as an artifact**: the harness now holds stdout, stderr, PTY transcripts, Provider request logs, and final projections, but CI does not upload them together on failure yet.
6. **Required activation remains an external action**: the workflow has the full three-platform matrix and a fail-closed `PR Gate`, but each applicable platform still needs ten consecutive flake-free runs and GitHub branch protection/rulesets must actually require `PR Gate`. Repository files cannot prove that remote setting is active.

## 3. Test-layer boundaries

### 3.1 UT

UT proves one module or pure rule. Failures should be direct, parallel execution safe, and independent of environmental timing.

Allowed in UT:

- pure data construction and deterministic fake clocks/IDs;
- reducers, parsers, serializers, policy, and redaction;
- Ratatui `TestBackend`;
- small in-memory fakes that do not cross the final binary.

Not allowed in UT:

- starting the final `latte-code` binary;
- TCP listeners or any external network;
- PTYs, shells, child processes, or real signals;
- wall-clock sleeps used for ordering;
- SQLite reopen, crash recovery, or multi-process contention.

`tempfile` is allowed for pure path/file-content algorithms. A test becomes Contract / Component once it depends on real atomic rename, symlink, SQLite, process-group, or OS-permission semantics.

### 3.2 Contract / Component

This layer verifies real infrastructure and public boundaries without requiring entry through the final binary:

- `latte-core` byte-compatible protocols, transition tables, and compile-fail authority boundaries;
- `latte-engine` SQLite migration/reopen, lease fencing, effect ledger, filesystem containment, symlinks, and process groups;
- `latte-headless` scripted HTTP/SSE, redacted cassette replay, Provider registry, agent loop, verification, and resume;
- public crate API lifecycles and the workspace dependency matrix;
- Markdown, link, and repository-structure checks.

Real SQLite, temp workspaces, loopback sockets, child processes, and signals are allowed, but every test needs a deadline, an isolated directory, and complete cleanup.

### 3.3 E2E

An E2E test must satisfy all of the following:

- Enter through `CARGO_BIN_EXE_latte-code` or a freshly built release artifact.
- Use real configuration layering, SQLite persistence, and process boundaries.
- Use a deterministic Provider harness on local `127.0.0.1`: behavioral scenarios default to a scripted server, while protocol-fidelity scenarios may use a redacted cassette replay server.
- Use a real PTY and the final binary for TUI scenarios.
- Prefer assertions against stdout/stderr, exit codes, CLI JSON, public Engine projections, and final workspace state.
- Never write private SQLite tables to manufacture success. A read-only probe is allowed only for thread projection evidence not yet exposed by the CLI.
- Give every scenario its own HOME, workspace, database, port, and secret sentinel.

A Provider E2E test is defined by running the production Provider adapter, serializer, HTTP/SSE parser, agent loop, and final binary through the local network boundary; it does not need to contact a real remote service. A real-Provider canary may be manual or scheduled and non-blocking. It cannot replace a deterministic merge gate.

## 4. Gate model

| Gate | Trigger | Blocking | Content | Target budget |
| --- | --- | --- | --- | ---: |
| G0 Static | Every PR | Yes | fmt, three-platform check/Clippy, Rustdoc, actionlint, ShellCheck, MSRV, architecture/repo checks, dependency audit | 3 min |
| G1 UT | Every PR | Yes | All pure crate-local UT; workspace UT-only line coverage >= 95% | 2 min |
| G2 Contract | Every PR | Yes | SQLite, FS, process, scripted/cassette Provider, public API, doc tests | 5 min |
| G3 E2E | Every PR | Yes | Portable final-binary CLI/Provider/SQLite on all three platforms; complete headless + TUI/PTY on Linux/macOS; independent line coverage >= 90% | 5 min/OS |
| G4 Coverage | Every PR | Yes | workspace/all-features/all-targets, total lines >= 90% | 5 min |
| G5 Release smoke | Release workflow | Yes | Start each release artifact, help, JSON list | 2 min/OS |
| Extended | Nightly/manual | No for PRs | Repetition, long cancellation, boundary-size matrix, live canary | 15 min |

Budgets are upper bounds, not time to consume with `sleep`. Jobs should run in parallel. A test has a default ten-second deadline; scenarios that explicitly verify timeout or cancellation may declare a longer bound.

### 4.1 PR aggregation and concurrency semantics

`.github/workflows/ci.yml` runs only for `main` pushes, PRs targeting `main` (`opened`, `synchronize`, `reopened`, `edited`, and `ready_for_review`), `merge_group`, and manual dispatch. The `edited` event prevents a missing required check when a PR is retargeted to `main`. Every underlying job has a stable name and timeout; required paths have no path filter, conditional skip, or automatic rerun:

- Linux, macOS, and Windows independently run check, Clippy, UT, Contract, portable E2E, and release build. Linux and macOS additionally run the Unix PTY/process E2E target.
- Repository quality runs fmt, Rustdoc, inventory, actionlint, and ShellCheck; Rust 1.93 MSRV, documentation tests, and dependency audit remain independently visible.
- `Coverage - UT (95%)`, `Coverage - E2E (90%)`, and `Coverage - total (90%)` are separate jobs and cannot compensate for one another.
- The stable `PR Gate` status uses job-level `always()` to wait for all 14 required jobs, then explicitly requires every `needs.<job>.result` to equal `success`. A failure, cancellation, or skip fails the gate.
- A new PR commit cancels the older run for that PR. `main` and merge-queue runs are never cancelled, preserving trunk and queue evidence.
- Three-platform release builds are dependencies of `PR Gate`. They prove artifact compilation and upload, but are not yet G5 release smoke.

Branch protection or a ruleset should require only the stable `PR Gate` status while leaving underlying statuses visible for diagnosis. The workflow cannot create that remote setting; PR blocking is active only after GitHub is actually configured to require the check.

### 4.2 Implemented command surface

The Makefile now provides these stable entry points:

```text
make test-unit
make test-contract
make test-e2e-portable
make test-e2e-unix
make test-e2e
make lint-ci
make test-doc
make test-all
make coverage
make ci
```

Target layout:

```text
crates/latte-code/tests/
  contract.rs
  contract/
    cli.rs
  e2e_portable.rs
  e2e_unix.rs
  e2e/
    portable.rs
    support.rs
    headless.rs
    provider.rs
    tools.rs
    tui.rs
    recovery.rs
  architecture.rs
  markdown_links.rs
```

- `test-unit`: `cargo test --workspace --lib --bins --all-features --locked`;
- `test-contract`: run each crate's public contract/component targets;
- `test-e2e-portable`: run three cross-platform CLI/Provider/SQLite journeys on Linux, macOS, and Windows;
- `test-e2e-unix`: run the 75 Unix headless, recovery, process, and PTY scenarios on Linux and macOS;
- `test-e2e`: compose both E2E targets on Unix;
- `lint-ci`: run actionlint and ShellCheck locally, using pinned containers when local tools are unavailable;
- `test-doc`: run workspace doc tests;
- `test-all`: compose every layer above;
- `test-inventory` (implemented as a repo check): ensure every new `tests/*.rs` target belongs to a known layer and cannot be silently omitted.

During migration, existing inline tests are initially classified as `crate-local`. Tests that use sockets, SQLite reopen, child processes, or real signals then move to component targets. Until that migration completes, `test-unit` must not be described as satisfying the pure-UT definition completely.

## 5. Required UT matrix

| ID | Crate | Required rule | Key assertions |
| --- | --- | --- | --- |
| UT-COR-001 | `latte-core` | run/thread transition tables | Legal/illegal transitions for every state, monotonic revision, immutable completion |
| UT-COR-002 | `latte-core` | v1/v2 protocol serialization | Version, field names, fail-closed invalid input, byte compatibility |
| UT-COR-003 | `latte-core` | redaction and bounds | No secret/control retention, safe structure preserved, bounded text |
| UT-ENG-001 | `latte-engine` | policy/classification | argv-first, shell/high-risk, deny precedence, no implicit allow |
| UT-ENG-002 | `latte-engine` | effect/permission validation | Revision, lease token, digest, single use, no partial commit on error |
| UT-ENG-003 | `latte-engine` | path/manifest algorithms | Component encoding, non-UTF-8 fail-closed behavior, globs, output caps |
| UT-HDL-001 | `latte-headless` | Provider parser/SSE state machine | Chunking, CRLF, tool calls, fallback, retry classification, cancellation |
| UT-HDL-002 | `latte-headless` | history/budget/redaction | Order, minimum retained segment, oversized failure, no secret in history |
| UT-HDL-003 | `latte-headless` | registry/binding | Stable aliases, exact scope/generation, validation before secret lookup |
| UT-TUI-001 | `latte-tui` | reducer action matrix | Every active state emits typed actions only |
| UT-TUI-002 | `latte-tui` | protected keys | Enter/Shift+Enter never approve permission or reconciliation |
| UT-TUI-003 | `latte-tui` | render/layout | Three size tiers, Unicode grapheme/display width, fixed blocking cards |
| UT-CLI-001 | `latte-code` | config merge/root discovery | defaults -> HOME -> workspace, array/scalar replacement, nearest Git root |
| UT-CLI-002 | `latte-code` | parser/exit/JSON mapping | All command shapes, stable codes, versioned envelope, redacted errors |

In addition to line coverage, the UT gate requires:

- state machine, permission, secret, and reconciliation tests to include negative assertions;
- no `#[ignore]` for P0 behavior;
- no automatic rerun that turns a first failure into success;
- controlled time and IDs; sleeps cannot prove ordering;
- every bug fix to add a regression test at the lowest responsible layer that fails before the fix and passes afterward.

## 6. E2E scenario matrix

### 6.1 Headless / final binary

| ID | Priority | Scenario | Required evidence | Current status |
| --- | --- | --- | --- | --- |
| E2E-H-001 | P0 | Start without configuration from a nested directory and run `--json list` | Exit 0, versioned JSON, DB at Git root | Present |
| E2E-H-002 | P0 | Scripted Provider completes a read-only task | Request contract, completed result, no mutation/permission | Present |
| E2E-H-003 | P0 | Deny a mutation request | File unchanged, effect failed, Provider not re-entered incorrectly | Present |
| E2E-H-004 | P0 | Allow mutation across process resume and pass verification | File changes once, single-use approval, complete evidence/handoff | Present |
| E2E-H-005 | P0 | Verification fails after mutation | Never completed, typed failure, auditable change and evidence | Present |
| E2E-H-006 | P0 | HOME/workspace configuration override | Workspace wins; relative DB path is still workspace-relative | Present |
| E2E-H-007 | P0 | Secret non-egress | Sentinel absent from stdout/stderr/JSON/transcript/persistence | Present |
| E2E-H-008 | P0 | Child-process timeout/cancellation | Entire process group gone, one terminal observation, no orphan | Present |
| E2E-H-009 | P0 | Kill during `Started`, restart into Unknown, then reconcile | No guessed success, no automatic retry, exact child/effect terminalized | Partial: public recovery semantics exist; real kill barrier is missing |
| E2E-H-010 | P1 | Provider malformed/timeout/retry matrix | Bounded retry, invalid success not retried, typed errors | Present |
| E2E-H-011 | P1 | Legacy v1 `show/list/resume` | Compatible envelope/exit codes, no thread backfill | Present |
| E2E-H-012 | P1 | Cassette-replay tool loop for every supported wire protocol | Recorded requests consumed exactly in order, tool result returned, final answer, no public network | Missing |

### 6.2 TUI / real PTY

| ID | Priority | Scenario | Required evidence | Current status |
| --- | --- | --- | --- | --- |
| E2E-T-001 | P0 | Start and explicitly exit | Raw/alternate/keyboard/paste modes restored in pairs | Present |
| E2E-T-002 | P0 | Shift+Enter multiline, Enter single submit | Exactly one durable user card with exact content | Present |
| E2E-T-003 | P0 | Permission card | Enter/Shift+Enter inert; only exact Ctrl+A or deny key acts | Present |
| E2E-T-004 | P0 | Ctrl+C during active run, then Ctrl+C again | Cancel task first, confirm exit second, terminal restored | Present |
| E2E-T-005 | P0 | Unknown reconciliation | Ctrl+R opens, Enter inert, Ctrl+A confirms exact effect only | Present |
| E2E-T-006 | P1 | Input request | Input persists and resumes once, never confused with permission | Present |
| E2E-T-007 | P1 | Resize, narrow terminal, Unicode, bracketed paste | No panic/lost input; blocking surface remains available | Present |
| E2E-T-008 | P1 | Provider configuration/transport failure | Prompt persisted and restored; secret not displayed | Present |

### 6.3 Release artifacts

| ID | Priority | Scenario | Platforms |
| --- | --- | --- | --- |
| E2E-R-001 | P0 | Release binary `--help` | Linux/macOS/Windows |
| E2E-R-002 | P0 | `--json list` under temporary HOME/workspace | Linux/macOS/Windows |
| E2E-R-003 | P0 | Start under PTY, exit, and restore | Linux/macOS |

Windows still does not claim safe mutation/process supervision. The current release job proves only a successful build; the startup smoke scenarios above are not active yet.

## 7. E2E harness constraints

### 7.1 `Scenario` fixture

The shared fixture holds at least:

- isolated `TempDir` workspace and HOME;
- `.git` root, configuration, and database path;
- unique secret sentinel;
- scripted Provider and cassette replay server;
- child/PTY handles;
- stdout, stderr, terminal transcript, and Provider request log;
- deadline and cleanup guard.

Fixture drop must terminate and reap every child/process group. Under coverage, every child keeps an `LLVM_PROFILE_FILE` containing `%p`. The PTY reader must continue draining through EOF instead of stopping while it waits for child exit.

### 7.2 Three-layer Provider verification model

| Mechanism | Network boundary | Primary proof | Gate |
| --- | --- | --- | --- |
| Scripted Provider | Local loopback with test-generated responses | Behavior, errors, timing, retry, cancellation | G2/G3 required |
| Cassette Replay Provider | Local loopback replaying redacted real interactions | Production protocol stack compatibility with real wire shapes | G2 required; at least one G3 scenario per supported wire protocol |
| Live Provider Canary | Real remote service | Credentials, provider availability, and remote protocol drift | Extended, non-blocking for PRs |

The layers are complementary. The Scripted Provider supplies controlled faults and state paths, cassette replay supplies real-protocol fidelity, and the live canary only detects drift outside committed fixtures.

#### 7.2.1 Scripted Provider

A mock server must do more than return one fixed 200 response. Each step contains:

```text
expected request predicate
response / streamed chunks / connection action
maximum call count
next step
```

It verifies:

- Authorization is sent only to the configured endpoint;
- model, messages, tools, tool order, and aliases are exact;
- resumed history contains only persistable data;
- unexpected extra requests fail immediately;
- retry, fallback, and cancellation counts are bounded.

Request assertions use semantic inspectors rather than only comparing a whole raw JSON snapshot. The harness exposes at least messages, tools, tool results, model, Authorization destination, request order, and exact call count, plus a published readiness barrier such as `wait_for_call(n)`.

#### 7.2.2 Cassette replay

A cassette records only the Provider HTTP/SSE transport boundary; the production Provider adapter, serializer, parser, request executor, and agent loop remain real. Recording and replay follow these rules:

- Recording requires an explicit local or manual command. CI is always replay-only, fails closed when a fixture is missing, and never falls back to the public network.
- Authorization, API keys, tokens, account/user identifiers, and secret-bearing query/header/body values are redacted. Suspected secrets prevent the cassette from being written.
- Non-semantic dynamic fields such as request IDs and timestamps are normalized. Model, messages, tools, tool results, and protocol fields must not be loosely ignored.
- Interactions are claimed in request-start order. Request mismatch, duplicate concurrent consumption, cursor exhaustion, or unconsumed interactions at test end all fail.
- Fixtures are versioned by wire protocol and scenario. Every update explains the upstream protocol change or production serialization change in review.
- A cassette proves only compatibility for its recorded path. Malformed streams, hangs, disconnects, 429/500 responses, retries, and cancellation remain Scripted Provider responsibilities.

The same replay server must support both G2 crate-level Provider contracts and G3 execution by the final `latte-code` binary through endpoint configuration. Only the latter closes the product-boundary loop across configuration, persistence, agent loop, and Provider protocol.

#### 7.2.3 Live Provider canary

- Run only a minimal read-only or side-effect-free tool loop by default; never allow implicit mutation.
- Obtain credentials only from a CI secret store or an explicitly provided developer environment, with token, cost, call-count, and wall-clock limits.
- Record the provider, protocol, status code, and redacted diagnostics on failure, but never block a PR or automatically rewrite a cassette.
- Treat an isolated canary failure as a remote-service, credential, or network signal. Suspect a code regression only when scripted or cassette gates fail as well.

### 7.3 Waiting and failure evidence

- Do not use fixed sleeps for readiness. Poll an explicit terminal marker, `wait_for_call(n)`, cassette interaction consumption, CLI JSON response, or public projection.
- Every poll has a deadline and prints current evidence on timeout.
- CI failures upload or print stdout, stderr, PTY transcript, mock request log, final workspace tree, and redacted projection.
- A secret-sentinel failure reports only the affected surface and location; it must not print the secret again.

### 7.4 Fault injection without a production backdoor

Do not add hidden environment variables or test-only subcommands to the release binary.

`Started -> Unknown` can be controlled through an externally observable barrier:

1. The scripted Provider requests a supervised process.
2. The process creates a workspace barrier, records its PID/PGID, and remains active.
3. The E2E runner waits for the barrier and public `Started` projection.
4. The runner sends SIGKILL to the final `latte-code` process.
5. The runner restarts the final binary with the same workspace/database.
6. The test asserts Unknown, no tool success, and no automatic retry.
7. The test completes exact reconciliation through CLI/TUI.

The fixture's finally/Drop path must independently terminate the PGID and wait for it to disappear so killing the parent cannot leave an orphan. This controls the crash window without adding a test control plane to the production protocol.

### 7.5 E2E naming constraints

- A test that calls only a mock Provider trait and bypasses the production adapter/serializer/parser is a Component test and must not be named Provider E2E.
- Running the production Provider stack through a loopback HTTP/SSE boundary is a Provider Contract. It becomes product E2E only when entered through the final binary.
- PTY/UI E2E proves user input, rendering, and terminal lifecycle. It cannot replace Provider E2E unless it completes a real Provider/tool loop.
- Tests using `#[ignore]`, conditional skip, or real API keys do not count toward required coverage and cannot prove that a P0 path is covered.

## 8. Coverage, flakes, and platforms

### 8.1 Coverage

- Workspace UT-only line coverage must be `>= 95%`, measured independently by `make coverage-unit`.
- Final-binary E2E line coverage must be `>= 90%`, measured independently by `make coverage-e2e`.
- E2E, Contract, and documentation-test hits do not count toward UT coverage. UT, Contract, or tests that directly call internal APIs do not count toward E2E coverage. Functional code must not be excluded to manufacture compliance.
- Keep the existing total line-coverage gate at `>= 90%`; never lower it.
- Produce crate/file reports in CI before recording stable baselines.
- Ratchet per-crate/file floors from measured baselines instead of inventing arbitrary numbers.
- Critical safety branches need explicit tests even when aggregate coverage does not fall.
- Branch coverage currently has no data. Collect and observe it before making it blocking.

See the [E2E authoring guide](../testing/e2e-authoring-guide.md) for the authoring and acceptance workflow.

### 8.2 Flakes

- Required gates do not rerun automatically; the first failure fails the gate.
- Before becoming required, a portable E2E must pass at least ten consecutive runs on Linux, macOS, and Windows with zero flakes; a Unix E2E must do so on Linux and macOS.
- Quarantine requires an issue, owner, expiry date, and replacement protection. Safety/permission/recovery P0 scenarios cannot be quarantined.
- Any hang is a test failure; waits are never unbounded.
- Increasing sleeps is not the default flake fix.

### 8.3 Platforms

- Linux/macOS: check, Clippy, UT, Contract, portable E2E, complete Unix headless/TUI E2E, and release build.
- Windows: check, Clippy, UT, Contract, portable final-binary E2E, and release build. Process supervision still fails closed and is not claimed as supported.
- Unix-only process/PTY scenarios use explicit `cfg(unix)` and run on both Linux and macOS.
- Documentation must not claim support for capabilities that are not tested on a platform.

## 9. Minimum tests by change type

| Change type | Minimum requirement |
| --- | --- |
| Pure parser/reducer/policy | Same-module UT including error/negative paths |
| Protocol/state schema | UT + byte/JSON contract + migration/compatibility contract |
| SQLite/effect/lease | Validator UT + reopen/fencing component + user-visible recovery E2E |
| Filesystem/process tool | Policy UT + real-OS component + allow/deny/cancel E2E |
| Provider protocol | Parser UT + scripted HTTP contract + redacted cassette replay + final-binary loopback E2E; real remote only as a non-blocking canary |
| CLI config/exit/JSON | UT + final-binary headless E2E |
| TUI key/reducer | Reducer UT + PTY E2E for protected actions |
| Pure TUI visual change | TestBackend UT; PTY E2E only when input, blocking actions, or terminal lifecycle changes |
| Bug fix | Regression at the lowest responsible layer; add E2E if the bug escaped to a user surface |

## 10. Phased rollout

### Phase 1: layering and visibility

1. Add `test-unit`, `test-contract`, `test-e2e`, `test-doc`, and `test-all`.
2. Split `cli.rs` into contract and `e2e` targets with a shared fixture.
3. Split GitHub Actions into visible UT, Contract, E2E, and three independent Coverage jobs, then aggregate them fail-closed under the stable `PR Gate` status.
4. Add an inventory check for integration targets.
5. Keep `make ci` as the single complete local gate.

Completion status: repository implementation is complete. Test targets are split by layer and platform boundary and protected by the inventory check. Three-platform portable E2E, Linux/macOS Unix E2E, release builds, static analysis, MSRV, and all three coverage profiles run as independent jobs. `PR Gate` strictly aggregates all 14 required statuses, and `make ci` remains the complete local Unix gate. GitHub branch protection/rulesets must still be configured remotely to require `PR Gate`; this document does not claim that enforcement is active. Use the latest measured baseline above for current test and coverage counts.

### Phase 2: close P0 user journeys

Implement in this order:

1. headless mutation deny/allow plus verification pass/fail;
2. secret non-egress;
3. TUI protected permission keys;
4. Ctrl+C cancel/exit;
5. process timeout/cancellation and no-orphan evidence.

Completion criteria: E2E-H-001 through H-008 and E2E-T-001 through T-004 are required and pass ten consecutive runs on both Linux and macOS without a flake.

### Phase 3: crash recovery and release artifacts

1. External-barrier `Started -> kill -> Unknown`.
2. Headless/TUI reconciliation.
3. Release artifact smoke.
4. Cassette replay for every supported wire protocol.
5. Per-crate/file coverage baseline ratchet.
6. Nightly live Provider canary and extended matrix.

Completion criteria: E2E-H-009, E2E-H-012, E2E-T-005, and E2E-R-001 through R-003 are required; the release workflow proves more than file construction.

## 11. Gate activation criteria

A test or job moves from shadow to required only when all criteria hold:

1. It has a stable, documented, single-command local entry point.
2. It passes at least ten consecutive runs on every applicable target platform: Linux/macOS/Windows for portable E2E, or Linux/macOS for Unix E2E.
3. Timeout, child cleanup, and PTY drain are explicitly bounded.
4. Failures produce sufficient redacted evidence.
5. It does not depend on the public network, a real Provider, or developer-specific configuration.
6. Every required cassette is redacted and versioned, and CI is explicitly replay-only.
7. The remote GitHub branch-protection/ruleset configuration actually requires the stable `PR Gate` status.
8. `make ci` and cloud CI execute the same test set.

## 12. First-round implementation result

This implementation completes Phase 1, the Phase 2 scenario implementation, and additional public recovery boundaries:

- reorganized the test tree and added Scenario, strict scripted Provider, and Drop-safe PTY harnesses;
- added layered Makefile/CI entry points and integration-target inventory;
- split final-binary E2E into a three-platform portable target and a Linux/macOS Unix PTY/process target without dropping the existing 75 Unix scenarios;
- added three-platform check, Clippy, UT, Contract, portable E2E, and release-build jobs plus Rust 1.93 MSRV, actionlint, ShellCheck, and locked Cargo resolution;
- split CI coverage into three independent statuses and added the fail-closed `PR Gate`, PR cancellation semantics, and merge-queue trigger contract; remote required-check configuration remains pending;
- migrated existing tests without losing assertions; final-binary scenarios now cover configuration, Provider behavior, tools, permission chains, cross-process resume, verification, public lifecycle/boundary behavior, legacy migration, and real PTYs;
- the read-only tool loop exposed that v1 checkpoints returned file secrets to the Provider verbatim; tool results now reuse the common redactor before persistence and Provider re-entry, with a legacy-checkpoint normalization regression UT;
- kept required paths free of `#[ignore]` and conditional skips;
- after fixing one PTY flake caused by the test's read-only probe opening SQLite too early, the final E2E target passed ten consecutive macOS runs; CI must supply the corresponding ten-run Linux evidence;
- TUI reconciliation/input is implemented; the real H-009 kill barrier, cassette replay, and release smoke remain next-round gaps.

This first makes the test gates visible, accurate, and complete before adding each missing behavioral proof.

## 13. Feasibility evidence

The design builds on working test seams rather than hypothetical infrastructure:

- `cargo test --workspace --lib --bins --all-features -- --list` currently finds 245 crate-local tests, and the current UT-only profile reaches 95.05%.
- The independent contract and E2E targets contain 93 integration tests: 15 Contract, 3 portable final-binary E2E, and 75 Unix final-binary E2E tests.
- The 2026-07-15 macOS baseline measured 95.05% UT-only, 80.78% E2E, and 96.64% all-target coverage; it predates the 90% E2E floor.
- Final-binary execution, a loopback Provider, a real PTY, cross-process SQLite resume, and terminal-mode restoration already have reusable implementations.
- The Provider endpoint already targets a loopback harness, so cassette replay can reuse the same final-binary path without a production backdoor.
- The runtime already has component tests for process `Started`, Unknown, restart recovery, and reconciliation. The external-barrier E2E lifts existing semantics to the final-binary boundary and does not require a production backdoor.
- At that baseline, the complete local `make ci` gate passed under the then-current floors; each layer remains independently runnable, and every coverage profile is cleaned before collection.
