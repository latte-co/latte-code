# Server 客户端集成方案（v2.5 修订）

状态：**已全部实现并合并（Phase 0/1/2/3 完成）**
日期：2026-08-17（v2.5 修订）；2026-08-21 收尾
评审：Monday × Friday 联合评审（基于 `32f902e`），11 个 P0 已闭合；Friday 五轮复审共 15 组 blocker 已修复

## 1. 背景与目标

当前三个前端（CLI `run`、TUI、HTTP server）各自在进程内嵌入 `EngineHandle`，互不共享 session 状态。Server 设计文档已将 CLI/TUI 画为 HTTP 客户端，但尚未实现。

**目标**：CLI `run` 和 TUI 统一走 server 的 HTTP + SSE 协议，server 成为唯一的 engine 宿主。多端共享 session、状态和事件。

**非目标**：远程访问、多 server 集群/owner routing、WebSocket 替代 SSE、浏览器/IDE attach。

**Breaking change 决策**：CLI 命令契约不保留 v1 兼容。`run/show/list/resume` 直接改为 session 语义，JSON envelope 升级 v2，不提供 v1 迁移层。旧 v1 run 的 `show/list/resume` 从 CLI 移除。

## 2. 架构

### 2.1 三种运行模式

```
模式 1：TUI（默认）
┌─────────────────────────────────────┐
│ latte-code（TUI 进程）               │
│  ┌──────────┐    HTTP+SSE           │
│  │ TUI      │◄───(随机端口)──┐      │
│  │ (专用线程 │               │      │
│  │  reducer)│               ▼      │
│  └──────────┘        ┌──────────┐  │
│                      │ Server   │  │
│                      │ (axum)   │  │
│                      └────┬─────┘  │
│                           │        │
│                      ┌────▼─────┐  │
│                      │ Engine   │  │
│                      └──────────┘  │
└─────────────────────────────────────┘

模式 2：CLI run（默认）
┌─────────────────────────────────────┐
│ latte-code run "prompt"（CLI 进程）  │
│  ┌──────────┐    HTTP+SSE           │
│  │ CLI      │◄───(随机端口)──┐      │
│  │ (blocking│               │      │
│  │  client) │               ▼      │
│  └──────────┘        ┌──────────┐  │
│                      │ Server   │  │
│                      │ (axum)   │  │
│                      └────┬─────┘  │
│                           │        │
│                      ┌────▼─────┐  │
│                      │ Engine   │  │
│                      └──────────┘  │
└─────────────────────────────────────┘

模式 3：独立 server + 本地客户端
┌──────────────────────┐     HTTP+SSE    ┌──────────────────┐
│ latte-code serve     │◄────────────────│ 另一个 TUI/CLI   │
│ (server 进程)        │                 │ (同机 loopback)  │
│  Server (axum:4096)  │                 └──────────────────┘
│       │              │
│  Engine (in-proc)    │
└──────────────────────┘
```

**模式 2 修正**：CLI run 的内嵌 server 与 TUI 一样 bind `127.0.0.1:0`（随机端口），不使用"无监听"模式。所有前端统一走 HTTP，包括同进程。

### 2.2 设计原则

1. **Server 是库，不是服务。** 客户端进程内嵌 server，同进程内 HTTP 通信。
2. **TUI reducer 不变。** `ThreadProjectionClient` trait 是已有边界，HTTP client 是另一个实现。
3. **CLI `run` 映射到 session 模型。** 一次性任务 = 创建 session + 流式观察 + 退出码映射。
4. **一套 HTTP 协议覆盖本地和独立 server。** 内嵌 server 和独立 server 用同一套路由和 DTO。
5. **不保留 in-process engine 直连路径。** 所有前端统一走 HTTP，即使同进程。
6. **Session 共享靠持久化存储。** SQLite/JSONL 是权威，server 只提供 live event。

### 2.3 同进程走 HTTP 的理由

- **单一路径**：内嵌和独立 server 用同一套 client 代码，不维护 in-process 和 HTTP 两套。
- **协议即边界**：HTTP API 是公开契约，强制 engine authority 不被绕过。
- **性能可接受**：loopback HTTP 延迟 < 1ms，对 TUI 交互无感知。

## 3. 连接模式

### 3.1 CLI 接口

```
latte-code                                    # TUI，内嵌 server（随机端口）
latte-code --server http://127.0.0.1:port     # TUI，连独立 server
latte-code run "prompt"                       # CLI run，内嵌 server（随机端口）
latte-code run --server http://127.0.0.1:port "prompt"  # CLI run，连独立 server
latte-code serve                              # 独立 server，端口 4096
latte-code serve --port 8080                  # 独立 server，自定义端口
```

### 3.2 连接规则

| 场景 | 命令 | Server 位置 | Token 来源 |
|---|---|---|---|
| TUI（默认） | `latte-code` | 同进程内嵌（随机端口） | 进程内已知 |
| TUI 连独立 server | `latte-code --server http://...` | 独立进程 | `server.token` 或 `--token` |
| CLI run（默认） | `latte-code run "prompt"` | 同进程内嵌（随机端口） | 进程内已知 |
| CLI run 连独立 server | `latte-code run --server http://... "prompt"` | 独立进程 | `server.token` 或 `--token` |
| 独立 server | `latte-code serve [--port N]` | 独立进程 | 写 `server.token` |

### 3.3 Token 管理

- `serve` 写 `$LATTE_CODE_HOME/server.token`（Unix 0600 权限；Windows 无 0600 等价，仅依赖文件系统 ACL）。
- `--server` 模式下，客户端默认读 `server.token`；也可以 `--token <token>` 显式指定。
- 内嵌 server 的 token 只在进程内，不落盘。

### 3.4 多 Server（收窄）

v1 只支持以下语义，不做 owner routing：

- **Durable state 共享**：所有 server 共享同一个 SQLite + JSONL。任何 server 都能读任何 session 的 durable state。
- **Live mutation 必须连 owner server**：mailbox、active cancellation、progress 都是 process-local（`thread.rs:139-150, 446-465`）。连 server B 操作 server A 正在跑的 session，queue/cancel/input 会失败（`InvalidState`）。
- **新 session 创建**：任何 server 都可以创建新 session（durable write 不依赖 live runtime state）。
- **Live event 隔离**：每个 server 进程有自己的 SSE 广播。要观察 live event，连 owner server。

## 4. API 扩展

现有端点已覆盖大部分操作。需要新增/修改以下端点：

### 4.1 Binding 发现

```
GET /v1/workspaces/{workspace_id}/bindings
Response: 200 { "bindings": [BindingCatalogEntry, ...] }
```

```rust
pub struct BindingCatalogEntry {
    pub provider_name: String,
    pub model: String,
    pub name: Option<String>,
    pub is_default: bool,
    pub binding: ThreadProviderBindingV2,
}
```

返回该 workspace 配置中所有可用的 provider binding，包含 TUI model picker 需要的 `name` 和 `is_default`（`ProviderModelEntry` 已有这两个字段，`registry.rs:310`）。

