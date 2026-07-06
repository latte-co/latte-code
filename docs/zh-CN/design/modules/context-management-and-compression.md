# 模块技术设计：Context Management and Compression

## 文档状态

本文定义 Lattecode 近期上下文管理与压缩设计。它位于当前 / 近期模块设计层：约束 `v0.1` 已有上下文预算行为，并说明下一步如何演进到 `ContextLedger`、lane-aware `ContextProjection`、`ToolOutputRef`、append-only revision / `CompactionRecord`，以及 cache-aware prompt rendering envelope。

本文引用长期 `StateStore`、`Fact` graph 和 token-aware provider window 时，仅表示后续 runtime evolution 方向，不表示当前 `src/` 已经实现这些能力。

英文对应文档：[`docs/en-US/design/modules/context-management-and-compression.md`](../../../en-US/design/modules/context-management-and-compression.md)。

## 1. 设计目标

Lattecode 的上下文压缩不是普通 transcript summary，而是可审计的历史转换：每次压缩都必须说明输入范围、保留内容、丢弃内容、引用的外部工具输出、预算决策和使用的 prompt 版本。

近期目标是形成以下链路：

```text
Session / Event Log / Evidence / Tool Outputs
  -> ContextLedger
  -> per-turn ContextProjection
  -> Stable Prefix / Append-only Ledger / Dynamic Suffix prompt rendering envelope
  -> PromptRegistry prompt messages + provider cache hints
  -> append-only CompactionRecord chain
```

核心不变量：

- task、acceptance、权限状态、验证结果、关键 evidence 不能因为压缩而静默丢失。
- 大型 tool output 不直接塞入 prompt；prompt 只保留摘要、`ToolOutputRef` 和截断 / 省略标记。
- 压缩记录是 append-only audit trail；后续 resume 必须能解释模型看到过什么、没看到什么、为什么没看到。
- Stable Prefix / Append-only Ledger / Dynamic Suffix 是 prompt 渲染和 provider cache 行为的 envelope；它包裹 10-lane `ContextLane` 模型，不替代 lane 本身。
- provider prompt cache 只是性能优化，不是状态来源、权限来源、事实来源或恢复来源；cache hit / miss / eviction 不能改变语义、证据、恢复和预算决策。
- cached prefix 仍然计入 provider context window；cache eligibility 必须服从 policy、data boundary、retention 和 secret-redaction 边界，不是所有稳定材料都允许被 provider-side cache。
- `.tmp/codeagent/claude-code`、`CodeWhale`、`codex`、`opencode` 只作为横向调研输入；不得把 `.tmp/` 代码当成 Lattecode 正式源码、接口或实现依据。

## 2. 三层边界

| 层级 | 当前状态 / 目标 | 允许表达 | 不得表达 |
| --- | --- | --- | --- |
| 当前 `v0.1` implementation | 使用 byte estimate 控制 `context.maxPromptBytes`；`ContextSnapshot.compactedSummary` 记录基础压缩摘要；对 transcript 和 tool result 做 basic trimming；`maxToolResultBytes` 和 `recentStepCount` 约束最近上下文 | 已有最小 `context_budget_gate`；可恢复 session snapshot；基础 transcript/tool-result trimming | 不声称已有 `ContextLedger`、lane budget、append-only `CompactionRecord` 或 token-aware provider window |
| 近期设计 | 引入 `ContextLedger`、lane-aware per-turn `ContextProjection`、lane budgets、`ToolOutputRef`、append-only revisions / `CompactionRecord`、cache-aware prompt envelope | 将压缩升级为带来源、预算、遗漏、cache eligibility 和恢复语义的历史转换 | 不把模型摘要或 provider cache 当成事实来源 / 状态来源；不把丢弃内容伪装成仍在 prompt 中 |
| 长期演进 | 与 `StateStore`、`Fact` graph、正式 `Evidence` freshness / invalidation、provider token window 结合 | 让 projection 从 facts/evidence/policy/action state 生成，并按真实 provider token budget 校准 | 不要求 `v0.1` 具备完整 runtime kernel 或全量 graph recovery |

## 3. 数据模型

### 3.1 `ContextLedger`

`ContextLedger` 是 session 级上下文账本。它不替代 event log，也不替代 `Evidence`；它负责把可进入 prompt 的上下文材料按 lane、来源、预算和保留策略索引起来。

