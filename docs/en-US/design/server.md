# Server Design

Status: **design proposal, skeleton implemented**
Date: 2026-08-15

## 1. Overview

latte-code server is a long-running process that manages workspaces and agent sessions. Clients connect via HTTP and communicate using JSON.

## 2. Goals

- **Multi-workspace**: One server manages multiple workspaces.
- **Multi-client**: Multiple clients can connect concurrently.
- **HTTP API**: Simple REST API for programmatic access.
- **SSE events**: Push notifications for state changes.

## 3. Non-Goals

- Remote access (v1 is local-only)
- Hot plugin reload
- Sub-agents
- Complex authentication

## 4. Architecture

```text
Clients (CLI / TUI / external)
   │
   │ HTTP + SSE
   ▼
┌─────────────────────────────────┐
│ Server Process                  │
│                                 │
│  HTTP Server (axum)             │
│    │                            │
│    ▼                            │
│  WorkspaceManager               │
│    ├─ Workspace A               │
│    │   └─ ThreadRuntimeService  │
│    └─ Workspace B               │
│        └─ ThreadRuntimeService  │
└─────────────────────────────────┘
```

## 5. HTTP API

### 5.1 Base

- **URL**: `http://127.0.0.1:<port>` (default 4096)
- **Content-Type**: `application/json`
- **Auth**: `Authorization: Bearer <token>` (required)

### 5.2 Authentication

- **v1**: Mandatory random Bearer token. Generated on server start, stored in `LATTE_CODE_HOME/server.token` (0600 permissions).
- **Discovery**: Clients read the token file to authenticate.
- **v2**: OAuth/JWT for remote access.

### 5.3 Workspace Management

**Create/Resolve Workspace**
```
POST /v1/workspaces
Body: { "path": "/absolute/path/to/workspace" }
Response: { "workspace_id": "ws_abc123", "path": "/canonical/path" }
```

The server canonicalizes the path and returns a stable `workspace_id`. Subsequent requests use this ID.

### 5.4 Session Endpoints

**Create Session**
```
POST /v1/workspaces/{workspace_id}/sessions
Headers: Idempotency-Key: <uuid>
Body: { "prompt": "...", "binding": {...} }
Response: 202 { "session_id": "...", "accepted_revision": 42 }
```

Returns 202 Accepted. The session is created and the first turn is queued. Clients use SSE to observe completion.

**Follow Up**
```
POST /v1/sessions/{id}/follow-up
Headers: Idempotency-Key: <uuid>
Body: { "prompt": "...", "expected_thread_revision": 42 }
Response: 202 { "accepted_revision": 43 }
```

**Switch Model**
```
POST /v1/sessions/{id}/model
Body: { "binding": {...}, "expected_thread_revision": 42 }
Response: 200 { "snapshot": {...} }
```

**Cancel**
```
POST /v1/sessions/{id}/cancel
Body: { "expected_thread_revision": 42, "expected_run_revision": 10 }
Response: 200 { "snapshot": {...} }
```

**Queue Follow Up**
```
POST /v1/sessions/{id}/queue
Body: { "prompt": "..." }
Response: 202 { "position": 0 }
```

**Resolve Permission**
```
POST /v1/sessions/{id}/permissions/{request_id}
Body: { "allow": true, "expected_thread_revision": 42, "expected_run_revision": 10 }
Response: 200 { "snapshot": {...} }
```

**Provide Input**
```
POST /v1/sessions/{id}/input
Body: { "request_id": "...", "value": "...", "expected_thread_revision": 42, "expected_run_revision": 10 }
Response: 200 { "snapshot": {...} }
```

**Reconcile Unknown Effect**
```
POST /v1/sessions/{id}/effects/{effect_id}/reconcile
Response: 200 { "snapshot": {...} }
```

**Get Session**
```
GET /v1/sessions/{id}
Response: 200 { "snapshot": {...} }
```

