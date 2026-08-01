# 全局 Session 与数据存储设计

状态：**部分已实现；全局 Storage Home 尚未启用。**

当前实现仍把 Runtime State 与 Conversation Card 保存到配置的数据库中，默认路径为
工作区下的 `.latte/latte-code.db`。Schema 9 Catalog Metadata、按 Workspace 过滤的
启动恢复、Session Scoped Lease，以及“提交已接受/Provider 失败”边界已在该数据库内
实现。全局 Product Home、跨 Workspace Catalog、按 Session 存储的 JSONL
Transcript、恢复、导入和迁移仍是目标设计；落地时必须保留现有 Engine 对 Effect、
Permission、Lease、Fencing、去重和 `Unknown` Reconciliation 的安全约束。

## 1. 已确定的决策

| 数据类型 | 权威存储 | 契约 |
| --- | --- | --- |
| Session 对话内容 | 全局、每 Session 一个 JSONL | 只追加 user、assistant、tool 与上下文记录。 |
| Project、Workspace、Session 元数据 | 全局 SQLite | 发现、搜索、生命周期、Provider Binding、血缘和归档状态。 |
| Run 与 Effect 控制状态 | 全局 SQLite | 事务化 Run 状态、Effect、Permission、Lease、Checkpoint、Evidence 与去重。 |
| Draft 与 Provider 运行时 | 进程内存 | 尚未接受的 Prompt、HTTP Stream、Retry、Cancellation、Delta 和原始 Provider Diagnostic。 |
| Credential | 不持久化 | 只允许持久化非密钥的 Credential Reference 与 Generation。 |

其他已确定决策：

- Session 和数据库状态绝不存放在 Workspace 目录下。
- Session 文件按 Workspace 分桶，不按日期分层。
- JSONL 是对话内容唯一的回放来源；SQLite 不复制 Transcript。
- 不引入 Transcript Outbox，也不建立持久化的 Provider Attempt 表。
- Prompt 一旦被接受，Provider 启动失败就是 Session 事实；只持久化有界、已脱敏的
  Failure Card，绝不持久化 Credential 或原始 Provider Diagnostic。
- Provider Streaming Delta 是瞬态数据；只有完整 Provider Outcome 才有资格持久化。

## 2. 术语

- **Project** 表示逻辑仓库身份，多个 Git Worktree 可以共享一个 Project。
- **Workspace** 表示一个物理 Checkout 或非 Git 工作目录。
- **Session** 表示一个用户可见的 Conversation。迁移期间它与现有
  `ThreadId` 一一对应，不引入第二套身份。
- **Run** 表示一次用户提交及其 Provider/Tool Continuation Loop。
- **Effect** 表示可能改变或观察外部状态、由 `latte-engine` 掌握权限的操作。
- **Draft** 表示尚未通过本地校验并到达持久提交点、只存在于内存的新 Session 或
  Follow-up。

## 3. 全局存储 Home

`LATTE_CODE_HOME` 可以覆盖产品数据 Home。默认值为：

```text
~/.latte/latte-code/
```

目标目录结构：

```text
~/.latte/
├── latte-code.jsonc
└── latte-code/
    ├── state.db
    └── sessions/
        ├── Users-bytedance-projects-latte-co-latte-code-a13f8c2d/
        │   ├── <session-id>.jsonl
        │   └── <session-id>.jsonl
        └── Users-bytedance-projects-codeagent-92b11d04/
            └── <session-id>.jsonl
```

Windows 通过解析出的用户 Home 使用同一套 Home 相对契约。应用不得在
Workspace 中创建数据库或 Session 目录。

Workspace 配置仍可控制项目行为，但不能控制全局存储位置。Workspace 层的
`database.path` 只为迁移兼容而继续接受，但会被忽略，绝不能重定向用户历史。
只有进程环境或可信用户配置可以选择存储 Home。

## 4. Workspace Storage Key

Session 分桶名称由 Canonical Workspace Root 生成：

1. 解析并规范化 Canonical Root 与路径分隔符。
2. 跨平台一致地规范化 Unicode。
3. 把不安全字符和路径分隔符替换为 `-`。
4. 限制可读前缀长度。
5. 追加 Canonical Root 的短 SHA-256 Digest。

Digest 用于避免 `/a/b-c` 和 `/a-b/c` 在文本清洗后发生碰撞。最终的
`storage_key` 存入 SQLite；Workspace 配置绝不能直接提供目录名。

每个 Git Worktree 是独立的 Workspace 分桶，但可以指向同一个 Project。
移动或删除 Workspace 不会移动或删除既有 Session 文件。新路径作为另一个
Workspace Record 关联，旧 Session 后续可显式重新绑定。