**Server 侧**：`BuiltWorkspace` 增加 `registry: Arc<ProviderRegistry>` 字段（当前只有 `engine` + `runtime`，registry 被 factory 闭包消费后不可达）。handler 调用 `registry.thread_binding_catalog(&engine.tool_descriptors())` 生成完整 binding 列表。

`ProviderRegistry` 新增方法：

```rust
pub fn thread_binding_catalog(
    &self,
    tools: &[ToolDescriptor],
) -> Result<Vec<BindingCatalogEntry>, RegistryError>
```

遍历 `model_catalog()`，对每条调用 `thread_binding_for_model`（需要 `ToolDescriptor`，通过 `engine.tool_descriptors()` 获取），组装 `BindingCatalogEntry`。

### 4.2 Session 创建：focus + 幂等

`CreateSessionRequest` 修改为：

```rust
pub struct CreateSessionRequest {
    /// 客户端生成的稳定 session ID（UUID v7）。用于零延迟 assigned 反馈和 crash-safe 幂等。
    pub thread_id: ThreadId,
    /// 客户端生成的稳定 command ID（UUID v7）。映射到 Idempotency-Key。
    pub command_id: ThreadCommandId,
    pub prompt: String,
    pub binding: serde_json::Value,
    #[serde(default)]
    pub focus: Option<String>,
}
```

**focus 持久化与 schema 迁移**（P0-2 闭合）：

- `threads_v2` 表新增 `focus TEXT` 列（可空）。**Schema 版本 11 → 12**：fresh schema 含 focus 列；`ALTER TABLE threads_v2 ADD COLUMN focus TEXT` 迁移；`SCHEMA_VERSION` 从 11 升到 12（`storage.rs:16`）。旧行读为 `NULL`。
- **Legacy import 兼容**：`import_legacy_database` 当前对 `threads_v2` 执行 `INSERT ... SELECT *`（`storage.rs:837-840`），目标库多一列后列数不匹配。必须改为显式列清单，旧库行映射 `NULL AS focus`。v9/v10 的 `threads_v2` 还缺 `parent_thread_id`，也需按 source version 补 `NULL AS parent_thread_id`。测试覆盖：v11 原地升级、v9-v11 导入。
- **Fork 继承 focus**：当前 fork 只复制 title/workspace/binding（`storage.rs:1902-1918`），不继承 focus。必须改为 `SELECT title,workspace_root,binding_json,focus FROM threads_v2 WHERE thread_id=?` 并在 INSERT 中包含 focus。否则 focused session 分叉后第一次 follow-up 静默退回 workspace 根上下文。测试覆盖：fork 后 focus 持久化 + snapshot 可见。
- `ThreadSnapshot` 新增 `focus: Option<String>` 字段。
- follow-up 恢复：`history_with_prompt` 从 session 记录读回 focus，传给 `context::build`。

**为什么不能放 service 字段**：`ThreadRuntimeService` 是 per-workspace 共享（`Arc<ThreadRuntimeService>` in `WorkspaceInstance`），不同 session 有不同 focus。service 级字段会串。

**幂等创建契约**（P0-5 闭合）：

- **Digest 绑定完整命令身份**：`digest = SHA256(canonical(operation_kind, protocol_version, workspace_identity, thread_id, prompt, binding, normalized_focus))`。不只 hash prompt+binding+focus——同 `command_id` 以相同 prompt、不同 `thread_id` 重试会错误 replay 另一个 session。
- **Idempotency-Key 与 body command_id 一致**：HTTP header `Idempotency-Key` 必须等于 body `command_id`，否则 400 拒绝。只保留一个身份来源，避免内存层和 durable 层各认一套。
- **`ThreadCommand::Start` 增加 focus 字段**：当前 envelope 缺 focus（`thread.rs:279-285`），目标定义需补上，否则 digest 计算和 replay 都不完整。
- 同 `command_id` + 同 digest → replay 已有 acceptance（200，非 202）
- 同 `command_id` + 不同 digest → 409 Conflict
- 同 `thread_id` 已存在（非 command_id 重放）→ 409 Conflict

**Crash runner 状态闭合**（P0-5 续）：

**竞态闭合路径**：当前 `start_one` 先 `acquire(thread_id)` 再 engine create（`thread.rs:304-316`）。crash 后立即重启时旧 lease 最长仍有效 60s，`acquire` 会先报 `EngineUnavailable`，根本进不到 durable dedup。必须固定为：

1. **dedup lookup 先于 lease acquire**：engine create 入口先查 `thread_command_dedup_v2`（无 lease），命中 → 返回 `Replayed`（不 acquire、不启动 runner）
2. **miss 后 acquire + 事务内二次检查**：dedup miss → acquire lease → 在同一事务内二次检查 dedup（防止并发请求同时 miss 后重复创建）→ 插入 dedup 记录 → 返回 `Created`

```rust
pub enum CreateOutcome {
    Created(ThreadSnapshot),
    Replayed(ThreadSnapshot),
}
```

- **只有 `Created` 启动 provider runner**。`Replayed` 不启动 runner——崩溃前的 provider task 已消失，重放只恢复 admission 状态。
- **Recovery owner**：`recover_at` 当前只在 storage open/import 时调用（`storage.rs:335,876`），不会在 lease 60s 到期后自动再扫。必须在 server 侧增加一个**周期 sweeper task**（如每 30s），由 server 生命周期 owner 启停。
- **Recovery 必须唤醒在线客户端**：当前 `Storage::recover_at` 返回 `Result<()>`，丢弃 `ThreadCommitResponse`（`storage.rs:4162-4170`），不经过 `EngineHandle::finish_thread_response` 的事件广播（`lib.rs:722-731`）。sweeper 必须调用 engine-level recovery API（如 `recover_expired_leases()`），该 API 在恢复事务提交后向对应 workspace 广播 `ResyncRequired`（或 committed `ThreadEventEnvelope`），让保持 SSE 连接的客户端收到 wake 并重拉 snapshot。测试：客户端保持 SSE → runner crash → lease 到期 → recovery → 客户端收到 wake → 重拉 snapshot 并退出（不能靠手工重连过关）。
- 测试覆盖：活 owner 持续续租不得误恢复 / owner crash 后只恢复一次 / 多 server sweeper 竞争（durable dedup 保证幂等）。
- **Exactly-once 范围限定**：只保证 "session admission exactly-once"（不重复创建 session）。create replay 不重放 external effect；Started 未确认结果转 `Unknown`，需显式 reconcile（reconciliation 不能补偿重复副作用，只能标记 Unknown 后人工/自动确认）。

**实现**：engine 的 `create_started_thread_v2` 接入已有的 `thread_command_dedup_v2` 表和 `commit_thread_run_update` 的 digest replay 逻辑（`storage.rs:499-503, 2005-2031`）。`ThreadCommandEnvelope { command_id, command: Start { thread_id, prompt, binding, focus } }` 为目标定义（当前 core 的 `Start` 缺 focus，需补上），把 create 路径接入。

内存 HTTP ledger（`ServerState.idempotency`）保留作为同进程快速 replay 路径，但不承担 crash correctness。

