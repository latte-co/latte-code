# Latte Code 概念体系对齐

状态：**分批进行中**。本文档定义 Latte Code 的规范概念层级与术语，并给出改名映射
与分批方案。批次 0（文档与死代码）、批次 1（`Thread → Session`，含 schema 13 迁移与
升级兼容）与批次 2（`Run → Turn`，含 schema 14/15 迁移、持久轮次计数与升级兼容）
**均已落地**；批次 3/4 待启动。各批次当前状态以第 5 节为准；已落地批次的权威是当前
代码，本文档记录其决策与映射，剩余批次仍按本文提案执行。

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

> 以上三例是改名前的缺陷取证：例 1、2 已随批次 1 修复，例 3 已随批次 2 修复
> （文件名与行号是改名前快照，部分文件已更名为 `session.rs`）。

---

## 2. 规范层级

以下是本提案确立的概念层级。每层给出规范名、当前实现名与身份标识。

```
Workspace                    工作区：一个文件系统根
  │                          （多个 Workspace 可归属同一 Project = 共享 git common dir）
  │
  └─ Session                 会话：一次持久对话，用户可见的顶层单位
     │                       实现：Session / SessionId / sessions（schema 13 改名）
     │                       上下文在此层累积、压缩、恢复
     │
     ├─ Transcript           记录：append-only 的卡片流，JSONL 为权威
     │                       实现：TranscriptPage / conversation_outbox 表
     │
     └─ Turn                 轮次：一次用户提交及其完整 Provider/Tool 循环
        │                    实现：Turn / TurnId / turns（schema 15 改名）
        │                    完成后不可变，按 ordinal 串成链
        │
        └─ Round             回合：Turn 内一次 assistant tool 调用批次
           │                 实现：无类型，靠 (entry.sequence, ordinal) 元组反查；
           │                 轮次数由持久计数器 tool_round_count 累计（schema 14）
           │
           └─ Call → Effect  单次工具调用，及其特权执行凭据
                             实现：ToolCall（provider 侧）/ effect_id: String
```

层级判据：

- **Session** 是用户在 `/sessions` 列表里看到的一行，有标题、工作区、创建时间。
  fork 产生新 Session 并记录 `parent_session_id`
  （`crates/latte-core/src/session.rs:273`；改名前为 `parent_thread_id`）。
- **Turn** 是一次「你说、它做完」。follow-up 产生新 Turn，同 Session。
  `SessionLifecycle` 挂在 Session 上，实际是 active Turn 状态的投影
  （`crates/latte-core/src/session.rs:60`；改名前注释自相矛盾地称其「区别于 child run」）。
- **Round** 是 Turn 内的一次 provider 往返所产生的 tool 调用批次。它是真实存在的
  执行单元 —— `handle_provider_tool_round`（`crates/latte-headless/src/session.rs:1419`）、
  `execute_tool_batch`（`:1491`，旧名 `continue_provider_tool_round`）—— 但没有类型，
  身份从 transcript 反查（`tool_round_for_call`，`:2399`）。一个 Turn 已完成多少个
  Round 由持久计数器 `session_turns.tool_round_count` 权威记录（schema 14 引入）。

---

## 3. 术语表

规范术语及其定义。**斜体**表示当前实现名与规范名不一致。

| 规范术语 | 定义 | 当前实现 |
| --- | --- | --- |
| Workspace | 一个文件系统根，会话的归属边界 | `workspace_root: String` ✓ |
| Project | 共享同一 git common dir 的 Workspace 分组 | `project_key`（仅存于 SQL，core 层缺席） |
| Session | 一次持久对话 | `Session` / `SessionId` / `sessions` ✓（批次 1，旧名 `threads_v2`） |
| Turn | Session 内一次用户提交及其完整循环 | `Turn` / `TurnId` / `turns` ✓（批次 2，旧名 `Run` / `runs`） |
| Round | Turn 内一次 assistant tool 调用批次 | *无类型* |
| Call | Round 内单次工具调用 | `ToolCall`（provider 侧）✓ |
| Effect | 一次特权操作的持久凭据与生命周期 | `effect_id: String`（无类型） |
| Transcript | Session 的 append-only 卡片流 | `TranscriptPage` ✓ / *表名 `conversation_outbox`* |
| Binding | Session 绑定的 provider + model + 指纹 | *`ProviderBinding` 与 `SessionProviderBinding` 同名不同物* |
| Lease | 执行权威的租约与 fencing token | `Lease` ✓ |
| Verification | 变更后的验证及其证据 | *`Evidence` / `VerificationEvidence` / `VerificationRecord` 三类型同概念* |
| Reconciliation | `Unknown` effect 的显式对账 | ✓ 全库一致，无需改动 |

