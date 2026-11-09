# Server 设计

状态：**已实现（HTTP REST + SSE），通过 `latte-code serve` 启动**
日期：2026-08-16（2026-11-09 增补端点清单）

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
│    │   └─ SessionRuntimeService  │
│    └─ Workspace B               │
│        └─ SessionRuntimeService  │
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
- **健康检查**：`GET /health` 是唯一免认证端点，仅作存活探针，返回 `ok`。

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

**列出可用 Binding**
```
GET /v1/workspaces/{workspace_id}/bindings
Response: 200 { "bindings": [BindingCatalogEntry, ...] }
```

返回该 workspace 合并配置中的完整 model 目录。每条 `BindingCatalogEntry` 含
`provider_name`、`model`、`name`、`is_default` 与完整 `SessionProviderBinding`，
供 TUI model picker 与远程客户端选取后透传（`StartWithModel` 不在客户端合成
binding）。

### 5.4 Session 端点

**创建 Session**
```
POST /v1/workspaces/{workspace_id}/sessions
Headers: Idempotency-Key: <uuid>
Body: { "session_id": "<uuid>", "command_id": "<uuid>", "prompt": "...", "binding": {...} }
Response: 202 { "session_id": "...", "accepted_revision": 42 }
```

`session_id` 和 `command_id` 由客户端生成（UUID v7）。`Idempotency-Key` 头必须等于 `command_id`。返回 202 Accepted。Session 已创建，首个 turn 已入队。客户端通过 SSE 观察完成状态。同一 `command_id` + 相同 payload 的重试返回 200（replay），不重复创建。

**Follow Up**
```
POST /v1/sessions/{id}/follow-up
Headers: Idempotency-Key: <uuid>
Body: { "command_id": "<uuid>", "prompt": "...", "expected_session_revision": 42 }
Response: 202 { "accepted_revision": 43, "workspace_id": "ws_..." }
```

`command_id` 由客户端生成（UUID v7），`Idempotency-Key` 头必须等于 `command_id`。`workspace_id` 用于订阅正确的 workspace 事件流。同一 `command_id` + 相同 payload 的重试返回 200（replay），不重复追加 turn；同一 `command_id` + 不同 payload 返回 422 `idempotency_mismatch`。

**切换模型**
```
POST /v1/sessions/{id}/model
Body: { "binding": {...}, "expected_session_revision": 42 }
Response: 200 { "snapshot": {...} }
```

**取消**
```
POST /v1/sessions/{id}/cancel
Body: { "expected_session_revision": 42, "expected_turn_revision": 10 }
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
Body: { "allow": true, "expected_session_revision": 42, "expected_turn_revision": 10 }
Response: 200 { "snapshot": {...} }
```

**提供输入**
```
POST /v1/sessions/{id}/input
Body: { "request_id": "...", "value": "...", "expected_session_revision": 42, "expected_turn_revision": 10 }
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

**按精确标题查找 Session**
```
GET /v1/workspaces/{workspace_id}/sessions/exact-title?q=...&cursor=...&limit=50
Response: 200 { "sessions": [...], "next_cursor": "..." }
```

**重命名 Session**
```
PATCH /v1/sessions/{id}
Body: { "title": "新标题" }
Response: 200 { "snapshot": {...} }
```

**分叉 Session**
```
POST /v1/sessions/{id}/fork
Body: { "title": "可选标题" }
Response: 200 { "snapshot": {...} }
```
Fork 把已提交对话历史复制到一个不携带运行时 Authority 的新 Session，并记录血缘。

**只读上下文用量投影**
```
GET /v1/sessions/{id}/context
Response: 200 { "session_id": "...", "usage": {...} }
```
精确字节 + 估算 token + 丢弃段数 + 压缩策略状态；不做 provider I/O、不改变
状态（见 [context-design.md](context-design.md) §4.5 与
[versioned-rpc-contract.md](versioned-rpc-contract.md) §3.3）。

**手动压缩空闲 Session**
```
POST /v1/sessions/{id}/compact
Response: 200 { "snapshot": {...}, "state": {"compacted" | "nothing_to_compact"} }
```
idle-only：非空闲或 revision 冲突返回 409；水位以下也强制；空态（无历史、
策略关闭、熔断、无可压缩边界）返回 200 `nothing_to_compact` 而非错误
（见 [context-design.md](context-design.md) §4.7）。

**装填一次性 Reminder**
```
POST /v1/sessions/{id}/reminder
Body: { "text": "..." }
Response: 200 { "session_id": "...", "bytes": 123 }
```
仅 `Ready` 会话可装填（非空闲 409）；文本脱敏后只存进程内存，下一次 turn
构建消费一次即清除，不进 transcript，重启即失
（见 [context-design.md](context-design.md) §4.8）。

### 5.5 事件（SSE）

```
GET /v1/workspaces/{workspace_id}/events
Accept: text/event-stream
```

按 workspace 的事件流。事件包含 `session_id` 用于路由。

```
event: session_changed
data: {"session_id": "...", "revision": 42}