### 4.3 Session 重命名

```
PATCH /v1/sessions/{session_id}
Body: { "title": "新标题" }
Response: 200 { "snapshot": {...} }
```

映射到 `engine.rename_thread_session_v2`。该方法返回 `ThreadSessionSummary`（非 `ThreadSnapshot`），server 需额外调用 `thread_snapshot_v2` 获取完整 snapshot 后返回。

**事件广播**：rename 后发送 `ServerEvent::ThreadChanged { session_id, revision }`，让其他客户端刷新。

### 4.4 Session 分叉

```
POST /v1/sessions/{session_id}/fork
Body: { "title": "可选标题" }
Response: 200 { "snapshot": {...} }
```

映射到 `engine.fork_thread_session_v2`。调用方生成 fork 的 `ThreadId`（`ThreadId::from_uuid(Uuid::now_v7())`）和 `now_ms` 时间戳。

**事件广播**：fork 后发送 `ServerEvent::ThreadChanged { session_id: fork_id, revision }`。

### 4.5 Progress 事件接线

当前 `ServerEvent::Progress` 变体存在但未接线。需要在 `WorkspaceInstance::new` 中为 `ThreadRuntimeService` 设置 `ThreadProgressSink`。

**标识维度修正**（P0-7）：`ThreadTransientProgress` 的变体只携带 `run_id`，不携带 `thread_id`。workspace SSE 混多 session，仅靠 run_id 无法可靠 demux。

修法：`ThreadProgressSink` trait 签名改为：

```rust
pub trait ThreadProgressSink: Send + Sync {
    fn observe(&self, thread_id: ThreadId, progress: ThreadTransientProgress);
}
```

`ProviderProgress`（`thread.rs:1757`）增加 `thread_id` 字段（构造点在 service 的 run 启动路径上，thread_id 在作用域内）。

`ServerEvent::Progress` 改为：

```rust
Progress {
    session_id: String,   // thread_id
    run_id: String,
    progress: serde_json::Value,
},
```

**接线点**：`WorkspaceInstance::new`（`workspace.rs:55`），在 `Arc::new(runtime)` 之前调用 `runtime.with_progress_sink(sink)`：

```rust
let sink: Arc<dyn ThreadProgressSink> = {
    let event_tx = event_tx.clone();
    Arc::new(move |thread_id: ThreadId, progress: ThreadTransientProgress| {
        let run_id = match &progress {
            ThreadTransientProgress::ProviderAttempt { run_id, .. }
            | ThreadTransientProgress::AssistantDelta { run_id, .. }
            | ThreadTransientProgress::ToolProgress { run_id, .. } => run_id.to_string(),
        };
        let _ = event_tx.send(ServerEvent::Progress {
            session_id: thread_id.to_string(),
            run_id,
            progress: serde_json::to_value(&progress).unwrap_or_default(),
        });
    })
};
let runtime = runtime.with_progress_sink(sink);
```

### 4.6 SSE 事件桥接（P0-4）

当前事件桥（`workspace.rs:128`）只转发 `LifecycleChanged`，其余 4 种 `ThreadEvent` 被 `_ => continue` 丢弃。这导致 HTTP TUI 的 transcript 只在生命周期边界刷新，tool 结果要等整轮结束。

修法：桥接转发所有 durable event 为 wake-up 信号：

```rust
let server_event = match event.event {
    ThreadEvent::LifecycleChanged { .. } => ServerEvent::ThreadChanged {
        session_id: event.thread_id.to_string(),
        revision: event.revision,
    },
    ThreadEvent::TranscriptAppended { .. }
    | ThreadEvent::RunLinked { .. }
    | ThreadEvent::BindingChanged { .. }
    | ThreadEvent::ReconciliationRequired { .. } => ServerEvent::ThreadChanged {
        session_id: event.thread_id.to_string(),
        revision: event.revision,
    },
};
```

客户端收到 `ThreadChanged` 后无条件刷新 snapshot（SSE 只作 wake-up，不依赖事件 payload 的完整性）。

## 5. TUI HTTP Client

### 5.1 架构

```
TUI Reducer（不变）
  │
  ├── ThreadProjectionClient（trait）
  │     └── HttpProjectionClient（新）
  │           ├── GET  /v1/workspaces/{ws}/sessions        → snapshots()
  │           ├── GET  /v1/sessions/{id}                   → session()
  │           ├── GET  /v1/workspaces/{ws}/sessions/search → search_session_catalog()
  │           └── poll() ← sticky 原子位 swap（Idle/Dirty/Lagged）
  │
  ├── Action Sink（FnMut(ThreadUiAction) → Result）
  │     └── HttpActionSink（新）
  │           ├── 始终返回 Ok(())，绝不返回 Err
  │           ├── 发送到 bounded action queue（固定 worker，per-session 顺序）
  │           └── 6 种 ThreadUiFeedback 全覆盖
  │
  └── Feedback/Progress
        ├── SSE 线程（专用 OS thread，reqwest::blocking）
        │     ├── ThreadChanged → 置 projection Dirty 原子位
        │     ├── Progress      → scoped accumulator（按 thread_id+run_id+kind 分桶）
        │     └── ResyncRequired → 置 projection Lagged 原子位
        └── Action worker（固定数量 OS 线程）
              └── SubmissionAssigned/Result/ModelSwitch/... → feedback channel
```

### 5.2 HttpProjectionClient

`ThreadProjectionClient` trait 有 7 个方法（2 个必须实现，5 个有默认实现）。`HttpProjectionClient` 覆写全部 7 个：

| 方法 | HTTP 映射 |
|---|---|
| `snapshots()` | `GET /v1/workspaces/{ws}/sessions` |
| `session_catalog()` | `GET /v1/workspaces/{ws}/sessions`（客户端转 summary） |
| `session(id)` | `GET /v1/sessions/{id}` |
| `exact_session_catalog(query)` | `GET /v1/workspaces/{ws}/sessions/search?q={query}` + 客户端精确过滤（UUID 精确 + title 精确） |
| `exact_session(query)` | `exact_session_catalog` + `session` |
| `search_session_catalog(query)` | `GET /v1/workspaces/{ws}/sessions/search?q={query}`（模糊） |
| `poll()` | sticky 原子位 `swap` → `ThreadProjectionPoll` |

```rust
pub struct HttpProjectionClient {
    http: reqwest::blocking::Client,
    base_url: String,
    token: String,
    workspace_id: String,
    /// Sticky 唤醒位：SSE 线程置位，poll() swap 取位并清零。
    /// 5 态映射：DIRTY→Event, LAGGED→Lagged, IDLE→Empty,
    /// CLOSED→Closed, ERROR→Error。
    wake_state: Arc<AtomicU8>,
    /// ERROR 状态的错误消息（AtomicU8 装不下文本，单独存储）。
    error_msg: Arc<Mutex<Option<String>>>,
}

const WAKE_IDLE: u8 = 0;
const WAKE_DIRTY: u8 = 1;
const WAKE_LAGGED: u8 = 2;
const WAKE_CLOSED: u8 = 3;
const WAKE_ERROR: u8 = 4;
```