最小字段方向：

```ts
type ContextLedgerEntry = {
  id: string;
  sessionId: string;
  runId: string;
  lane: ContextLane;
  sourceType: "task" | "event" | "artifact" | "evidence" | "tool_output" | "snapshot" | "compaction" | "resume";
  sourceRef: string;
  summary: string;
  promptText?: string;
  toolOutputRef?: ToolOutputRef;
  hardPreserve: boolean;
  redaction: "none" | "redacted" | "omitted";
  createdAt: string;
};
```

Ledger entry 的 `summary` 是 prompt 可用摘要，不等同于事实；事实晋升仍属于长期 `StateStore` / `Fact` graph 责任。

### 3.2 per-turn `ContextProjection`

每次 model call 前生成一个 per-turn `ContextProjection`。它是 PromptRegistry 的输入，而不是持久事实源。

```ts
type TurnContextProjection = {
  id: string;
  sessionId: string;
  runId: string;
  stepId: string;
  promptId: string;
  promptVersion: string;
  lanes: ProjectedLane[];
  segments: PromptRenderSegment[];
  stablePrefixCacheKey?: StablePrefixCacheKey;
  omittedEntryIds: string[];
  redactedEntryIds: string[];
  compactionRecordIds: string[];
  budget: ContextBudgetDecision;
  createdAt: string;
};
```

Projection 必须记录 omitted / redacted entry，而不是只输出最终 prompt 文本。

### 3.3 Append-only revision 与 active pointer

动态 `task` 和 `phase_artifact` 不能被渲染为可原地修改的 prefix block。它们必须通过 append-only revision 表达，projection 只通过 active pointer 选择当前可见版本。

```ts
type TaskSpecRevision = {
  id: string;
  parentId?: string;
  sessionId: string;
  sourceEventId: string;
  taskText: string;
  acceptance: string[];
  constraints: string[];
  nonGoals: string[];
  createdAt: string;
};

type PhaseArtifactRevision = {
  id: string;
  parentId?: string;
  sessionId: string;
  phase: "context" | "plan" | "patch" | "verify" | "handoff";
  artifactKind: "ContextPack" | "ChangePlan" | "PatchSummary" | "VerificationResult" | "AgentHandoff";
  sourceEventId: string;
  contentRef: string;
  createdAt: string;
};

type ArtifactRevision = TaskSpecRevision | PhaseArtifactRevision;

type ActiveArtifactPointer = {
  id: string;
  lane: "task" | "phase_artifact";
  artifactKind: string;
  activeRevisionId: string;
  updatedByEventId: string;
  previousRevisionId?: string;
  createdAt: string;
};
```

Revision 不变量：

- `TaskSpecRevision`、`PhaseArtifactRevision`、`ArtifactRevision` 都是 append-only；修订只能新增 revision，不能覆盖旧 revision 内容。
- `parentId` 表示同一 artifact lineage 的上一版；没有 parent 的 revision 必须能追溯到初始 task / phase event。
- 每个 revision 必须绑定 `sourceEventId`，以便审计它来自用户输入、agent 产物、verification 结果还是 handoff。
- `TaskSpecRevision` 的 active 版本是 `task` lane 的 hard-preserve 输入；旧版本可进入 audit / compaction，但不能冒充当前任务。
- `PhaseArtifactRevision` 的 active 版本按当前 phase hard / soft 策略进入 `phase_artifact` lane；旧版本只作为历史 artifact / evidence 参与摘要。

`ActiveArtifactPointer` 有效性与 repair 规则：

- 指针必须指向存在、未 redacted 到不可用、且 lineage 与 `artifactKind` 匹配的 revision。
- 指针更新必须记录 `previousRevisionId` 和 `updatedByEventId`；缺少事件来源时进入 repair，不静默接受。
- 指针指向缺失 revision：block 当前 hard lane，尝试从 event log 重放；重放失败则请求 repair / pending input。
- 指针指向非最新但无冲突：可按 pointer 的审计顺序继续，但必须记录 stale-pointer marker。
- 两个 active pointer 指向同一 artifact 的不同 revision：按下方冲突矩阵处理，不把两版拼接为一个任务或阶段产物。

Active pointer 冲突矩阵：

