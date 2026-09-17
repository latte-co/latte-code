# Context 架构与上下文管理策略

状态：**v1 已实现**（分层模型、SummarizeOnDiscard 策略、策略选择器数据化、
主动水位、确定性省略层、token 估算与只读用量投影）；文中标注 v2 的条目
（TUI 预算展示、跨模型 handoff）未实现。
日期：2026-09-13（2026-09-16 增补用量投影）
关联：[Harness Profile 抽象设计](harness-profile.md)——策略选择器是 profile 的组成部分

---

## 1. 问题陈述

每次 provider 请求都要回答同一个问题：**这个模型此刻应该看到什么？** 在这个问题
被系统化表述之前，答案散落在实现里：仓库上下文的字节上限、历史窗口的 newest-first
循环、压缩的触发条件——各自正确，但没有一份文档定义它们的层次、优先级和演进
边界。本文钉死这些内容；任何上下文相关的新能力（策略、记忆、可见性）必须落进
本文的框架，而不是绕过它。

## 2. 分层模型

一个请求的 context 由四层构成，每层有自己的权威、预算和更替权：

```
┌─ L0 transient  瞬态进度（spinner/streaming/mailbox）
│    权威：进程内；永不进入请求、永不持久化
│
├─ L1 static     静态层：system prompt（profile 槽位）+ 仓库上下文
│    权威：profile + workspace 文件（AGENTS.md、focus manifest）
│    预算：context_cap_bytes（字节硬上限）；每请求重建，路径包含性校验
│
├─ L2 history    历史层：transcript 扫描成的 user 段序列
│    权威：每 Session JSONL（append-only）
│    预算：max_request_bytes / max_input_bytes / reserved_output_bytes
│
└─ L3 derived    派生层：对 L2 被更替范围的摘要（CompactSummary 卡片）
     权威：同样是 JSONL——派生物一旦持久化就是权威数据
     预算：max_summary_source_bytes 约束摘要源，产出随 L2 预算校验
```

三条不变量贯穿所有层：

1. **字节权威**：所有硬边界（拒绝、截断、进窗判定）按字节精确执行、fail-closed；
   token 只做估算，服务于决策与展示，永不进入 fail-closed 判定。
2. **JSONL 单一权威**：L2/L3 都以 JSONL 为准。派生物（摘要）一旦写入就和其他
   卡片同等权威——replay 时按同字节重放，确定性在字节层面成立。
3. **脱敏边界**：进 L1/L2/L3 的文本都在写入点经 `redact_session_text`；请求侧
   产出的摘要文本（provider 输出）入 L3 前再次脱敏。

## 3. 历史层的选择与更替

### 3.1 保留优先级（规范）

历史内容的重要性次序（摘要 prompt 与未来策略必须遵守）：

1. 用户目标与对目标的纠偏
2. 已做决策及其理由
3. 验证证据（命令、结果、失败原因）
4. 工具结果的关键内容（路径、结论、错误）
5. 过程叙述与小 talk（最先可弃）

recency 窗口不保证这个次序——它只是 v1 的**选择策略**，不是价值判断。摘要
prompt（`agent.summarize` 槽位）已按此优先级书写。

### 3.2 段边界语义

User 卡开启一个段；Assistant/ToolResult 追加到当前段；ToolCall/Permission/
Input/Failure/Completion/System 卡不产生 provider 消息（除
`provider_tool_round_aborted=permission_denied` 的 Failure 合成拒绝结果）；
**CompactSummary 卡是边界**：它按位置 supersede 之前的所有段，自身作为一个
user 段参与后续窗口。执行依据是**位置**，不是 payload：卡片 payload 里的
`superseded_through_sequence` 是生成时刻的审计水位（记录当时已知的最晚被
更替序号），窗口裁剪不读它——两者口径的差异是有意的，避免"以为按水位裁剪、
实际按位置裁剪"的二义性。因此摘要源必须包含"卡片落点之前的全部被更替文本"——
包括触发本次请求的 prompt（卡片在 turn 启动事务后落盘，落点在当前 user 卡之后）。