- GET 请求使用 `reqwest::blocking`（在 `spawn_blocking` 线程上调用，不与 tokio runtime 冲突）。
- `poll()` 做 `swap(WAKE_IDLE)`，映射到 `ThreadProjectionPoll`：`DIRTY → Event`，`LAGGED → Lagged`，`IDLE → Empty`，`CLOSED → Closed`，`ERROR → Error(msg)`（从 `error_msg` 读取）。
- SSE 线程收到 `ThreadChanged` → `fetch_max(DIRTY, Release)`；收到 `ResyncRequired` → `fetch_max(LAGGED, Release)`；断线 → `fetch_max(CLOSED, Release)`；重连失败 → 先写 `error_msg`（`lock` + `store`），再 `store(ERROR, Release)`。
- `poll()` 用 `swap(WAKE_IDLE, Acquire)` 读取，ERROR 时从 `error_msg` 读取文本（`lock` + `take`）。先写 msg 再 store state，保证 poll 看到 ERROR 时 msg 已就绪。
- **重连成功**：进入读循环前用 `store(LAGGED)` **替换**旧状态（不是 `fetch_max`）——`LAGGED` 隐含全量 resync，覆盖任何未消费的 `DIRTY`/`CLOSED`/`ERROR`。这避免了 `fetch_max` 让高编号的 `ERROR(4)` 吞掉重连后的 `LAGGED(2)` 的竞态。
- **重连**：SSE 线程带退避重连（1s 初始，指数退避上限 30s），重连成功后先 `store(LAGGED)` 再继续接收事件。
- **确定性 UT**：`Error/Closed → reconnect success（发生在 poll 前）→ poll 必须观测 Lagged`；并发 `Dirty` 不丢。

### 5.3 HttpActionSink（P0-6）

**核心契约：sink 始终返回 `Ok(())`，绝不返回 `Err`。**

`apply_thread_actions` 尾部（`thread.rs`）：`action => sink(action).map_err(TuiError::Action)?`——sink 返回 Err 会直接退出整个 TUI。所有 HTTP 失败必须通过 feedback 通道回送。

**非阻塞**：action 发送到 bounded action queue，固定数量 worker（如 2 个）执行阻塞 HTTP POST，TUI 线程不等待。per-session 串行化（同一 session 的 action 按序执行）。

**6 种 `ThreadUiFeedback` 全覆盖**：

| Action | HTTP 映射 | Feedback |
|---|---|---|
| `Start` | POST /sessions（client thread_id + command_id） | 立即 `SubmissionAssigned`（client 生成 thread_id），POST 完成后 `SubmissionResult` |
| `StartWithModel` | POST /sessions（从 /bindings catalog 选取完整 binding 透传） | 同上 |
| `FollowUp` | POST /v1/sessions/{id}/follow-up（带 expected_thread_revision） | `SubmissionResult` |
| `QueueFollowUp` | POST /v1/sessions/{id}/queue | `SubmissionResult` |
| `Cancel` | POST /v1/sessions/{id}/cancel | `Command` |
| `ProvideInput` | POST /v1/sessions/{id}/input | `InputSubmissionResult` |
| `ResolvePermission` | POST /v1/sessions/{id}/permissions/{req_id} | `Command` |
| `SwitchModel` | POST /v1/sessions/{id}/model（带 expected_thread_revision） | `ModelSwitchResult` |
| `ReconcileUnknown` | POST /v1/sessions/{id}/effects/{effect_id}/reconcile | `Command` |
| `RenameSession` | PATCH /v1/sessions/{id} | `SessionManagement(Updated)` |
| `ForkSession` | POST /v1/sessions/{id}/fork | `SessionManagement(Forked)` |
| `RefreshSnapshots` / `ShowSessions` / `SearchSessions` / `OpenSession` / `Quit` | UI 内部，无 HTTP | 无 |

**409 Conflict 处理**：revision fence 冲突时，发送 feedback 触发 snapshot 刷新（不报错退出）。**注意**：现有 reducer 中 `SubmissionResult(Err)` / `InputSubmissionResult(Err)` 会触发刷新，但 `Command(Err)` 和 `ModelSwitchResult(Err)` 只显示错误（`thread.rs:4734-4748`）。HTTP worker 在收到 409 时必须同时置 projection dirty 原子位，确保下一次 `poll()` 返回 `Event` 触发 `RefreshSnapshots`。

**StartWithModel 不能"客户端合成 binding"**：`thread_binding_for_model` 需要 `&[ToolDescriptor]`（`registry.rs:387`），客户端没有 engine 的 tool descriptors。客户端从 `/bindings` catalog 选取完整 binding 透传（`resolve_thread_bound` 会重新派生比对，透传安全）。

### 5.4 执行模型（P0-11 闭合）

迁移前 `#[tokio::main]` → async `execute()` → 同步 `execute_tui()`，TUI loop 占住 Tokio worker。`Handle::block_on` 在同 runtime 线程上会 panic；`std::thread::spawn` + `handle.join()` 仍在 Tokio worker 上阻塞。Phase 1 落地后 `execute_tui` 本身是 async，TUI loop 经 `spawn_blocking` 运行在阻塞线程池（见下方固定执行模型）。

**固定执行模型**：

```
主 Tokio runtime（多线程）
  ├── 内嵌 server task（serve_with_shutdown + 私有 shutdown channel）
  └── execute() 通过 spawn_blocking 把 TUI 放到阻塞线程池

spawn_blocking 线程（TUI loop，同步 crossterm + ratatui）
  ├── GET：reqwest::blocking（同线程，loopback ~1ms）
  ├── POST：发送到 bounded action queue（不 spawn 独立线程）
  └── poll()：sticky dirty/lagged 原子位 swap

Action worker（固定数量 OS 线程，如 2 个）
  └── 从 bounded action queue 取 action → reqwest::blocking POST → feedback channel
      └── per-session 顺序：同一 session 的 action 串行化（按 thread_id 分桶）

SSE 线程（std::thread::spawn，专用）
  └── reqwest::blocking GET /events（流式）
        ├── ThreadChanged → 置 projection dirty 原子位
        ├── Progress → scoped accumulator（按 thread_id+run_id+kind 分桶）
        └── ResyncRequired → 置 projection lagged 原子位
```

**关键决策**：