| 冲突 | 处理 | 结果 |
| --- | --- | --- |
| `task` pointer vs newer user task revision | newer user revision 优先 | 更新 pointer；旧 pointer 进入 audit；如果用户 revision 缺 acceptance，则请求补齐而不是沿用旧 acceptance |
| `task` pointer vs agent-generated task rewrite | 用户来源优先 | agent rewrite 只能作为 proposal / phase artifact；不得覆盖 active task |
| `phase_artifact` pointer vs failed verification revision | verification 结果优先约束后续 phase | pointer 可指向失败结果；后续 patch / plan 必须看到失败原因 |
| `phase_artifact` pointer vs handoff draft conflict | 最新已审计 event 优先 | 保留冲突 marker；必要时 block handoff 直到选择 active draft |
| pointer revision missing / hash mismatch | event log replay 优先 | replay 成功则修复 pointer；失败则进入 repair blocker |

### 3.4 `CompactionRecord` 最小不变量

`CompactionRecord` 是 append-only 压缩链记录。最低必须包含：

```ts
type CompactionRecord = {
  id: string;
  parentId?: string;
  sessionId: string;
  runId: string;
  inputRange: {
    messageRefStart?: string;
    messageRefEnd?: string;
    evidenceRefStart?: string;
    evidenceRefEnd?: string;
    ledgerEntryIds: string[];
  };
  outputSummary: string;
  retainedLanes: ContextLane[];
  discardedLanes: ContextLane[];
  redactedMarkers: string[];
  omittedMarkers: string[];
  toolOutputRefs: ToolOutputRef[];
  budgetDecision: ContextBudgetDecision;
  promptId: string;
  promptVersion: string;
  createdAt: string;
};
```

不变量：

- `id` 全局唯一；`parentId` 指向上一条 compaction record，形成可验证链。
- `inputRange` 必须能定位被压缩的 message / evidence / ledger 范围。
- `outputSummary` 必须声明其来源范围，不得引入范围外事实。
- `retainedLanes` 与 `discardedLanes` 必须显式记录；hard lane 被丢弃时必须进入 blocker 或 repair 状态。
- redacted / omitted 内容必须以 marker 进入记录，不能在 summary 中伪装为已知内容。
- `toolOutputRefs` 必须覆盖压缩过程中被外部化的大型工具输出。
- `budgetDecision` 和 prompt id/version 必须能复现当时为什么压缩。

### 3.5 `ToolOutputRef`、`EvidenceRef` 与降级语义

`ToolOutputRef` 表示大型或敏感 tool output 的外部化引用；`Evidence` 表示可被 handoff、verification 或后续 runtime 审计引用的证据。二者可以互相引用，但职责不同。

```ts
type ToolOutputRef = {
  id: string;
  toolCallId: string;
  evidenceId?: string;
  storage: "event_log" | "local_blob" | "artifact_file" | "external_store";
  uri: string;
  sha256?: string;
  byteLength?: number;
  mimeType?: string;
  summary: string;
  truncated: boolean;
  redaction: "none" | "redacted" | "permission_required" | "unavailable";
  createdAt: string;
};

type EvidenceRef = {
  id: string;
  evidenceId: string;
  sourceRef: string;
  summary: string;
  freshness: "active" | "stale" | "invalidated" | "unavailable";
  degradation: "none" | "summary_only" | "ref_only" | "blocked";
  createdAt: string;
};
```

边界规则：

- 存储选择：`v0.1` 可继续用 event log / session snapshot 摘要；近期实现可将大型输出写入本地 blob 或 artifact file。无论存储在哪里，prompt 只保留 summary、ref id、truncated marker 和 redaction marker。
- missing / invalid ref：不得从旧 summary 反推原始输出；projection 必须标记 `unavailable`，并根据该 lane 是否 hard-preserve 决定 block、降级或请求重新运行安全 tool。
- permission / read failure：如果引用存在但读取权限失败，prompt 只能展示 ref、失败原因和 `permission_required` marker；不能泄漏路径外内容，也不能把读取失败内容当作已验证 evidence。
- redaction recovery：redacted output 的原文不能通过 compaction summary 恢复。恢复时只能使用 redacted marker、允许公开的摘要和可重新获取的非敏感 evidence；如任务依赖原文，必须进入 pending input 或 repair。
- `Evidence` boundary：`Evidence` 记录证明、验证和审计含义；`ToolOutputRef` 记录原始或大型输出位置。`Evidence.summary` 可引用 `ToolOutputRef.id`，但不能假设该引用永远可读。
- `EvidenceRef` degradation：active evidence 可作为 hard context；stale evidence 只能以 `summary_only` 或 `ref_only` 进入 prompt；invalidated / unavailable evidence 若支撑 active acceptance 或 verification，必须 `blocked`，否则可降级并记录 marker。
- block / degrade：hard lane 依赖不可读 ref 时 block；soft lane 依赖不可读 ref 时可 degrade 为 summary / marker。任何 degrade 都必须进入 projection metadata 和 `CompactionRecord` / recovery 输出。