### 3.1 需要消歧的复用词

以下单词在不同层级重复使用，含义不同。规范做法：**加层级前缀**。批次 2 已完成
run/turn 维度的消歧，其余仍待批次 3/4。

| 词 | 多重含义 | 建议 / 状态 |
| --- | --- | --- |
| `revision` | Turn revision（`SessionTurnSummary.turn_revision`）/ Session 快照裸字段 `revision`（`session.rs:243`） | turn 侧已加前缀为 `turn_revision`（批次 2）；线协议参数为 `expected_session_revision` / `expected_turn_revision`。快照结构体内的 session 级裸 `revision` 留待批次 3 统一为 `session_revision` |
| `sequence` | transcript seq / snapshot seq / event seq（三者同源）/ v1 RuntimeEvent seq（独立） | 前三者保留 `sequence`；v1 事件序列仍按批次 4 的 v1 退役处理 |
| `ordinal` | Session 内 Turn 序号（`SessionTurnSummary.ordinal`，`session.rs:172`）/ Round 内 Call 下标（`execute_tool_batch` 的 start ordinal） | 建议 `turn_ordinal` / `call_ordinal`，留待批次 3 随 `Round` 类型一起落地 |
| `checkpoint` | v1 协调者自定义恢复负载（`storage.rs:431`）/ v2 引擎生成的 effect 阶段标记（`lib.rs:130`） | 后者改 `effect_stage_marker`，待批次 3 |
| `catalog` | Session catalog / Workspace catalog / Model catalog / Command catalog | 保留但强制加限定词，禁止裸用 |
| `draft` | 会话草稿 `NewSessionDraft` / 模型预选 `draft_model` / 输入暂存 `deferred_composer_draft` | `session_draft` / `pending_model_selection` / `deferred_composer_text`，待批次 3 |
| `failed` | HTTP 层 = 500 内部错误 / CLI 层 = 非 5xx 兜底 | **含义相反，必须改**：CLI 侧改 `unclassified`，批次 4 处理（见批次 4 的三维度澄清） |

---

## 4. 关键约束：Turn 改名与守卫退役必须分开

**`Session` 与 `Turn` 的改名难度不对称。** 本节是批次 2 落地前的约束分析与其
实际解法，作为决策记录保留；评审明确指出：**改名本身不证明旧权限守卫可以移除，
两件事必须分别论证、分别落地。**

### 4.1 Session 改名是干净的（已随批次 1 落地）

`Session` 是纯 v2 概念，无 v1 同名类型。唯一障碍是一张 v1 遗留死表：

```sql
-- 改名前 storage.rs:369
CREATE TABLE sessions(id TEXT PRIMARY KEY, created_at_ms INTEGER NOT NULL);
```

全库零读写（grep `FROM sessions` / `INTO sessions` 只命中 schema 定义本身）。
schema 13 迁移 drop 掉它腾出名字，再把 v2 表改为无前缀名（见 5.1）。

### 4.2 Turn 改名不干净：两条路与实际选择

改名前 v2 的 run **就是** v1 的 run，不是相似概念：

- SQL 外键：`thread_runs_v2.run_id TEXT PRIMARY KEY REFERENCES runs(run_id)`
- 类型复用：session 侧 turn 摘要的 id 类型就是 v1 `RunId`，不存在独立类型
- 状态映射：session 侧状态是 `RunStatus` 的一对一无损映射
- revision 直读：`run_revision: state.revision`

当时区分「脱离 session 的 v1 run」与「挂在 session 上的 v2 turn」靠运行时布尔判断
（`is_session_linked_turn` 的前身），12+ 处 `reject_linked_turn`（旧名
`reject_linked_run`）守卫是这个类型缺陷的补丁：挂在 session 上的 turn 禁止走 v1
直连提交路径，必须走 session commit。

当时分析只有两条路：

1. 连 v1 `runs` 表一起改 —— 波及 v1 兼容路径，风险高；
2. 造 `TurnId` 与 `RunId` 双类型 —— 正是当前混乱的成因，不可取。

**批次 2 实际走了第 1 条路的「物理改名」版本，并把改名与守卫退役严格切开：**

