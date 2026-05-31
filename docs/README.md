# Fluxcode Documentation

Fluxcode documentation is organized by language:

- [English (en-US)](./en-US/README.md)
- [中文 (zh-CN)](./zh-CN/README.md)

The current design posture is evolutionary: Fluxcode should first become a basic, working local-first code agent, then gradually introduce harness-native runtime structure as the implementation proves which abstractions are necessary.

Formal documentation in `docs/zh-CN/` and `docs/en-US/` should stay structurally aligned. The formal design entry is `design/architecture-overview.md` in each language. Current / near-term module designs live under `design/modules/`, while accepted long-term runtime evolution targets live under `design/runtime-evolution/`.

Each language tree is organized into:

- `design/modules/`: current / near-term module-level technical designs.
- `design/runtime-evolution/`: accepted long-term runtime evolution targets. These are not generic unaccepted proposals, but they also do not imply current `v0.1` implementation completeness.
- `proposals/`: not-yet-implemented proposals and idea documents that are not yet part of the current design set.
- `milestones/`: milestone targets and completed records.
- `research/`: research facts and cross-product comparisons.
