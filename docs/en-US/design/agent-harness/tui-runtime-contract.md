# TUI Runtime Contract

Status: **design proposal; not implemented.**

Chinese counterpart: [TUI Runtime 契约](../../../zh-CN/design/agent-harness/tui-runtime-contract.md).

## 1. Decision

`latte-tui` is a pure presentation projection and reducer. It does not resolve credential, create Provider, access SQLite, read/write workspace, execute Effect, or turn a key event into untyped engine call. Composition root provides typed command sink, snapshot loader, event source, clock, terminal backend, and shutdown handle.

## 2. Three layers

1. **Reducer**: pure function from key/mouse/resize, snapshot, durable event, and transient progress to local model plus typed UI action.
2. **Runtime adapter**: owns crossterm input, timer, event subscription, snapshot refresh, and terminal lifecycle; it has no product decision.
3. **Renderer**: reads model and draws Ratatui frame only; no I/O, spawn, or blocking work.

External capability is trait-injected so fake event source, fake clock, TestBackend, or VT100 terminal replaces real terminal in tests.

## 3. Interaction and recovery

Composer remains editable while Session runs. Submitted prompt displays local queue state; asynchronous Turn Runner mailbox receipt decides actual order. Permission, input-request, and reconciliation card own their full key event and cannot be bypassed by slash popup, Enter, or stale overlay.

On event gap, subscription failure, or reconnect, adapter clears transient progress and reloads snapshot plus current transcript page; it redraws only on model change. Normal exit, error, panic, SIGINT, and terminal suspend all restore raw mode, alternate screen, keyboard enhancement, and cursor state.

## 4. Accessibility and acceptance

Input edits by grapheme cluster and CJK/emoji width uses terminal cells. Provider/tool/path/error text is control-character filtered, byte-bounded, and rendered under public redaction. A small terminal degrades layout instead of hiding composer, pending permission, or exit path.

- UT covers reducer keyboard/focus/overlay/queue/gap recovery/approval-negative path with no terminal I/O.
- Rendering tests use TestBackend/VT100 for narrow screen, Unicode, long transcript, resize, permission card, and terminal restoration frame.
- E2E uses real PTY for composer, queued prompt, cancel, permission, event reconnect, and terminal restoration; it continuously drains output and waits for explicit readiness, never sleep.