- v1 的「命令」侧（`RuntimeCommand` 经 CLI/RPC 入口）已确认是死代码；运行时所有
  turn 行都由 session 工作流创建（直连建 turn 的 pub API `create_turn` 仅剩
  engine 自身测试调用）。「状态 + 事件」侧则是 v2 与直连面共用的地基 —— 物理图
  （`runs` / `events` / `effects` / `run_read_model` 等）因此不能删，但可以改名：
  schema 15 在 `PRAGMA legacy_alter_table=OFF` 下把物理图整体改名
  （`runs → turns` 等，外键引用随列改名级联），UUID 值、revision、幂等记录、
  审批身份一律不动。
- 但「一个 turn 是否挂在 session 上」至今仍是运行时判定
  （`is_session_linked_turn` = `SELECT EXISTS(... session_turns ...)`，
  `storage.rs:1183`），不是类型区分。v1 直连引擎操作面（process / effect 提交等）
  仍然存在且仍可能收到任意 turn id，所以 12+ 处 `reject_linked_turn` 守卫
  **原样保留，仅随改名改了名**（`lib.rs:847, 935, 2177, 2199, 2221, 2238, 2305,
  2316, 2330, 2348, 2354, 2367`；`process.rs:385, 438, 487`），错误类型同步更名为
  `LinkedTurnRequiresSessionCommit`。
- **守卫退役的前置条件（未来独立批次）**：证明 v1 直连操作面已无任何生产调用方，
  再删除守卫与其错误类型；或者引入真正的类型区分（session-bound turn 的新类型）。
  这属于安全边界收缩，需要独立的调用面审计与回归，不能搭车改名。

### 4.3 v1 的实际存活范围（改名前调查结论）

- 死：v1 CLI/RPC 命令入口（旧 `RuntimeCommand` 的外部调用路径）——
  `server_client.rs` 把 v1 `--allow`/`--deny` 变为硬错误，README:50 的说法正确；
  roadmap:80 「v1 CLI 兼容路径」是过期条目。
- 活：物理 turn 图（改名前的 `runs` 表，schema 15 后为 `turns`，及其状态/事件行）
  与 `RuntimeEvent` 类型 —— 每次 turn 状态迁移都写一行 `RuntimeEvent` 到 `events`
  表（storage 内 10+ 生产写入点），session 工作流直接依赖。批次 2 改了图名但没有
  动这个事件类型；只有 `RuntimeCommand` 一侧（命令信封与枚举）无外部生产调用方。
  两者的最终退役属于批次 4 而非批次 2。

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
| `data-storage.md` 自相矛盾 | 先统一到 Run，批次 2 已整体改名为 Turn；过期的 Schema 11 断言已修正 | 同文件；当前 `SCHEMA_VERSION = 15` |
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
- **轮次预算默认为无限、可选显式开启，并按 turn 的持久计数器累计（评审 P2/P3）**：
  `session.max_tool_rounds` 的类型是 `Option<u32>`，省略或 `null` 即无限（这是
  默认值）；显式值必须 ≥ 1。默认不再强制 48 轮终止——防止不收敛 turn 的兜底手段
  是单次 provider 请求超时（`session.provider_timeout_ms`）、用户取消、I/O 上限与
  进程清理，它们独立于轮次预算始终生效。预算只在 **provider 响应到达之后、决定是否
  执行下一个工具批次时**检查：响应不再调用工具（最终收尾）永远不会被拦，即使该 turn
  已处于预算边界；只有「还要再开一个工具批次」且已达上限时，turn 才以可重试失败
  结束。计数必须在同一 turn 内跨 input 回答、工具批准、重启续跑累计。早期基于
  「最后一个 User 消息之后」的消息计数会被 input 回答（它也持久化为 `User`
  transcript 卡片）重置，模型可「调工具→请求输入」循环突破预算；而 transcript 投影
  是尾 500 截断的、且 outbox 行会在同步进 JSONL 后删除，按 transcript 现数在长 turn
  上必然少计（实测 47 轮 × 6 调用只数出 38）。最终方案是持久计数器
  `session_turns.tool_round_count`（schema 14 引入并对存量库 JSON1 回填）：提交
  `Assistant` 且 `payload.tool_calls` 非空的 transcript 卡片时，在同一事务里
  `+1`，运行时经 `persisted_tool_rounds` 读取；历史 turn 的计数器各自独立、input
  回答只加 `User` 卡片，都不会重置计数。由「工具→input→回答→再工具被预算拦住」、
  「预算为 1 时最终收尾仍可完成」以及「>500 张卡片跨 input/批准/重启续跑计数不错」
  的最终二进制 E2E 共同守护。

