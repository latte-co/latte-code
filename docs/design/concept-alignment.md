# Latte Code 概念体系对齐

状态：**批次 0-4 已全部落地。** 批次 0（文档与死代码清理）、批次 1
（`Thread → Session`，schema 13：drop v1 死表、v2 表去前缀改名、幂等 digest 与
验证 effect 前缀的升级兼容桥、binding 指纹兼容）与批次 2（`Run → Turn`，schema 14
持久轮次计数 `tool_round_count` + schema 15 物理图改名；线协议硬改名，旧数据经
serde alias 与 legacy import 只读兼容）已随 PR #17 落地；批次 3（`RoundId`、`EffectId`、
`EffectAuthority`/`EffectView`、`ProjectKey`、`ProviderModelBinding`、`binding_version`、
`session_revision`、`turn_ordinal`/`call_ordinal`、`effect_stage_marker`、
`ActiveSession::SessionDraft`）与批次 4（三维状态文档化、TUI 区分 `Permission denied` /
`Cancelled` / `Last turn failed · ready`、CLI `unclassified` 与 `idempotency_mismatch`、
死 v1 命令/信封类型退役且保留 `reject_linked_turn` 守卫）已全部实现并通过契约测试守护。

---

## 1. 规范层级

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
        │                    完成后不可变，按 turn_ordinal 串成链
        │
        └─ Round             回合：Turn 内一次 assistant tool 调用批次
           │                 实现：RoundId(u64) / ToolRound / ToolRoundPosition；
           │                 轮次数由持久计数器 tool_round_count 累计（schema 14）
           │
           └─ Call → Effect  单次工具调用，及其特权执行凭据
                             实现：ToolCall（provider 侧）/ EffectId + EffectAuthority（引擎私有）/ EffectView（脱敏投影）
```

层级判据：

- **Session** 是用户在 `/sessions` 列表里看到的一行，有标题、工作区、创建时间。
  fork 产生新 Session 并记录 `parent_session_id`。
- **Turn** 是一次「你说、它做完」。follow-up 产生新 Turn，同 Session。
  `SessionLifecycle` 挂在 Session 上，实际是 active Turn 状态的投影。
- **Round** 是 Turn 内一次 provider 往返所产生的 tool 调用批次，由强类型
  `RoundId(u64)`、`ToolRound` 与 `ToolRoundPosition` 表达（Assistant 工具轮卡片显式
  持久化 `"round_id"`，旧卡片兼容回退到 `entry.sequence`）。一个 Turn 已完成多少个
  Round 由持久计数器 `session_turns.tool_round_count` 权威记录；预算检查只发生在
  「还要再开一个工具批次」时，input 回答与权限批准不会重置计数。

## 2. 术语表

规范术语及其定义。**斜体**表示当前实现名与规范名不一致。

| 规范术语 | 定义 | 当前实现 |
| --- | --- | --- |
| Workspace | 一个文件系统根，会话的归属边界 | `workspace_root: String` ✓ |
| Project | 共享同一 git common dir 的 Workspace 分组 | `ProjectKey` / `ProjectSummary` / `SessionSummary.project_key` ✓ |
| Session | 一次持久对话 | `Session` / `SessionId` / `sessions` ✓ |
| Turn | Session 内一次用户提交及其完整循环 | `Turn` / `TurnId` / `turns` ✓ |
| Round | Turn 内一次 assistant tool 调用批次 | `RoundId` / `ToolRound` / `ToolRoundPosition` ✓ |
| Call | Round 内单次工具调用 | `ToolCall`（provider 侧，`call_ordinal`）✓ |
| Effect | 一次特权操作的持久凭据与生命周期 | `EffectId` / `SessionEffectKind` / `EffectAuthority`（权威）+ `EffectView`（展示）✓ |
| Transcript | Session 的 append-only 卡片流 | `TranscriptPage` ✓ / *表名 `conversation_outbox`* |
| Binding | Session 绑定的 provider + model + 指纹 | `ProviderModelBinding`（模型侧）与 `SessionProviderBinding`（含 `binding_version`）✓ |
| Lease | 执行权威的租约与 fencing token | `Lease` ✓ |
| Verification | 变更后的验证及其证据 | `Evidence` / `VerificationStatus` ✓ |
| Reconciliation | `Unknown` effect 的显式对账 | ✓ 全库一致 |

### 2.1 已完成消歧的复用词

以下单词在不同层级重复使用，含义不同。规范做法：**加层级前缀**。批次 2-4 已全部完成消歧：

| 词 | 多重含义 | 当前状态 |
| --- | --- | --- |
| `revision` | Turn revision（`SessionTurnSummary.turn_revision`）/ Session 快照修订（`session_revision`） | 已统一为 `turn_revision` 与 `session_revision`（含 `SessionSnapshot`、`SessionEventEnvelope`、`ManualCompactionState::Compacted` 与 SSE `session_changed`，带 `#[serde(alias = "revision")]` 读取兼容）✓ |
| `sequence` | transcript seq / snapshot seq / event seq（三者同源） | 保留 `sequence`；死 v1 命令/信封类型已退役 ✓ |
| `ordinal` | Session 内 Turn 序号 / Round 内 Call 下标 | 已拆分为 `SessionTurnSummary.turn_ordinal`（带 `alias = "ordinal"`）与 `call_ordinal` ✓ |
| `checkpoint` | v1 协调者自定义恢复负载 / v2 引擎生成的 effect 阶段标记 | v2 侧已更名为 `effect_stage_marker`（`session_effect_stage_marker` / `effect_stage_marker_json`）✓ |
| `catalog` | Session catalog / Workspace catalog / Model catalog / Command catalog | 保留并强制加限定词，禁止裸用 ✓ |
| `draft` | 会话草稿 / 模型预选 / 输入暂存 | 已统一为 `ActiveSession::SessionDraft`、`pending_model_selection` 与 `deferred_composer_text` ✓ |
| `failed` | HTTP 层 = 500 内部错误 / CLI 层 = 非 5xx 兜底 | CLI 侧已更名为 `ClientError::Unclassified`（`code = "unclassified"`），并补齐 422 `ClientError::IdempotencyMismatch`（`code = "idempotency_mismatch"`）✓ |

