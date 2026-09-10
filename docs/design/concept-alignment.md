# Latte Code 概念体系对齐

状态：**分批进行中**。本文档定义 Latte Code 的规范概念层级与术语，并给出改名映射
与分批方案。批次 0（文档与死代码）与批次 1（`Thread → Session`，含 schema 13 迁移与
升级兼容）**已落地**；批次 2（`Run → Turn`）仍被 v1 状态/事件退役阻塞，批次 3/4 待
启动。各批次当前状态以第 5 节为准；已落地批次的权威是当前代码，本文档记录其决策与
映射，剩余批次仍按本文提案执行。

调查范围：`crates/` 全部六个 crate、`docs/` 全部设计文档、HTTP 契约与契约测试、
CLI/TUI 用户可见文案。所有结论附 `file:line` 证据。

---

## 1. 问题陈述

同一个领域概念在代码、协议、界面、文档四层使用不同名称，且不存在任何翻译层说明。
最集中的两处：

- 「一次持久对话」有四个名字：`Session`（类型）、`session`（HTTP 路径与 CLI）、
  `conversation`（注释与 TUI 面板标题）、`Session`（引擎 API 注释与错误串）。
- 「一次用户提交」有四个名字：`Run`（类型与 HTTP 字段）、`turn`（HTTP 注释、服务端
  日志、TUI 提示）、`child`（架构文档与 core 注释）、`linked run`（守卫函数名）。

后果不是风格问题，而是可观察的契约缺陷。举三例：

1. 创建会话时请求体发 `thread_id`、响应体回 `session_id`，是同一个 UUID
   （`crates/latte-server/src/http.rs:265` vs `:277`，赋值证据 `:584`）。契约测试
   固化了这个不一致（`crates/latte-server/tests/contract.rs:173` 发送 →
   `:182` 读取）。
2. SSE 事件名为 `thread_changed`，其 payload 字段为 `session_id`
   （`http.rs:154-157`、`:1212`）。
3. TUI 同一屏幕上，transcript 卡片标题写 `"Run {n} · Running"`
   （`crates/latte-tui/src/thread.rs:2360-2361`），状态栏写
   `"Follow-up queued behind the active turn"`（`:1795`），二者指同一对象，
   界面无任何说明。

---

## 2. 规范层级

以下是本提案确立的概念层级。每层给出规范名、当前实现名与身份标识。

```
Workspace                    工作区：一个文件系统根
  │                          （多个 Workspace 可归属同一 Project = 共享 git common dir）
  │
  └─ Session                 会话：一次持久对话，用户可见的顶层单位
     │                       当前实现：Session / SessionId / threads_v2
     │                       上下文在此层累积、压缩、恢复
     │
     ├─ Transcript           记录：append-only 的卡片流，JSONL 为权威
     │                       当前实现：TranscriptPage / conversation_outbox 表
     │
     └─ Turn                 轮次：一次用户提交及其完整 Provider/Tool 循环
        │                    当前实现：Run / RunId / thread_runs_v2
        │                    完成后不可变，按 ordinal 串成链
        │
        └─ Round             回合：Turn 内一次 assistant tool 调用批次
           │                 当前实现：无类型，靠 (entry.sequence, ordinal) 元组反查
           │
           └─ Call → Effect  单次工具调用，及其特权执行凭据
                             当前实现：ToolCall（provider 侧）/ effect_id: String
```

层级判据：

- **Session** 是用户在 `/sessions` 列表里看到的一行，有标题、工作区、创建时间。
  fork 产生新 Session 并记录 `parent_thread_id`（`crates/latte-core/src/thread.rs:257`）。
- **Turn** 是一次「你说、它做完」。follow-up 产生新 Turn，同 Session。
  `SessionLifecycle` 名义挂在 Session 上，实际是 active Turn 状态的投影
  （`thread.rs:57` 注释称「区别于 child run」，但 `:61-75` 七个变体逐条描述 child，
  注释自相矛盾）。
- **Round** 是 Turn 内的一次 provider 往返所产生的 tool 调用批次。它是真实存在的
  执行单元 —— `handle_provider_tool_round`（`crates/latte-headless/src/thread.rs:1320`）、
  `continue_provider_tool_round`（`:1397`）—— 但没有类型，身份从 transcript 反查
  （`tool_round_for_call`，`:2139-2167`）。

