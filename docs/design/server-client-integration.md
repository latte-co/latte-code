# Server 客户端集成方案

状态：**待评审**
日期：2026-08-17

## 1. 背景与目标

当前三个前端（CLI `run`、TUI、HTTP server）各自在进程内嵌入 `EngineHandle`，互不共享 session 状态。Server 设计文档已将 CLI/TUI 画为 HTTP 客户端，但尚未实现。

**目标**：CLI `run` 和 TUI 统一走 server 的 HTTP + SSE 协议，server 成为唯一的 engine 宿主。多端共享 session、状态和事件。

**非目标**：远程访问（v2）、多 server 集群、WebSocket 替代 SSE。

## 2. 架构

### 2.1 三种运行模式

```
模式 1：TUI（默认）
┌─────────────────────────────────────┐
│ latte-code（TUI 进程）               │
│  ┌──────────┐    HTTP+SSE           │
│  │ TUI      │◄───(随机端口)──┐      │
│  │ (reducer │               │      │
│  │  renderer)               │      │
│  └──────────┘               ▼      │
│                    ┌──────────┐    │
│                    │ Server   │    │
│                    │ (axum)   │    │
│                    └────┬─────┘    │
│                         │          │
│                    ┌────▼─────┐    │
│                    │ Engine   │    │
│                    │ (in-proc)│    │
│                    └──────────┘    │
└─────────────────────────────────────┘
  其他本地客户端（浏览器/IDE）可连随机端口

模式 2：CLI run（默认）
┌─────────────────────────────────────┐
│ latte-code run "prompt"（CLI 进程）  │
│  ┌──────────┐    直接调用           │
│  │ CLI      │──────────────┐        │
│  │ (stream  │              │        │
│  │  output) │              ▼        │
│  └──────────┘        ┌──────────┐  │
│                      │ Server   │  │
│                      │ (无监听) │  │
│                      └────┬─────┘  │
│                           │        │
│                      ┌────▼─────┐  │
│                      │ Engine   │  │
│                      └──────────┘  │
└─────────────────────────────────────┘

模式 3：独立 server + 远程客户端
┌──────────────────────┐     HTTP+SSE    ┌──────────────────┐
│ latte-code serve     │◄────────────────│ 浏览器 / IDE /   │
│ (server 进程)        │                 │ 另一个 TUI/CLI   │
│  Server (axum:4096)  │                 └──────────────────┘
│       │              │
│  Engine (in-proc)    │
└──────────────────────┘
```

### 2.2 设计原则

1. **Server 是库，不是服务。** 客户端进程内嵌 server，同进程内 HTTP 通信。
2. **TUI reducer 不变。** `ThreadProjectionClient` trait 是已有边界，HTTP client 是另一个实现。
3. **CLI `run` 映射到 session 模型。** 一次性任务 = 创建 session + 流式观察 + 退出码映射。
4. **一套 HTTP 协议覆盖本地和远程。** 内嵌 server 和独立 server 用同一套路由和 DTO。
5. **不保留 in-process engine 直连路径。** 所有前端统一走 HTTP，即使同进程。
6. **Session 共享靠持久化存储。** SQLite/JSONL 是权威，server 只提供 live event。

### 2.3 同进程走 HTTP 的理由

- **单一路径**：本地和远程用同一套 client 代码，不维护 in-process 和 HTTP 两套。
- **IDE/浏览器可 attach**：TUI 进程的 server 监听随机端口，其他客户端可以连入。
- **协议即边界**：HTTP API 是公开契约，强制 engine authority 不被绕过。
- **性能可接受**：loopback HTTP 延迟 < 1ms，对 TUI 交互无感知。

## 3. 连接模式

### 3.1 CLI 接口

```
latte-code                                    # TUI，内嵌 server（随机端口）
latte-code --server http://host:port          # TUI，连远程 server
latte-code run "prompt"                       # CLI run，内嵌 server（无监听）
latte-code run --server http://host:port "prompt"  # CLI run，连远程 server
latte-code serve                              # 独立 server，端口 4096
latte-code serve --port 8080                  # 独立 server，自定义端口
```

### 3.2 连接规则

