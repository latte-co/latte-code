# Server Design

Status: **design proposal, partially implemented**
Date: 2026-08-15

## 1. Overview

latte-code server is a long-running process that manages multiple workspaces and their agent sessions. Clients (CLI, TUI, desktop, web) connect to the server via a unix socket and communicate using a JSON-based protocol.

The server is the single authority for:
- Workspace lifecycle (load, unload, switch)
- Session/thread management (create, resume, fork)
- Agent loop execution
- Effect authorization and execution
- Persistent state (SQLite + JSONL)

## 2. Goals

- **Multi-workspace**: One server process can manage multiple workspaces simultaneously.
- **Multi-client**: Multiple clients can connect to the same server, each bound to a workspace.
- **Simple protocol**: JSON over unix socket, length-prefixed frames.
- **Strong typing**: All messages are serde-serializable Rust types.
- **Effect authority**: The server is the only place where effects (file changes, process execution) are authorized and executed.

## 3. Non-Goals

- **Remote access**: v1 is local-only (unix socket). Remote/multi-user is v2.
- **Hot plugin reload**: Plugins are loaded at startup. Runtime changes are v2.
- **Sub-agents**: Hierarchical agent spawning is v2.
- **HTTP/WebSocket**: unix socket is the only transport in v1.

## 4. Architecture

```text
Clients (CLI / TUI / desktop / web)
   │
   │ unix socket (JSON, length-prefixed frames)
   ▼
┌─────────────────────────────────────────┐
│ Server Process                          │
│                                         │
│  ┌─────────────┐    ┌────────────────┐  │
│  │  Transport  │───▶│  Connection    │  │
│  │  (unix      │    │  Manager       │  │
│  │   socket)   │    │                │  │
│  └─────────────┘    └────────────────┘  │
│                                │        │
│                                ▼        │
│  ┌─────────────────────────────────┐   │
│  │  WorkspaceManager               │   │
│  │  ┌───────────┐ ┌───────────┐    │   │
│  │  │ Workspace │ │ Workspace │    │   │
│  │  │ Instance  │ │ Instance  │    │   │
│  │  │ (path A)  │ │ (path B)  │    │   │
│  │  └───────────┘ └───────────┘    │   │
│  └─────────────────────────────────┘   │
│                                │        │
│                                ▼        │
│  ┌─────────────────────────────────┐   │
│  │  ThreadRuntimeService (per      │   │
│  │  workspace)                     │   │
│  │  - Thread management            │   │
│  │  - Agent loop                   │   │
│  │  - Effect authorization         │   │
│  └─────────────────────────────────┘   │
└─────────────────────────────────────────┘
```

### 4.1 Components

**Transport**: Listens on a unix socket, accepts connections, reads/writes frames.

**Connection Manager**: Tracks active connections, their bound workspace, and their subscriptions.

**WorkspaceManager**: Lazily creates and caches `WorkspaceInstance`s. Each instance has its own `ThreadRuntimeService`.

**ThreadRuntimeService**: The existing per-workspace service that manages threads, agent loops, and effects. Unchanged from the current design.

## 5. Protocol

### 5.1 Transport

- **Socket**: Unix domain socket at `$LATTE_CODE_HOME/server.sock` (or configurable path).
- **Frames**: 4-byte big-endian length prefix + JSON payload.
- **Encoding**: UTF-8 JSON.

### 5.2 Message Types

All messages are wrapped in a `ServerFrame`:

```rust
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum ServerFrame {
    Command(ServerCommand),
    Response(ServerResponse),
    Event(ServerEvent),
}
```

### 5.3 Commands

Commands are client → server. The first command on a connection must be `SelectWorkspace`.

```rust
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerCommandPayload {
    /// Select the workspace for this connection. Must be first.
    SelectWorkspace { path: String },

    /// Thread commands (Start, FollowUp, SwitchModel, etc.)
    Thread(ThreadCommand),

    /// Queue a follow-up message.
    QueueFollowUp { thread_id: ThreadId, prompt: String },

    /// Reconcile an unknown effect.
    ReconcileUnknown { thread_id: ThreadId, effect_id: String },

    /// Queries
    ListSessions,
    SearchSessions { query: String },
    GetSession { thread_id: ThreadId },

    /// Subscriptions
    Subscribe { thread_id: ThreadId },
    Unsubscribe { thread_id: ThreadId },
}
```

### 5.4 Responses

Responses are server → client, correlated by `command_id`.

```rust
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerResponsePayload {
    /// Command received (not necessarily durable).
    Received,
    /// Command completed with a snapshot.
    Completed { snapshot: ThreadSnapshot },
    /// Command completed with a list of sessions.
    Sessions { sessions: Vec<ThreadSnapshot> },
    /// Command failed.
    Error { error: ServerError },
}
```

### 5.5 Events

Events are server → client, pushed to subscribed connections.

```rust
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    /// Thread state changed (wake-up signal; client should fetch snapshot).
    ThreadChanged { thread_id: ThreadId, revision: u64 },
    /// Transient progress (best-effort, may be lost).
    Progress { thread_id: ThreadId, progress: ThreadTransientProgress },
}
```

### 5.6 Error Model