### 3.3 策略选择器：数据，不是行为

上下文管理策略 = **选择算法 + 更替算法**的组合。遵循 profile 基石
（数据不是行为，见 harness-profile 设计 §3），策略以**数据化的选择器**表达：

```rust
/// latte-core::CompactionStrategy（数据化的策略选择器）
pub enum CompactionStrategy {
    /// 关闭：窗口丢弃即静默更替（v1 之前的原始行为）。
    Off,
    /// 窗口丢弃发生时被动摘要；估算用量达到 trigger_ratio 时主动摘要；
    /// tool 批次之间也会在同一阈值上主动压缩（见 §4）。
    SummarizeOnDiscard,
    /// 先用确定性骨架层（无 provider 调用）替换被更替前缀里的旧 tool
    /// result；骨架层不足以治愈丢弃/水位时再回落摘要层。provider 以
    /// context-overflow 拒绝请求时，同一机制提供每 turn 一次的强制恢复
    /// （见 §4.6）。
    ElideToolResultsThenSummarize,
}
```

loop `match` 选择器执行对应算法；profile 携带选择器 + 参数
（`trigger_ratio` / `retain_ratio` / `max_summary_source_bytes` / prompt 槽），因此：

- 可整体快照进 session 记录、可版本化、可作契约测试 fixture；
- 新策略 = 新 enum 变体 + loop 新分支，profile schema 向后兼容（minor 演进）；
- 禁止 trait 对象注入——策略实现不进 profile，权限天花板不被绕开。

## 4. 策略：SummarizeOnDiscard（被动 + 主动，已实现）

**两个触发形状共用一个规划器：**

- **被动（reactive）**：newest-first 精确字节拟合装不下全部历史时，被丢出的
  旧前缀必须压缩——否则就是静默丢失。边界 = 拟合保留边界。
- **主动（proactive）**：拟合不丢弃任何段，但估算用量达到
  `trigger_ratio`（默认 90%）时，在撞墙前先压缩。保留后缀按
  `retain_ratio`（默认占预算 20%，可配 1..=80）从最新段向旧按**整段**累加；
  tool-call/result 对因此永不被拆散，进行中的 turn 段整体保留。

**触发点有两个：**

1. **turn 前**（`prepare_history`）：新 follow-up / input 续答建窗时；
2. **tool 批次之间**（`run_provider_turn` 循环内）：一批工具全部执行完、结果
   已持久化、下一次 provider 请求发出前。压缩成功后从"摘要卡落盘后的快照"
   重新投影消息，而不是继续用内存数组——进行中的 turn 是完整保留后缀，
   assistant tool call 与 tool result 的配对在重建后仍然有序。

**近期原文保留：** CompactSummary 卡 payload 记录
`retain_from_sequence`（保留后缀最旧段的首张卡序号）。后续窗口投影时，摘要
消息在最前、该序号之后的段**逐字**重放（JSONL 中卡追加在尾部，纯文件顺序
表达不了"摘要在前、原文在后"，由 payload 边界重建）。无该字段的旧卡保持
"supersede 一切"的原始语义；边界为 `None`（除了新 prompt 没有任何保留段）
时，新 prompt 文本显式折进摘要源（§3.2）。旧摘要段永远不进保留后缀，而是
并入新摘要——summarize prompt 明确要求合并 prior checkpoint、丢弃过期事实。

**执行：** 以 `agent.summarize` 槽位为 system、被更替范围的有界纯文本为
user，发起一次受限 provider 请求（不带 tools，超时/取消与常规请求一致）；
摘要经脱敏后作为 CompactSummary 卡持久化（turn 前路径在 turn 启动提交后
立即追加，tool 轮间路径以 `:round:N` source key 追加）。