| 场景 | 命令 | Server 位置 | Token 来源 |
|---|---|---|---|
| TUI（默认） | `latte-code` | 同进程内嵌（随机端口） | 进程内已知 |
| TUI 连远程 | `latte-code --server http://host:port` | 远程 | `server.token` 或 `--token` |
| CLI run（默认） | `latte-code run "prompt"` | 同进程内嵌（无监听） | 进程内已知 |
| CLI run 连远程 | `latte-code run --server http://host:port "prompt"` | 远程 | `server.token` 或 `--token` |
| 独立 server | `latte-code serve [--port N]` | 独立进程 | 写 `server.token` |

### 3.3 Token 管理

- `serve` 写 `$LATTE_CODE_HOME/server.token`（0600 权限，现有行为不变）。
- `--server` 模式下，客户端默认读 `server.token`；也可以 `--token <token>` 显式指定。
- TUI 内嵌 server 的 token 只在进程内，不落盘。TUI 在界面上显示连接 URL 供 IDE/浏览器 attach。

### 3.4 多 Server

支持多 server 并存：

- **多个 TUI 实例**：各自内嵌 server 绑随机端口，互不冲突。
- **多个 `serve`**：`--port` 区分，各自写 `server.token`（后写覆盖先写）。非默认端口的 server 用 `--token` 显式指定。
- **Session 共享**：所有 server 共享同一个 SQLite + JSONL（durable state）。任何 server 都能读写任何 session。
- **Live event 隔离**：每个 server 进程有自己的 SSE 广播。要共享 live event，连同一个 server。
- **写冲突**：engine 的 revision/lease fence 处理并发写，冲突返回 409 Conflict，客户端刷新 snapshot 重试。

## 4. API 扩展

现有端点已覆盖大部分操作。需要新增以下端点：

### 4.1 Binding 发现

```
GET /v1/workspaces/{workspace_id}/bindings
Response: 200 { "bindings": [ThreadProviderBindingV2, ...] }
```

返回该 workspace 配置中所有可用的 provider binding。TUI model picker 和 CLI `run` 用此端点选择模型。

Server 侧：从 `AppConfig::load(workspace_root)` 的 `ProviderRegistry` 导出 binding 列表。

### 4.2 Session 创建增加 focus 字段

`CreateSessionRequest` 增加可选字段：

```rust
pub struct CreateSessionRequest {
    pub prompt: String,
    pub binding: serde_json::Value,
    #[serde(default)]
    pub focus: Option<String>,
}
```

Server 侧将 `focus` 传给 `ThreadRuntimeService::start`，由 server 构建 `ContextBundle`（复用 `latte_headless::context::build`）。

### 4.3 Session 重命名

```
PATCH /v1/sessions/{session_id}
Body: { "title": "新标题" }
Response: 200 { "snapshot": {...} }
```

映射到 `engine.rename_thread_session_v2`。

### 4.4 Session 分叉

```
POST /v1/sessions/{session_id}/fork
Response: 200 { "snapshot": {...} }
```

映射到 `engine.fork_thread_session_v2`。

### 4.5 Progress 事件接线

当前 `ServerEvent::Progress` 变体存在但未接线。需要在 workspace builder 中为 `ThreadRuntimeService` 设置 `ThreadProgressSink`：

```rust
let progress_tx = workspace_event_tx.clone();
let sink: Arc<dyn ThreadProgressSink> = Arc::new(move |progress| {
    let _ = progress_tx.send(ServerEvent::Progress {
        session_id: progress.run_id().to_string(),
        progress: serde_json::to_value(&progress).unwrap_or_default(),
    });
});
let runtime = runtime.with_progress_sink(sink);
```

SSE 客户端收到 `progress` 事件后，反序列化 `serde_json::Value` 回 `ThreadTransientProgress`。

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
  │           └── poll() ← SSE 后台任务 mpsc channel
  │
  ├── Action Sink（FnMut(ThreadUiAction) → Result）
  │     └── HttpActionSink（新）
  │           ├── Start          → POST /v1/workspaces/{ws}/sessions
  │           ├── FollowUp       → POST /v1/sessions/{id}/follow-up
  │           ├── QueueFollowUp  → POST /v1/sessions/{id}/queue
  │           ├── Cancel         → POST /v1/sessions/{id}/cancel
  │           ├── ProvideInput   → POST /v1/sessions/{id}/input
  │           ├── ResolvePermission → POST /v1/sessions/{id}/permissions/{req_id}
  │           ├── SwitchModel    → POST /v1/sessions/{id}/model
  │           ├── ReconcileUnknown → POST /v1/sessions/{id}/effects/{effect_id}/reconcile
  │           ├── RenameSession  → PATCH /v1/sessions/{id}
  │           ├── ForkSession    → POST /v1/sessions/{id}/fork
  │           └── RefreshSnapshots → 触发 snapshot 刷新（无 HTTP 调用）
  │
  └── Feedback/Progress Channel
        └── SSE 后台任务
              ├── thread_changed  → 触发 snapshot 刷新 → ThreadUiInput::Snapshot
              ├── progress       → ThreadUiInput::Progress
              └── resync_required → ThreadUiInput::Lagged
