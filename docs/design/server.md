# Server 设计

状态：**已实现（HTTP REST + SSE），通过 `latte-code serve` 启动**
日期：2026-08-16

## 1. 概述

latte-code server 是一个长运行进程，管理多个 workspace 和 agent session。客户端通过 HTTP 连接，使用 JSON 通信。

## 2. 目标

- **多 Workspace**：一个 server 管理多个 workspace。
- **多客户端**：多个客户端可以并发连接。
- **HTTP API**：简单的 REST API，供程序化访问。
- **SSE 事件**：状态变更的推送通知。

## 3. 非目标

- 远程访问（v1 仅本地）
- 热插件重载
- 子 agent
- 复杂认证

## 4. 架构

```text
Clients (CLI / TUI / external)
   │
   │ HTTP + SSE
   ▼
┌─────────────────────────────────┐
│ Server Process                  │
│                                 │
│  Message Bus (Gateway)          │
│    ├─ Command Routing           │
│    ├─ Event Collection          │
│    └─ Event Distribution        │
│         │                       │
│         ▼                       │
│  WorkspaceManager               │
│    ├─ Workspace A               │
│    │   └─ ThreadRuntimeService  │
│    └─ Workspace B               │
│        └─ ThreadRuntimeService  │
└─────────────────────────────────┘
```

Gateway 充当轻量消息总线：
- **命令**：路由到对应的 workspace/session
- **事件**：从 workspace 收集并分发给订阅者
- **v1 范围**：仅路由和事件扇出。不包含调度、跨 workspace 消息或插件系统（v2）。

## 5. HTTP API

### 5.1 基础

- **URL**：`http://127.0.0.1:<port>`（默认 4096）
- **Content-Type**：`application/json`
- **认证**：`Authorization: Bearer <token>`（必须）

### 5.2 认证

- **v1**：强制随机 Bearer token。server 启动时生成，存储在 `LATTE_CODE_HOME/server.token`（0600 权限）。
- **发现**：客户端读取 token 文件进行认证。
- **v2**：OAuth/JWT，支持远程访问。

### 5.3 Workspace 管理

**创建/解析 Workspace**
```
POST /v1/workspaces
Body: { "path": "/absolute/path/to/workspace" }
Response: { "workspace_id": "ws_abc123", "path": "/canonical/path" }
```

Server 会规范化路径并返回稳定的 `workspace_id`。后续请求使用此 ID。

### 5.4 Session 端点

**创建 Session**
```
POST /v1/workspaces/{workspace_id}/sessions
Headers: Idempotency-Key: <uuid>
Body: { "prompt": "...", "binding": {...} }
Response: 202 { "session_id": "...", "accepted_revision": 42 }
```

返回 202 Accepted。Session 已创建，首个 turn 已入队。客户端通过 SSE 观察完成状态。

**Follow Up**
```
POST /v1/sessions/{id}/follow-up
Headers: Idempotency-Key: <uuid>
Body: { "prompt": "...", "expected_thread_revision": 42 }
Response: 202 { "accepted_revision": 43, "workspace_id": "ws_..." }
```

`workspace_id` 用于订阅正确的 workspace 事件流。

**切换模型**
```
POST /v1/sessions/{id}/model
Body: { "binding": {...}, "expected_thread_revision": 42 }
Response: 200 { "snapshot": {...} }
```

**取消**
```
POST /v1/sessions/{id}/cancel
Body: { "expected_thread_revision": 42, "expected_run_revision": 10 }
Response: 200 { "snapshot": {...} }
```

**排队 Follow Up**
```
POST /v1/sessions/{id}/queue
Body: { "prompt": "..." }
Response: 202 { "position": 0 }
```

**解析权限请求**
```
POST /v1/sessions/{id}/permissions/{request_id}
Body: { "allow": true, "expected_thread_revision": 42, "expected_run_revision": 10 }
Response: 200 { "snapshot": {...} }
```

**提供输入**
```
POST /v1/sessions/{id}/input
Body: { "request_id": "...", "value": "...", "expected_thread_revision": 42, "expected_run_revision": 10 }
Response: 200 { "snapshot": {...} }
```

**调和未知 Effect**
```
POST /v1/sessions/{id}/effects/{effect_id}/reconcile
Response: 200 { "snapshot": {...} }
```

**获取 Session**
```
GET /v1/sessions/{id}
Response: 200 { "snapshot": {...} }
```

**列出 Session**
```
GET /v1/workspaces/{workspace_id}/sessions?cursor=...&limit=50
Response: 200 { "sessions": [...], "next_cursor": "..." }
```

**搜索 Session**
```
GET /v1/workspaces/{workspace_id}/sessions/search?q=...&cursor=...&limit=50
Response: 200 { "sessions": [...], "next_cursor": "..." }
```

### 5.5 事件（SSE）

```
GET /v1/workspaces/{workspace_id}/events
Accept: text/event-stream
```

按 workspace 的事件流。事件包含 `session_id` 用于路由。

```
event: thread_changed
data: {"session_id": "...", "revision": 42}

event: progress
data: {"session_id": "...", "run_id": "...", "progress": {...}}

event: resync_required
data: {}
```

SSE 是通知通道，不是持久事件日志：事件不带 `id:` 字段，server 不缓存历史事件、
不支持 replay，客户端不发送 `Last-Event-ID`。所有状态权威来自
`GET /v1/sessions/{id}` 的 snapshot。