1. **TUI loop 用 `spawn_blocking(...).await`**，不是 `std::thread::spawn` + `handle.join()`。`spawn_blocking` 把同步 TUI 放到 Tokio 的阻塞线程池，不占 async worker；`execute()` 可以 `await` 它的完成。
2. **Bootstrap GET 也在 `spawn_blocking` 中**：`resolve_workspace_blocking` 不能在 Tokio worker 上直接调用（单 worker runtime 会卡死内嵌 server）。
3. **Action worker 用固定 OS 线程 + `reqwest::blocking`**（不用 Tokio task + blocking client，会阻塞 async worker）。bounded queue + 固定 worker，per-session 串行化。
4. **bounded queue 满时语义**：sink 仍返回 `Ok(())`，但立即发送与 action 对应的失败 feedback（如 `SubmissionResult(Err("action queue full"))`），不静默丢弃。
5. **Projection 唤醒用 sticky 原子位**：`AtomicU8` 状态位（`Idle | Dirty | Lagged | Closed | Error`，5 态），SSE 线程 `fetch_max` 置位（保证 `LAGGED > DIRTY > IDLE` 优先级），重连成功用 `store(LAGGED)` 替换旧状态，TUI `poll()` 时 `swap` 取位并清零。
6. **Progress 用 scoped accumulator**：按 `(thread_id, run_id, kind/tool_name)` 分桶，`AssistantDelta` 有界追加，`ProviderAttempt`/`ToolProgress` 取最新。向 reducer 交付前按当前 active thread demux。不用全局 latest-slot（会丢 chunk + 串 session）。
7. **不用 `tokio::runtime::Runtime::new()` 在 TUI 侧**：client 侧纯 `reqwest::blocking` + `std::thread`，避免双 runtime 复杂性。

### 5.5 Binary 接线

`execute_tui` 改为 async。**Ownership 形状**：`ClientWorkersOwner::start()` 返回 `(Owner, Inputs)`——Owner（cancel token + JoinHandles）留在 async 侧；Inputs（wake_state、action_queue sender、两个**独占 Receiver**）整体 move 进 TUI closure。`run_with_feedback_and_progress` 接收 `&Receiver`，closure 内以引用调用。**Client cancel token 与 server shutdown token 分开**。

```rust
async fn execute_tui() -> i32 {
    // 1. 解析 --server / --token 参数
    let server_config = resolve_server_config(&args)?;

    // 2. 连接或内嵌 server（bootstrap GET 在 spawn_blocking 中）
    let (base_url, token, workspace_id, server_shutdown) = match server_config {
        ServerConfig::Remote { url, token } => {
            let (url, token, ws) = tokio::task::spawn_blocking(move || {
                resolve_workspace_blocking(&url, &token, &root)
            }).await??;
            (url, token, ws, None)
        }
        ServerConfig::Embedded => {
            let (state, token) = build_server_state(&root, &storage_home)?;
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let port = listener.local_addr()?.port();
            let shutdown = CancellationToken::new();
            let server_shutdown = shutdown.clone();
            let server_task = tokio::spawn(async move {
                latte_server::serve_with_shutdown(state, listener, async move {
                    server_shutdown.cancelled().await;
                }).await
            });
            let url = format!("http://127.0.0.1:{port}");
            let (url, token, ws) = tokio::task::spawn_blocking(move || {
                resolve_workspace_blocking(&url, &token, &root)
            }).await??;
            (url, token, ws, Some((shutdown, server_task)))
        }
    };

    // 3. 启动 client workers，拆分为 Owner（async 侧）+ Inputs（move 进 TUI）
    let (client_owner, inputs) = ClientWorkersOwner::start(&base_url, &token, &workspace_id);
    let ClientWorkerInputs { wake_state, action_queue, feedback_rx, progress_rx } = inputs;

    // 4. TUI loop 在 spawn_blocking 线程上运行（Inputs 整体 move）
    let tui_result = tokio::task::spawn_blocking(move || {
        let mut projection = HttpProjectionClient::new(&base_url, &token, &workspace_id, wake_state);
        let sink = HttpActionSink::new(&base_url, &token, &workspace_id, action_queue);
        latte_tui::run_with_feedback_and_progress(
            &mut projection, startup, sink, &feedback_rx, &progress_rx,
        )
    }).await;

    // 5. 退出：先 cancel + join client workers（所有模式），再 signal server（仅 embedded）
    client_owner.shutdown().await;  // cancel token + join SSE/action OS threads
    if let Some((shutdown, server_task)) = server_shutdown {
        shutdown.cancel();
        let _ = server_task.await;  // 确认端口释放
    }
    tui_result
}
```

**ClientWorkersOwner / ClientWorkerInputs**：

```rust
struct ClientWorkersOwner {
    cancel: CancellationToken,
    handles: Vec<std::thread::JoinHandle<()>>,
}
impl ClientWorkersOwner {
    fn start(url: &str, token: &str, ws: &str) -> (Self, ClientWorkerInputs) {
        // 启动 SSE 线程 + action worker 线程，返回 (owner, inputs)
    }
    async fn shutdown(self) {
        self.cancel.cancel();
        // 通过 spawn_blocking join OS threads，避免阻塞 async worker
        tokio::task::spawn_blocking(move || {
            for handle in self.handles { let _ = handle.join(); }
        }).await.ok();
    }
}

struct ClientWorkerInputs {
    wake_state: Arc<AtomicU8>,       // cloneable，projection 用
    action_queue: Sender<Action>,    // cloneable，sink 用
    feedback_rx: Receiver<ThreadUiFeedback>,       // 独占，move 进 TUI
    progress_rx: Receiver<ThreadTransientProgress>, // 独占，move 进 TUI
}
```

**退出顺序**（分阶段，两个独立 token）：
1. `client_owner.shutdown()`：cancel client token，join SSE/action OS threads
2. `server_shutdown.cancel()` + `server_task.await`：仅 embedded 模式，确认端口释放

**退出测试**：embedded/remote 两种模式——SSE 空闲时可退出、worker 全部 join、embedded 端口可立即重新 bind。

**内嵌 server 生命周期**（P0-9 闭合）：
- 不使用 `serve_on`（它监听进程级 Ctrl+C，TUI 的第一次 Ctrl+C 会先关 server）。
- 使用 `serve_with_shutdown` + 独立的 server shutdown token（与 client cancel token 分开）。
- **SSE 不卡住 shutdown**：`/events` 的 `BroadcastStream` 不会自行结束，Axum graceful shutdown 等待活动连接。Server-side SSE handler 必须 `select!` on **server shutdown token**，shutdown 时主动结束 stream。Client cancel token 只停 client 侧 SSE/action worker 并 drop 连接，不触达 server。
- **退出顺序**：前端退出 → `client_owner.shutdown()`（cancel client token + join SSE/action workers）→ `server_shutdown.cancel()` + `server_task.await`（仅 embedded，确认端口释放）。
- **有限超时**：GET/POST 设置 connect timeout 5s、read timeout 30s。SSE 设置 **idle/read timeout 30s**（不是整个 request timeout——健康长连接不应被强制终止；30s 无事件则判定断线，触发重连）。SSE heartbeat 15s（server 定期发注释行保持连接）。

## 6. CLI `run` HTTP Client（Breaking Change）

### 6.1 命令契约（v2，不兼容 v1）

```
latte-code run [--focus <path>] [--json] [--server url] [--token token] <prompt>
latte-code list [--json] [--server url] [--token token]
latte-code show <session-id> [--json] [--server url] [--token token]
latte-code resume <session-id> <prompt> [--json] [--server url] [--token token]
```