## 4. Lane-aware 上下文模型

近期上下文使用 10 个 lanes。每个 lane 有 hard / soft 保留策略、预算和降级方式。

| Lane | 内容 | 默认策略 |
| --- | --- | --- |
| `control` | system / developer / policy / permission gate 约束、prompt contract | hard-preserve；超预算时 block |
| `task` | 用户目标、scope、acceptance、non-goals、constraints、blockers | hard-preserve；不能被普通压缩删除 |
| `runtime_baseline` | `AGENTS.md` snapshot/hash、config 摘要、workspace boundary、declared commands | hard-preserve 摘要；原文可外部化 |
| `phase_artifact` | `ContextPack`、`ChangePlan`、`PatchSummary`、`VerificationResult`、`AgentHandoff` 草稿 | 按当前 phase 优先保留 |
| `evidence` | evidence summary、verification refs、diff refs、file snapshot refs | hard for active acceptance / verification；其余可摘要 |
| `tool_output` | read/search/shell/MCP 等原始或大型输出 | 默认外部化为 `ToolOutputRef`，prompt 只保留摘要和 marker |
| `working_set` | 当前修改文件、相关片段、打开问题、活跃 hypotheses | soft-preserve；确定无关后可降级 |
| `recent_tail` | 最近若干轮 user/assistant/tool 交互 | soft-preserve；先确定性裁剪旧 tail |
| `compaction` | compaction chain 摘要、omitted/redacted markers | hard metadata；summary 可分层压缩 |
| `resume` | pending input、resume marker、未完成 permission/question、恢复状态 | hard-preserve；与 event log 冲突时按恢复优先级处理 |

### 4.1 映射到当前正式 baseline lanes

当前正式 `Code Agent Loop` baseline 可简化为四类：Task / Artifact / Evidence / Recent loop。10-lane 模型向它们映射如下：

| 近期 lane | 当前 baseline lane | 说明 |
| --- | --- | --- |
| `control` | Task | 约束 agent 行为的控制信息参与任务约束；权限状态在 resume 中另行 hard-preserve |
| `task` | Task | 直接对应用户目标、scope、acceptance 和 non-goals |
| `runtime_baseline` | Task | 作为执行约束和 workspace baseline 注入，不是普通 recent transcript |
| `phase_artifact` | Artifact | 对应 `ContextPack`、`ChangePlan`、`PatchSummary`、`VerificationResult`、`AgentHandoff` |
| `evidence` | Evidence | 对应 tool invocation、diff、verification、file snapshot、handoff refs |
| `tool_output` | Evidence | 原始输出外部化；可被 evidence 引用，但 prompt 中只保留摘要 / ref |
| `working_set` | Artifact | 当前活跃文件、片段和 hypothesis 是阶段产物的工作子集 |
| `recent_tail` | Recent loop | 对应当前 `recentStepCount` 保留的最近交互 |
| `compaction` | Evidence | 记录 context transformation 的审计 evidence，而不是任务事实 |
| `resume` | Task | 恢复入口和 pending input 约束下一步执行，必须优先于普通摘要 |

## 5. Prompt rendering / cache envelope

Stable Prefix / Append-only Ledger / Dynamic Suffix 是 prompt rendering envelope，用于描述 provider prompt cache 的稳定边界和增量渲染边界。它不改变 lane 语义：projection 仍先按 10-lane `ContextLane` 选择、裁剪和降级材料，再把 lane 内容渲染到三个 segment。

