# Latte Code single-session TUI prototypes

This directory contains design-only HTML prototypes for Latte Code's first single-session TUI. It does not modify the Rust implementation.

Open [`index.html`](./index.html) directly, then use the state controls or number keys `1`–`4`.

The rejected line-art logo experiment has been removed from the main prototype. [`logo-study.html`](./logo-study.html) keeps the next iteration isolated: a source-derived half-block pixel mark in three palette treatments.

## Prototype states

- `#idle`: product identity, environment contract, and first prompt.
- `#working`: user request, compact thought indicator, grouped tool activity, streaming status, and composer.
- `#permission`: a high-priority permission card with an explicit operation summary. Enter remains inert.
- `#complete`: final response, changed files, and verification evidence.

Direct links also work with a query parameter, for example `index.html?state=permission`.

## Design boundaries

- One focused session owns the viewport.
- All four prototype states share one bounded 4:3 terminal viewport.
- There is no session sidebar, repository explorer, MCP dashboard, or workbench navigation.
- User prompts are visually separated; assistant text remains lightweight.
- Tool calls and results are grouped into one expandable activity tree.
- Every empty composer starts at one line and grows only when multiline input needs more room.
- Empty-state environment details share the brand row and stack only on narrow viewports.
- The composer owns printable input. Navigation shortcuts belong to a separate mode.
- Permission and reconciliation states interrupt the flow with explicit, non-Enter confirmation.
- Empty-state branding collapses to a compact header after the first prompt.

## Mapping to the current projection

| Current data | Prototype presentation |
| --- | --- |
| `TranscriptKind::User` | highlighted user request |
| `TranscriptKind::Assistant` | assistant prose or streaming line |
| `ToolCall` + matching `ToolResult` | one grouped, expandable tool row |
| `ThreadTransientProgress` | active tool status or assistant stream |
| `ThreadPendingRequest::Permission` | pinned permission card |
| `TranscriptKind::Failure` | red failure block |
| `TranscriptKind::Completion` | final answer plus handoff evidence |

Context usage, cost, repository branch, and exact diff statistics should only appear after the runtime exposes authoritative presentation data. The implementation should not invent placeholder metrics.