**降级与熔断：** 摘要请求失败、空回复、或摘要导致重建请求超预算 → 回落纯
窗口行为，并持久化一张 System 审计卡，不阻断 turn。连续 3 次失败后，本进程
内该会话不再尝试摘要（静默丢弃兜底），进程重启清零；一次成功即清零。熔断
状态是进程内的，不进 JSONL：失败多为瞬态（summarizer 不可用/凭据问题），
审计卡已保留全部记录。

已知的确定性张力：摘要是 lossy 且非确定的，但它持久化进 JSONL 权威。字节层面
replay 确定性不受影响（同一卡片字节重放）；语义保真度取决于 summarizer 模型，
这是接受的设计代价，不是缺陷。

## 4.5 只读用量投影（已实现）

"此刻这个会话的下一个请求有多满"是一个只读派生问题，答案由
`latte_core::ContextUsage`（纯数据、可序列化、`deny_unknown_fields`）承载，
经 `GET /v1/sessions/{session_id}/context` 暴露：

- 字节字段精确：`request_budget_bytes`（与窗口同一公式
  `min(max_request_bytes, max_input_bytes - reserved_output_bytes)`）、
  `used_bytes`（system + 拟合保留历史段的 wire 字节）、`remaining_bytes`
  （饱和减法）、`context_cap_bytes`；
- token 字段是估算：profile 的 `TokenEstimateParams.bytes_per_token` 向上取整，
  只服务展示与未来的主动触发，永不进入 fail-closed 判定；
- `discarded_segments`：当前历史下 newest-first 拟合会从最旧端丢出的段数，
  也就是下次压缩要更替的范围；
- `compaction_strategy` / `trigger_ratio` / `proactive_compaction_due`：解析后的
  策略状态。due 的判定是估算 token 交叉相乘的整数比较
  （`used*100 >= budget*trigger_ratio`），且仅当策略非 `Off` 才可能为真——
  loop 在 turn 前与 tool 批次之间按同一判定主动压缩（§4），投影本身只读
  不触发。

投影不含未来 prompt：历史段全部按"可丢弃"拟合，连最新历史段都装不下时
`used` 只剩 system、`discarded_segments` 计全部段，投影本身不报错；硬错误
（当前 prompt 不可丢弃时拒绝建窗）仍是 turn 构建路径独有的责任。投影不做
provider I/O、不改变任何状态。用量不进权威 `SessionSnapshot`：它是 profile 与
快照的派生值，随 profile 解析而变化，不属于 JSONL 权威状态。

## 4.6 确定性省略层与 provider-overflow 恢复（ElideToolResultsThenSummarize，已实现）

**第一层：确定性骨架（无 provider 调用）。** 被更替前缀里的每条旧 tool
result，其 `Tool` 消息保留 `role`/工具名/`tool_call_id`（assistant tool call
与 result 的 provider 语法配对不断），content 替换为无密骨架：

```
[elided tool result: tool=<name>, original_bytes=<n>, status=ok|error(, truncated)]
```

- 骨架只含工具名与**字节数**与状态；`error` 状态仅依据结果 payload 的 `error`
  键判定，错误文本本身绝不回显（防止错误信息里的敏感片段回流）；
- 段的摘要源纯文本同步镜像替换，因此即使回落摘要层，summarizer 读到的也是
  骨架而不是完整工具转储；
- 变换幂等：已是骨架的结果再次投影仍是同一骨架。

**与摘要层共用同一规划器。** 被动丢弃/主动水位算出边界后，先跑省略层：

- 被动：省略后重新 newest-first 拟合若能装下全部段 → 已治愈；
- 主动：省略后估算用量降回 `trigger_ratio` 以下 → 已治愈；
- 治愈且 `enforce_budget` 通过 → 直接以 `Elided` 投影发请求，**不产生摘要
  请求、不花模型费用**；未治愈 → 省略后的视图喂给 §4 的摘要层，熔断/降级
  语义不变。

