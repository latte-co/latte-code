# Latte Code documentation

- [Project README](../../README.md): install, CLI/TUI use, layered `latte-code.jsonc` provider configuration, safety, and development checks.
- [Capability roadmap](roadmap.md): dependency-ordered product-capability checklist; only implemented and automatically verified capabilities are checked.
- [Architecture](design/architecture-overview.md): implemented crate and runtime boundaries.
- [Global session and data storage](design/data-storage.md): implemented global state path, workspace-scoped Session catalog, and transient Provider-configuration failures, plus the proposed JSONL, recovery, and migration contract.
- [Slash commands](design/slash-commands.md): implemented built-in catalog and Session commands, plus the proposed composer popup, prompt-command extensions, and remaining typed actions.
- Agent harness design proposals: [asynchronous turn runner](design/agent-harness/asynchronous-turn-runner.md), [session storage and recovery](design/agent-harness/session-store-and-recovery.md), [effect authority and policy](design/agent-harness/effect-authority-and-policy.md), [extension and delegation](design/agent-harness/extensions-and-delegation.md), [events, projections, and replay](design/agent-harness/event-projection-and-replay.md), [TUI runtime contract](design/agent-harness/tui-runtime-contract.md), and [verification harness](design/agent-harness/verification-harness.md).
- [UT / E2E testing gates](design/testing-gates.md): test layers, blocking matrix, E2E scenarios, and phased rollout.
- [E2E authoring guide](testing/e2e-authoring-guide.md): feature-level UT/E2E policy, harness usage, assertion checklist, and the independent UT 95% / E2E 80% coverage measurements.
- [Chinese documentation](../zh-CN/README.md).

This tree documents the current Rust implementation and explicitly status-marked designs that are not fully implemented yet.