## 3. 关键决策：改名与守卫退役必须分开

**「改名」不证明旧权限守卫可以移除——两件事必须分别论证、分别落地。** 批次 2 只
统一了词汇，没有删除任何安全边界。

- v2 turn 曾经就是 v1 的 run（外键、类型复用、状态映射、revision 直读）。区分
  「脱离 session 的 v1 turn」与「挂在 session 上的 v2 turn」靠运行时判定
  （`is_session_linked_turn`），不是类型区分。
- v1 直连引擎操作面（process / effect 提交等）仍然存在，仍可能收到任意 turn id，
  因此 `reject_linked_turn` 守卫**原样保留，仅随改名更名**，错误类型为
  `LinkedTurnRequiresSessionCommit`。
- **守卫退役的前置条件（未来独立批次）**：证明 v1 直连操作面已无任何生产调用方，
  再删除守卫与其错误类型；或引入真正的类型区分（session-bound turn 的新类型）。
  这属于安全边界收缩，需要独立的调用面审计与回归，不能搭车改名。
- v1 的实际存活范围：v1 CLI/RPC 命令入口与无调用方的死类型（`CommandId`、
  `CommandEnvelope`、`ReadModelEnvelope`、`RuntimeCommand`、`PermissionDecision`、
  `HeadlessOutcome` 以及无生产写入的 `RuntimeEvent::{ToolStarted, ToolCompleted,
  EvidenceRecorded}`）已在批次 4 退役；物理 turn 图（`turns` 表与状态/事件行）及
  `RuntimeEvent::StateChanged` 仍活——每次 turn 状态迁移都写一行 `RuntimeEvent`，
  session 工作流直接依赖。

## 4. 批次 3 与批次 4 落地记录

### 批次 3：结构性补强（已落地）

1. **`RoundId(u64)` 与 `ToolRound`**：Assistant 工具轮卡片显式持久化 `"round_id"`，旧卡片兼容回退到 `entry.sequence`；`ToolRoundPosition` 使用 `call_ordinal` / `start_call_ordinal`。
2. **`EffectId` 与 `SessionEffectKind`**：1..=512 字节无控制字符校验类型，统一解析 `session-effect:` / `session-verification:`（兼容旧 `thread-*` 前缀）。
3. **`EffectAuthority` 与 `EffectView`**：`EffectAuthority` 仅存于引擎私有 `session_effect_canonical`，公开列 `effects.descriptor_json` 与 transcript 只写经 `EffectView::from_authority` 脱敏后的视图。
4. **`ProjectKey` 与 `ProjectSummary`**：进入 `latte-core`，并在 `SessionSummary.project_key` 与 `EngineHandle::project_key()` 暴露跨 worktree 逻辑仓库归属。
5. **`SessionProviderBinding.binding_version`**：消除与协议版本号的歧义（`#[serde(alias = "version")]`），幂等创建去重同时兼容新旧 digest。
6. **`session_revision`、`turn_ordinal`、`effect_stage_marker` 与 `ActiveSession`**：完成快照/SSE/压缩状态/TUI 模型字段的全面前缀化与消歧。