## 5. 全局 SQLite 职责

全局数据库包含两个逻辑区域，两者都不保存可回放的完整对话 Transcript。

### 5.1 Catalog

Catalog 包含以下等价数据：

```text
projects
  project_id
  vcs_identity
  display_name
  created_at_ms
  updated_at_ms

workspaces
  workspace_id
  project_id
  canonical_root
  storage_key
  git_common_dir
  branch
  first_seen_at_ms
  last_seen_at_ms

sessions
  session_id
  project_id
  workspace_id
  content_path
  content_format
  title
  preview
  lifecycle
  provider_binding_json
  latest_run_id
  forked_from_session_id
  forked_from_seq
  last_content_seq
  last_content_bytes
  created_at_ms
  updated_at_ms
  archived_at_ms
```

`last_content_seq` 和 `last_content_bytes` 是可修复缓存。它们与文件不一致时，
以 JSONL 为准。

Provider Binding 保留 Resume 校验所需的完整非密钥语义绑定：Provider
Name/Type、Protocol、Model、配置与工具指纹、Alias、Credential Reference、
Data Scope 和 Credential Generation。它绝不包含 Credential Value。

### 5.2 Engine Control Plane

SQLite 继续作为以下数据的权威来源：

- Run 状态与 Revision。
- Session 当前 Active Run。
- Effect 声明、仅 Engine 可读的精确 Descriptor、Attempt 与 Observation。
- Pending Permission 与非密钥 Input Request。
- Runtime Checkpoint 与 Verification Evidence。
- Command 和 Source Key 去重。
- Lease Ownership 与 Fencing Token。

这些记录不是对话内容。把它们迁移到 JSONL 会失去阻止外部 Effect 重复执行
或错误报告所需的事务和 Compare-And-Swap 能力。

## 6. 按 Session 管理 Ownership

当前配置的数据库使用带 Scope 的 Runtime Lease：

```text
runtime_lease
  scope（主键：runtime 或 thread:<session-id>）
  owner
  fencing_token
  expires_at_ms
```

Legacy Headless Run 共用 `runtime` Scope；不同 Thread v2 Session 使用不同 Scope，
因此可以并发运行。一个 Session 最多只有一个有效 Engine Owner 和一个 JSONL
Writer。Lease 过期后重新获取必须推进全局单调的 Fencing Token。旧 Owner 不能
开始或观察 Effect，并且失去 Ownership 后必须关闭 Session Writer。

一次持久化 `WaitingPermission` 或 `WaitingInput` 操作正常返回时，会先在同一事务
中把关联 Run 与 Active Row 的 Lease Token 置为零，再删除 Lease Row。Token 零表示
安全静止：启动恢复会保留这个等待中的 Child；后续 Coordinator 必须先获取新的全局
Fencing Epoch 才能写入。没有 Lease 且仍保留非零 Token 则表示 Owner 非正常丢失，
继续执行保守的 Interrupted/`Unknown` 恢复。用户在新 Epoch 中显式 Allow 已 Prepare
的 Effect 时，Engine 会重新验证私有 Canonical Descriptor，并在进入 `Started` 前
原子地把单次 Permission Capability 与 Operation Digest 一起重绑定到新 Epoch。

## 7. JSONL 契约

第一行是最小的自描述 Header，不是 Session Catalog 的副本：

```json
{"record":"session","format_version":1,"session_id":"019...","workspace_id":"019...","created_at_ms":1780000000000}
```

后续按顺序只追加 Conversation Record：

```json
{"record":"message","entry_id":"019...","seq":1,"run_id":"019...","created_at_ms":1780000000001,"role":"user","content":"修复失败的测试"}
{"record":"message","entry_id":"019...","seq":2,"run_id":"019...","created_at_ms":1780000000100,"role":"assistant","content":"我会先检查。","finish_reason":"stop","usage":{"input_tokens":100,"output_tokens":20,"cache_read_tokens":0}}
```

可回放 Record 刻意保持精简：

- `message`，Role 为 `system`、`user`、`assistant` 或 `tool`。
- 完整的 Assistant Tool-Call Envelope。
- 通过 `tool_call_id` 关联的 Tool Result。
- `context_checkpoint` 和 `compaction_summary`。

每条 Entry 都有稳定的 `entry_id`、单调递增的 `seq`、可选 `run_id` 和有界
Content。Provider 生成的 ID 在写入前必须满足现有的安全 Opaque ID 语法。

JSONL 不包含：

- Provider 配置、Credential、Model 启动、HTTP、Transport、Timeout 或 Retry
  Exhaustion 错误。
