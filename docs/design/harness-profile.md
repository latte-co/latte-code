# Harness Profile 抽象设计（v1 薄切片）

状态：**v1 已实现**（类型 + 解析管道 + loop 接线 + 权限天花板，#18）；
上下文分层与压缩策略的架构见 [Context 架构](context-design.md)；
**批次 2 部分实现**（#19 后续切片）：compaction 本体（摘要 turn 替代静默丢弃、
失败降级审计、JSONL 持久化、`session.compaction.enabled` 配置面）已落地；
主动水位（`trigger_ratio`/`retain_ratio`）与确定性省略层
（`session.compaction.mode:"elide_then_summarize"`，先骨架化旧 tool result、
不足再摘要）已落地，并承接 provider context-overflow 的每 turn 一次强制恢复
（见 context-design.md §4/§4.6）；idle-only 手动 `/compact`（HTTP + CLI，
水位以下强制、明确空态、409 非空闲）已落地（§4.7）；预算可见性 TUI 状态栏
（头部第二行 meter：填充百分比、估算 token、omitted/compaction due 提示）已落地
（context-design.md §4.5）；
未实现：binding 快照三字段（schema 迁移）、
config `profiles` 覆盖段、`ToolPresentation`/`StopSemantics`。
日期：2026-09-12
范围：定义 `HarnessProfile` 的概念位置、核心类型、解析管道、约束边界与首个消费者
（context compaction）的集成点。工具呈现与 stop 语义是 v2 扩展方向，v1 不含。

调查范围：`crates/` 全部 crate 中与模型行为相关的硬编码点、provider 配置结构、
session 存储与 binding 持久化链路。所有结论附 `file:line` 证据（基于 `3fd8521`）。

---

## 1. 问题陈述

同一个 agent loop 对不同模型的"共事方式"目前散落三处，互相脱节：

1. **预算一刀切。** `SessionHistoryPolicy` 是全局单例语义：二进制层从 config 的
   `session` 段构造一份策略，所有模型共用（`crates/latte-code/src/lib.rs:78`、
   `:149`、`:182-183`；`latte-code.config.example.jsonc` 的 `session` 段无模型维度）。
2. **每模型配置与运行时断连。** registry 已支持按模型声明
   `context_window` / `max_tokens` / `reasoning_effort`
   （`crates/latte-headless/src/registry.rs:130-140`），但这些值只参与配置校验
   （拒绝 `context_window == 0` 等，`registry.rs:750-755`），从未进入请求预算或
   loop 行为。
3. **system prompt 单一硬编码。** `system_prompt()` 是与模型无关的唯一模板
   （`crates/latte-headless/src/session.rs:2281`）。

直接后果：context 构建是 newest-first 的字节窗口，装不下的旧 segment **静默丢弃、
无摘要**（`session.rs:1236-1252`；单条 turn 自身超限才 fail-closed 报错
`session.rs:1242-1245`）。模型丢失早期决策与工具结果而不自知，这是任务质量问题，
先于硬失败暴露。

compaction 要落地，上述参数必须按 binding 解析——这就是 Harness Profile 要解决的
问题。

## 2. 规范模型：两个正交维度，在 binding 汇合

```
binding (provider_name + model)
   ├── 协议维 → Provider adapter：怎么连、怎么编解码（HTTP/SSE/tool-call 线格式）
   └── 模型维 → HarnessProfile：怎么跟这个模型共事（context/prompt/语义）
              ↓
   Session Runner 各持一份，组合运行
```

- Provider 是 N:M 关系的一侧：一个 provider（如 openai-chat）可服务多个模型，
  各需不同 profile；同一模型可经多个 provider（官方 API / 网关 / 本地 vLLM）到达，
  profile 可复用。
- **禁止的耦合**：profile 不得成为 Provider trait 的字段或方法（一个 provider 服务
  多模型时无法返回单一 profile，且 profile 需要独立版本化持久化）；profile 也不得
  反向构造 provider。
- 真正的组合发生在 Session 侧：Runner 持有 `(Provider 实例, Profile 快照, Engine
  handle)`，生命周期归 Session。
- 既有汇合点：`SessionProviderBinding`（`crates/latte-core/src/session.rs:115-127`）。
  TUI model picker 切换 binding 时 provider 与 profile 一起重解析，在不可变 child
  边界生效，复用现有 `BindingChanged` 事件（`session.rs:408`）链路。

## 3. 核心类型

**决策：Profile 是数据，不是行为。** plain struct + resolve 函数，不做带虚方法的
trait 对象。理由：数据可整体快照进 session 记录、可版本化迁移、可作为契约测试的
fixture；行为对象三者皆难。这与仓库"typed state、显式错误"的既有风格一致。

