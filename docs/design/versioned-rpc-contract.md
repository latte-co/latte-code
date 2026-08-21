# 版本化 RPC 与 Event Backpressure 契约

状态：**设计中**
日期：2026-08-21
关联：[server.md](server.md)、[server-client-integration.md](server-client-integration.md)

## 1. 背景与目标

server-client-integration 三个阶段合并后，HTTP+SSE 成为唯一的前端-引擎通道。
当前 API 能跑，但缺少正式契约：

- **DTO 不透明**：`CreateSessionRequest.binding` 是 `serde_json::Value`，客户端和
  server 各自手抄结构，没有编译期保障。
- **错误契约不完整**：server.md 列了 `unavailable` 但实现没有；error type 散在
  `error_response()` 辅助函数里，没有正式枚举。
- **SSE 语义未声明**：server.md 画了 `id: 42` 和 `Last-Event-ID`，但实现没有
  event ID、没有 replay。broadcast lag 时发 `resync_required`，但这是实现行为，
  不是契约。
- **版本政策缺失**：`/v1/` 前缀已存在，但没有稳定性承诺、演进规则或 deprecation
  窗口。

**目标**：把 HTTP+SSE API 从"能跑"变成"有契约"，让多端（CLI/TUI/Web/远程）可以
基于同一份正式契约开发，不复制 Engine authority。

**非目标**：
- 不换传输协议（不引入 gRPC、WebSocket、GraphQL）。
- 不做远程访问/多 server 集群（v2 范围）。
- 不做 OAuth/JWT 认证（v2 范围）。
- 不实现 event replay 或持久 event log（见 §5 决策）。

## 2. 版本化政策

### 2.1 URL 版本

- 所有端点在 `/v1/` 前缀下。`/health` 例外（无版本，基础设施探针）。
- **v1 稳定性承诺**：v1 端点的请求/响应结构、状态码、错误类型在 v1 生命周期内
  只做向后兼容变更（见 §7）。
- **v2 演进**：breaking change 走 `/v2/` 前缀，v1 和 v2 并存一个 deprecation
  窗口（至少 2 个 minor release），v1 在窗口后移除。
- **早期例外（已决策 2026-08-21）**：项目早期、无外部用户期间，允许 breaking change
  直接改 v1，不强制走 /v2/ 并存。但必须在 CHANGELOG 和 PR 描述中明确标注 breaking。
  第一个外部用户出现后，恢复正式 v2 政策。

### 2.2 什么是 breaking change

| 变更类型 | Breaking? | 处理 |
|---|---|---|
| 新增可选字段（`#[serde(default)]`） | 否 | 直接加到 v1 |
| 新增端点 | 否 | 直接加到 v1 |
| 新增错误 type | 否* | 直接加到 v1 |
| 新增 SSE event type | 否* | 直接加到 v1 |
| 移除/重命名字段 | 是 | v2 |
| 改变字段类型 | 是 | v2 |
| 改变状态码语义 | 是 | v2 |
| 移除端点 | 是 | v2 |
| 移除错误 type | 是 | v2 |

\* 客户端必须容忍未知错误 type 和未知 SSE event type（当作 `failed` / 忽略）。
这是 v1 契约的一部分：**未知变体不是 breaking change**。

### 2.3 版本协商

- 不使用 `Accept-Version` header 或 content negotiation。URL 前缀是唯一版本标识。
- server 不实现 v2 时，`/v2/` 请求返回 `404`（不是 `400`）。

## 3. 类型化 DTO 契约

### 3.1 原则

- 所有请求/响应 DTO 在 `latte-server` 中定义为 `pub struct`，派生
  `Serialize/Deserialize`。
- **禁止 `serde_json::Value` 作为字段类型**，除非该字段是真正的不透明 JSON
  （如 transcript payload）。
- DTO 与 `latte-core` 类型共享时，直接引用 core 类型（如 `ThreadId`、
  `ThreadSnapshot`），不重新定义。

### 3.2 当前需类型化的字段

| 端点 | 字段 | 当前类型 | 目标类型 |
|---|---|---|---|
| `POST /v1/workspaces/{ws}/sessions` | `binding` | `serde_json::Value` | `ThreadProviderBindingV2` |
| `POST /v1/sessions/{id}/model` | `binding` | `serde_json::Value` | `ThreadProviderBindingV2` |

`GET /v1/workspaces/{ws}/bindings` 的 `binding` 字段**已经是**
`ThreadProviderBindingV2`（`BindingCatalogEntry` 在 `latte-headless/src/registry.rs`
中已类型化），无需改动。

`ThreadProviderBindingV2` 已在 `latte-core` 定义且派生 `Serialize/Deserialize`，
直接引用即可。server 侧的 `resolve_thread_bound` 已经消费这个类型，类型化后
消除一层 JSON 序列化/反序列化。

### 3.3 DTO 清单

以下 DTO 已类型化，保持不变：