```

### 5.2 HttpProjectionClient

```rust
pub struct HttpProjectionClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
    workspace_id: String,
    event_rx: std::sync::mpsc::Receiver<ProjectionEvent>,
}

enum ProjectionEvent {
    ThreadChanged { session_id: String, revision: u64 },
    Progress(ThreadTransientProgress),
    ResyncRequired,
    Closed,
}
```

- 构造时启动 SSE 后台任务（`tokio::spawn`），连接 `GET /v1/workspaces/{ws}/events`。
- SSE 任务解析事件帧，发送 `ProjectionEvent` 到 `std::sync::mpsc::channel`。
- `poll()` 做 `try_recv()`，映射到 `ThreadProjectionPoll`。
- `snapshots()` / `session()` / `search_session_catalog()` 做 HTTP GET。
- 断线自动重连（1s 间隔退避），`resync_required` 触发全量重拉。

### 5.3 同步/异步桥接

TUI 事件循环是同步的（crossterm），HTTP 是 async。方案：

- SSE 后台任务运行在独立 tokio runtime 上（`tokio::runtime::Runtime::new()` 在 TUI 启动时创建）。
- HTTP GET 调用用 `tokio::runtime::Handle::block_on()` 在 TUI 线程中执行。
- 或用 `reqwest::blocking`（独立线程池，不与 TUI 的 tokio runtime 冲突）。

### 5.4 Binary 接线

`execute_tui` 的改动：

```rust
async fn execute_tui(/* ... */) -> i32 {
    // 1. 解析 --server / --token 参数
    let server_config = resolve_server_config(&args)?;

    // 2. 连接或内嵌 server
    let (base_url, token, workspace_id) = match server_config {
        ServerConfig::Remote { url, token } => {
            // 远程模式：解析 workspace
            let ws = resolve_workspace(&url, &token, &root).await?;
            (url, token, ws)
        }
        ServerConfig::Embedded => {
            // 内嵌模式：同进程启动 server（随机端口）
            let (state, token) = build_server_state(&root, &storage_home)?;
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let port = listener.local_addr()?.port();
            tokio::spawn(latte_server::serve_on(state, listener));
            let url = format!("http://127.0.0.1:{port}");
            let ws = resolve_workspace(&url, &token, &root).await?;
            (url, token, ws)
        }
    };

    // 3. 构造 HTTP client
    let mut projection = HttpProjectionClient::new(&base_url, &token, &workspace_id);
    let sink = HttpActionSink::new(&base_url, &token, &workspace_id);

    // 4. 启动 TUI（reducer/renderer 不变）
    latte_tui::run_with_feedback_and_progress(
        &mut projection,
        startup,
        sink,
        feedback_rx,
        progress_rx,
    )
}
```

## 6. CLI `run` HTTP Client

### 6.1 流程

```
latte-code run [--focus <path>] [--json] [--server url] [--token token] <prompt>
  │
  ├─ 内嵌 server（默认）或连接远程 server
  ├─ POST /v1/workspaces { path } → workspace_id
  ├─ GET /v1/workspaces/{ws}/bindings → 选默认 binding
  ├─ POST /v1/workspaces/{ws}/sessions
  │    { prompt, binding, focus? } → session_id, accepted_revision
  │
  ├─ SSE 流式观察
  │    ├── progress → 打印流式文本到 stdout
  │    └── thread_changed → GET /v1/sessions/{id} 检查状态
  │
  ├─ Session 完成
  │    ├── 打印最终结果（表格或 JSON）
  │    └── exit code 映射
  │
  └── Ctrl+C → POST /v1/sessions/{id}/cancel → exit 130