**重连**：重连后客户端立即全量 resync（拉取订阅的 session snapshot），不依赖
断线期间的事件补发。

**事件类型**：
- `thread_changed`：持久化唤醒信号。客户端应拉取 session 快照。
- `progress`：瞬态流式进度通知，丢失只影响 UI 流畅度，不影响正确性。
- `resync_required`：客户端必须重新拉取全部状态。
- 未知 event type：客户端必须忽略（不报错、不断开），以便 v1 内新增事件类型。

**背压**：server 用 broadcast channel 扇出事件，每个客户端有独立 receiver，
慢消费者不影响其他客户端。receiver 落后于 channel 容量时，server 发送
`resync_required`，客户端全量拉 snapshot——**不断开连接**。

**心跳**：server 周期性发送 SSE keep-alive 注释行（`: heartbeat`，当前实现间隔
2s），防止中间代理超时并保持关闭响应及时。客户端 30s 无事件/心跳判定断线，
触发重连 + resync。

### 5.6 错误

```json
{
  "error": {
    "type": "rejected|unauthorized|not_found|idempotency_mismatch|conflict|failed",
    "message": "...",
    "current_revision": 42
  }
}
```

正式错误类型枚举（`error.type` 的合法值）：

| type | HTTP 状态码 | 含义 | 可重试? |
|---|---|---|---|
| `rejected` | 400 | 请求参数无效（含 JSON 解析/反序列化失败） | 否 |
| `unauthorized` | 401 | Bearer token 缺失/无效 | 否 |
| `not_found` | 404 | session/workspace 不存在 | 否 |
| `idempotency_mismatch` | 422 | 同一 Idempotency-Key 搭配不同 payload | 否（客户端 bug） |
| `conflict` | 409 | revision fence 冲突 | 是（刷新 snapshot 后重试） |
| `failed` | 500 | server 内部错误 | 是（指数退避） |

- `current_revision` 仅在 `conflict` 时出现，供客户端刷新后重试。
- `message` 是人类可读的英文诊断，不保证稳定；客户端应基于 `type` 编程。
- 客户端必须容忍未知错误 type（当作 `failed` 处理）。

### 5.7 幂等性

- **Idempotency-Key 头**：用于持久化变更（Create Session、Follow Up）。Server 按 `(token, idempotency_key)` 去重。
- **重试**：相同 key 返回相同结果。
- **至多一次**：非变更命令至多执行一次。

## 6. Workspace 管理

- **标识**：Server 生成稳定的 `workspace_id`（如 `ws_<hash>`）。
- **解析**：`POST /v1/workspaces` 规范化路径并返回 ID。
- **生命周期**：首次请求时创建，缓存，空闲时卸载（v2）。
- **隔离**：每个 workspace 有自己的 `ThreadRuntimeService`、配置和 provider 绑定。
- **Session 绑定**：Session 持久化绑定到 `workspace_id`。所有 session 操作验证归属权。

## 7. 并发

- **按 session 串行化**：同一 session 的变更通过 revision/lease 串行化。
- **跨 session 并发**：不同 session 可以并发执行。
- **Workspace 初始化**：single-flight 防止重复创建。
- **长操作**：Create/Follow Up 立即返回 202。最终状态通过 SSE 投递。

## 8. 安全

- **v1**：强制 Bearer token。仅本地（127.0.0.1）。token 存储在 0600 文件中。
- **v2**：远程访问，使用 OAuth/JWT。
- **Effect 权限**：HTTP 层只做认证、workspace/session 校验、DTO 解码，然后调用 `ThreadRuntimeService`。Engine 仍然是唯一的 effect 权威。
- **资源限制**：请求体（最大 64 MiB）、并发请求数、SSE 连接数、分页上限。
- **CORS**：默认禁用。Host/Origin 校验。

## 9. 持久化

复用现有模型：
- **SQLite**：用户全局数据库，含 `workspace_id` 列。
- **JSONL**：每 session 的 transcript 文件。

## 10. 实现状态

### 已完成
- [x] HTTP server（axum），包含所有 REST 端点
- [x] Auth 中间件（Bearer token）
- [x] 按 workspace 的事件桥接（SSE），能容忍 broadcast 滞后
- [x] WorkspaceManager，single-flight 创建
- [x] 所有 session 端点（create/get/follow-up/cancel/queue/resolve-permission/provide-input/reconcile）
- [x] 异步 create/follow-up：持久化 + 注册后返回 202，turn 后台执行，通过 SSE 观察完成
- [x] `Idempotency-Key` 持久化变更去重，按 `(token, key)` 索引
- [x] 版本栅栏：cancel/permission/input 校验 thread 和 run 版本，不匹配时返回 409 + 当前版本
- [x] list/search/get 返回 workspace engine 的真实持久化快照
- [x] Server 模式接入 `latte-code` 二进制（`latte-code serve [--port N]`），0600 token 文件，优雅关闭
- [x] 单元测试和最终二进制 E2E（portable），覆盖 HTTP 接口和 session 生命周期

### 待办
- [ ] Binding 发现端点，让远程（非 co-located）客户端能获取有效的 `ThreadProviderBindingV2`
- [ ] 性能测试
- [ ] 远程访问认证（v2）

## 11. 开放问题

1. 默认端口（4096）？
2. Workspace 卸载超时（v2）？
3. Token 轮换策略？
4. SSE 重放缓冲区大小？