| Segment | 来源 lanes | Cache 语义 | 约束 |
| --- | --- | --- | --- |
| Stable Prefix | 稳定 system / developer policy、固定 prompt contract、允许 cache 的静态说明 | 可生成 `StablePrefixCacheKey` 并附加 provider cache hint | 只能包含通过 policy/dataBoundary/secret-redaction 检查且 retention 允许 provider-side cache 的材料 |
| Append-only Ledger | `task` active revision、`runtime_baseline` revision、`compaction` metadata、active `evidence` refs、phase artifact audit trail | 不作为可变 prefix 覆盖；按 append-only 顺序渲染，可局部形成 cacheable chunks | revision / pointer / compaction record 必须可审计；新增内容只追加，不重写历史 |
| Dynamic Suffix | 当前 turn 指令、recent tail、active working set、tool-call schema、provider-specific suffix | 默认不 cache 或短期 cache | 可随 turn 变化；不得承载唯一状态来源 |

```ts
type PromptRenderSegment = {
  kind: "stable_prefix" | "append_only_ledger" | "dynamic_suffix";
  laneIds: ContextLane[];
  contentRef: string;
  tokenEstimate?: number;
  byteEstimate?: number;
  cacheEligibility: "eligible" | "ineligible" | "redacted" | "boundary_restricted";
};

type StablePrefixCacheKey = {
  promptId: string;
  promptVersion: string;
  modelProvider: string;
  modelId: string;
  stablePrefixHash: string;
  policyRevisionId: string;
  dataBoundaryRevisionId: string;
  secretRedactionRevisionId: string;
  baselineRevisionIds: string[];
};
```

Cache contract：

- provider prompt cache 只是性能优化；cache hit / miss / eviction 只允许影响 cache-control metadata、latency 和 cost metadata，不得影响 `ContextProjection` 的语义内容、恢复路径、evidence 可见性或 budget gate 决策。
- cached prefix 仍计入 provider context window；预算计算必须按完整 prompt 估算，不得因为 provider cache hit 而把 prefix 从 token / byte budget 中移除。
- `StablePrefixCacheKey` 的任何组成项变化都必须使旧 key 失效：prompt version、provider/model、stable prefix hash、policy revision、data boundary revision、secret redaction revision、baseline revision 任一变化都触发 re-render。
- cache eligibility 由 policy、data boundary、retention 和 secret-redaction 共同决定；材料即使稳定，也可能因为用户数据、机密、路径边界或 provider retention 策略而不可 provider-side cache。
- cache invalidation record 必须可审计，记录 invalidating event、旧 key、新 key、re-render decision 和是否允许 provider cache hint。

### 5.1 True stable prefix 与 conditional baseline

True stable prefix 只包含在一个 session / run 范围内不会随 workspace 观察变化而改变、且允许 provider-side cache 的材料。`AGENTS.md`、config、workspace boundary、declared commands、skills、MCP server 状态属于 conditional baseline：它们可能在文件、环境、权限或工具注册变化时失效，不能被当成无条件稳定 prefix。

```ts
type BaselineRevisionRecord = {
  id: string;
  source: "AGENTS" | "config" | "workspace" | "commands" | "skills" | "mcp";
  observedHash: string;
  revisionId: string;
  invalidatingEventId?: string;
  reRenderDecision: "reuse" | "rerender" | "block_for_recheck";
  createdAt: string;
};
```

Baseline revision / audit 规则：

- 每个 conditional baseline 都必须记录 `source`、`observedHash`、`revisionId`、可选 `invalidatingEventId` 和 `reRenderDecision`。
- baseline hash 未变化且 policy/data boundary 未变化时可 `reuse`；hash 变化必须 `rerender`；缺少读取权限或边界不确定时 `block_for_recheck`。
- conditional baseline 可进入 Append-only Ledger 或 cacheable chunk，但只有通过 cache eligibility 检查的内容才能进入 Stable Prefix cache hint。
- baseline 审计记录是恢复和解释依据；provider cache 不是 baseline 状态来源。

## 6. Budget / reserve 算法

预算算法必须先确定性裁剪，再使用模型 summary / checkpoint。原因是确定性裁剪可复现，而模型 summary 会引入解释层。

建议流程：