- `CreateWorkspaceRequest` / `WorkspaceResponse`
- `CreateSessionRequest` / `SessionCreatedResponse`
- `FollowUpRequest` / `SwitchModelRequest` / `CancelRequest`
- `QueueFollowUpRequest` / `ResolvePermissionRequest` / `ProvideInputRequest`
- `ErrorResponse` / `ErrorBody`
- `PaginationQuery`

以下响应体当前是 `serde_json::Value` 或 `Json<Value>`，应在 v1 内类型化
（非 breaking，因为 JSON 结构不变）：

- `GET /v1/sessions/{id}` → `SessionResponse { snapshot: ThreadSnapshot }`
- `POST /v1/sessions/{id}/follow-up` → `FollowUpResponse { accepted_revision: u64, workspace_id: String }`（`workspace_id` 是客户端订阅正确事件流的必需字段，当前实现已返回）
- `POST /v1/sessions/{id}/cancel` → `SessionResponse`
- `POST /v1/sessions/{id}/model` → `SessionResponse`
- `POST /v1/sessions/{id}/queue` → `QueueResponse { position: u64 }`
- `POST /v1/sessions/{id}/permissions/{req_id}` → `SessionResponse`
- `POST /v1/sessions/{id}/input` → `SessionResponse`
- `POST /v1/sessions/{id}/effects/{effect_id}/reconcile` → `SessionResponse`
- `PATCH /v1/sessions/{id}` → `SessionResponse`
- `POST /v1/sessions/{id}/fork` → `SessionResponse`
- `GET /v1/workspaces/{ws}/sessions` → `SessionListResponse { sessions, next_cursor }`
- `GET /v1/workspaces/{ws}/sessions/search` → `SessionListResponse`
- `GET /v1/workspaces/{ws}/bindings` → `BindingsResponse { bindings: Vec<BindingCatalogEntry> }`

## 4. 错误契约

### 4.1 错误类型枚举

正式定义 v1 错误 type（`ErrorBody.error_type` 的合法值）：

| error_type | HTTP 状态码 | 含义 | 可重试? |
|---|---|---|---|
| `rejected` | 400 | 请求参数无效 | 否 |
| `unauthorized` | 401 | Bearer token 缺失/无效 | 否 |
| `not_found` | 404 | session/workspace 不存在 | 否 |
| `idempotency_mismatch` | 422 | 同一 Idempotency-Key 搭配不同 payload | 否（客户端 bug，不应重试） |
| `conflict` | 409 | revision fence 冲突 | 是（刷新 snapshot 后重试） |
| `failed` | 500 | server 内部错误 | 是（指数退避） |

**移除**：server.md 中的 `unavailable`（503）未实现且无场景，从文档删除。

### 4.2 错误响应结构

```json
{
  "error": {
    "type": "conflict",
    "message": "revision mismatch: expected 42, current 43",
    "current_revision": 43
  }
}
```

- `current_revision` 仅在 `conflict` 时出现，供客户端刷新后重试。
- `message` 是人类可读的英文诊断，不保证稳定。客户端应基于 `type` 编程，
  不基于 `message`。

### 4.3 客户端错误映射

`ClientError` 枚举已覆盖所有错误类型，保持不变：

| ClientError | exit_code | code | 对应 HTTP |
|---|---|---|---|
| `Unreachable` | 71 | `server_unreachable` | 连接失败/超时 |
| `Usage` | 2 | `usage` | 400 |
| `NotFound` | 4 | `not_found` | 404 |
| `Unauthorized` | 70 | `unauthorized` | 401 |
| `Conflict` | 1 | `conflict` | 409 |
| `Internal` | 70 | `internal` | 5xx |
| `Failed` | 1 | `failed` | 其他 |

## 5. SSE Event Backpressure 契约

### 5.1 决策：Resync 是契约

**SSE 是通知通道，不是持久事件日志。** 客户端不依赖 SSE 事件的完整性或顺序性；
所有状态权威来自 `GET /v1/sessions/{id}` 的 snapshot。

这是当前实现的正式化，不是新设计。选择此方案的理由：

- **简单**：不需要 event ID、replay buffer、持久 log。
- **正确**：broadcast channel 天然不保证 delivery；假装保证会引入微妙 bug。
- **足够**：TUI/CLI 的交互模式是"收到通知 → 拉 snapshot"，不是"重放事件流"。

### 5.2 事件类型

| event | data 字段 | 语义 | 丢失影响 |
|---|---|---|---|
| `thread_changed` | `{session_id, revision}` | session 的持久状态变更 | 客户端下次 resync 时补齐 |
| `progress` | `{session_id, run_id, progress}` | 瞬态流式进度 | 丢失只影响 UI 流畅度，不影响正确性 |
| `resync_required` | `{}` | 客户端必须全量 resync | 不适用（这就是 resync 信号） |

**未知 event type**：客户端必须忽略（不报错、不断开）。这允许 v1 内新增 event type。

### 5.3 Backpressure 语义

- **Broadcast channel**：server 用 `tokio::sync::broadcast` 扇出事件。每个客户端
  有独立的 receiver。