**审计与不可变。** 每次省略追加一张 `tool_result_elision` 卡（append-only，
source key 区分触发点：turn 启动、`:input`、`:round:N` 强制恢复），payload
为 `tool_result_sequences`；审计文本记录省略条数。投影时汇总全部该类卡片得到
全局单调的"已省略序号集合"。卡本身永远不会变成 provider 消息；**完整原文仍在
JSONL**，省略只发生在模型可见投影层。

**Provider context-overflow：每 turn 一次的强制恢复。** provider 以 HTTP
400/413 拒绝且响应体命中结构化信号（`error.code`/`error.type` 为
`context_length_exceeded`、`string_above_max_length` 等）或纯文本标记
（"maximum context length"、"context window" 等）时，分类为类型化的
`ProviderError::ContextOverflow`，与普通 HTTP 错误区分。循环在收到该错误时：

1. 当 turn 尚未用过恢复 → 跳过水位闸门强制执行一次收缩（先省略层；策略为
   纯摘要时直走摘要层）；
2. 只有当快照 revision 前进（收缩确实落盘）**且**重建请求严格更短
   **且**通过精确预算时，才重试同一 turn；
3. 重试后再次 overflow（或收缩无收益）→ 走通用错误路径，turn 以 retryable
   失败收尾、会话保持可用。

已知限制：开放 turn 内部不做省略（会拆散进行中的 tool-call/result 语法配对）；
若 overflow 发生在历史可更替范围为空（例如首条 prompt 自身超限），恢复无收缩
对象，按失败处理。

## 4.7 手动 `/compact`（idle-only，已实现）

显式手动压缩复用同一套分层与卡片，只在触发资格与失败语义上不同：

- **端点/CLI**：`POST /v1/sessions/{id}/compact` 与 `latte-code compact
  <session-id>`，无请求体。
- **idle-only（fail-closed）**：仅 `Ready` 且无活跃 runner 的会话可执行；
  `running`/`waiting_permission`/`waiting_input` 等一律 409。卡片落库走一条
  独立的存储闸门：只有 `compact_summary` / `tool_result_elision` 两种 kind、
  只能挂在会话**最新 turn** 上，并继续受 session revision CAS 与 lease fencing
  约束——已完成 turn 不接受任意 transcript 写入。并发 follow-up 抢跑时 CAS
  失败，映射为 409，客户端 refetch 后重试。
- **水位以下也强制**：没有"丢弃"概念，边界直接由 §4 的整段 `retain_ratio`
  后缀规则给出（无开放 prompt，全部段可移动；最新段仍始终保留）。分层顺序
  不变：策略含省略层时先确定性省略——省略产出非空且过精确预算即收工，不产生
  模型请求；否则进入摘要层。
- **空态即成功（200 `nothing_to_compact`，不是错误）**：无历史（`empty`）、
  策略 `Off`（`disabled`）、熔断中（`breaker_tripped`）、只有一个不可压缩
  段（`nothing_to_compress`）。幂等：对已压缩到最小的会话重复调用仍是
  空态，revision 不变。
- **失败语义不同**：自动路径摘要失败会降级并继续当前 turn；手动压缩没有
  in-flight turn 可续，摘要失败/摘要超预算直接报错（仍计入熔断），不写降级
  审计卡。
- 卡片 source key 与自动路径区分：`:manual` 后缀
  （`{turn_id}:compact-summary:manual` / `…:tool-result-elision:manual`）。

## 4.8 前缀稳定性契约与非持久 reminder 槽（已实现）

provider 的 prompt cache 以 system 开头的稳定前缀为键；工作区文件（AGENTS.md、
根清单）每次 turn 都可能变化，旧实现把 repo context 渲染进 system prompt，任何
一次编辑都会使全部前缀缓存失效。本节把"模型可见形状"与"持久层"解耦：