1. 读取 provider / config 的 prompt budget。当前 `v0.1` 使用 byte estimate；近期继续保留 byte fallback；长期引入 provider token-aware window。即使 stable prefix cache 命中，也按完整 prefix + ledger + suffix 计算 provider context window。
2. 预留 reserve：response reserve、tool-call schema reserve、permission / resume reserve、emergency marker reserve。
3. 组装 hard lanes：`control`、`task`、active `resume`、当前 phase 必需 artifact、active acceptance evidence、compaction metadata。
4. 如果 hard lanes 已超预算：不得丢弃 hard lane；返回 `context_budget_gate` blocker，并报告超预算 lane 与建议修复。
5. 对 soft lanes 做确定性裁剪：优先裁剪 `recent_tail` 旧项、`tool_output` 原文、低相关 `working_set`、过期 phase artifacts。
6. 将大型 `tool_output` 外部化为 `ToolOutputRef`；prompt 保留摘要、ref id、byte/token estimate、truncated marker。
7. 仍超预算时，生成 model summary / checkpoint，并写入 `CompactionRecord`；summary 输入范围和输出必须可审计。
8. 再次估算；如 byte/token mismatch 或 provider cache policy 拒绝导致 provider 拒绝 prompt，则进入 provider-aware fallback：记录 provider error、禁用该次 cache hint、缩小 soft lanes、提高 reserve、必要时 block，而不是删除 hard lanes。

`ContextBudgetDecision` 至少应记录：budget source、max estimate、reserved estimate、used estimate、hard-lane estimate、裁剪步骤、是否触发 model summary、fallback 原因、provider token rejection / cache-control rejection metadata。cache 状态不得作为允许超预算的理由。

## 7. Resume / recovery 重建

恢复时按以下材料重建 context：

```text
session metadata
  + append-only event log
  + latest context snapshot
  + ContextLedger entries
  + CompactionRecord chain
  + retained recent tail
  + pending input / resume marker
  -> reconstructed ContextLedger
  -> next-turn ContextProjection
```

冲突优先级：

| 冲突 | 优先级 | 处理 |
| --- | --- | --- |
| event log vs snapshot | event log 优先 | snapshot 是 checkpoint；event log 是状态转换来源。snapshot 缺失事件时从 event log replay；snapshot 有但 event log 不支持的状态进入 repair |
| ledger vs retained tail | ledger 优先 | retained tail 是 prompt convenience。若 tail 与 ledger entry / compaction chain 不一致，丢弃 tail 并从 ledger 重建 |
| pending permission vs compacted decision | pending permission 优先 | compaction summary 不能替代用户批准 / 拒绝。存在未决 permission/question 时，resume lane hard-preserve 并等待输入 |
| `ToolOutputRef` summary vs ref read result | readable ref 优先；不可读时 marker 优先 | ref 可读且 hash 匹配时可重新摘要；缺失 / hash 不匹配时标记 unavailable，不从 summary 猜测原文 |
| `EvidenceRef` active vs stale / invalidated | freshness 优先 | active evidence 可进入 hard context；stale 降级为 summary/ref marker；invalidated 且支撑 active acceptance 时 block |
| active pointer vs revision chain | 指针审计完整性优先 | 指针必须指向可追溯 revision；缺失、hash mismatch 或多 pointer 冲突时按 active pointer 冲突矩阵 repair |
| compaction parent chain vs latest record | parent chain 完整性优先 | latest record 找不到 parent 或输入范围损坏时进入 damaged-chain repair，不继续无审计压缩 |

恢复输出必须包含：重建成功 / 降级 / repair 状态、不可用 refs、损坏 compaction record、被保留的 pending input。

## 8. 与现有模块集成

| 模块 | 近期集成方式 |
| --- | --- |
| `AgentLoop` | 在每次 model call 前请求 `ContextProjection`；在 tool result、phase artifact、permission、handoff 时追加 ledger entry；上下文超预算时返回 canonical `PendingInput` 或 blocker |
| `PromptRegistry` | 接收 projection lanes、render segments、budget decision、prompt id/version、cache eligibility；prompt 中显式渲染 summary/ref/truncation/redaction markers，并只对 eligible stable prefix 附加 provider cache hints |
| `Evidence` | 继续绑定 tool invocation、verification、diff、handoff；新增与 `ToolOutputRef`、`CompactionRecord` 的引用关系 |
| `Session` | 保存 context snapshot、event log、pending input、compaction chain head；resume 时用冲突优先级重建 |
| future `StateStore` | 长期从 `Evidence` / artifact 中晋升 `Fact`；projection 读取 fact/evidence freshness，而不是读取 transcript summary |
| future `ContextProjection` runtime module | 将本文近期 lane model 收敛为 runtime evolution 中带来源、预算、遗漏和信任边界的正式 projection |

