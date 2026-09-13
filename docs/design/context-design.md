# Context 架构与上下文管理策略

状态：**v1 已实现**（分层模型、SummarizeOnDiscard 策略、策略选择器数据化）；
文中标注 v2 的条目（工具结果省略、主动触发、预算可见性、跨模型 handoff）未实现。
日期：2026-09-13
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
user 段参与后续窗口。因此摘要源必须包含"卡片落点之前的全部被更替文本"——
包括触发本次请求的 prompt（卡片在 turn 启动事务后落盘，落点在当前 user 卡之后）。

### 3.3 策略选择器：数据，不是行为

上下文管理策略 = **选择算法 + 更替算法**的组合。遵循 profile 基石
（数据不是行为，见 harness-profile 设计 §3），策略以**数据化的选择器**表达：

```rust
/// latte-core::CompactionStrategy（数据化的策略选择器）
pub enum CompactionStrategy {
    /// 关闭：窗口丢弃即静默更替（v1 之前的原始行为）。
    Off,
    /// 窗口丢弃发生时，以 agent.summarize 槽位生成摘要卡并持久化。
    SummarizeOnDiscard,
    /// v2 预留：对被更替范围内的工具结果做省略（保留路径/结论/错误骨架），
    /// 对其余文本做摘要。形状与语义由本文 §5 定义，实现未开始。
    ElideToolResultsThenSummarize,
}
```

loop `match` 选择器执行对应算法；profile 携带选择器 + 参数
（`trigger_ratio` / `max_summary_source_bytes` / prompt 槽），因此：

- 可整体快照进 session 记录、可版本化、可作契约测试 fixture；
- 新策略 = 新 enum 变体 + loop 新分支，profile schema 向后兼容（minor 演进）；
- 禁止 trait 对象注入——策略实现不进 profile，权限天花板不被绕开。

## 4. v1 策略：SummarizeOnDiscard（已实现）

触发：窗口选择发生丢弃 且 策略为 `SummarizeOnDiscard`。
执行：以 `agent.summarize` 槽位为 system、被更替范围的有界纯文本（含当前
prompt，见 §3.2）为 user，发起一次受限 provider 请求（超时/取消与常规请求
一致）；摘要经脱敏后作为 CompactSummary 卡在 turn 启动提交后立即持久化，
并把 commit 返回的新 snapshot 传给后续 turn（追加会推进 session revision）。
降级：摘要请求失败或摘要导致重建请求超预算 → 回落 `Off` 的行为，并持久化一张
System 审计卡。摘要源按 `max_summary_source_bytes` 截断（char-boundary 安全）。

已知的确定性张力：摘要是 lossy 且非确定的，但它持久化进 JSONL 权威。字节层面
replay 确定性不受影响（同一卡片字节重放）；语义保真度取决于 summarizer 模型，
这是接受的设计代价，不是缺陷。

## 5. v2 方向（声明，未实现——本文不因本节改变 checklist 状态）

- **ElideToolResultsThenSummarize**：对被更替范围内的 ToolResult 卡生成骨架
  （工具名、目标路径、结果规模、错误码），仅对剩余文本摘要。省略是确定性变换
  （无 provider 调用），应先于摘要执行以降低摘要源规模。
- **主动触发**：`trigger_ratio` 按估算 token 占 `context_cap_bytes` 的比例在
  撞墙前触发；估算参数来自 profile 的 `TokenEstimateParams`。不进入 fail-closed。
- **`/compact` 手动命令**：复用同一条摘要路径，挂入 slash command catalog。
- **预算可见性**：估算 token 余量、临近触发提示进 snapshot 与 TUI/server 投影。
- **跨模型 handoff**：摘要生成模型与消费模型不同 profile 时，摘要 prompt 与
  token 估算随消费方 profile 解析；生成方 profile 记录进卡片 payload。

## 6. 测试契约

- 策略为 `Off` 时与历史行为逐字节一致（零变化迁移证明，UT 固化）。
- `SummarizeOnDiscard` 的完整旅程（摘要请求形状、续请求含摘要、被更替文本
  不回归、卡片持久化）由 UT + final-binary E2E 双层守护。
- 降级路径（摘要失败/超预算）必须产出审计卡且不阻断 turn。
- 段边界语义（CompactSummary supersede）有独立 UT。
- 策略选择器的 serde 往返与 `deny_unknown_fields` 负向用例。