- **稳定 system 头（head）**：profile 模板的 `{repository_context}` 注入点恒以
  **空串**渲染。对同一 binding，每个 turn、每次进程重启后的 system 头逐字节相同；
  工作区文件变化不移动缓存前缀。
- **repo context 移到非持久尾部消息**：`context::build` 采集的内容（根 AGENTS.md、
  focus 嵌套 AGENTS.md、根清单）包进
  `<repository-context> … </repository-context>` 框架，以 **user 角色**挂在
  持久历史之后、当前 prompt 之前。该消息只存在于发往 provider 的请求里：
  **不写 JSONL、不进 transcript、不参与任何持久化**（磁盘内容以工作区文件本身为
  权威，下一个 turn 重新采集）。
- **一次性 `<system-reminder>` 槽**：进程内
  `HashMap<SessionId, String>`，由 `POST /v1/sessions/{id}/reminder`
  （body `{"text": …}`）装填，下一次 turn 构建时**消费一次即清除**，以
  `<system-reminder>` 框架 user 消息挂在 repo tail 之后、prompt 之前。同样
  非持久（重启即失，JSONL 中永不出现）。装填仅允许 `Ready` 会话；非 idle 一律
  409。文本在写入边界先过 `redact_session_text`（token/密钥脱敏），trim 后为空
  或超过 `REMINDER_CAP_BYTES = 4096` 字节返回 400；响应回显脱敏后字节数。
- **框架防伪造**：框架正文内出现的同名开/闭标签（ASCII 大小写不敏感）一律替换
  为方括号形态（`[tag]` / `[/tag]`），工作区文本无法提前关闭框架或冒充嵌套块。
- **wire 顺序**：
  `[system 头] [summary?] [持久历史…] [repository-context?] [system-reminder?] [当前 prompt]`。
  tool 轮内重建（mid-turn rebuild）以活跃 turn 的第一段为界：volatile 块整块
  重新插在该边界之前，已完成历史在前、开放 turn（prompt、assistant 批次、tool
  result）原文在后——同一 turn 内只重建一次 volatile 并全程复用，轮内前缀也保持
  稳定。审批恢复路径重建 repo tail，但不补发已消费的 reminder。
- **预算紧张时的丢弃顺序（newest-first 精确字节拟合）**：reminder（最新、可丢）
  → repository 快照（可丢）→ 持久历史整段 → prompt（强制；放不下即 fail-closed）。
  存活集合天然嵌套：预算收缩时 reminder 先消失，然后是 repo，然后是历史；
  prompt 与 head 永不因可丢尾部被静默删掉。
- **不可变承诺不变**：持久层依旧只 append；模型可见层从不重写 system 头，易变
  更新只能附加在尾部。压缩是唯一允许的边界替换，且压缩后开启新的缓存序列
  （摘要消息改变形状，不属于本节约束的稳定前缀）。

## 5. v2 方向（声明，未实现——本文不因本节改变 checklist 状态）

- **TUI 预算展示**：§4.5 的只读用量投影尚未接入终端状态栏；目标形状是打开会话
  时按 session id best-effort 拉取，在头部第二行渲染填充百分比、估算 token 与
  `discarded_segments`/due 琥珀提示，不产生新的写路径。
- **压缩请求复用对话前缀**：今天摘要请求是独立 system + 纯文本（deepseek-harness
  的做法是重放原对话 system+tools+messages、只在末尾加压缩指令，使辅助请求成为
  上一次请求的真前缀以命中 provider 缓存）；受限于当前摘要卡模型，记入后续项。
- **跨模型 handoff**：摘要生成模型与消费模型不同 profile 时，摘要 prompt 与
  token 估算随消费方 profile 解析；生成方 profile 记录进卡片 payload。

## 6. 测试契约

- 策略为 `Off` 时与历史行为逐字节一致（零变化迁移证明，UT 固化）。
- `SummarizeOnDiscard` 的完整旅程（摘要请求形状、续请求含摘要、被更替文本
  不回归、卡片持久化）由 UT + final-binary E2E 双层守护。
