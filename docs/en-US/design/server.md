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
- **Auth**: `Authorization: Bearer <token>` (optional for local)

### 5.2 Endpoints

**Create Session**
```
POST /v1/workspaces/{path}/sessions
Body: { "prompt": "...", "binding": {...} }
Response: { "session_id": "...", "snapshot": {...} }
```

**Follow Up**
```
POST /v1/sessions/{id}/follow-up
Body: { "prompt": "...", "expected_revision": 42 }
Response: { "snapshot": {...} }
```

**Cancel**
```
POST /v1/sessions/{id}/cancel
Response: { "snapshot": {...} }
```

**Resolve Permission**
```
POST /v1/sessions/{id}/permissions/{request_id}
Body: { "allow": true }
Response: { "snapshot": {...} }
```

**Provide Input**
```
POST /v1/sessions/{id}/input
Body: { "request_id": "...", "value": "..." }
Response: { "snapshot": {...} }
```

**Get Session**
```
GET /v1/sessions/{id}
Response: { "snapshot": {...} }
```

**List Sessions**
```
GET /v1/workspaces/{path}/sessions
Response: { "sessions": [...] }
```

**Events (SSE)**
```
GET /v1/sessions/{id}/events
Accept: text/event-stream
```

### 5.3 SSE Events

```
event: thread_changed
data: {"thread_id": "...", "revision": 42}

event: progress
data: {"thread_id": "...", "progress": {...}}
```

### 5.4 Errors

```json
{
  "error": {
    "type": "rejected|unauthorized|failed|unavailable",
    "message": "..."
  }
}
```

## 6. Workspace Management

- **Identity**: Canonical absolute path is the unique key.
- **Selection**: Workspace is specified in the URL path (`/v1/workspaces/{path}/...`).
- **Lifecycle**: Created on first request, cached, unloaded when idle.
- **Isolation**: Each workspace has its own `ThreadRuntimeService`, config, and provider bindings.

## 7. Concurrency

- **Per-workspace serialization**: Commands to the same workspace are serialized.
- **Cross-workspace concurrency**: Commands to different workspaces execute concurrently.
- **Long operations**: Start/FollowUp return immediately with `Received`; final state is delivered via SSE.
- **Fencing**: All mutations carry `expected_revision` to prevent TOCTOU.

## 8. Security

- **v1**: Local-only (127.0.0.1), optional Bearer token.
- **v2**: Remote access with OAuth/JWT.
- **Effect authority**: Server routes to engine; engine is the sole effect authority.
- **Resource limits**: Request size, concurrent requests, SSE connections.

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
- [ ] Workspace identity (canonicalize + single-flight)
- [ ] Reader/worker/writer pattern
- [ ] Query commands
- [ ] Client library
- [ ] Tests

## 11. Open Questions

1. Default port?
2. Workspace unload timeout?
3. Token generation for local mode?
4. SSE reconnection strategy?
