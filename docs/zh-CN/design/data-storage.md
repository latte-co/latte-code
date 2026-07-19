# 全局 Session 与数据存储设计

状态：**部分已实现。**

全局 Product State Database、按 Workspace 隔离的 Session Catalog、Schema 9
Metadata、启动恢复与 Provider Configuration 失败边界已经实现。Conversation
Card 仍保存在 SQLite。本文中按 Session 存储的 JSONL Transcript、恢复与迁移
部分仍是目标设计；落地时必须保留现有 Engine 对 Effect、Permission、Lease、
Fencing、去重和 `Unknown` Reconciliation 的安全约束。

## 1. 已确定的决策

| 数据类型 | 权威存储 | 契约 |
| --- | --- | --- |
| Session 对话内容 | 全局、每 Session 一个 JSONL | 只追加 user、assistant、tool 与上下文记录。 |
| Project、Workspace、Session 元数据 | 全局 SQLite | 发现、搜索、生命周期、Provider Binding、血缘和归档状态。 |
| Run 与 Effect 控制状态 | 全局 SQLite | 事务化 Run 状态、Effect、Permission、Lease、Checkpoint、Evidence 与去重。 |
| Draft 与 Provider 运行时 | 进程内存 | Draft Prompt、HTTP Stream、Retry、Cancellation、Delta 和启动错误。 |
| Credential | 不持久化 | 只允许持久化非密钥的 Credential Reference 与 Generation。 |

其他已确定决策：

- Session 和数据库状态绝不存放在 Workspace 目录下。
- Session 文件按 Workspace 分桶，不按日期分层。
- JSONL 是对话内容唯一的回放来源；SQLite 不复制 Transcript。
- 不引入 Transcript Outbox，也不建立持久化的 Provider Attempt 表。
- Provider 启动失败不是 Session 事实，绝不持久化。
- Provider Streaming Delta 是瞬态数据；只有完整 Provider Outcome 才有资格持久化。

## 2. 术语

- **Project** 表示逻辑仓库身份，多个 Git Worktree 可以共享一个 Project。
- **Workspace** 表示一个物理 Checkout 或非 Git 工作目录。
- **Session** 表示一个用户可见的 Conversation。迁移期间它与现有
  `ThreadId` 一一对应，不引入第二套身份。
- **Run** 表示一次用户提交及其 Provider/Tool Continuation Loop。
- **Effect** 表示可能改变或观察外部状态、由 `latte-engine` 掌握权限的操作。
- **Draft** 表示尚未到达持久化提交点、只存在于内存的新 Session 或 Follow-up。

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
`database.path` 必须明确拒绝，不能静默重定向用户历史。只有进程环境或可信
用户配置可以选择存储 Home。

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

所有 Workspace 共用一个全局数据库后，不能继续使用当前数据库级 Singleton
Lease。目标 Lease 必须按 Session 隔离：

```text
session_leases
  session_id（主键）
  owner
  fencing_token
  expires_at_ms
```