- 主动压缩：到水位未丢弃时即压缩；保留后缀为整段、新卡携带
  `retain_from_sequence`，后续窗口"摘要在前 + 近期原文逐字"。
- tool 轮间压缩：压缩后从落盘快照重建，assistant tool call 与 tool result
  配对不断、顺序不乱。
- 熔断：连续 3 次失败后不再尝试；一次成功清零。
- 降级路径（摘要失败/超预算）必须产出审计卡且不阻断 turn。
- 段边界语义（CompactSummary supersede 与 retain 边界重放）有独立 UT。
- 省略层：`elide_prefix` 替换 Tool 消息与段源文本且幂等（UT）；被动丢弃被
  省略治愈时不产生摘要请求（UT）；无 tool result 可省略时回落摘要层（UT）；
  完整原文仍在 JSONL、审计卡携带 `tool_result_sequences`（final-binary E2E）。
- provider-overflow：结构化 code 与纯文本标记的分类矩阵（UT）；一次性恢复
  成功（省略 → 严格缩短 → 重试，UT + final-binary E2E）；恢复权用尽后第二次
  overflow 以 retryable 失败收尾、会话保持可用（UT）。
- 手动压缩：水位以下强制摘要/省略（UT + final-binary E2E，含跨进程边界重放）；
  空态（单段 / Off / 熔断）返回 `nothing_to_compact` 且 revision 不变
  （UT + E2E；`empty` 为无 turn 的防御分支，正常建会话流程不可达）；非 Ready
  会话 409（UT + E2E）；存储闸门白名单只允许两种压缩卡写最新 turn，其他 kind
  在 ready 会话被拒（engine UT）。
- 前缀稳定性：同一 binding 的 system 头在 AGENTS.md 编辑前后、跨进程逐字节一致
  （UT + final-binary E2E）；repo 快照只出现在带框架的尾部 user 消息且定位在本轮
  prompt 前一条，turn 1 见 V1、turn 2 见编辑后的 V2（UT + E2E）；框架与快照内容
  均不落 JSONL（UT + E2E）。
- reminder 槽：装填在写入边界脱敏、空文本/超 4096 字节为 400（headless UT +
  HTTP UT）；仅 `Ready` 可装填，停放会话 409 并回带 current revision
  （headless UT + HTTP UT + final-binary E2E）；下一次请求携带且仅携带一次
  `<system-reminder>`（位置在 prompt 之前）、再下一次请求消失、transcript 与
  JSONL 中均不存在（UT + E2E）；未知 session 404、坏 id/坏 body 400（HTTP UT）。
- 框架转义：正文内伪造的开/闭标签按 ASCII 大小写不敏感降级为方括号，包装层自身
  标签恰好出现一次（UT）。
- 丢弃顺序：reminder → repo → history 的嵌套阈值（各层独立变异点）由
  newest-first fitter UT 固化；mid-turn 重建时 volatile 块精确插在活跃 turn
  边界之前、summary 位于头下第一位（UT）。
- 轮间压缩与挥发性尾部并存（final-binary E2E）：开放 turn 的第二个 tool 轮超
  预算时，轮间摘要请求之后的重建形状为
  `[头, 摘要, 已完成历史, repo tail, 开放 turn 原文]`——活跃 turn 的完整 tool
  result 不被骨架化、尾部恰好落在活跃 prompt 前一条；轮间被动省略治愈同理（旧
  turn 大结果骨架化、开放 turn 新结果逐字），且均不产生摘要模型请求。
- 主动省略（水位触发、尚无丢弃）与手动省略（`:manual` 卡）各自由独立
  final-binary E2E 固化：不产生摘要请求、后续跨进程请求投影骨架、完整原文留在
  JSONL。
- 策略选择器的 serde 往返与 `deny_unknown_fields` 负向用例。