---

## 3. 术语表

规范术语及其定义。**斜体**表示当前实现名与规范名不一致。

| 规范术语 | 定义 | 当前实现 |
| --- | --- | --- |
| Workspace | 一个文件系统根，会话的归属边界 | `workspace_root: String` ✓ |
| Project | 共享同一 git common dir 的 Workspace 分组 | `project_key`（仅存于 SQL，core 层缺席） |
| Session | 一次持久对话 | *`Session` / `SessionId` / `threads_v2`* |
| Turn | Session 内一次用户提交及其完整循环 | *`Run` / `RunId` / `thread_runs_v2`* |
| Round | Turn 内一次 assistant tool 调用批次 | *无类型* |
| Call | Round 内单次工具调用 | `ToolCall`（provider 侧）✓ |
| Effect | 一次特权操作的持久凭据与生命周期 | `effect_id: String`（无类型） |
| Transcript | Session 的 append-only 卡片流 | `TranscriptPage` ✓ / *表名 `conversation_outbox`* |
| Binding | Session 绑定的 provider + model + 指纹 | *`ProviderBinding` 与 `SessionProviderBinding` 同名不同物* |
| Lease | 执行权威的租约与 fencing token | `Lease` ✓ |
| Verification | 变更后的验证及其证据 | *`Evidence` / `VerificationEvidence` / `VerificationRecord` 三类型同概念* |
| Reconciliation | `Unknown` effect 的显式对账 | ✓ 全库一致，无需改动 |

### 3.1 需要消歧的复用词

以下单词在不同层级重复使用，含义不同。规范做法：**加层级前缀**。

| 词 | 当前的多重含义 | 建议 |
| --- | --- | --- |
| `revision` | v1 run revision（`protocol.rs:37`）/ v2 thread revision（`thread.rs:235, 362`） | `session_revision` / `turn_revision`，全部加前缀 |
| `sequence` | transcript seq / thread snapshot seq / thread event seq（三者同源）/ v1 run event seq（独立） | 前三者保留 `sequence`，第四者改 `run_event_sequence` |
| `ordinal` | Session 内 Turn 序号（`thread.rs:170`）/ Round 内 Call 下标（`headless/thread.rs:1410`） | `turn_ordinal` / `call_ordinal` |
| `checkpoint` | v1 协调者自定义恢复负载（`storage.rs:431`）/ v2 引擎生成的 effect 阶段标记（`lib.rs:130`） | 后者改 `effect_stage_marker` |
| `catalog` | Session catalog / Workspace catalog / Model catalog / Command catalog | 保留但强制加限定词，禁止裸用 |
| `draft` | 会话草稿 `NewSessionDraft` / 模型预选 `draft_model` / 输入暂存 `deferred_composer_draft` | `session_draft` / `pending_model_selection` / `deferred_composer_text` |
| `failed` | HTTP 层 = 500 内部错误（`http.rs:1096`）/ CLI 层 = 非 5xx 兜底（`server_client.rs:69`） | **含义相反，必须改**：CLI 侧改 `unclassified` |

---

## 4. 关键约束：Turn 改名被 v1 阻塞

**`Session` 与 `Turn` 的改名难度不对称。**

### 4.1 Session 改名是干净的

`Session` 是纯 v2 概念，无 v1 同名类型。唯一障碍是一张 v1 遗留死表：

```sql
-- crates/latte-engine/src/storage.rs:369
CREATE TABLE sessions(id TEXT PRIMARY KEY, created_at_ms INTEGER NOT NULL);
```

全库零读写（grep `FROM sessions` / `INTO sessions` 只命中 schema 定义本身）。
drop 掉即腾出名字。

### 4.2 Turn 改名不干净

v2 的 run **就是** v1 的 run，不是相似概念：

- SQL 外键：`thread_runs_v2.run_id TEXT PRIMARY KEY REFERENCES runs(run_id)`
  （`storage.rs:469`）
- 类型复用：`SessionRunSummary.run_id` 类型是 `RunId`，不存在 `SessionRunId`
  （`thread.rs:168`）