**与 v1 的差异**：
- `run`：v1 返回 `RunState`（含 handoff、failure.code）；v2 创建 session 并流式观察，最终从 `ThreadSnapshot` 推导结果。
- `show <run-id>` → `show <session-id>`：ID 类型从 `RunId` 改为 `ThreadId`。
- `list`：v1 列 run；v2 列 session。
- `resume <run-id> (--allow|--deny)` → `resume <session-id> <prompt>`：从"恢复中断 run + 权限决策"改为"对已有 session 发 follow-up"。旧 `--allow/--deny` 权限工作流移除（权限决策通过 TUI 或 permission endpoint）。
- JSON envelope 升级 v2。

### 6.2 流程

```
latte-code run [--focus <path>] [--json] [--server url] [--token token] <prompt>
  │
  ├─ 内嵌 server（默认，bind 127.0.0.1:0）或连接独立 server
  ├─ POST /v1/workspaces { path } → workspace_id
  ├─ GET /v1/workspaces/{ws}/bindings → 选默认 binding
  ├─ 生成 client thread_id + command_id
  ├─ POST /v1/workspaces/{ws}/sessions
  │    { thread_id, command_id, prompt, binding, focus? } → 202/200
  │
  ├─ GET /v1/workspaces/{ws}/events（SSE 流式观察）
  │    ├── 连接后立即 GET /v1/sessions/{id}（unconditional resync，§8.1）
  │    ├── progress → 打印流式文本到 stderr（--json 模式 stdout 保持纯净）
  │    └── thread_changed → GET /v1/sessions/{id} 检查状态
  │
  ├─ Session 到达终态（按 §6.4 完整判定表）
  │    ├── 打印最终结果
  │    └── exit code 映射
  │
  └── Ctrl+C → POST /v1/sessions/{id}/cancel → exit 130
```

### 6.3 输出格式（v2 JSON）

**非 `--json`**：流式 progress 文本输出到 **stderr**；最终 session 摘要输出到 stdout（lifecycle、model、token 用量从 snapshot 推导）。

**`--json`**：**stdout 只输出最终完成 envelope**（单个 JSON，非 NDJSON 流）。progress 流式文本输出到 stderr，不混入 stdout：

```json
{
  "version": 2,
  "status": "completed|failed|waiting|denied|cancelled|interrupted|reconciliation_required",
  "data": {
    "session": { ... ThreadSnapshot ... }
  }
}
```

错误：

```json
{
  "version": 2,
  "status": "failed",
  "error": { "code": "...", "message": "..." }
}
```

### 6.4 Exit Code 映射

退出判定基于 **lifecycle + latest run status + failure_code + 本地 Ctrl+C 原因** 的完整表：

| 判定条件 | Exit Code | status 字段 | 说明 |
|---|---|---|---|
| 本地 Ctrl+C（用户中断） | 130 | `cancelled` | POST cancel 后退出 |
| lifecycle=Ready + latest run=Completed | 0 | `completed` | 成功 |
| lifecycle=Ready + latest run=Failed + failure_code=PermissionDenied | 11 | `denied` | 权限被拒绝 |
| lifecycle=Ready + latest run=Failed（其他） | 1 | `failed` | Agent 执行失败 |
| lifecycle=WaitingPermission / WaitingInput | 10 | `waiting` | 需要权限/输入（非交互模式无法提供） |
| lifecycle=Interrupted | 130 | `interrupted` | 被中断（非用户主动） |
| lifecycle=ReconciliationRequired | 1 | `reconciliation_required` | 需要 reconcile |
| lifecycle=Failed | 1 | `failed` | 不可恢复失败 |
| Usage Error | 2 | — | 参数错误 |
| Server Unreachable | 71 | — | Server 连接失败（不复用 EXIT_INTERNAL=70） |

**注意**：
- `ThreadLifecycle::Ready` 同时表示 child 完成、权限拒绝、retryable failure。必须检查 latest run 的 `status`（`ThreadRunStatus`）和 `failure_code` 区分。
- `ThreadRunSummary` 需扩展 `failure_code: Option<FailureCode>` 字段（typed enum，非 String）以支持 exit 11 判定。
- `Cancelled` 不是 `ThreadLifecycle` 变体——它是本地 Ctrl+C 动作的结果，不来自 snapshot。
- `Interrupted` 和 `ReconciliationRequired` 是合法的 lifecycle 终态，必须映射。

### 6.5 `--focus` 支持

`focus` 路径通过 `CreateSessionRequest.focus` 传给 server。Server 侧持久化到 session 记录，后续 follow-up 自动恢复（§4.2）。

### 6.6 `resume` / `show` / `list`

- `list` → `GET /v1/workspaces/{ws}/sessions`，格式化输出 session 列表。
- `show <session-id>` → `GET /v1/sessions/{id}`，格式化输出 session snapshot。
- `resume <session-id> <prompt>` → `POST /v1/sessions/{id}/follow-up`，然后流式观察（同 `run` 的 SSE 流程）。

## 7. TUI 语义等价性契约

HTTP 路径必须保持以下 reducer 输入序列与 in-process 路径等价：

### 7.1 Feedback 通道

| 变体 | 触发时机 | HTTP 路径 |
|---|---|---|
| `SubmissionAssigned` | Start/StartWithModel action 后立即 | client 生成 thread_id，零延迟 |
| `SubmissionResult` | POST /sessions 或 follow-up 完成后 | spawn 线程 → feedback channel |
| `InputSubmissionResult` | POST /input 完成后 | 同上 |
| `ModelSwitchResult` | POST /model 完成后 | 同上 |
| `Command` | Cancel/ResolvePermission/Reconcile 完成后 | 同上 |
| `SessionManagement` | Rename/Fork 完成后 | 同上 |

### 7.2 Pending Submission 关联

- client 生成 thread_id → `SubmissionAssigned` 零延迟 → reducer 立即关联 pending submission。
- snapshot 先到、assigned 已到：reducer 在 `SubmissionAssigned` 中检查 session 是否已加载并切换（`thread.rs:840-865`）。
- POST 失败：`SubmissionResult(Err)` 清除 pending submission。

### 7.3 事件唤醒粒度

- in-process：`poll()` 对任何 engine 事件返回 `Event` → `RefreshSnapshots`。
- HTTP：SSE 桥接转发所有 durable ThreadEvent（§4.6）→ `ThreadChanged` → `poll()` 返回 `Event` → `RefreshSnapshots`。
- 等价性：durable transcript 变更（tool 调用/结果、assistant 消息）逐条触发刷新，不等生命周期边界。

### 7.4 Progress 流

- in-process：`ThreadProgressSink` → mpsc → TUI `progress_rx`。
- HTTP：`ThreadProgressSink`（带 thread_id）→ `ServerEvent::Progress` → SSE → **scoped accumulator** → TUI `progress_rx`。
- **Scoped accumulator**：按 `(thread_id, run_id, kind/tool_name)` 分桶。`AssistantDelta` 有界追加（reducer 按 run 追加 chunk），`ProviderAttempt`/`ToolProgress` 取最新。向 reducer 交付前按当前 active thread demux，后台 session 的 progress 不显示。
- progress 是瞬态，snapshot 刷新/断线/重连时清空（reducer 已有 `model.progress.clear()`）。