### 批次 4：状态与错误词汇对齐（已落地）

系统显式区分三个正交状态维度：

1. **会话是否还能继续（`SessionLifecycle`，持久投影）**——`ready` 表示「会话可以
   接收下一个 turn」，不是「上一个 turn 成功」。`lifecycle=ready` 时最新 turn 可能
   是 completed、retryably failed 或 permission-denied 三种结局。
2. **最后一个 turn 的结局（`SessionTurnSummary.status` + `failure_code`）**——
   与维度 1 回答的问题不同：一个问「会话能否继续」，一个问「刚才那次结果如何」。
3. **本次 CLI 调用的进程结局（`TerminalOutcome`，仅 CLI 侧）**——`classify()` 把
   前两个维度合成进程退出分类。

本批已完成：
- TUI 顶栏、底部状态栏、Turn 标题与 `Failure` 卡片显式区分 `Permission denied`、`Cancelled` 与 `Last turn failed · ready`。
- CLI `ClientError` 将非 5xx 未知错误更名为 `Unclassified`（`code = "unclassified"`），并为 HTTP 422 新增 `IdempotencyMismatch`（`code = "idempotency_mismatch"`）。
- 退役全部死 v1 信封与命令类型，同时原样保留 `reject_linked_turn` / `LinkedTurnRequiresSessionCommit` 权限守卫。

## 5. 遗留决策点

### 5.1 `Conversation` 术语边界（已收敛）

TUI 与核心状态已统一使用 Session（`ActiveSession`、`SessionDraft`、`/new` 描述与未绑定分组标题 `"Session"`），仅在 `conversation history` / `ConversationStore`（底层 JSONL 消息流存储）这一技术含义上保留。

### 5.2 `conversation_outbox` 表是否改名

该表同时承担 transcript 持久存储 + JSONL 落盘的事务性出箱两个职责；改名前需先
决定是否拆表。批次 2 已在该表上把 `run_id` 列更名为 `turn_id`。

### 5.3 transcript payload 的双重信任级别

同一个 `payload` 字段：`payload.tool_calls` 是 provider 续跑队列，重启后据此恢复，
属权威；effect descriptor 绝不可从 transcript 重建。类型上完全区分不出来，这是
安全边界问题，不只是命名问题。

### 5.4 Round 与 compaction 的交互

Round 身份依赖 `transcript_entry.sequence`；任何改变 sequence 或移除 entry 的
压缩方案会破坏 `tool_round_for_call` 反查。批次 3 的 `RoundId` 应先于这类方案
落地。schema 14 的持久计数器 `tool_round_count` 不受压缩影响（计数不依赖反查），
但「某个 call 属于第几 Round」的反查仍受影响。现状压缩只追加卡片、不改写既有
sequence，该张力目前是受控的。

### 5.5 两个 `timeout_ms` 的边界

- `session.provider_timeout_ms`（默认 60s）：每一次 provider HTTP 请求的墙钟预算。
- `providers.<name>.timeout_ms`（默认 60s）：provider 传输客户端自身的请求超时，
  与上一项**取 min**。
- `verification.timeout_ms`（默认 120s）：限制变更后验证命令的执行时长，与前两者
  无关。

### 5.6 `session_ref` 的隐私边界

Provider 请求头携带的 `session_ref` 是内部 SessionId 的**无盐 SHA-256 截断**。它的
唯一隐私属性是「不向原本不知道该 UUID 的一方暴露内部标识符」；不能声称跨
Provider 不可关联——它对每个 Provider 都是同一个稳定值（这正是它能用于分组的
原因），也不能防住已知 UUID 的猜测。若未来需要跨 Provider 不可关联，必须改用按
Provider 分域加盐的 HMAC。