```

### 6.2 输出格式

**非 `--json`**：流式输出 + 最终结果表格（与当前 `render_run` 一致）。

**`--json`**：
- 流式阶段：每行一个 JSON envelope（progress delta）。
- 完成：输出最终 JSON 结果。
- 错误：输出 JSON error envelope。

### 6.3 Exit Code 映射

| Session 状态 | Exit Code | 说明 |
|---|---|---|
| Completed | 0 | 成功 |
| Failed | 1 | Agent 执行失败 |
| WaitingPermission | 10 | 需要权限（非交互模式无法批准） |
| Denied | 11 | 权限被拒绝 |
| Cancelled | 130 | 用户中断 |
| Usage Error | 2 | 参数错误 |
| Server Unreachable | 70 | Server 连接失败 |

### 6.4 `--focus` 支持

`focus` 路径通过 `CreateSessionRequest.focus` 传给 server。Server 侧构建 `ContextBundle` 注入 system prompt。客户端不做 context 构建。

### 6.5 `resume` / `show` / `list`

- `list` → `GET /v1/workspaces/{ws}/sessions`，格式化输出。
- `show <run-id>` → `GET /v1/sessions/{id}`，格式化输出。
- `resume <run-id>` → `POST /v1/sessions/{id}/follow-up`（需要 prompt 参数；映射到 follow-up 即可）。

## 7. AgentRuntime 退役

迁移完成后：

- `AgentRuntime`（`headless/src/runtime.rs`）不再被 CLI `run` 使用。
- `RuntimeCommandService`（`headless/src/service.rs`）是 v1 遗留，无前端使用。
- 两者标记 `#[deprecated]`，保留一个 release cycle，后续移除。
- `latte-headless` 保留 `ThreadRuntimeService`、`ProviderRegistry`、`context`、`provider` 等共享模块。

## 8. 分阶段实施

### Phase 0：Server 基础设施（~2 天）

- [ ] 内嵌 server 构造逻辑（`build_server_state` 提取为可复用函数）
- [ ] `--server` / `--token` CLI 参数解析
- [ ] `GET /v1/workspaces/{ws}/bindings` 端点
- [ ] `CreateSessionRequest` 增加 `focus` 字段
- [ ] `PATCH /v1/sessions/{id}` 重命名端点
- [ ] `POST /v1/sessions/{id}/fork` 分叉端点
- [ ] Progress 事件接线（`ThreadProgressSink` → `ServerEvent::Progress`）
- [ ] UT + E2E 覆盖

### Phase 1：TUI HTTP Client（~3 天）

- [ ] `HttpProjectionClient` 实现（REST reads + SSE poll）
- [ ] `HttpActionSink` 实现（action → HTTP POST）
- [ ] SSE 后台任务（断线重连、resync 处理）
- [ ] TUI 启动时内嵌 server + HTTP client 接线
- [ ] `--server` 远程连接模式
- [ ] TUI reducer/renderer 不变
- [ ] E2E：TUI → 内嵌 server → engine 全链路（真实 PTY）

### Phase 2：CLI `run` HTTP Client（~2 天）

- [ ] `run` 命令改为内嵌 server + HTTP client
- [ ] SSE 流式输出到 stdout
- [ ] Exit code 映射
- [ ] `--focus` / `--json` / `--server` 支持
- [ ] `list` / `show` / `resume` 改为 HTTP client
- [ ] E2E：CLI → 内嵌 server → engine 全链路

### Phase 3：清理（~1 天）

- [ ] 标记 `AgentRuntime` / `RuntimeCommandService` 为 deprecated
- [ ] 移除 binary 中的 in-process engine 直连路径
- [ ] 更新文档（server 设计文档、roadmap、HELP 文本）
- [ ] 覆盖率补齐

**总计：~8 个工作日。**

## 9. 风险

### 9.1 同进程 HTTP 开销

Loopback HTTP 延迟 < 1ms，但 TUI 的高频 snapshot 刷新可能累积。如果性能不达标，可考虑 snapshot 刷新走 in-process 直调（绕过 HTTP），或用 Unix domain socket 替代 TCP。

### 9.2 SSE 重连与状态一致性

断线重连后 `resync_required` 触发全量 snapshot 重拉。Permission/input request 在 snapshot 中，重拉即可恢复。

### 9.3 多客户端并发写

多个 TUI/CLI 同时操作同一 session。Server 的 per-session 串行化（revision/lease fence）已处理。409 Conflict 时客户端刷新 snapshot 重试。

### 9.4 Windows 兼容性

内嵌 server 模式在 Windows 上可用（TCP loopback）。`serve --daemon` 不支持 Windows，需手动 `serve`。