- Authorization Header、API Key 或原始 Credential Value。
- Provider Stream Handle、Cancellation Token、Timer 或 Partial Delta。
- 仅 Engine 可读的可执行 Effect Descriptor。
- 尚未组装完整的 Assistant Message。

正常写入只能追加。Single Writer 在语义边界写入完整行并调用 `sync_data`，
不会为每个 Streaming Delta 执行 Sync。崩溃修复允许把一条撕裂的末行裁剪到
最后一个完整换行；除此之外禁止原地改写历史。

## 8. Draft 与物化生命周期

### 8.1 新 Session

新 Prompt 首先成为内存 Draft：

```text
Prompt
→ 校验非密钥 Provider Binding
→ 持久化 Session、Child Run 与 User Card
→ 在该 Session Lease 下启动 Child
→ 在内存中解析 Credential Reference
→ 构造 Provider
→ 发起第一次 Provider Request
```

持久创建之前的 Validation 或 Storage Failure 不留下 Session/Run，并精确保留
Draft。创建之后的 Configuration、Credential、Model、Authentication、
Transport、Timeout 或其他 Provider 启动失败，会用一个有界、已脱敏的 Failure
Card 终结该 Child；User Card 不会被删除或复制回 Composer。Provider 构造失败可
重试：Session 回到 `Ready`，后续提交创建新的不可变 Child。原始 Provider
Diagnostic 与 Credential Value 继续只存在于进程内。

持久化提交点是已校验 User Submission 被接受的时刻，早于 Provider 构造和网络
I/O。应用执行：

1. 插入不可被列表发现的 `materializing` Session Metadata Row。
2. 写入并 Sync JSONL Header 与 User Message。
3. 创建持久化 Child Run/Control State。
4. 以 `Running` 状态把 Session 标记为可发现。

完整 Assistant Message、Tool-Call Envelope、Input Request 或已脱敏 Failure 只在
这个持久提交边界之后追加。

启动时删除没有有效文件的 `materializing` Row，或根据有效的自描述文件修复
Catalog Metadata。不能因为 Provider 启动失败而删除一个已经接受的 Session。

### 8.2 Follow-up

Follow-up 使用相同边界：先校验，再在构造 Provider 之前原子追加 User Card 并创建
Child。Provider 构造失败会追加可重试 Failure Card；此前已完成的 Child 保持不可
变，Session 回到 `Ready` 接收下一个 Follow-up。持久化工具工作之后的 Provider
Request 失败，则保留已有的 `Failed`、`Interrupted` 或
`ReconciliationRequired` Control State。只有有界、已脱敏的展示文本可以持久化。

## 9. Effect 顺序

完整 Provider Tool-Call Outcome 使用以下顺序：

```text
追加并 Sync Assistant Tool-Call Message
→ SQLite Prepare Effect
→ 必要时处理 Permission
→ SQLite Started
→ 执行 Effect
→ SQLite Observed 或 Unknown
→ 追加并 Sync Tool Result
```

该顺序的恢复语义如下：

| 崩溃位置 | 必须得到的结果 |
| --- | --- |
| Tool-Call Message 之前 | 没有 Session Content，也没有 Effect。 |
| Tool-Call 已追加、`Prepared` 之前 | 已知 Tool Call，但可以确定未执行。 |
| `Prepared` 之后、`Started` 之前 | Effect 以未启动状态终结。 |
| `Started` 之后、Observation 之前 | Effect 变成 `Unknown`，必须 Reconciliation。 |
| Observation 之后、Tool Result 追加之前 | 从 SQLite Observation 重建并追加 Provider-Safe Result。 |
| Tool Result 追加之后 | 正常从 JSONL 继续。 |

精确可执行 Descriptor 继续只存在于 Engine 私有 SQLite 中。JSONL 只保存有界、
已脱敏的 Provider History 表示。`Started` 之前 JSONL 追加失败会中止执行；
Effect Observation 之后追加失败会停止 Run，且不得重试 Effect，恢复时根据权威
Observation 修复缺失的 Tool Result。

## 10. 读取与 Projection

- 全局 Session Discovery、Search、Archive Filter、Project Grouping 和 Workspace
  Grouping 只查询 SQLite。
- 打开 Session 时加载有界 JSONL Tail 与当前 SQLite Control Projection。
- Provider History 只从 JSONL Conversation Record 重建，并从最新可用 Context
  Checkpoint 开始。
- TUI Projection 合并 JSONL Message、SQLite Effect/Permission State 和内存中的
  瞬态 Provider Progress。
- Event Gap 或重连时重新加载该组合 Projection；新的 JSONL Session 不需要第二
  份持久化 Transcript Event Table。