```rust
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerError {
    Rejected { message: String },    // Invalid command, state conflict
    Unauthorized { message: String }, // Not authorized
    Failed { message: String },      // Execution failed
    Unavailable { message: String }, // Server temporarily unavailable
}
```

## 6. Workspace Management

### 6.1 Workspace Selection

Each connection is bound to a workspace via `SelectWorkspace`. The server:
1. Validates the path (canonicalize, check it exists).
2. Gets or creates a `WorkspaceInstance` via `WorkspaceManager`.
3. Binds the connection to that workspace.

All subsequent commands on the connection operate within that workspace.

### 6.2 Workspace Lifecycle

- **Created**: On first `SelectWorkspace` for a path.
- **Cached**: Kept in memory while connections are active.
- **Unloaded**: When idle (no connections, no active threads) for a timeout. State is persisted.
- **Recreated**: On next `SelectWorkspace`, state is loaded from persistence.

### 6.3 Workspace Isolation

Each workspace has its own:
- `ThreadRuntimeService` (threads, agent loops)
- Configuration (merged from defaults, user, workspace)
- Provider bindings
- SQLite database (or separate DB file)

Workspaces share:
- Server process
- LLM credentials (if configured globally)
- OS resources

## 7. Concurrency Model

- **One thread per connection**: Each connection is handled by a tokio task.
- **Per-workspace serialization**: Commands to the same workspace are serialized via the `ThreadRuntimeService`'s existing locking.
- **Cross-workspace concurrency**: Commands to different workspaces can execute concurrently.
- **Event broadcasting**: Events are broadcast to all connections subscribed to the relevant thread.

## 8. Security Model

### 8.1 Authentication

v1: Unix socket file permissions (filesystem-based access control). The socket is created with `0600` permissions, so only the owner can connect.

v2: Token-based authentication for remote access.

### 8.2 Effect Authority

The server is the only place where effects are authorized and executed. Clients send commands, the server:
1. Validates the command.
2. Checks permissions (policy, approvals).
3. Executes the effect (file change, process).
4. Records the result.

Clients never directly access the filesystem or processes.

### 8.3 Data Isolation

- Clients only see data from their bound workspace.
- Redacted projections: Clients see only what they need (no raw credentials, no internal state).

## 9. Persistence

- **SQLite**: Per-workspace database for threads, sessions, effects, permissions.
- **JSONL**: Per-thread transcript files, append-only.
- **Location**: `$LATTE_CODE_HOME/workspaces/<workspace-hash>/`

## 10. Comparison with Other Projects

### 10.1 Codex

- **Similar**: Multi-thread, per-thread agent loop, JSON-RPC server.
- **Different**: Codex uses stdio/unix socket/websocket; latte-code uses unix socket only. Codex has hierarchical sub-agents; latte-code v1 doesn't.

### 10.2 opencode

- **Similar**: Multi-instance (per-project), HTTP+SSE, instance-based isolation.
- **Different**: opencode uses HTTP headers for workspace selection; latte-code uses connection-level binding. opencode has a worker thread for TUI; latte-code uses separate processes.

### 10.3 DSH

- **Similar**: AgentLoop as factory, per-agent scope, effect authority.
- **Different**: DSH is in-process (Cordis); latte-code is client-server. DSH has fine-grained scopes; latte-code v1 uses workspace-level isolation.

### 10.4 latte-code's Differentiators

- **Effect authority**: First-class effect lifecycle (Declared → Prepared → Started → Observed/Unknown) with strong consistency guarantees.
- **Strong typing**: All protocol messages are Rust types, serde-serialized.
- **Process supervision**: Built-in process group management for tool execution.
- **Redacted projections**: Clients see only what they need, not raw state.

## 11. Implementation Status

### 11.1 Done

- [x] Protocol types (`latte-core/src/server.rs`)
- [x] Transport layer (`latte-server/src/transport.rs`)
- [x] WorkspaceManager (`latte-server/src/workspace.rs`)
- [x] Basic server with command routing (`latte-server/src/lib.rs`)
- [x] Multi-workspace support
- [x] Serialization round-trip tests

### 11.2 In Progress

- [ ] Query commands (ListSessions, SearchSessions, GetSession)
- [ ] Event forwarding from ThreadRuntimeService to server
- [ ] Per-connection subscription tracking

### 11.3 TODO

- [ ] Client library (for CLI/TUI)
- [ ] Server binary (standalone executable)
- [ ] Workspace idle timeout and unloading
- [ ] Reconnection logic
- [ ] Integration tests

## 12. Open Questions

1. **Socket location**: `$LATTE_CODE_HOME/server.sock` vs per-workspace sockets?
2. **Workspace unload timeout**: How long should an idle workspace stay loaded?
3. **Event delivery**: Should events be per-connection or per-workspace broadcast?
4. **Backpressure**: How to handle slow clients?
5. **Crash recovery**: What happens to in-flight commands when the server restarts?

## 13. References

- [Architecture Overview](./architecture-overview.md)
- [Data Storage](./data-storage.md)
- [Thread Runtime Service](../../latte-headless/src/thread.rs)
- [Protocol Types](../../latte-core/src/server.rs)