**协议决策点（已定并执行）**：v1 端点承诺过稳定性
（`versioned-rpc-contract.md` §2.1），字段改名属 breaking change，按该文档
§2.2 本应走 v2。但当前处于早期阶段，#16 已有先例（list 分页、422→400）在 v1 内
直接 breaking。**决定：v1 内直接破坏性改名，不加兼容层。**

### 批次 2：Turn 改名（破坏性，已落地；守卫不退役）

状态：**已完成**。`Run* → Turn*` 贯穿领域类型、模块、HTTP/SSE 线协议、CLI `--json`
字段、TUI 文案与物理存储，由 schema 14/15 迁移承载；旧持久化数据只通过
serde alias 与 legacy import 保持**只读**兼容，线协议字段一律硬改名
（`active_turn_id` / `latest_turn_id` / `expected_turn_revision` 等），UUID 值、
revision 数值、幂等记录与审批身份不变。CLI 的 `run` 是动作子命令（`latte-code run
"…"`），不是领域名词，按评审要求保留不改。

落地内容：

- 类型：`RunState → TurnState`、`RunId → TurnId`、`RunStatus → TurnStatus`、
  session 侧 `SessionRunSummary/SessionRunStatus → SessionTurnSummary/
  SessionTurnStatus`；线协议与 SSE 字段、TUI 卡片标题（`"Turn {n}"` /
  "Turn activity"）全部统一。
- 存储（两次迁移）：schema 14 给 `session_runs` 增加 `tool_round_count` 持久
  计数器并用 JSON1 对存量库回填；schema 15 在 `PRAGMA legacy_alter_table=OFF`
  下把物理图整体改名（`runs → turns`、`run_read_model → turn_read_model`、
  `run_baselines → turn_baselines`、`session_runs → session_turns`、
  `session_active_runs → session_active_turns`，及 `run_id → turn_id`、
  `run_revision → turn_revision`、`latest_run_id → latest_turn_id`、
  `parent_run_id → parent_turn_id` 等列）。
- 只读兼容：`TurnState` / `EventEnvelope` / `ReadModelEnvelope` /
  `SessionTurnSummary` / `SessionSnapshot` / `SessionEvent` 等持久化 JSON 结构上
  加 `#[serde(alias = "run_id")]`、`alias = "run"`、`alias = "runs"` 等反序列化
  别名；legacy import 显式按旧列名读、新列名写（v9–12 历史库）。升级前 durable
  accept 的命令重试、等待中的 verification 审批仍按批次 1 的 digest/前缀兼容桥
  继续被识别。
- 配置：`session.max_tool_rounds` 为可选预算，默认无限（机制见批次 1 末条）；旧
  `thread` 配置块在 AppConfig 合并**之前**归一化为 `session`，`thread` 与
  `session` 同时出现直接报错（避免默认值已含 `session` 导致的别名/未知键冲突，
  否则迁移后启动即失败）；用户级与工作级配置都覆盖，并由「旧配置 → 启动 → 恢复
  旧会话」E2E 守护。
- **守卫不退役（评审硬性要求）**：`reject_linked_turn` 12+ 处守卫原样保留（见
  4.2）。「改名」与「删除旧权限守卫」是两件严格分开的事：改名只统一词汇，不改变
  任何安全边界；守卫退役需要独立的调用面审计。

### 批次 3：结构性补强（独立于改名，未启动）

批次 2 只统一词汇，以下「命名体现不出层级 / 缺类型」的问题原样保留，仍待本批：

1. **Round 无类型** —— 身份是 `(transcript_entry.sequence, ordinal)` 元组，
   从 payload 反序列化恢复（`tool_round_for_call`，
   `crates/latte-headless/src/session.rs:2399`）。批次 2 已落地持久计数器
   `tool_round_count`（回答「这个 turn 已完成多少 Round」），但「第 N 个 Round」
   仍不是一个可引用的类型。建议引入 `RoundId` 或至少一个 `Round` 结构。
2. **`effect_id` 是 String** —— 隐含结构
   `session-effect:{turn_id}:{round_sequence}:{ordinal}:{tool_call_id}`
   （`crates/latte-headless/src/session.rs:1509`，批次 2 前为 `thread-effect:…run_id…`）
   无解析器，与全库 `typed_id!` 惯例（`crates/latte-core/src/ids.rs:6-34`）不一致。
3. **`SessionEffectDescriptor` 与 `SessionEffectPresentation` 字段完全同构**
   （`crates/latte-engine/src/lib.rs:294` 与 `:339`），仅靠是否脱敏区分，名字上
   分不出哪个是可执行权威。建议改名体现信任级别，如 `EffectAuthority` /
   `EffectView`。
