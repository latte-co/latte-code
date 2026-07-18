# Events, Projections, and Replay

Status: **design proposal; not implemented.**

Chinese counterpart: [事件、投影与回放](../../../zh-CN/design/agent-harness/event-projection-and-replay.md).

## 1. Decision

Latte Code separates durable domain events from transient progress. The engine writes the former transactionally with state projection and a per-Thread increasing sequence. The latter describes process-local streaming, mailbox, and spinner presentation only. TUI must not treat transient event as authoritative state.

Existing `ThreadSnapshot.revision`, `sequence`, and `ThreadEventEnvelope` form the target protocol base. New events must not reuse or change protocol-v1 serialization semantics.

## 2. Event contract

A durable event carries at least `event_id`, `thread_id`, sequence, related Run, time, typed redacted payload, and source key. Run/Effect/Permission/Transcript update, revision increment, and event append commit atomically; a repeated source key creates one state change and one visible event.

Transient progress may carry Provider delta, `InputQueued`, waiting state, and local reconnect hint, but never raw credential, private descriptor, or unfinished assistant content. Event buffer is bounded. A slow consumer, reconnect, or sequence gap makes adapter discard transient state and reload authoritative snapshot/page.

## 3. Replay and audit

Conversation replay builds model-visible history from JSONL. SQLite snapshot, Effect ledger, checkpoint, and evidence build control state; event log does not replace Effect recovery. Replay never calls Provider, executes tool, or reconsumes approval. It explains what a user saw and why an Effect is `Unknown` without exporting private descriptor or secret.

Event retention and pagination are bounded. Session detail first reads snapshot and recent transcript page; older content is cursor-loaded. Subscription accelerates refresh only and is never the sole read path.

## 4. Acceptance

- UT covers atomic state/projection/event writes, source-key deduplication, monotonic sequence, and pagination boundary.
- UT proves reconnect, slow consumer, gap, and malformed payload cannot retain false transient TUI state; replay has no Provider/Tool side effect and rebuilds public snapshot from JSONL plus SQLite.
- E2E kills and restarts TUI/client and proves snapshot reload restores permission, input, and Unknown-Effect cards without missed progress events.