不同 Session 可以并发运行。一个 Session 最多只有一个有效 Engine Owner 和
一个 JSONL Writer。Lease 过期后重新获取必须推进 Fencing Token。旧 Owner
不能开始或观察 Effect，并且失去 Ownership 后必须关闭 Session Writer。

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
→ 在内存中解析 Credential Reference
→ 构造 Provider
→ 发起第一次 Provider Request
```

配置、Credential、Model、Authentication、Transport、Timeout 或其他启动失败
不会留下 Session Row、Run Row、JSONL、持久化日志记录或 Telemetry Payload。
已脱敏的展示错误只保留在当前 UI State 中，Prompt 回到 Composer 供用户重试。

持久化提交点是第一个完整且有效的 Provider Outcome：

- 完整 Assistant Message。
- 完整 Assistant Tool-Call Envelope。
- 有效 Input Request。

到达提交点后，应用执行：

1. 插入不可被列表发现的 `materializing` Session Metadata Row。
2. 写入并 Sync JSONL Header、User Message 与完整 Provider Outcome。
3. 创建该 Outcome 所需的持久化 Run/Control State。
4. 根据真实 Lifecycle 把 Session 标记为可发现。

启动时删除没有有效文件的 `materializing` Row，或根据有效的自描述文件修复
Catalog Metadata。空的失败 Session 绝不能出现在 Session Discovery 中。

### 8.2 Follow-up

Follow-up 在第一个完整 Provider Outcome 之前同样只是内存 Draft。启动失败
不会创建 Child Run，也不会追加 User Prompt。原 Session 保持字节级不变，
Prompt 回到 Composer。

完整 Outcome 到达后才物化 Run，并一起追加 User/Outcome Record。如果持久化
工具工作之后的 Provider Request 失败，只保留 `Interrupted` 或
`ReconciliationRequired` 等最低限度通用 Run State，仍然不持久化 Provider
Error Text。

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
`database.path` 变为非法配置。

## 14. 必须验证的场景

实现完成前，UT 与 Final-Binary E2E 至少必须证明：

- Provider 启动失败不会改变 Session 数量或 JSONL Tree。
- SQLite、JSONL 和持久化应用日志中都不存在 Provider 启动错误文本。
- Workspace 中不存在 Latte Code Database 或 Session 文件。
- 两个 Workspace 共用全局 Database，但使用不同 Session Bucket。
- 两个不同 Session 可以并发运行；同一 Session 的两个 Writer 会被 Fencing。
- JSONL Tail Repair 只删除一条撕裂的末行。
- Follow-up 失败后原 Session 保持不变。
- `Started` Effect 在崩溃或失去 Lease 后变为 `Unknown`。
- 已 Observed Effect 缺少 JSONL Tool Result 时可以修复，且不会再次执行 Effect。
- Archive、Fork、Workspace Rebinding 和幂等 Legacy Import 保留历史与 Lineage。

这些场景继续遵守仓库独立的 UT 95%、Final-Binary E2E 80% 和 All-Target 90%
覆盖率卡点。

## 15. 当前实现状态

Latte Code 当前使用 `$HOME/.latte/latte-code/state.db` 作为唯一 Product State
Database。绝对路径的 `LATTE_CODE_HOME` 可以覆盖 Product Home。旧的
`database.path` JSONC 字段为保持配置兼容仍可被解析，但不再重定向产品状态。

迁移 9 为 v2 Session Metadata 增加有界、脱敏的 Title 与 Canonical
`workspace_root`。Catalog 查询不会反序列化 Transcript Row。TUI 按当前
Canonical Workspace 过滤 Session，启动时恢复最新匹配项，`/new` 创建瞬态
Draft，`/sessions` 与 `/resume` 提供显式选择。

新对话在 Provider Binding 解析成功前保持进程内状态。Credential 缺失、
Binding/Configuration 非法或 Provider 构造失败时，不会创建 `threads_v2` Row、
关联 Run、Transcript Entry 或持久化 Error Record；TUI 恢复 Composer Draft 并
展示无密钥错误。已经持久化的 Engine 安全状态不会因此被删除。

JSONL Transcript Layout、Repair、Session Scoped Writer 与移除 SQLite
Transcript Duplication 尚未实现。全局数据库也继续保留供 Headless CLI 读取的
既有 v1 Run/Control Record。

UT 覆盖 Product Home 解析、迁移 9、Catalog Metadata 与 Provider Configuration
不物化状态。最终二进制 E2E 覆盖全局数据库、`/resume`、不额外调用 Provider 的
`/new`，以及 Provider Setup 失败后仍为空的 Catalog。

## 16. 交付阶段

1. 增加全局 Product Home、Workspace Storage Key、全局 Catalog 和 Per-Session
   Lease。
2. 引入 Draft 新 Session/Follow-up 生命周期，使 Provider 启动失败保持瞬态。
3. 增加有界 JSONL Writer、Reader、Tail Repair、Checkpoint 和组合 Projection。
4. 把现有带 Fencing 的 Effect 生命周期与 JSONL Tool Call/Tool Result 顺序集成。
5. 增加 Legacy Import、全局 Session Discovery、Archive、Fork、Delete 与
   Workspace Rebinding。