```rust
/// crates/latte-core/src/profile.rs（新；与 SessionProviderBinding 同层）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessProfile {
    /// 稳定标识，如 "generic-openai-chat"。随快照持久化。
    pub profile_id: String,
    /// 结构化版本号。major 变更 = 语义不兼容，需要显式迁移。
    pub version: ProfileVersion,
    /// v1 实现维度。
    pub context: ContextPolicy,
    /// v1 实现维度。
    pub prompts: SystemPromptSpec,
}

pub struct ProfileVersion { pub major: u32, pub minor: u32 }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextPolicy {
    // 吸收 SessionHistoryPolicy 全部字段（session.rs:96-116），语义不变：
    pub max_request_bytes: usize,
    pub max_input_bytes: usize,
    pub reserved_output_bytes: usize,
    pub context_cap_bytes: usize,
    pub max_tool_rounds: Option<u32>,
    pub provider_timeout_ms: u64,
    // 新增：compaction 配置（策略选择器 + 参数，见 context-design.md §3.3）
    pub compaction: CompactionPolicy,
    // 新增：token 估算参数（见 §3.1 字节/token 取舍）
    pub token_estimate: TokenEstimateParams,
}

pub struct CompactionPolicy {
    pub strategy: CompactionStrategy,
    /// 主动触发阈值：估算用量占请求预算的比例（1..=100，默认 90）。
    pub trigger_ratio: u8,
    /// 压缩后逐字保留的近期后缀占请求预算的比例（1..=80，默认 20，
    /// 按整段累加，见 context-design.md §4）。
    pub retain_ratio: u8,
    /// 摘要请求使用的 prompt 槽（见 SystemPromptSpec）。
    pub summary_prompt_id: String,
    /// 单次摘要最多覆盖的历史范围（字节），防摘要请求自身超限。
    pub max_summary_source_bytes: usize,
}

pub struct SystemPromptSpec {
    /// prompt 槽位集合。v1 两个槽：
    ///   "agent.system"   —— 主 system prompt（稳定头）
    ///   "agent.summarize" —— compaction 摘要指令
    /// 槽位内容 = 内置模板 + 每模型覆盖（config 提供）。
    ///
    /// "agent.system" 中的 `{repository_context}` 注入点恒以空串渲染：
    /// 工作区快照（AGENTS.md / 根清单）改走非持久的 `<repository-context>`
    /// 尾部 user 消息，保证同一 binding 的 system 头跨 turn、跨进程逐字节
    /// 稳定（前缀缓存契约见 context-design.md §4.8）。
    pub slots: BTreeMap<String, String>,
}
```

`ToolPresentation`（工具 schema 呈现/命名规则）与 `StopSemantics`（stop reason 到
完成/继续的映射）是 v2 扩展方向：v1 的 `HarnessProfile` **不含**这两个字段，
等第二个真实消费者（多 provider 时期）出现时再以新字段加入（minor 版本演进），
避免投机设计与死字段。

### 3.1 字节预算与 token 估算的取舍

现状预算是**字节**精确的（`wire_bytes`，`session.rs:1241`），可确定性执行
fail-closed。精确 token 计数需要 per-model tokenizer，依赖重且引入不确定性。
v1 决策：

- **字节预算保持权威**：所有硬边界（拒绝、截断）仍按字节执行，行为可复现。
- **token 估算用于决策与展示**：`TokenEstimateParams`（如 bytes-per-token 系数，
  按 profile 声明）驱动 compaction 触发判断与"预算余量"可见性。估算值不进入
  fail-closed 判定。

这满足 roadmap"可解释 token 预算"的第一步（可见、可解释），把"精确 tokenization"
留给确实需要的后续版本。

## 4. 解析管道

```rust
/// crates/latte-headless/src/profile.rs（已实现）
impl ProfileCatalog {
    pub fn resolve(&self, binding: &SessionProviderBinding)
        -> Result<ResolvedProfile, ProfileError>;
}
```

### 4.1 来源与优先级

解析输入是 `SessionProviderBinding`，来源三层，后者覆盖前者，逐字段合并：

1. **内置 catalog**（代码内常量表）。v1 只有一条：`generic-openai-chat`——
   即当前 `SessionHistoryPolicy::default()` + 现行 `system_prompt()` 的值原样搬家，
   保证行为零变化迁移。