4. **Project 在 core 层缺席** —— `SessionTurnSummary` 所在的会话摘要只到
   `workspace_root`（`crates/latte-core/src/session.rs:267`），无 `project_key`，
   跨 worktree 归属只存在于 SQL 外键（`crates/latte-engine/src/storage.rs:675`
   `projects` 表）。
5. **`SessionProviderBinding` 双版本号** —— 字段 `version: u32` 指 binding 自身
   版本（`BINDING_VERSION = 1`，`crates/latte-headless/src/registry.rs:17`），
   读名字容易误解为会话协议版本；批次 1 后 `V2` 后缀已随 thread 改名消失，但
   版本号语义仍建议澄清。
6. **session 级裸 `revision`** —— 快照结构内 session 修订仍是裸字段
   `SessionSnapshot.revision`（`session.rs:243`），turn 侧已在批次 2 加前缀
   `turn_revision`；建议对称改为 `session_revision`。

### 批次 4：状态与错误词汇对齐（独立，用户可见，未启动）

评审指出的核心问题：**`ready` 与 `denied`/`failed` 不是同一维度，不能横向画在
一张映射表里。** 当前系统实际存在三个独立维度，各自回答不同问题：

1. **会话是否还能继续（`SessionLifecycle`，持久投影）**
   `ready` 的含义是「会话可以接收下一个 turn」，**不是**「上一个 turn 成功」。
   它的文档原话（`crates/latte-core/src/session.rs:60-63`）：newest child 要么
   completed，要么 failed with a retryable error；存储层还把 permission-denied
   并入可继续集合（`storage.rs:1736-1743` follow-up 父代允许 completed /
   retryable / permission-denied；`storage.rs:3141-3147` 明确「Denial
   terminalizes this immutable child, but it does not terminalize the
   conversation」）。所以 `lifecycle=ready` 时，最新 turn 可能是 completed、
   retryably failed 或 permission-denied 三种结局。
2. **最后一个 turn 的结局（`SessionTurnSummary.status` + `failure_code`）**
   取值 `completed` / `failed`（带 `FailureCode`，如 `PermissionDenied`、
   `RuntimeFailed`、`VerificationFailed`）等。这是维度 1 投影所依据的底层事实，
   但两者回答的问题不同：一个问「会话能否继续」，一个问「刚才那次结果如何」。
3. **本次 CLI 调用的进程结局（`TerminalOutcome`，仅 CLI 侧）**
   `classify()`（`crates/latte-code/src/server_client.rs:310-331`）把上面两个
   维度合成一个进程退出分类：`lifecycle=Ready + 最新 turn completed →
   completed`；`lifecycle=Ready + 最新 turn failed(PermissionDenied) → denied`；
   其它 failed → `failed`；另有 `waiting` / `interrupted` /
   `reconciliation_required`。

   因此 `{"status":"denied", "data":{"session":{"lifecycle":"ready"}}}` **不是
   自相矛盾**，而是两个维度各自为真：上一个 turn 被拒绝（denied），但会话仍可
   继续（ready）。批次 4 的工作不是强行对齐取值，而是：(a) 在 HTTP/CLI/文档里
   明确这两个字段分属两个维度，避免读者把 ready 读成成功；(b) TUI 补 denied /
   cancelled 的可见表达；(c) CLI 侧的合成规则（维度 3）显式文档化。

仍待本批处理的具体缺陷（与维度澄清独立）：

- HTTP `rejected` → CLI `usage`、HTTP `failed` → CLI `internal` 两处改名
  （`server_client.rs:115-120`），其中字符串 `failed` 在 HTTP 层指 500 内部
  错误、在 CLI 层指非 5xx 兜底，含义相反（见 3.1）；CLI 侧建议改 `unclassified`。
- `idempotency_mismatch`(422) 在 CLI 侧无映射分支，退化为
  `{"code":"failed","message":"422 Unprocessable Entity: ..."}`
  （`server_client.rs:104-122`）。
- core 中 v1 状态/事件类型（`RuntimeEvent` / `RuntimeCommand` /
  `TurnStatus` 的 v1 读写路径）的最终退役属于本批；批次 2 只改了名，没有删类型。

## 6. 遗留决策点

### 6.1 是否引入 `Conversation` 作为第三层名词