Compaction 只追加 `context_checkpoint` 或 `compaction_summary`，不会删除或改写
更早的行。

## 11. Session 操作

- **Archive** 只更新 `archived_at_ms`，不移动 JSONL。
- **Fork** 在目标 Workspace 分桶创建独立新文件，复制到所选 Sequence 为止的
  Content，并在 SQLite 记录 `forked_from_session_id` 和 `forked_from_seq`。
- **Hard Delete** 必须显式执行，并删除 Session Catalog Row、Control State、
  JSONL 和归属该 Session 的附件。默认 UI 操作仍是 Archive。
- **Workspace 丢失或移动** 绝不删除历史。原路径不可用时，Resume 必须显式绑定
  一个有效 Workspace。

## 12. 安全与限制

- 产品 Home 和 Workspace Bucket 使用仅用户可访问的目录权限，Session 文件使用
  仅用户可访问的文件权限。
- 数据跨入 JSONL 或公开 SQLite Projection 前必须完成 Redaction 与 Validation。
- 精确可执行 Input 始终留在 Engine Private Boundary 后面。
- 未来若支持大型二进制附件，应按 Content Digest 存放在 JSONL 之外；JSONL 只
  保存有界 Reference。
- Line Size、Tool Result、Context Checkpoint、Scan 与 Tail Repair 都必须有界。
- Workspace 不能重定向全局存储，也不能提供 Storage Bucket Name。

## 13. 迁移

从当前 Workspace 数据库迁移必须是增量且幂等的：

1. 解析全局 Product Home 并初始化全局 Schema。
2. 打开 Workspace 时检测旧 `.latte/latte-code.db`。
3. 把 Project、Workspace、Session、Run 与 Effect Metadata 导入全局数据库。
4. 把 `thread_transcript_v2` Content 导出到 Workspace JSONL Bucket，保留顺序、
   ID、Redaction 和 Lineage。
5. 记录 Import Fingerprint，防止重试时重复 Session 或 Effect。
6. 在用户显式清理前保持 Legacy Database 不变。

新 Session 使用 `jsonl_v1`。滚动迁移期间既有 SQLite Session 继续可读，或由
显式流程迁移；绝不静默删除旧表。启用全局存储契约后，Workspace 层的
`database.path` 仍可为迁移兼容而解析，但会被忽略。

## 14. 必须验证的场景

实现完成前，UT 与 Final-Binary E2E 至少必须证明：

- Provider 启动失败会保留已接受 User Card 并追加一个有界 Failure Card，不重复
  Prompt，也不把它恢复到 Composer。
- SQLite、JSONL 和持久化应用日志中都不存在 Credential Value 或原始 Provider
  Diagnostic。
- Workspace 中不存在 Latte Code Database 或 Session 文件。
- 两个 Workspace 共用全局 Database，但使用不同 Session Bucket。
- 两个不同 Session 可以并发运行；同一 Session 的两个 Writer 会被 Fencing。
- JSONL Tail Repair 只删除一条撕裂的末行。
- Follow-up 失败只追加一个不可变失败 Child，并保留所有更早 Child；可重试的配置
  失败允许继续提交 Follow-up。
- `Started` Effect 在崩溃或失去 Lease 后变为 `Unknown`。
- 已 Observed Effect 缺少 JSONL Tool Result 时可以修复，且不会再次执行 Effect。
- Archive、Fork、Workspace Rebinding 和幂等 Legacy Import 保留历史与 Lineage。

这些场景继续遵守仓库独立的 UT 95%、Final-Binary E2E 80% 和 All-Target 90%
覆盖率卡点。

## 15. 当前实现状态

Latte Code 当前遵守 `database.path`，默认值为工作区根目录下的
`.latte/latte-code.db`。相对路径以该根目录解析，也支持绝对路径。目前尚未启用
`LATTE_CODE_HOME` Product State 切换、Legacy Import 或产品级全局数据库默认值。

迁移 9 为 v2 Session Metadata 增加有界、脱敏的 Title 与 Canonical
`workspace_root`。Catalog 查询不会反序列化 Transcript Row。TUI 按当前
Canonical Workspace 过滤 Session，启动时恢复最新匹配项，`/new` 创建瞬态
Draft，`/sessions` 与 `/resume` 提供显式选择。
升级 v8 数据库时，只有数据库物理位于当前 Canonical Workspace 内，才能认领旧
Session Row；迁移会在 Schema 事务内补齐 Workspace 与 Title。若外置或共享 v8
数据库含有无法归属的 Session Row，则返回明确的 Migration Error，不会静默归到
调用方 Workspace。