- 状态映射：`SessionRunStatus` 是 `RunStatus` 的一对一无损映射
  （`storage.rs:4909-4920`）
- revision 直读：`run_revision: state.revision`（`storage.rs:5004`）

区分 v1 run 与 v2 turn 的机制是运行时布尔判断 `is_thread_linked_run(run_id)`
（`lib.rs:743`），12+ 处 `reject_linked_run` 守卫是这个类型缺陷的补丁
（`lib.rs:842, 930, 2169, 2191, 2213, 2230, 2296, 2307, 2321, 2339, 2345, 2358`；
`process.rs:385, 438, 487`）。

因此 `Run → Turn` 只有两条路：

1. 连 v1 `runs` 表一起改 —— 波及 v1 兼容路径，风险高；
2. 造 `TurnId` 与 `RunId` 双类型 —— 正是当前混乱的成因，不可取。

**结论：Turn 改名应等 v1 退役，与之绑定为同一批次。**

### 4.3 v1 的实际存活范围

已确认 v1 的「命令」侧是死代码，「状态 + 事件」侧仍是 v2 地基：

- 死：`RuntimeCommand` / `CommandEnvelope` / `ReadModelEnvelope`
  （`protocol.rs:6, 44, 54`）生产代码零引用，仅测试使用
- 活：`RuntimeEvent`（`protocol.rs:92`）仍被 storage 写入
  （`storage.rs:1310, 1633, 3511`）
- 活：`runs` 表 / `RunState` / `RunStatus` —— v2 直接依赖

另需修正文档矛盾：`README.md:50` 称 v1 run-id 契约已移除，
`docs/roadmap.md:80` 却打勾保留「v1 CLI 兼容路径」。代码站 README 一侧
（`server_client.rs:131` 注释、`:188-192` 把 v1 `--allow`/`--deny` 变为硬错误）。
roadmap:80 是过期条目。

---

## 5. 分批方案

### 批次 0：文档与死代码（无行为变更）

状态：**已完成**，除 v1 `sessions` 死表外全部落地。

不触碰任何类型名与协议字段，只消除虚假信息。

| 项 | 动作 | 证据 |
| --- | --- | --- |
| `TurnSupervisor` 虚构类型 | 改为 `SessionRuntimeService`，并区分「已实现」与「提案」词汇 | `asynchronous-turn-runner.md:24`，全库零 Rust 定义 |
| 同文档其余虚构类型 | `RuntimeInput` / `ControlInput` / `TrustedReminder` / `ReminderSource` / `InputId` 标注为提案 | 同上，全部零命中 |
| `InputQueued` progress | 标注未实现；实际只有 3 个变体 | `asynchronous-turn-runner.md:72, 84` + `event-projection-and-replay.md:16` vs `thread.rs:395-409` |
| `data-storage.md` 自相矛盾 | 术语表 `:38` 定义 Run，`:432` 却写 Turn，统一到 Run（批次 2 再整体改名）；顺带修正过期的 Schema 11 断言 | 同文件；`SCHEMA_VERSION = 12` |
| roadmap:80 过期条目 | 与 README:50 对齐 | 见 4.3 |
| ~~v1 `sessions` 死表~~ | **推迟到批次 1**：drop 需要 schema 迁移，而批次 1 本就要写一个 | `storage.rs:369`、`:847` |
| `EffectStatus::Declared` | 标注为测试专用状态，生产路径从 `Prepared` 起步 | `storage.rs:3752-3753` 带 `#[cfg(test)]` |
| `ProviderOutcome` 冗余别名 | 移除 `type ProviderOutcome = ProviderResponse;` | `provider.rs:78` |

### 批次 1：Session 改名（破坏性，已落地）

状态：**已完成**。`Thread* → Session*` 贯穿全部六个 crate、SQL 表、HTTP/SSE 契约、
CLI `--json` 字段、配置键 `thread` → `session` 与模块文件名（`thread.rs` → `session.rs`）。

影响面统计（改名前 `crates/**/*.rs` 混合命名标识符）：