2. **应用 config 的 `session` 段**（`ProfileCatalog` 构造时传入的 base）。
   覆盖规则（`layer_budget`）：base 中**等于历史默认值**的字段不覆盖内置——
   继续跟随内置 profile 的值；只有用户显式改动的字段才覆盖内置。这样内置
   profile 的预算是"活"的：未来调整内置预算时，未做覆盖的配置自动跟随，
   而不是被 config 默认值静默替代（后者会让内置层沦为死代码，v2 的
   per-profile 预算也会被全局一刀切吞掉）。
   已知边界：把字段**显式设成默认值**的用户同样会跟随未来内置变化；区分
   两者需要 config 层按字段记录"是否显式设置"，v1 不做，留待 `profiles`
   覆盖段（第 3 层）一并解决。
3. **provider config 的 model options**。`context_window` 映射为
   `context_cap_bytes` 的收紧上限（token × bytes_per_token，只紧不松；
   推导系数属于内置 profile 的职责）。

config 顶层可选 `profiles` 段（按 `(provider, model)` 或 `profile_id` 覆盖
ContextPolicy / prompt 槽位，`deny_unknown_fields`）属于 v2；届时按字段
presence 语义取代第 2 层的"等于默认值"启发式。

**两套合并口径并存**：预算字段用"config 等于历史默认值则跟随 builtin"，
compaction 用"config 策略非 Off 则整体取 config、否则取 builtin"。后者避免
builtin 层死代码且 config 激活即生效；前者保证未触碰字段跟随 builtin 演进。
新增字段时二选一并在此处记录口径。另：`CompactionPolicy.trigger_ratio` 与
`retain_ratio` 的配置面是 `session.compaction.trigger_ratio` /
`retain_ratio`（0 = 跟随 profile 默认）；`summary_prompt_id` 仍是 profile
内部值，`validate()` 对非 Off 策略统一校验——v1 默认值安全。

### 4.2 fail-closed 与已持久化 binding 的兼容

- 已知 `provider_type`（v1：`openai-chat`、`embedded`、`test`）总能解析到内置
  profile——**fail-closed 不等于把现有配置挡在门外**，迁移负担为零。
  白名单同时是**升级兼容边界**：binding 原样落库（含 `provider_type`，
  `SessionProviderBinding::validate()` 不约束取值），历史上任何 release 可能
  持久化过的取值都必须继续可解析，否则老 session 升级后无法恢复。`test`
  就是为此保留的夹具类型；新增 provider adapter 时必须同步扩展白名单并
  带升级回归测试。
- 未知 `provider_type`（未来 adapter）且用户未显式提供 profile：解析报错，session
  创建被拒绝。绝不静默套用 `generic-openai-chat`。
- 用户覆盖值必须通过 `ContextPolicy` 的 `validate()`，不合法即拒绝，无部分生效。

### 4.3 解析时机

binding 选定或切换时解析一次（`registry.rs` 的 `session_binding_for_model` /
`resolve_session_bound` 一线），产物随请求进入 loop。Session 存续期间 profile 不
重读配置——中期切 binding 走既有 child 边界机制重新解析。

## 5. 权限天花板

roadmap 已划线："（Harness Profile）不能依赖不透明的模型名猜测来放宽权限"
（`docs/roadmap.md` 配置节）。落地为三条硬规则：

1. **schema 隔绝**：`HarnessProfile` 及其子结构没有权限、effect、policy、工具
   allowlist 字段；`serde(deny_unknown_fields)` 使任何试图夹带权限语义的配置键
   在解析期报错，而非忽略。
2. **域校验**：resolve 完成后断言 profile 只影响 prompt 文本、请求预算与 loop
   内部节奏；`latte-engine` 的 authority、permission、deny glob 路径不读取 profile。
3. **内置 profile 优先级最低**：用户覆盖只能收紧（调小预算）或改写 prompt 文本，
   不能把任何 fail-closed 校验改为放行。

## 6. 快照与持久化

**决策：快照字段并入 `SessionProviderBinding`**（`latte-core/src/session.rs:115-127`），
新增三个字段，与既有 `config_fingerprint` / `tools_fingerprint` 同构：

```rust
pub struct SessionProviderBinding {
    // ... 现有字段不变 ...
    pub profile_id: String,
    pub profile_version: ProfileVersion,
    /// resolved profile 数据的稳定哈希，resume 时一致性校验。
    pub profile_fingerprint: String,
}
```

- **不存整个 profile 数据**，只存标识 + 版本 + 指纹。数据可由代码内置 catalog +
  配置重放得到；指纹用于检测"重放结果与创建时不一致"（配置漂移）。