### 7.5 Sink 错误语义

- sink 绝不返回 `Err`（返回 Err = 退出 TUI）。
- HTTP 失败通过 feedback 通道回送，reducer 在状态栏显示错误。
- 409 Conflict：HTTP worker 同时置 projection Dirty 原子位，确保下一次 `poll()` 返回 `Event` 触发 `RefreshSnapshots`。注意 `Command(Err)` 和 `ModelSwitchResult(Err)` 在 reducer 中只显示错误不触发刷新（`thread.rs:4734-4748`），必须靠 Dirty 位补偿。

## 8. 协议时序契约

### 8.1 SSE (Re)connect Resync

**规则**：每次 SSE 连接/重连后，客户端必须**立即**执行一次无条件 snapshot resync，不等待 server 发送 `resync_required`。

**理由**：
- SSE 订阅是 `broadcast::subscribe()`，无 replay、无 event id、无初始 barrier。
- CLI `run` 是 POST 后才订阅——快速完成的终态事件可能在订阅前已发送。
- 断线重连有丢事件窗口（broadcast 不为断开的 receiver 缓冲）。
- `resync_required` 只在 broadcast lag 时发送，覆盖不了订阅前/断线窗口。

**实现**：
- SSE 线程连接成功后，立即通过 projection channel 发送 `ResyncRequired` 信号。
- 客户端收到 `ResyncRequired` 后执行全量 snapshot 刷新。
- TUI：`RefreshSnapshots`（reducer 已有 `Lagged` 处理）。
- CLI `run`：GET /v1/sessions/{id}，如果已终态则直接退出。

### 8.2 幂等创建与故障模型

**故障模型**：

| 故障场景 | 保护机制 |
|---|---|
| 客户端超时，server 存活 | 内存 ledger replay（同 Idempotency-Key） |
| 客户端超时，server 存活，ledger 丢失（极端） | client command_id + durable digest replay |
| server crash 在 durable accept 后、202 前 | client command_id + durable digest replay（重启后命中 `thread_command_dedup_v2`，返回 `Replayed`，不重启 provider） |
| server crash 在 durable accept 前 | 用同一 thread_id/command_id 重试创建原目标 session（无副作用） |
| 同 command_id + 不同 payload | 409 Conflict（digest mismatch） |
| 同 thread_id 已存在（非重放） | 409 Conflict |

**契约**：
- client 生成 `thread_id`（UUID v7）+ `command_id`（UUID v7）。
- `Idempotency-Key` header 必须等于 body `command_id`，否则 400 拒绝。
- engine 在 create 的同一事务内检查 `thread_command_dedup_v2`：同 command_id+digest → replay（返回 `Replayed`）；digest mismatch → 409。
- 只有 `Created` 启动 provider runner；`Replayed` 走 orphan recovery（lease 到期后恢复）。
- **Exactly-once 范围**：只保证 session admission exactly-once（不重复创建 session），不保证 provider/effect execution exactly-once。
- 内存 ledger 只做同进程快速 replay，不承担 crash correctness。

### 8.3 内嵌 Server 生命周期

- **启动**：bind `127.0.0.1:0` → `tokio::spawn(serve_with_shutdown(state, listener, shutdown_token))`。
- **运行**：TUI/CLI 前端通过 HTTP 与内嵌 server 通信。
- **两个独立 token**：client cancel token（控制 client 侧 SSE/action workers）和 server shutdown token（控制内嵌 server）分开。Server-side SSE handler `select!` on **server shutdown token**，shutdown 时主动结束 stream。Client cancel token 只停 client workers 并 drop 连接，不触达 server。
- **退出顺序**：先 cancel/join client workers，再 cancel/await embedded server。
- **Ctrl+C**：TUI 的第一次 Ctrl+C 是取消手势/二次退出，不触发 server shutdown。server shutdown 只由独立 token 触发。
- **不使用 `serve_on`**：它监听进程级 Ctrl+C/SIGTERM，会与前端手势冲突。
- **有限超时**：GET/POST 设置 connect timeout 5s、read timeout 30s。SSE 设置 idle/read timeout 30s（不是整个 request timeout），heartbeat 15s。

## 9. AgentRuntime 退役

迁移完成后的实际状态（比原计划更彻底）：

- `AgentRuntime`（原 `headless/src/runtime.rs`）与 `RuntimeCommandService`（原 `headless/src/service.rs`）已**直接删除**，未走 `#[deprecated]` + 保留一个 release cycle 的过渡。项目处于早期阶段，无外部用户依赖 v1 内部 API，删除比长期共存更干净。
- `latte-headless` 保留 `ThreadRuntimeService`、`ProviderRegistry`、`context`、`provider` 等共享模块；`runtime.rs` 仅保留 `VerificationPlan`（v2 `ThreadRuntimeService` 仍消费它）。
- v1 `HeadlessCommand`（Run/Resume/Show/List）从 CLI 移除，替换为 session 语义命令。
- `SessionServer` trait 收窄为 CLI session 命令的抽象（run/list/show/resume/cancel + snapshot/events）；TUI 操作（rename/fork/switch_model/queue/input/permission/reconcile/search/bindings）只存在于 `ServerHandle` 的 inherent 方法上，TUI 直接使用 `ServerHandle`，不经 trait。

## 10. 分阶段实施

### Phase 0：Server 基础设施（~4 天）

- [x] `BuiltWorkspace` 增加 `registry: Arc<ProviderRegistry>` 字段
- [x] `ProviderRegistry::thread_binding_catalog` 方法
- [x] `GET /v1/workspaces/{ws}/bindings` 端点（返回 `BindingCatalogEntry` 含 name/is_default/binding）
- [x] `CreateSessionRequest` 增加 `thread_id`、`command_id`、`focus` 字段
- [x] `ThreadRuntimeService`：`start`/`start_accepted`/`start_one` 签名增加 `focus: Option<&Path>`
- [x] `initial_messages` / `history_with_prompt` 透传 focus
- [x] focus 持久化：`threads_v2` 表新增 `focus` 列，`ThreadSnapshot` 新增 `focus` 字段；**Schema 11 → 12 迁移**（`SCHEMA_VERSION` 升级 + `ALTER TABLE` + fresh schema 含 focus）
- [x] **Legacy import 兼容**：`import_legacy_database` 的 `threads_v2` INSERT 改显式列清单，旧库映射 `NULL AS focus`；v9/v10 还需 `NULL AS parent_thread_id`；测试 v11 原地升级 + v9-v11 导入
- [x] **Fork 继承 focus**：`create_thread_session_fork` 的 SELECT/INSERT 包含 focus；测试 fork 后 focus 持久化
- [x] `ThreadCommand::Start` 增加 `focus` 字段
- [x] **Digest 绑定完整命令身份**：`operation_kind + protocol_version + workspace_identity + thread_id + prompt + binding + normalized_focus`
- [x] **Idempotency-Key 与 body command_id 一致性校验**（不一致 → 400）
- [x] **Dedup lookup 先于 lease acquire**：engine create 入口先查 `thread_command_dedup_v2`（无 lease），命中 → `Replayed`；miss → acquire + 事务内二次检查 → `Created`
- [x] engine create 返回 `Created | Replayed`；只有 `Created` 启动 provider runner
- [x] engine create 路径接入 `thread_command_dedup_v2`（同 command_id+digest replay，mismatch 409）
- [x] **Recovery sweeper task**：server 侧周期 task（如每 30s）调用 engine-level recovery API（恢复事务提交后广播 `ResyncRequired` 或 committed `ThreadEventEnvelope`）；测试活 owner 续租不误恢复 / owner crash 只恢复一次 / 多 server sweeper 竞争 / **客户端保持 SSE 时 recovery 后收到 wake 并退出**
- [x] `PATCH /v1/sessions/{id}` 重命名端点（+ThreadChanged 事件）
- [x] `POST /v1/sessions/{id}/fork` 分叉端点（+ThreadChanged 事件）
- [x] `ThreadProgressSink` trait 增加 `thread_id` 参数；`ProviderProgress` 增加 `thread_id` 字段
- [x] `ServerEvent::Progress` 增加 `run_id` 字段
- [x] Progress 事件接线（`WorkspaceInstance::new` 中 `with_progress_sink`）
- [x] SSE 桥接转发所有 durable ThreadEvent（不只 LifecycleChanged）
- [x] server builder 补 `.with_verification(config.plan())`
- [x] `ThreadRunSummary` 增加 `failure_code: Option<FailureCode>` 字段（typed enum）
- [x] UT + E2E 覆盖