`conversation` 仍在 TUI 与类型名中与 session 混用：`/new` 命令描述
（`crates/latte-tui/src/command.rs:58`："Start a new conversation draft"）、
无 turn 归属的分组标题（`crates/latte-tui/src/session.rs:2361` 返回
`"Conversation"`）、`ActiveConversation` 类型（`session.rs:403`）。本提案主张
**废弃 conversation 作为领域概念名**，统一为 Session，仅在
`conversation history`（指发给 provider 的 message 序列）这一技术含义上保留。
改名前已先把 provider 侧注释中的 "conversation" 限定到该技术含义。

### 6.2 `conversation_outbox` 表是否改名

该表原名 `thread_transcript_v2`，v11 迁移改为 outbox，因为它同时承担两个职责：
transcript 持久存储 + JSONL 落盘的事务性出箱。出箱确认后行会被删除
（`acknowledge_conversation_outbox`，`storage.rs:2068`），但 `TranscriptPage`
又是快照的一部分。

职责重叠是真实的，改名前需先决定是否拆表。注意批次 2 已在该表上把
`run_id` 列更名为 `turn_id`，并以其作为 schema 14 计数器回填的数据源。

### 6.3 transcript payload 的双重信任级别

同一个 `payload` 字段，一部分是运行时权威、一部分严禁作为权威：

- 权威：`payload.tool_calls` 是 provider 续跑队列，重启后据此恢复
  （`crates/latte-headless/src/session.rs:2396` 明言 "provider grammar, not a
  best-effort display summary"）
- 严禁：effect descriptor 绝不可从 transcript 重建
  （`crates/latte-engine/src/lib.rs:292`：checkpoint / event / provider-history
  message 同列禁止来源）

类型上完全区分不出来。这是安全边界问题，不只是命名问题。

### 6.4 Round 与 compaction 的交互

若引入 transcript 压缩（见 `.tmp/openai-chat-findings.md` P1-3），Round 身份
依赖 `transcript_entry.sequence`，压缩若改变 sequence 或移除 entry，会破坏
`tool_round_for_call`（`session.rs:2399`）的反查。批次 3 的 `RoundId` 应先于
compaction 落地。另需注意：schema 14 的持久计数器 `tool_round_count` 是独立
累计值，压缩 transcript 不会影响预算计数（计数不再依赖反查），但「某个 call
属于第几 Round」的反查仍受压缩影响。

### 6.5 两个 `timeout_ms` 的边界

配置里有两个同名超时，作用对象不同，容易混淆：

- `session.provider_timeout_ms`（默认 60s）：**每一次 provider HTTP 请求**的
  墙钟预算，在 headless 构造 `ProviderContext.deadline` 时生效
  （`crates/latte-headless/src/session.rs:1791-1793`）。
- `providers.<name>.timeout_ms`（默认 60s，`crates/latte-headless/src/
  registry.rs:884`）：该 provider 传输客户端自身的请求超时，在 HTTP 调用处与
  上面的 deadline **取 min**（`crates/latte-headless/src/provider.rs:512-514`
  与 `:667-669`），即两个值里更短的先生效。
- `verification.timeout_ms`（默认 120s）：与上面两者无关，限制的是变更后
  验证命令（默认 `cargo test --workspace`）的执行时长。

### 6.6 `session_ref` 的隐私边界

Provider 请求头里携带的 `session_ref`（`crates/latte-headless/src/provider.rs:105`
`session_ref_for`）是内部 SessionId 的**无盐 SHA-256 截断**。它的唯一隐私属性
是「不向原本不知道该 UUID 的一方暴露内部标识符」；不能声称跨 Provider
不可关联——它对每个 Provider 都是同一个稳定值（这正是它能用于分组的原因），
也不能防住已知 UUID 的猜测。若未来需要跨 Provider 不可关联，必须改用
按 Provider 分域加盐的 HMAC。

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
| v1 `RuntimeCommand` 族 | `protocol.rs:55` | 仅 v1 类型内部互引（`CommandEnvelope` 等）与测试使用，无外部生产调用方；批次 4 退役 |
| v1 死表 `sessions(id)` | 批次 1 前的 `storage.rs:369` | **已在 schema 13 删除**；名字已被 v2 `sessions` 表占用（原 `threads_v2`），该行历史状态不再适用 |

文档使用陈述句描述未实现类型（如「`latte-headless` 拥有 `TurnSupervisor`」），
读者无法区分已落地与提案。约定：**未实现概念必须显式标注**；已落地批次的
file:line 证据会随重构漂移，本文以改名前快照取证并注明批次，权威以当前代码为准。