- resume 校验：指纹不一致时**按记录的语义继续**（fail-open 向后兼容）并产生一条
  显式 warning 事件（transcript 可见），不阻断恢复——阻断会把"用户改了配置"变成
  "旧 session 全部不可用"。权限相关变更不受此豁免（见 §5：profile 本就不携带
  权限）。
- `BindingChanged`（`session.rs:408`）事件 payload 带上三个新字段，SSE 消费者
  （TUI/server 投影）可见模型行为参数的变化。

## 7. 版本与迁移

- `ProfileVersion.minor`：新增可选字段、模板措辞调整。旧快照直接兼容。
- `ProfileVersion.major`：语义不兼容（预算推导规则变化、槽位含义变化）。旧 session
  resume 时按记录的 `(profile_id, major)` 选择执行路径：内置 catalog 保留历史
  major 版本的只读副本（数量预期 ≤ 2），不为旧版本执行新语义。
- 内置 catalog 的版本演进属于代码变更，走正常 PR 评审；用户 profile 的版本由
  config 显式声明，缺省绑定内置最新 minor。

## 8. 首个消费者：context compaction 集成点

profile 的价值由真实消费者逼出形状；v1 的唯一消费者是 compaction。

1. **ContextPolicy 替换全局策略**。`crates/latte-code/src/lib.rs:78/149/182` 构造
   `SessionHistoryPolicy` 的位置改为从 resolved profile 取；config `session` 段
   语义并入 `profiles` 覆盖层（保留一段废弃期，读旧段时 warning）。
2. **摘要 turn 替代静默丢弃**。窗口构建（`session.rs:1236-1252`）在 break 处记录
   被丢弃的 segment 范围；当策略为 `SummarizeOnDiscard` 且触发条件满足：
   - 以 `agent.summarize` 槽位 prompt + 被丢弃范围发起一次受限 provider 请求
     （受 `max_summary_source_bytes` 与既有请求超时约束）；
   - 摘要结果写入一条新的 transcript record（新 `TranscriptKind`，JSONL 权威，
     投影/回放同步扩展——schema 变更走既有版本化迁移机制）；
   - 后续窗口构建为 `[system, summary_record, 保留的近期 segments]`。
   摘要失败不阻断主循环：降级为现状的静默丢弃，并记录失败事件。
3. **可见性**。每 turn 结束把"估算 token 余量 / 是否临近 compaction 阈值"写入
   snapshot（进 TUI 状态栏与 server 投影），落实"可解释预算"第一步。
4. **`/compact` 手动命令**。挂入现有 slash command catalog，复用同一条摘要路径。

## 9. 测试策略

按仓库门禁（UT 95% / E2E 90%，`docs/design/testing-gates.md`）：

- **resolution 契约测试**：每个内置 profile 的 fixture 断言"同 binding → 同
  fingerprint"（确定性）；三层来源覆盖合并的每个字段一例。
- **fail-closed 用例**：未知 provider_type 无显式 profile；用户覆盖未过
  `validate()`；覆盖含未知键（deny_unknown_fields）。
- **权限天花板用例**：profile 解析结果不含任何权限域字段的类型级断言。
- **compaction 路径 E2E**：loopback provider 驱动超长历史触发窗口截断 → 断言摘要
  record 落盘、后续请求包含摘要、静默丢弃降级路径、`/compact` 路径。
- **快照一致性**：resume 时指纹漂移产生 warning 事件且不阻断。

## 10. 非目标

- 不做 capability negotiation、未知 profile 的运行时协商（v1 只有静态解析）。
- 不做 per-profile conformance suite（待多 profile 并存后按 roadmap 补齐）。
- 不做 Provider adapter（Anthropic / Responses API 仍是独立未完成项）。
- 不实现 `ToolPresentation` / `StopSemantics` 的任何行为。
- 不做精确 tokenizer、跨 session 的 profile 用量统计。
- 不改变 latte-engine 的 authority / permission 任何路径。

## 11. 分期

- **v1（本设计）**：类型 + 解析管道 + 权限天花板 + 快照持久化 + context/prompt
  两维 + compaction 消费 + `/compact` + 可见性。
- **v2（多 provider 时期）**：`ToolPresentation`（schema 呈现）、`StopSemantics`
  （stop 映射）、fail-closed fallback 的 conformance suite、精确 token 计量。

## 关联文档

- [能力 Roadmap](../roadmap.md)——"配置、凭据与模型连接"节的 Harness Profile 条目
- [全局 Session 与数据存储](data-storage.md)——binding 持久化与 fingerprint 惯例
- [异步 Turn Runner](agent-harness/asynchronous-turn-runner.md)——loop 消费侧契约