| 标识符（改名前） | 出现次数 |
| --- | --- |
| `thread_session_v2` | 58 |
| `ThreadSessionSummary` | 50 |
| `thread_sessions_v2` | 18 |
| `thread_sessions_v2_for_workspace` | 17 |
| `thread_sessions_v2_by_exact_title_for_workspace` | 14 |
| `..._by_exact_title_for_workspace_paged` | 11 |
| `thread_sessions_v2_paged` / `thread_sessions_paged` | 5 / 5 |
| `thread_session_fork` | 2 |

这些缝合词本身就是分裂的产物，改名后自然消失。典型如
`Json<SessionListResponse<latte_core::ThreadSessionSummary>>`（`http.rs:620`）
—— 一个类型表达式里 Session 与 Session 各出现一次。

同批必须修正的协议不一致（均已落地）：

- 请求体 `thread_id` → `session_id`（与响应体统一）
- SSE 事件名 `thread_changed` → `session_changed`
- CLI `--json` 内层 `thread_id` → `session_id`
- 配置键 `thread.*` → `session.*`（`max_tool_rounds` / `provider_timeout_ms` 等）
- 先 drop 掉 v1 死表 `sessions`（迁移 1–12 中建表但全库无 INSERT/SELECT/UPDATE，
  无外键引用），腾出这个名字，再把 v2 表改为无前缀名。**批次 0 已确认它是死表
  但推迟了 drop，两件事共用同一次 schema 迁移（12 → 13）**。
- schema 13 的最终 v2 表名：`threads_v2 → sessions`、`thread_runs_v2 → session_runs`、
  `thread_active_runs_v2 → session_active_runs`、`thread_events_v2 → session_events`、
  `thread_command_dedup_v2 → session_command_dedup`、
  `thread_commit_sources_v2 → session_commit_sources`、
  `thread_effect_canonical_v2 → session_effect_canonical`；列 `thread_id` / `parent_thread_id`
  统一改为 `session_id` / `parent_session_id`。`_v2` 后缀一并去掉——`session_` 前缀
  已与 v1 `runs` / `events` 命名区隔。
- `conversation_outbox` 保留原名（它同时是 transcript 存储与 JSONL 出箱，见 6.2）；
  仅其 `thread_id` 列改名为 `session_id`。
- legacy import 改写为读旧名（attach 的 `legacy_import.threads_v2` 等历史表）写新名
  （`main.sessions` 等）；版本门收紧为 `9..SCHEMA_VERSION`（不包含当前版本——当前
  schema 的库没有可导入的旧表）。
- **升级重放兼容（破坏性改名的读侧补偿）**：schema 13 只改 DDL，但升级前已持久化的
  幂等记录与权限边界按旧命名存数据，重试必须仍被识别为同一条命令、而不是误判
  `idempotency_mismatch`：
  - 幂等 digest：`session_command_dedup.digest` 里旧值用 `thread.start` /
    `thread.follow_up` 命名空间和 `thread_id` / `expected_thread_revision` 键。
    重放比对时同时接受新旧两种 digest（`legacy_{create,follow_up}_command_digest`
    逐字节复现旧摘要；协议版本值与 binding 序列化均未变，仅操作命名与键名不同）。
  - 结果快照：旧 `result_json` 的 id 字段是 `thread_id` / `parent_thread_id`；
    `SessionSnapshot` / `SessionSummary` / `SessionEventEnvelope` 上加
    `#[serde(alias = "thread_id")]` / `alias = "parent_thread_id"` 只读兼容反序列化，
    线协议仍为硬 break。
  - 验证 Effect：升级前停在 `waiting_permission` 的验证 effect id 是
    `thread-verification:<run>`；审批识别改为同时接受
    `session-verification:` 与 `thread-verification:` 前缀，否则旧验证会被误当成
    普通 provider 工具续跑而失败。
  - 由最终二进制 E2E 守护：真实构造 v13 库后反向到 v12，分别验证「升级前 durable
    accept 后重试同 `command_id` 重放为 200」与「升级前等待中的 verification gate
    升级后批准并完成」。