### Phase 1：TUI HTTP Client（~4 天）

- [x] `HttpProjectionClient` 实现（REST reads + mpsc `ProjectionEvent` 通道 poll：ThreadChanged/Closed；原设计的 5 态 sticky 原子位在评审中简化为通道）
- [x] Action dispatch 实现（闭包 over `ServerHandle`，非阻塞，6 种 feedback 全覆盖，绝不返回 Err，409 置 dirty 位，queue 满发失败 feedback；原设计的 `HttpActionSink` struct 在评审中简化为闭包）
- [x] SSE 线程（专用 OS 线程，reqwest::blocking，demux：progress scoped accumulator / dirty 信号 / 带退避重连）
- [x] Progress scoped accumulator（按 thread_id+run_id+kind 分桶，AssistantDelta 有界追加，交付前按 active thread demux）
- [x] 执行模型：TUI loop 用 `spawn_blocking(...).await`，bounded action queue + 固定 OS 线程 worker，per-session 顺序
- [x] `ClientWorkers` RAII owner（所有模式：cancel + join SSE/action workers）
- [x] TUI 启动时内嵌 server（serve_with_shutdown + 独立 server shutdown token）+ HTTP client 接线
- [x] `--server` 连接独立 server 模式
- [x] TUI reducer/renderer 不变
- [x] E2E：TUI → 内嵌 server → engine 全链路（真实 PTY，`e2e_unix`）
- [x] HTTP/SSE client 跨平台 E2E（`e2e_portable`，Linux/macOS/Windows）

### Phase 2：CLI `run` HTTP Client（~3 天）

- [x] `run` 命令改为内嵌 server + HTTP client（breaking change，session 语义）
- [x] SSE 流式 progress 输出到 stderr（`--json` 模式 stdout 只输出最终 envelope）
- [x] Exit code 映射（lifecycle + latest run status + failure_code + 本地 Ctrl+C 完整表）
- [x] `--focus` / `--json` / `--server` 支持
- [x] `list` / `show` / `resume` 改为 session 语义
- [x] v1 `HeadlessCommand` 移除
- [x] JSON envelope v2
- [x] E2E：CLI → 内嵌 server → engine 全链路（`e2e_portable`）

### Phase 3：清理（~1 天）

- [x] ~~标记 `AgentRuntime` / `RuntimeCommandService` 为 deprecated~~ → 实际直接删除（见 §9）
- [x] 移除 binary 中的 in-process engine 直连路径
- [x] 更新文档（server 设计文档、roadmap、HELP 文本）
- [x] 覆盖率补齐（CI 三门全绿；另清理了 `SessionServer` trait 上无生产调用方的 TUI 操作方法，收窄为 CLI session 命令抽象）

**总计：~12 个工作日。**（v1 估算 8 天，两轮评审后因 P0 闭合 + schema 迁移 + crash runner 恢复增加 4 天）

## 11. 风险

### 11.1 同进程 HTTP 开销

Loopback HTTP 延迟 < 1ms，但 TUI 的高频 snapshot 刷新可能累积。如果性能不达标，可考虑用 Unix domain socket 替代 TCP。不回退 in-process 直调（违反单一 HTTP 边界原则）。

### 11.2 SSE 重连与状态一致性

断线重连后无条件 resync（§8.1）。Permission/input request 在 snapshot 中，重拉即可恢复。

### 11.3 多客户端并发写

多个 TUI/CLI 同时操作同一 session。Server 的 per-session 串行化（revision/lease fence）已处理。409 Conflict 时客户端刷新 snapshot 重试。

### 11.4 Windows 兼容性

内嵌 server 模式在 Windows 上可用（TCP loopback）。`write_server_token` 在 Windows 无 0600 等价（裸 `fs::write`），不声称 Unix 权限等价。`rename` 在 Windows 不覆盖已有文件，token 写入需处理目标已存在的情况。

### 11.5 Breaking change 迁移

CLI 命令契约不兼容 v1。用户脚本/CI 需要适配 session 语义命令和 JSON v2 envelope。在 CHANGELOG 和 HELP 文本中明确标注 breaking changes。

### 11.6 覆盖率余量

PR #7 后存量 lib.rs CLI 代码距 95% UT 门余量较薄。本方案新增 ~12 天代码（HttpProjectionClient/HttpActionSink/新端点/focus 持久化/dedup 接线/schema 迁移/recovery sweeper），UT 必须从第一天跟上，不能留到 Phase 3"覆盖率补齐"。新代码的 UT 覆盖率目标 ≥ 95%，E2E 覆盖率不低于现有门（90%）。

## 12. CLI 决策记录

**决策**：CLI 命令契约 breaking change，不保 v1 兼容。

**理由**：
- v1 `resume <run-id> --allow/--deny` 的权限工作流与 session 模型不兼容（run_id vs thread_id，权限决策 vs follow-up）。
- v1 `RunState` JSON 包含 handoff、failure.code 等 HTTP snapshot 不具备的字段，补 typed endpoint 复刻 v1 契约会增加不必要的 server 表面积。
- 项目处于早期阶段，没有外部用户依赖 v1 CLI 契约。
- Breaking change 让 CLI 命令直接对齐 HTTP API 语义，减少长期维护负担。

**影响**：
- `run/show/list/resume` 命令语义和 ID 类型变更。
- JSON envelope 升级 v2。
- 旧 v1 run 的 `show/list/resume` 从 CLI 移除。
- AgentRuntime / RuntimeCommandService 标记 deprecated。
