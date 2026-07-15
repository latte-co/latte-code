# Latte Code single-session TUI prototypes

This directory contains design-only HTML prototypes for Latte Code's first single-session TUI. It does not modify the Rust implementation.

Open [`index.html`](./index.html) directly, then use the state controls or number keys `1`–`8`.

The rejected line-art logo experiment has been removed from the main prototype. [`logo-study.html`](./logo-study.html) keeps the next iteration isolated: a source-derived half-block pixel mark in three palette treatments.

## Prototype states

- `#idle`: product identity, environment contract, and first prompt.
- `#working`: user request, compact thought indicator, grouped tool activity, streaming status, and composer.
- `#permission`: a high-priority permission card with an explicit operation summary. Enter remains inert.
- `#complete`: final response, changed files, and verification evidence.
- `#todo`: a pinned, collapsible Todo region with done, active, pending, blocked, and skipped task states.
- `#subagent`: a non-Team main session with task-scoped background Subagents, nested spawn lineage, handoffs routed back through Main, and one Main-owned composer.
- `#team`: an Agent Team main session where one Lead remains the default user interlocutor while stable members continue in the background; the pinned Team Dock, owner-tagged Todo, and single targetable composer remain available during execution.
- `#agents`: a context-sensitive inspector. From `#subagent` it shows the Main call tree and read-only task sessions; from `#team` it shows Team membership plus nested children, effective model metadata, shared Tasks, and live Activity.

Direct links also work with a query parameter, for example `index.html?state=permission`.

## Design boundaries

- One focused session owns the viewport.
- All eight prototype states share one bounded 4:3 terminal viewport.
- The default session has no sidebar, repository explorer, MCP dashboard, or permanent workbench navigation.
- User prompts are visually separated; assistant text remains lightweight.
- Agent activity is grouped by intent into Inspecting, Applying, and Verifying phases.
- Completed phases collapse to one line; tool results form an optional third disclosure level.
- Active phase status is not repeated as a separate assistant progress paragraph.
- A run owns one pinned Todo projection between the transcript and composer; it updates in place and collapses to the active item plus progress.
- In non-Team mode, Main owns the conversation and all temporary Subagents return questions, permission requests, and handoffs through Main. Users do not directly steer these task-scoped workers.
- In Team mode, Lead owns the main conversation, delegates work, consumes handoffs, and publishes a single synthesized result; stable members publish only milestones into the main transcript.
- A pinned Team Dock tracks live status without blocking the composer. The composer targets Lead by default and can explicitly address a member without creating another chat surface.
- The Agents inspector distinguishes stable Team members from nested runtime children, exposes each instance's effective model, and separates Sessions, Tasks, and Activity without a second composer or hidden reasoning.
- Conversation text uses a compact 14px rhythm; secondary metadata stays at 12px and tool rows avoid panel-sized padding.
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