迁移 10 把 Singleton Runtime Lease 改为带 Scope 的 Lease Row。Legacy Headless
Run 继续使用 `runtime` Scope；Thread v2 则为每个 Session 使用独立的
`thread:<session-id>` Scope。每次获取都会使用独立的 Coordinator Owner，因此同一
Session 的第二个 Coordinator 会被拒绝，而不同 Session 仍可并发；Runtime 在一次
操作返回时释放 Lease，持久化的 Input/Permission 等待会进入无 Writer 的安全静止，
而不是被误判为 Orphan Run。Fencing Token 仍保持全局单调，因此 Session 并发不会
削弱重启恢复语义。
Thread Lease 被释放时如果 Child 仍 Active，表示 Coordinator 非正常退出：同一释放
事务会先中断 Child；如果已有 `Started` Effect，则把它们标记为 `Unknown` 并要求
Reconciliation，然后才删除 Lease。因此 TUI 不需要等进程重启，就能离开无 Lease
的虚假 `Running` Projection。

新对话只在本地 Prompt 与非密钥 Binding 校验阶段保持进程内状态。一旦接受，TUI
会在解析 Credential 或构造 Provider 之前，通过同一事务持久化 `threads_v2` Row、
关联 Run、User Transcript Entry、精确 Lease Token 与 Durable `Start` Event；创建
与 Start 之间不会提交 Token 为零的 Running Session。Credential 缺失或 Provider
构造失败会追加无密钥、可重试
的 Failure Card，让 Session 回到 `Ready`；Composer 保持为空且可继续输入。语法
非法的 Binding 或持久边界之前的 Storage Failure 仍会恢复 Draft。
一次已经发出的 Provider Request 若发生 HTTP、Authentication、Transport、Timeout
或 Model Selection Failure，也使用相同的可重试 Child Failure Path：已接受的 User
Card 与已脱敏 Failure 保持持久化，后续 Follow-up 创建新 Child。非法 Successful
Response 与不安全的 Provider ID 仍属于 Terminal Protocol Failure。
TUI 只使用已脱敏且来源为 New-Session/Follow-up Commit Path 的 User Card 对账
Composer Submission；文本相同的 Input-Request Answer 不能误确认它。Input Answer
使用独立的 Submission Identity，并绑定 Session、Run 与 Request ID。该 Request
拥有 Editor 时，Shift+Enter 始终插入换行；Command 失败后，只有 Authoritative
Snapshot 证明精确 Input Card 未提交，才恢复输入值。Terminal Session 的普通提交
不会消费 Composer Draft；如果 Active Child 在 Queued Follow-up 提交前结束，则
恢复该 Draft。

Model Selection 是 Session Binding Transition，而不是 Editor Preference。只有
不存在 Active Child 的 `Ready` Session 可以在精确 `thread:<session-id>` Lease
与 Expected Revision 下切换。该事务替换完整的非密钥 Provider Binding、追加一条
有界 System Card，并发出 `BindingChanged`。TUI 在刷新的 Snapshot 包含所选
Provider 与 Model 前阻止竞争的 Follow-up。这个 Transition 不解析 Provider
Credential；Provider 构造及其已脱敏 Failure 都属于下一个已经持久接受的 Child。

JSONL Transcript Layout、Repair、Session Scoped Writer、全局 Product Home、
跨 Workspace Discovery、Legacy Import 与移除 SQLite Transcript Duplication
尚未实现。当前配置的数据库继续保留供 Headless CLI 读取的既有 v1 Run/Control
Record。

UT 覆盖配置路径解析、迁移 9/10、Catalog Metadata、Scoped Authority 与持久、
可重试的 Provider Configuration Failure。最终二进制 E2E 覆盖 Workspace 本地和
显式配置的数据库、`/resume`、不额外调用 Provider 的 `/new`、长 Transcript 的尾部
恢复与 Follow-up，以及 Provider Setup 失败后在同一 Session 中进行多行重试。

## 16. 交付阶段

1. 增加全局 Product Home、Workspace Storage Key、全局 Catalog 和 Per-Session
   Lease。
2. 引入 Draft 校验与持久 New Session/Follow-up 接受边界，使 Provider 启动失败
   成为可见、可重试的 Child Failure。
3. 增加有界 JSONL Writer、Reader、Tail Repair、Checkpoint 和组合 Projection。
4. 把现有带 Fencing 的 Effect 生命周期与 JSONL Tool Call/Tool Result 顺序集成。
5. 增加 Legacy Import、全局 Session Discovery、Archive、Fork、Delete 与
   Workspace Rebinding。