**List Sessions**
```
GET /v1/workspaces/{workspace_id}/sessions?cursor=...&limit=50
Response: 200 { "sessions": [...], "next_cursor": "..." }
```

**Search Sessions**
```
GET /v1/workspaces/{workspace_id}/sessions/search?q=...&cursor=...&limit=50
Response: 200 { "sessions": [...], "next_cursor": "..." }
```

### 5.5 Events (SSE)

```
GET /v1/workspaces/{workspace_id}/events
Accept: text/event-stream
```

Per-workspace event stream. Events include `session_id` for routing.

```
id: 42
event: thread_changed
data: {"session_id": "...", "revision": 42}

id: 43
event: progress
data: {"session_id": "...", "progress": {...}}

id: 44
event: resync_required
data: {}
```

**Reconnection**: Clients use `Last-Event-ID` header. If the server has no replay buffer, it sends `resync_required` and the client re-fetches state.

**Event types**:
- `thread_changed`: Durable wake-up. Client should fetch session snapshot.
- `progress`: Best-effort progress. Not replayed on reconnect.
- `resync_required`: Client must re-fetch all state.

**Backpressure**: Slow clients are disconnected. Progress events can be dropped; `thread_changed` events are queued with a limit.

### 5.6 Errors

```json
{
  "error": {
    "type": "rejected|unauthorized|failed|unavailable|conflict",
    "message": "...",
    "current_revision": 42
  }
}
```

- `409 Conflict`: Revision mismatch. `current_revision` is included for client to retry.
- `401 Unauthorized`: Invalid or missing token.
- `400 Bad Request`: Invalid input.
- `503 Unavailable`: Server temporarily unavailable.

### 5.7 Idempotency

- **Idempotency-Key header**: For durable mutations (Create Session, Follow Up). Server dedupes by `(token, idempotency_key)`.
- **Retries**: Same key returns the same result.
- **At-most-once**: Non-mutation commands are at-most-once.

## 6. Workspace Management

- **Identity**: Server generates stable `workspace_id` (e.g., `ws_<hash>`).
- **Resolution**: `POST /v1/workspaces` canonicalizes path and returns ID.
- **Lifecycle**: Created on first request, cached, unloaded when idle (v2).
- **Isolation**: Each workspace has its own `ThreadRuntimeService`, config, and provider bindings.
- **Session binding**: Sessions are durably bound to `workspace_id`. All session operations validate ownership.

## 7. Concurrency

- **Per-session serialization**: Mutations to the same session are serialized by revision/lease.
- **Cross-session concurrency**: Different sessions can execute concurrently.
- **Workspace initialization**: Single-flight to prevent duplicate creation.
- **Long operations**: Create/Follow Up return 202 immediately. Final state is delivered via SSE.

## 8. Security

- **v1**: Mandatory Bearer token. Local-only (127.0.0.1). Token stored in 0600 file.
- **v2**: Remote access with OAuth/JWT.
- **Effect authority**: HTTP layer only does auth, workspace/session validation, DTO decoding, and calls `ThreadRuntimeService`. Engine remains the sole effect authority.
- **Resource limits**: Request body (64 MiB max), concurrent requests, SSE connections, pagination limits.
- **CORS**: Disabled by default. Host/Origin validation.

## 9. Persistence

Uses existing model:
- **SQLite**: User-global database with `workspace_id` column.
- **JSONL**: Per-session transcript files.

## 10. Implementation Status

### Done (skeleton)
- [x] Protocol types
- [x] WorkspaceManager skeleton
- [x] Basic command routing

### TODO
- [ ] HTTP server (axum)
- [ ] SSE implementation
- [ ] Workspace identity (stable ID + single-flight)
- [ ] Reader/worker/writer pattern
- [ ] All endpoints from section 5
- [ ] Token generation and validation
- [ ] Client library
- [ ] Tests

## 11. Open Questions

1. Default port (4096)?
2. Workspace unload timeout (v2)?
3. Token rotation policy?
4. SSE replay buffer size?