## 9. 测试策略

测试不能只覆盖 overflow。最小测试矩阵：

| 场景 | 断言 |
| --- | --- |
| hard-lane preservation | task / acceptance / control / pending input / active evidence 在压缩后仍存在；超预算时 block 而不是删除 |
| missing `ToolOutputRef` | projection 标记 unavailable；不会从旧 summary 猜测原始输出；根据 lane 策略 block 或降级 |
| invalid / unreadable `ToolOutputRef` | hash mismatch、权限失败、路径不可读分别生成 marker；敏感内容不进入 prompt |
| damaged compaction chain | 缺 parent、输入范围损坏、record 顺序断裂时进入 repair；不继续生成无审计 summary |
| hard lane over budget | 返回 `context_budget_gate` blocker，列出超预算 lane 和 reserve，不丢弃 hard lane |
| reproducible projection after resume | event log + snapshot + ledger + compaction chain 重建出的 projection 与中断前关键 lanes 一致 |
| byte/token mismatch fallback | byte estimate 通过但 provider token 拒绝时，触发 fallback prune / reserve；hard lane 仍保留 |
| provider cache rejection fallback | provider 拒绝 cache-control / retention hint 时，禁用本次 cache hint 并重渲染；projection 语义不变 |
| cache semantic equivalence | cache disabled、miss、evicted、hit 四种路径生成相同语义 `ContextProjection`；只有 cache-control、cost、latency metadata 可不同 |
| cache eligibility boundary | policy、dataBoundary、secret redaction、retention 任一不允许时，stable material 不生成 provider-side cache hint；敏感内容不进入 cached prefix |
| conditional baseline audit | `AGENTS` / config / workspace / commands / skills / MCP 变化记录 source、observed hash、revision id、invalidating event id、re-render decision |
| active pointer conflict | `task` / `phase_artifact` pointer 冲突按矩阵 repair 或 block；不拼接冲突 revision |
| deterministic pruning order | 相同 ledger 和 budget 产生相同裁剪顺序、omitted ids 和 budget decision |
| model summary audit | 触发 model summary 时写入 `CompactionRecord`，包含输入范围、prompt version、retained/discarded lanes |
| redaction recovery | redacted marker 可恢复，原文不可从 summary 还原；依赖原文时进入 pending input / repair |

## 10. 分阶段子路线图

| 阶段 | 目标 | 验收 |
| --- | --- | --- |
| v0.1 hardening | 保持现有 byte-estimate compaction；明确 hard lanes；tool result 摘要包含 truncation marker；测试 hard-lane preservation 和 overflow blocker | 不改变公开 source contract；补齐 context budget 单元 / 集成测试 |
| v0.2 ledger slice | 引入最小 `ContextLedger` entry、`ToolOutputRef`、`EvidenceRef`、task / phase artifact revision 与 active pointer；大型输出外部化；PromptRegistry 渲染 ref / marker | missing ref、permission failure、deterministic pruning、active pointer conflict 测试通过 |
| v0.3 audit compaction | 引入 append-only `CompactionRecord` chain 与 conditional baseline audit records；压缩摘要带输入范围、retained/discarded lanes、prompt version | damaged chain、model summary audit、baseline invalidation、resume projection reproducibility 测试通过 |
| v0.4 cache-aware rendering | 引入 Stable Prefix / Append-only Ledger / Dynamic Suffix envelope、`StablePrefixCacheKey`、cache eligibility 和 cache semantic equivalence tests | cache disabled/miss/evicted/hit 语义一致；provider cache rejection fallback 不改变 projection |
| v0.5 runtime alignment | 与 `StateStore` / `Fact` graph / token-aware provider windows 对齐，收敛到长期 `ContextProjection` module | provider token fallback、fact/evidence provenance、policy trace 可审计 |

## 11. 非目标

- 不在当前设计中实现 long-term memory 或跨设备同步。
- 不把模型生成 summary 晋升为 `Fact`。
- 不把 provider prompt cache、cache key 或 cache hit 结果当成 Lattecode 状态、事实、权限或恢复来源。
- 不通过 context compaction 绕过 permission、path boundary、redaction 或 evidence freshness。
- 不要求 `v0.1` 立即引入完整 `StateStore`、`ActionGraph` 或 token-aware provider SDK 适配。