- **Lag 处理**：receiver 落后于 channel 容量时，server 发 `resync_required`，
  客户端全量拉 snapshot。不断开连接。
- **慢消费者隔离**：一个慢客户端不影响其他客户端（broadcast 天然隔离）。
- **无 event ID**：SSE 事件不带 `id:` 字段。客户端不发送 `Last-Event-ID`。
- **无 replay**：server 不缓存历史事件。重连后客户端立即 resync（§8.1 of
  server-client-integration.md）。

### 5.4 心跳

- server 每 15s 发送 SSE keep-alive 注释行（`: heartbeat`），防止中间代理超时。
- 客户端 30s 无事件/心跳判定断线，触发重连 + resync。

### 5.5 与 server.md 的对齐

server.md §5.5 当前描述了 `id: 42` 和 `Last-Event-ID`，与实现不符。修正为：
- 移除 `id:` 行和 `Last-Event-ID` 描述。
- 移除"慢客户端会被断开"（实际是 lag → resync，不断开）。
- 补充"未知 event type 必须忽略"。

## 6. 分页契约

### 6.1 当前实现（v1 实际行为）

- **List**（`GET /v1/workspaces/{ws}/sessions`）：完全忽略 `PaginationQuery`（cursor 和
  limit 都不生效），返回 workspace 全部 session，`next_cursor` 恒为 `null`。
- **Search**（`GET /v1/workspaces/{ws}/sessions/search`）：只消费 `limit`，
  `limit.unwrap_or(50).clamp(1, 200)`；忽略 cursor，`next_cursor` 恒为 `null`。
- **Exact-title**（`GET /v1/workspaces/{ws}/sessions/exact-title`）：同 Search。
- `limit=0` 会被 clamp 为 1（不是空页）。
- cursor 参数被接受但不生效（客户端传了不报错，但不影响结果）。

### 6.2 目标行为（Phase B 实施）

当前实现是 single-page 的。正式分页契约需要 server 侧实现 cursor 分页：

- cursor 是不透明字符串，客户端不解析。
- `next_cursor` 为 `null` 表示无更多页。
- 默认 50，最大 200。超过 200 截断为 200。
- `limit=0` 返回空页（不是错误）。

**实施要求**：Phase B 必须先实现 server 侧 cursor 分页（list + search + exact-title），
再把 §6.2 升格为正式契约。在实现落地前，§6.1 是 v1 的实际契约。

## 7. 兼容性规则

### 7.1 向后兼容变更（直接加到 v1）

- 新增可选字段（`#[serde(default)]`）
- 新增端点
- 新增错误 type（客户端容忍未知 type）
- 新增 SSE event type（客户端容忍未知 type）
- 新增枚举变体（客户端容忍未知变体）

### 7.2 Breaking 变更（走 v2）

- 移除/重命名字段或端点
- 改变字段类型
- 改变状态码语义
- 收紧校验（原来合法的请求变非法）

### 7.3 客户端兼容义务

v1 客户端必须：

- 容忍未知 JSON 字段（`serde` 默认行为）。
- 容忍未知错误 type（当作 `failed`）。
- 容忍未知 SSE event type（忽略）。
- 容忍未知枚举变体（当作未知/默认）。

## 8. 实施计划

### Phase A：文档对齐（~0.5 天）

- [ ] 修正 server.md §5.5：移除 event ID/Last-Event-ID，补充 resync 契约
- [ ] 修正 server.md §5.6：移除 `unavailable`，对齐错误类型枚举
- [ ] 本设计文档合并到 `docs/design/`

### Phase B：DTO 类型化 + 分页实现（~1.5 天）

- [ ] `CreateSessionRequest.binding` / `SwitchModelRequest.binding` 改为
      `ThreadProviderBindingV2`（`BindingCatalogEntry.binding` 已经是该类型，无需改动）
- [ ] 响应体从 `Json<Value>` 改为类型化 struct（§3.3 清单）
- [ ] 实现 server 侧 cursor 分页（list + search + exact-title），使 §6.2 升格为正式契约
- [ ] UT + E2E 覆盖

### Phase C：契约测试（~1 天）

- [ ] 新增 contract test：每个端点的请求/响应 JSON schema 快照
- [ ] 新增 contract test：错误类型枚举完整性（含 422 idempotency_mismatch）
- [ ] 新增 contract test：SSE event type 完整性
- [ ] CI 中运行 contract test，防止意外 breaking change

**总计：~3 个工作日。**

## 9. 已决策

- **v2 触发条件（已决策 2026-08-21）**：项目早期、无外部用户期间，允许 breaking change
  直接改 v1，不强制走 /v2/ 并存。但必须在 CHANGELOG 和 PR 描述中明确标注 breaking。
  第一个外部用户出现后，恢复"任何 breaking 走 v2"的正式政策。
- **binding 类型化的迁移**：现有 session 的 binding 已持久化为 JSON。类型化后
  反序列化是否兼容？（`ThreadProviderBindingV2` 派生了 `Deserialize`，且字段
  未变，应该兼容，但需要测试验证。）
