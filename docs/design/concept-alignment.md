# Latte Code 概念体系对齐

状态：**批次 0-2 已落地，批次 3/4 待启动。** 批次 0（文档与死代码清理）、批次 1
（`Thread → Session`，schema 13：drop v1 死表、v2 表去前缀改名、幂等 digest 与
验证 effect 前缀的升级兼容桥、binding 指纹兼容）与批次 2（`Run → Turn`，schema 14
持久轮次计数 `tool_round_count` + schema 15 物理图改名；线协议硬改名，旧数据经
serde alias 与 legacy import 只读兼容）均已随 PR #17 落地。实施细节、影响面统计与
当时的取证见该 PR 与 git 历史；本文保留仍然有效的规范层级、术语表、关键决策与
剩余工作，已落地部分以当前代码为权威。

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
  fork 产生新 Session 并记录 `parent_session_id`。
- **Turn** 是一次「你说、它做完」。follow-up 产生新 Turn，同 Session。
  `SessionLifecycle` 挂在 Session 上，实际是 active Turn 状态的投影。
- **Round** 是 Turn 内一次 provider 往返所产生的 tool 调用批次。它是真实存在的
  执行单元（`handle_provider_tool_round` / `execute_tool_batch`），但没有类型，
  身份从 transcript 反查（`tool_round_for_call`）。一个 Turn 已完成多少个 Round
  由持久计数器 `session_turns.tool_round_count` 权威记录；预算检查只发生在「还要
  再开一个工具批次」时，input 回答与权限批准不会重置计数。

## 2. 术语表

规范术语及其定义。**斜体**表示当前实现名与规范名不一致。

| 规范术语 | 定义 | 当前实现 |
| --- | --- | --- |
| Workspace | 一个文件系统根，会话的归属边界 | `workspace_root: String` ✓ |
| Project | 共享同一 git common dir 的 Workspace 分组 | `project_key`（仅存于 SQL，core 层缺席） |
| Session | 一次持久对话 | `Session` / `SessionId` / `sessions` ✓ |
| Turn | Session 内一次用户提交及其完整循环 | `Turn` / `TurnId` / `turns` ✓ |
| Round | Turn 内一次 assistant tool 调用批次 | *无类型* |
| Call | Round 内单次工具调用 | `ToolCall`（provider 侧）✓ |
| Effect | 一次特权操作的持久凭据与生命周期 | `effect_id: String`（无类型） |
| Transcript | Session 的 append-only 卡片流 | `TranscriptPage` ✓ / *表名 `conversation_outbox`* |
| Binding | Session 绑定的 provider + model + 指纹 | *`ProviderBinding` 与 `SessionProviderBinding` 同名不同物* |
| Lease | 执行权威的租约与 fencing token | `Lease` ✓ |
| Verification | 变更后的验证及其证据 | *`Evidence` / `VerificationEvidence` / `VerificationRecord` 三类型同概念* |
| Reconciliation | `Unknown` effect 的显式对账 | ✓ 全库一致，无需改动 |

### 2.1 需要消歧的复用词

以下单词在不同层级重复使用，含义不同。规范做法：**加层级前缀**。批次 2 已完成
run/turn 维度的消歧，其余仍待批次 3/4。

| 词 | 多重含义 | 建议 / 状态 |
| --- | --- | --- |
| `revision` | Turn revision（`SessionTurnSummary.turn_revision`）/ Session 快照裸字段 `revision` | turn 侧已加前缀为 `turn_revision`（批次 2）；线协议参数为 `expected_session_revision` / `expected_turn_revision`。session 级裸 `revision` 留待批次 3 统一为 `session_revision` |
| `sequence` | transcript seq / snapshot seq / event seq（三者同源）/ v1 RuntimeEvent seq（独立） | 前三者保留 `sequence`；v1 事件序列仍按批次 4 的 v1 退役处理 |
| `ordinal` | Session 内 Turn 序号（`SessionTurnSummary.ordinal`）/ Round 内 Call 下标 | 建议 `turn_ordinal` / `call_ordinal`，留待批次 3 随 `Round` 类型一起落地 |
| `checkpoint` | v1 协调者自定义恢复负载 / v2 引擎生成的 effect 阶段标记 | 后者建议改 `effect_stage_marker`，待批次 3 |
| `catalog` | Session catalog / Workspace catalog / Model catalog / Command catalog | 保留但强制加限定词，禁止裸用 |
| `draft` | 会话草稿 `NewSessionDraft` / 模型预选 `draft_model` / 输入暂存 `deferred_composer_draft` | `session_draft` / `pending_model_selection` / `deferred_composer_text`，待批次 3 |
| `failed` | HTTP 层 = 500 内部错误 / CLI 层 = 非 5xx 兜底 | **含义相反，必须改**：CLI 侧改 `unclassified`，批次 4 处理 |

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
- v1 的实际存活范围：v1 CLI/RPC 命令入口已死（`--allow`/`--deny` 是硬错误）；
  物理 turn 图（`turns` 表与状态/事件行）与 `RuntimeEvent` 类型仍活——每次 turn
  状态迁移都写一行 `RuntimeEvent`，session 工作流直接依赖。`RuntimeCommand` 一侧
  无外部生产调用方，其最终退役属于批次 4。