- **旧会话 Provider binding 升级兼容（评审 P1）**：升级保留旧 `binding_json`，但本批
  带入了两项会改变指纹的改动，直接全等比对会让旧会话的 follow-up / input / 工具批准
  全部解析失败：
  - provider 新增 `headers` 字段：空 map 加 `skip_serializing_if`，无自定义 headers 的
    provider 配置指纹与升级前逐字节一致；配置了非空 headers 仍参与绑定。
  - 内建工具描述从占位符 `Engine-owned <name> operation` 改为真实文档，改变
    `tools_fingerprint`。`resolve_session_bound` 增加窄兼容桥：除 `tools_fingerprint`
    外的安全身份字段（provider/model/版本/config 指纹/凭证引用与代数/data scope/
    aliases）必须全等；tools 指纹额外接受「用当前工具集按旧占位符描述重算」的值。
    这不是放宽：重算基于当前工具，任何 `name` / `input_schema` / `effect` / `version`
    漂移都会失配，只接受纯描述文字（模型指引字段，非权限边界）。
  - 由跨平台最终二进制 E2E 守护：持久化真实旧版（占位符）指纹的 binding，升级后
    follow-up 仍能解析 provider 并跑完。
- **轮次预算按 active run 的持久记录累计（评审 P2）**：`max_tool_rounds` 必须在同一
  run 内跨 input 回答、工具批准、重启续跑累计。早期基于「最后一个 User 消息之后」的
  消息计数会被 input 回答（它也持久化为 `User` transcript 卡片）重置，模型可
  「调工具→请求输入」循环突破预算。改为按 active `run_id` 计数持久 transcript 中
  `Assistant` 且 `payload.tool_calls` 非空的卡片（`durable_tool_rounds_for_run`）；
  历史 turn 的 run_id 不同、input 回答只加 `User` 卡片，三者都不会重置。由
  「工具→input→回答→再工具，第三轮被预算拦住」的最终二进制 E2E 守护。

**协议决策点（已定并执行）**：v1 端点承诺过稳定性
（`versioned-rpc-contract.md` §2.1），字段改名属 breaking change，按该文档
§2.2 本应走 v2。但当前处于早期阶段，#16 已有先例（list 分页、422→400）在 v1 内
直接 breaking。**决定：v1 内直接破坏性改名，不加兼容层。**

### 批次 2：Turn 改名（与 v1 退役绑定）

前置条件：v1 `runs` 表与 `RunState` 退役，或确立独立的 v2 turn 存储。

届时一并解决：

- `Run → Turn`，`RunId → TurnId`
- 移除 12+ 处 `reject_linked_run` 守卫（类型上即可区分，无需运行时查库）
- `revision` / `ordinal` 加层级前缀（见 3.1）
- TUI 文案统一（`"Run {n}"` 与 `"active turn"` 二选一）

### 批次 3：结构性补强（独立于改名，可并行）

以下不是改名，是命名体现不出层级的地方需要补类型：

1. **Round 无类型** —— 身份是 `(transcript_entry.sequence, ordinal)` 元组，
   从 payload 反序列化恢复（`headless/thread.rs:2139-2167`）。建议引入
   `RoundId` 或至少一个 `Round` 结构。
2. **`effect_id` 是 String** —— 隐含结构
   `thread-effect:{run_id}:{round_sequence}:{ordinal}:{tool_call_id}`
   （`headless/thread.rs:1416`）无解析器，与全库 `typed_id!` 惯例
   （`ids.rs:6-34`）不一致。
3. **`SessionEffectDescriptor` 与 `SessionEffectPresentation` 字段完全同构**
   （`lib.rs:289-295` vs `:334-340`），仅靠是否脱敏区分，名字上分不出哪个是
   可执行权威。建议改名体现信任级别，如 `EffectAuthority` / `EffectView`。
4. **Project 在 core 层缺席** —— `ThreadSessionSummary` 只到 `workspace_root`
   （`thread.rs:255`），无 `project_key`，跨 worktree 归属只存在于 SQL 外键
   （`storage.rs:679`）。
5. **`SessionProviderBinding` 双版本号** —— 类型名后缀 `V2` 指 thread 协议版本，
   字段 `version: u32` 指 binding 自身版本（`BINDING_VERSION = 1`,
   `registry.rs:17`）。读名字必然误解。

### 批次 4：状态与错误词汇对齐（独立，用户可见）

三套状态枚举值域重叠但不相等，面向同一用户：