event: progress
data: {"session_id": "...", "turn_id": "...", "progress": {...}}

event: resync_required
data: {}
```

SSE 是通知通道，不是持久事件日志：事件不带 `id:` 字段，server 不缓存历史事件、
不支持 replay，客户端不发送 `Last-Event-ID`。所有状态权威来自
`GET /v1/sessions/{id}` 的 snapshot。

**重连**：重连后客户端立即全量 resync（拉取订阅的 session snapshot），不依赖
断线期间的事件补发。

**事件类型**：
- `session_changed`：持久化唤醒信号。客户端应拉取 session 快照。
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
- **隔离**：每个 workspace 有自己的 `SessionRuntimeService`、配置和 provider 绑定。
- **Session 绑定**：Session 持久化绑定到 `workspace_id`。所有 session 操作验证归属权。

## 7. 并发

- **按 session 串行化**：同一 session 的变更通过 revision/lease 串行化。
- **跨 session 并发**：不同 session 可以并发执行。
- **Workspace 初始化**：single-flight 防止重复创建。
- **长操作**：Create/Follow Up 立即返回 202。最终状态通过 SSE 投递。

## 8. 安全

- **v1**：强制 Bearer token。仅本地（127.0.0.1）。token 存储在 0600 文件中。
- **v2**：远程访问，使用 OAuth/JWT。
- **Effect 权限**：HTTP 层只做认证、workspace/session 校验、DTO 解码，然后调用 `SessionRuntimeService`。Engine 仍然是唯一的 effect 权威。
- **资源限制**：请求体（最大 64 MiB）、并发请求数、SSE 连接数、分页上限。
- **CORS**：默认禁用。Host/Origin 校验。

## 9. 持久化

复用现有模型：
- **SQLite**：用户全局数据库，含 `workspace_id` 列。
- **JSONL**：每 session 的 transcript 文件。

## 10. 实现状态

### 已完成
- [x] HTTP server（axum），包含所有 REST 端点
- [x] Auth 中间件（Bearer token；`GET /health` 免认证）
- [x] 按 workspace 的事件桥接（SSE），能容忍 broadcast 滞后
- [x] WorkspaceManager，single-flight 创建
- [x] 所有 session 端点（create/get/follow-up/cancel/queue/resolve-permission/provide-input/reconcile/rename/fork）
- [x] `GET /v1/workspaces/{ws}/bindings` binding 发现端点（TUI model picker 与远程客户端透传 binding）
- [x] `GET /v1/workspaces/{ws}/sessions/exact-title` 精确标题查询
- [x] `GET /v1/sessions/{id}/context` 只读用量投影、`POST /v1/sessions/{id}/compact` 手动压缩、`POST /v1/sessions/{id}/reminder` 一次性 reminder 槽
- [x] 异步 create/follow-up：持久化 + 注册后返回 202，turn 后台执行，通过 SSE 观察完成
- [x] `Idempotency-Key` 持久化变更去重，按 `(token, key)` 索引
- [x] 版本栅栏：cancel/permission/input 校验 session 和 turn 版本，不匹配时返回 409 + 当前版本
- [x] list/search/get 返回 workspace engine 的真实持久化快照
- [x] Server 模式接入 `latte-code` 二进制（`latte-code serve [--port N]`），0600 token 文件，优雅关闭
- [x] 单元测试和最终二进制 E2E（portable），覆盖 HTTP 接口和 session 生命周期

### 待办
- [ ] 性能测试
- [ ] 远程访问认证（v2）

## 11. 运行模式与多 Server 语义

> 本节并入原 server-client-integration 方案（PR #11–#14，已全部落地）中仍然有效
> 的部署与决策内容；当时的实施记录见该 PR 与 git 历史。

### 11.1 三种运行模式

1. **内嵌（默认）**：TUI 与 `run`/`list`/`show`/`context`/`compact`/`resume` 在进程
   内启动 embedded server（loopback 随机端口，token 仅存内存），全部经 HTTP+SSE
   访问 engine——同进程也没有 in-process 直连路径，协议即边界。
2. **连接独立 server**：`--server <url>`，token 读 `$LATTE_CODE_HOME/server.token`
   或 `--token`。
3. **独立 server**：`latte-code serve [--port N]`（默认 4096）。

### 11.2 多 Server 语义（v1 不做 owner routing）

- **Durable state 共享**：所有 server 共享同一 SQLite + JSONL，任何 server 都能读
  任何 session 的 durable state。
- **Live mutation 必须连 owner server**：mailbox、active cancellation、progress 是
  process-local；连 server B 操作 server A 正在跑的 session，queue/cancel/input
  会失败（`InvalidState`）。
- **新 session 创建**：任何 server 都可以创建（durable write 不依赖 live runtime
  state）。
- **Live event 隔离**：每个 server 进程有自己的 SSE 广播；要观察 live event，连
  owner server。

### 11.3 幂等创建故障模型

| 故障场景 | 保护机制 |
|---|---|
| 客户端超时，server 存活 | 内存 ledger replay（同 Idempotency-Key） |
| 客户端超时，ledger 丢失（极端） | client `command_id` + durable digest replay |
| server crash 在 durable accept 后、202 前 | 重启后命中 `session_command_dedup`，返回 `Replayed`，不重启 provider |
| server crash 在 durable accept 前 | 同 `session_id`/`command_id` 重试创建原目标 session（无副作用） |
| 同 `command_id` + 不同 payload | 422 `idempotency_mismatch` |
| 同 `session_id` 已存在（非重放） | 409 Conflict |

Exactly-once 范围限定：只保证 session admission exactly-once（不重复创建
session），不保证 provider/effect execution exactly-once；`Started` 后未确认的
结果转 `Unknown`，走显式 reconciliation。内存 ledger 只做同进程快速 replay，
不承担 crash correctness。

### 11.4 内嵌 Server 生命周期与退出顺序

- client cancel token 与 server shutdown token 分开；server-side SSE handler
  `select!` 在 server shutdown token 上，shutdown 时主动结束 stream。
- 退出顺序：前端退出 → cancel/join client workers（SSE/action）→ cancel 并 await
  embedded server，确认端口释放。
- SSE 无 replay：每次连接/重连后客户端立即无条件全量 resync（§5.5）。

### 11.5 CLI 命令契约决策记录

CLI 命令契约曾按 [versioned-rpc-contract.md](versioned-rpc-contract.md) §2.1 的
早期例外条款直接 breaking change：`run/show/list/resume` 切换到 session 语义、
JSON envelope v2、v1 run-id 契约与 `--allow`/`--deny` 移除。当时判断依据：早期
无外部用户依赖，v1 CLI 契约与 session 模型不兼容（run-id vs session-id、权限
决策 vs follow-up），直接对齐 HTTP API 语义减少长期维护负担。

## 12. 开放问题

已定：默认端口为 4096（`--port 0` 使用临时端口）；SSE 不设重放缓冲区，
以 `resync_required` + 全量 resync 为契约（见 [versioned-rpc-contract.md](versioned-rpc-contract.md) §5）。

仍未决：

1. Workspace 卸载超时（v2）？
2. Token 轮换策略？