## 4. 剩余批次

### 批次 3：结构性补强（未启动）

1. **Round 无类型**——身份是 `(transcript_entry.sequence, ordinal)` 元组，从
   payload 反序列化恢复（`tool_round_for_call`）。建议引入 `RoundId` 或至少一个
   `Round` 结构；`RoundId` 应先于任何改变 transcript sequence 语义的新特性落地。
2. **`effect_id` 是 String**——隐含结构
   `session-effect:{turn_id}:{round_sequence}:{ordinal}:{tool_call_id}` 无解析器，
   与全库 `typed_id!` 惯例不一致。
3. **`SessionEffectDescriptor` 与 `SessionEffectPresentation` 字段完全同构**，
   仅靠是否脱敏区分，名字上分不出哪个是可执行权威。建议改名体现信任级别，如
   `EffectAuthority` / `EffectView`。
4. **Project 在 core 层缺席**——会话摘要只到 `workspace_root`，无 `project_key`，
   跨 worktree 归属只存在于 SQL 外键。
5. **`SessionProviderBinding` 双版本号**——字段 `version: u32` 指 binding 自身
   版本（`BINDING_VERSION = 1`），读名字容易误解为会话协议版本。
6. **session 级裸 `revision`**——快照结构内 session 修订仍是裸字段，turn 侧已在
   批次 2 加前缀；建议对称改为 `session_revision`。

### 批次 4：状态与错误词汇对齐（未启动）

当前系统实际存在三个独立维度，各自回答不同问题：

1. **会话是否还能继续（`SessionLifecycle`，持久投影）**——`ready` 表示「会话可以
   接收下一个 turn」，不是「上一个 turn 成功」。`lifecycle=ready` 时最新 turn 可能
   是 completed、retryably failed 或 permission-denied 三种结局。
2. **最后一个 turn 的结局（`SessionTurnSummary.status` + `failure_code`）**——
   与维度 1 回答的问题不同：一个问「会话能否继续」，一个问「刚才那次结果如何」。
3. **本次 CLI 调用的进程结局（`TerminalOutcome`，仅 CLI 侧）**——`classify()` 把
   前两个维度合成进程退出分类。因此
   `{"status":"denied", "data":{"session":{"lifecycle":"ready"}}}` **不是自相
   矛盾**。批次 4 的工作：(a) 在 HTTP/CLI/文档里明确两个字段分属两个维度，避免
   读者把 ready 读成成功；(b) TUI 补 denied / cancelled 的可见表达；(c) CLI 侧的
   合成规则显式文档化。

仍待本批处理的具体缺陷：

- HTTP `rejected` → CLI `usage`、HTTP `failed` → CLI `internal` 的映射中，字符串
  `failed` 在两层含义相反，CLI 侧建议改 `unclassified`。
- `idempotency_mismatch`(422) 在 CLI 侧无映射分支，退化为 code `failed`。
- core 中 v1 状态/事件类型（`RuntimeEvent` / `RuntimeCommand` / `TurnStatus` 的
  v1 读写路径）的最终退役属于本批。

## 5. 遗留决策点

### 5.1 是否引入 `Conversation` 作为第三层名词

`conversation` 仍在 TUI 与类型名中与 session 混用（`/new` 命令描述、分组标题、
`ActiveConversation` 类型）。决策方向是**废弃 conversation 作为领域概念名**，统一
为 Session，仅在 `conversation history`（发给 provider 的 message 序列）这一技术
含义上保留。

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
