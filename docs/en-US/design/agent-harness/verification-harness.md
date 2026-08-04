# Verification Harness and Deterministic Testing

Status: **design proposal; not implemented.**

Chinese counterpart: [验证 Harness 与确定性测试](../../../zh-CN/design/agent-harness/verification-harness.md).

## 1. Decision

Harness is replaceable production-boundary dependency and test fixture, not a second agent runtime or a test backdoor around `latte-engine`. It drives final `latte-code` binary, scripted Provider, temporary global state home, SQLite, event subscription, and real PTY while remaining network-free, credential-free, bounded, and cleanable.

Independent coverage floors remain UT 95%, final-binary E2E 90%, and all-target 90%. This document defines the substrate for those floors; existing testing gates and E2E authoring guide remain delivery rules.

## 2. Replaceable dependencies

Production composition root injects Provider transport/stream and controllable request recorder; clock, random ID, cancellation/timeout scheduler; runtime event source and snapshot loader; state-home/path resolver; terminal input/output/backend; and process supervisor with real implementation only on supported platforms, through traits or explicit constructors.

Test doubles obey the same production schema, policy, and capacity. Product code cannot accept wider commands or skip policy under `cfg(test)`. Fixtures use only loopback `127.0.0.1` Provider and strictly assert model, messages, tools, request order, and exact count. Protocol-fidelity scenarios may use redacted cassette, never Authorization or secret.

## 3. Test layers

| Layer | Goal | Principal doubles |
| --- | --- | --- |
| Core/Engine UT | state machine, policy, storage, Effect, recovery | fake clock/ID, temporary SQLite, constrained workspace |
| Headless UT | turn runner, history, Provider/tool coordination | scripted Provider, event collector, fake timer |
| TUI UT/render | reducer and cell-level output | fake event source, TestBackend, VT100 |
| Final-binary E2E | user-visible cross-crate journey | loopback Provider, temporary home, real binary/PTY |

Every behavioral change needs lowest-responsibility UT and at least one final-binary E2E. Portable Provider/SQLite/CLI journeys run on Linux, macOS, and Windows. PTY, Unix signal, and process-group journeys run in Unix jobs only; Windows never pretends to support them.

## 4. Failure evidence, cleanup, and acceptance

Every scenario bounds wall-clock, output, request count, and resource count. On failure fixture gathers redacted stdout/stderr, PTY transcript, Provider request log, snapshot, and event summary; on either outcome it closes listener, child process group, database, and temporary directory. Readiness uses Provider call, event, rendered text, or child exit, never fixed sleep.

- UT proves every fake and production boundary follows the same schema, policy, and capacity.
- E2E proves timeout cleanup of child process and loopback server and no repeat-run port/state leak.
- CI fails closed for three-platform check/Clippy/UT/contract/portable E2E/release build, Unix PTY/process E2E, independent coverage, and documentation links.