| `SessionLifecycle` | `SessionRunStatus` | CLI `TerminalOutcome` | TUI |
| --- | --- | --- | --- |
| `ready` | `completed` | `completed` | "Ready" / 卡片显示 "Completed" |
| — | — | `denied` | 无对应显示 |
| — | — | `cancelled` | 无对应显示 |

具体缺陷：

- `lifecycle=ready` 在 CLI 输出为 `"completed"`（`server_client.rs:311-314`），
  同一 JSON 内 `{"status":"denied", "data":{"session":{"lifecycle":"ready"}}}`
  自相矛盾。
- `denied` / `cancelled` 只存在于 CLI（`server_client.rs:275, 363`），
  HTTP 协议与 TUI 无对应值。
- HTTP `rejected` → CLI `usage`、HTTP `failed` → CLI `internal` 两处改名
  （`server_client.rs:112-117`）。
- 字符串 `failed` 在两层含义相反（见 3.1）。
- `idempotency_mismatch`(422) 在 CLI 侧无映射分支，退化为
  `{"code":"failed","message":"422 Unprocessable Entity: ..."}`
  （`server_client.rs:103-122`）。

---

## 6. 遗留决策点

### 6.1 是否引入 `Conversation` 作为第三层名词

当前 `conversation` 一词在注释、TUI 面板标题（`tui/thread.rs:2353`）、
`/new` 命令描述（`command.rs:58`）中与 session 混用。本提案主张
**废弃 conversation 作为概念名**，统一为 Session，仅在
`conversation history`（指发给 provider 的 message 序列）这一技术含义上保留。

### 6.2 `conversation_outbox` 表是否改名

该表原名 `thread_transcript_v2`，v11 迁移改为 outbox（`storage.rs:657-659`），
因为它同时承担两个职责：transcript 持久存储 + JSONL 落盘的事务性出箱。
出箱确认后行会被删除（`acknowledge_conversation_outbox`，`storage.rs:1838-1845`），
但 `TranscriptPage` 又是快照的一部分。

职责重叠是真实的，改名前需先决定是否拆表。

### 6.3 transcript payload 的双重信任级别

同一个 `payload` 字段，一部分是运行时权威、一部分严禁作为权威：

- 权威：`payload.tool_calls` 是 provider 续跑队列，重启后据此恢复
  （`headless/thread.rs:2145` 明言 "provider grammar, not a best-effort
  display summary"）
- 严禁：effect descriptor 绝不可从 transcript 重建
  （`engine/lib.rs:285-287`）

类型上完全区分不出来。这是安全边界问题，不只是命名问题。

### 6.4 Round 与 compaction 的交互

若引入 transcript 压缩（见 `.tmp/openai-chat-findings.md` P1-3），
Round 身份依赖 `transcript_entry.sequence`，压缩若改变 sequence 或移除 entry，
会破坏 `tool_round_for_call` 的反查。批次 3 的 `RoundId` 应先于 compaction 落地。

---

## 7. 附录：未实现但已在文档中声明的概念

| 概念 | 文档位置 | 代码状态 |
| --- | --- | --- |
| `TurnSupervisor` | `asynchronous-turn-runner.md:24` | 零命中，实际是 `SessionRuntimeService` |
| `RuntimeInput` / `ControlInput` | 同文档 §2 | 零命中 |
| `TrustedReminder` / `ReminderSource` / `InputId` | 同文档 §2 | 零命中 |
| `InputQueued` / `consumed` / `expired` progress | 同文档 `:72, :84` | 零命中，实际只有 3 变体 |
| Harness Profile | `roadmap.md:39, 65, 68, 232` | 零命中，均为未勾选条目 |
| `EffectStatus::Declared` | `storage.rs:281` | 唯一写入者带 `#[cfg(test)]` |
| v1 `RuntimeCommand` 族 | `protocol.rs:6, 44, 54` | 生产零引用 |
| SQL `sessions` 表 | `storage.rs:369` | 零读写 |

文档使用陈述句描述未实现类型（如「`latte-headless` 拥有 `TurnSupervisor`」），
读者无法区分已落地与提案。批次 0 应确立约定：**未实现概念必须显式标注**。
