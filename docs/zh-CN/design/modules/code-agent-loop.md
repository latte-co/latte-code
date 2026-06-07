# 模块技术设计：Code Agent Loop

## 文档状态

本文定义 Fluxcode `v0.1` 的基础 code agent 循环。它基于对 Claude Code、CodeWhale、Codex、opencode 四类 conversation-native code agent 的临时源码调研结论，但不把 `.tmp/` 下源码当作 Fluxcode 正式源码或构建依据。

英文对应文档：[`docs/en-US/design/modules/code-agent-loop.md`](../../../en-US/design/modules/code-agent-loop.md)。

## 1. 结论

生产级可用的 basic code agent 最小形态不是纯状态机，也不是裸 ReAct。它应是：

```text
Persistent TaskRun
  + phase-gated ReAct
  + typed tool contract
  + permission / path / edit / shell / verification gates
  + append-only event log
  + structured handoff
```

外层 phase runner 只负责工程边界：阶段顺序、预算、权限、结构化产物校验、持久化和恢复。内层仍保留 ReAct query loop：模型可以多轮调用工具、观察结果、修正计划和继续执行。

因此，`fluxcode run "实现一个贪吃蛇游戏"` 要真正可运行，至少需要：

- 能接入真实模型 provider，而不是只返回 fake model 默认消息。
- 能通过 CLI、local command、local skill、minimal MCP bridge 和 built-in tools 统一生成 `TaskSpec`。
- 能加载 config、`AGENTS.md`、session snapshot 和 pinned constraints，形成可复用 `ContextPack`。
- 能在目标仓库中读取、搜索、编辑、创建文件。
- 能对写入和 shell 执行做 gate。
- 能运行仓库已声明的验证命令。
- 能把结果交付为 `AgentHandoff`，包括 changed files、verification、risks、blockers。

如果目标仓库没有应用框架、测试框架或依赖决策，Fluxcode 必须明确阻塞并请求用户确认，不能静默 scaffold、安装依赖或发布。

## 2. Basic Code Agent 最小功能集

| 能力 | v0.1 最小要求 | 不做什么 |
| --- | --- | --- |
| CLI entry | `fluxcode run <task>` 启动真实 agent loop，输出 JSON / text handoff | 不做 TUI / IDE cockpit |
| Config | 加载 project-local JSONC config，覆盖 models、runtime、tools、permissions、session、commands、skills、MCP | 不做 secrets 存储或全局策略平台 |
| `AGENTS.md` loader | 读取 repo root / cwd 边界内的 `AGENTS.md`，记录 snapshot/hash 并注入 context | 不把未追踪文本直接拼入 prompt |
| Session / TaskRun | 创建可恢复 `TaskRunState`，保存 phase、status、steps、artifacts、tool calls、events、context snapshot | 不做 cloud sync、多设备或多用户协作 |
| Model loop | 支持真实 `ModelClient` + `FakeModelClient` 测试；phase 内部允许 ReAct | 不把 provider SDK 泄漏到 core loop |
| Provider abstraction | 先支持 `fake` 与 `openai-compatible` | 不做完整 provider catalog |
| Tool contract | 每个 tool 有 schema、mutating、risk、permission、summary、references | 不允许裸函数工具 |
| Built-in tools | 提供最小 read/search/edit/write/shell/manifest/diff 能力 | 不做大型工具生态或插件市场 |
| Minimal MCP bridge | 从 config-defined servers list/call tools，统一走 permission、evidence、trace、session | 默认 disabled 或 explicit enabled；不做 marketplace、resource / prompt platform、server management UI |
| Local skills | 加载本地 instruction / workflow / command bundle，注入 context / prompt registry | 不做 hub、install、publish、marketplace；skill 不能直接执行 side effects |
| Local commands | 读取 built-in / local command specs，将命令转成 `TaskSpec` / phase event | command 不绕过 agent-loop、permission 或 session |
| Read/search | 支持目录、文件读取、文本搜索，输出可截断摘要和 references | 不做大型索引系统 |
| Edit/write | 支持 `edit_file` 严格 old/new 局部替换和受控 `write_file` 创建新文件 | 不鼓励整文件无差别重写 |
| Shell verify | 仅运行声明命令、配置 allowlist 或用户确认命令 | 不开放任意 shell |
| Prompt registry | system prompt + phase prompt 有 id/version/schema | 不把 prompt 散落在代码字符串中 |
| Context budget | 使用 `TaskSpec`、artifacts、evidence summary、recent steps 构造 prompt | 不无限拼 transcript |
| Evidence/event | 每次 tool call、permission decision、phase artifact 写入事件或 evidence | 不只靠自然语言总结 |
| Handoff | 输出 changed files、verification、risks、blockers、next steps | 不把失败包装成成功 |
| Recovery | append-only event log + session snapshot 可恢复运行状态 | 不做复杂 graph recovery |

`v0.1` 主链路固定为：

```text
CLI / local command / local skill / minimal MCP bridge / built-in tools
  -> TaskSpec
  -> Session / TaskRunState
  -> ContextPack
  -> AgentLoop / PhaseRunner
  -> PermissionDecision
  -> Evidence / StepTrace
  -> AgentHandoff
```

MCP、skill 和 command 的最小边界如下：

- MCP：只支持 config-defined servers、list tools 和 call tool。MCP tool 必须转换为 Fluxcode tool contract，并统一走 permission、evidence、trace 和 session；默认 disabled 或 explicit enabled；不能绕过权限；不做 marketplace、resource / prompt platform 或 server management UI。
- Skill：只支持 local skill loader。skill 是 instruction / workflow / command bundle，可注入 context / prompt registry；不能直接执行 side effects；不做 hub、install、publish 或 marketplace。
- Command：只支持 built-in / local command specs。command 必须 route through `TaskSpec`、phase event 和 session system；不能直接调用 tool 绕过 agent-loop、permission 或 session。

### 2.1 TUI / output boundary

本文重新建立 `v0.1` 的 TUI / output decisions。`v0.1` release-critical path 仍是 headless JSON / text `AgentHandoff`；正式 TUI、IDE cockpit 或 full-screen cockpit 不进入 `v0.1` release acceptance。

输出边界如下：

- `--output json` 和 `--output text` 必须保留，且自动化、CI、non-TTY 和重定向场景不能依赖 TUI。
- `--ui tui` 或独立 experimental command 只能是 opt-in；没有显式选择时，CLI 应维持 headless handoff 行为。
- TUI 只消费 runtime events 和 `AgentHandoff` / view model，不驱动 permission、session、runtime mutation，也不改变 schema、evidence、trace 或 handoff 语义。
- runtime core 不 import `react`、`ink` 或 `@opentui/*`；UI dependency 必须留在 renderer adapter 或 experimental package 边界内。
- `PlainTextRenderer` 是 non-TTY、CI、snapshot、crash fallback 的默认可用路径；Fluxcode 不自研完整 terminal renderer。

Renderer 边界采用稳定 view model：

```text
runtime event stream
  -> stable TuiViewModel
  -> InkRenderer | OpenTuiRenderer | PlainTextRenderer
```

`InkRenderer` 可以作为 `v0.2` / `v0.3` 的 experimental PoC 方向；如果选定的 Ink 路径要求 Node `>=22`，必须有 Node gate，也可以采用 `Ink v6` pin、optional / lazy import 或独立 experimental package。`OpenTuiRenderer` 只作为 `v0.4+` adapter evaluation gate；只有当 `ActionGraph` 在 `v0.5+` 成为实际 UX surface 后，才考虑 cockpit hardening candidate。主 runtime 不引入 Bun / Zig / native build chain。

## 3. Loop 形态

`v0.1` 采用 phase-gated ReAct：

```text
Intake
  -> Understand
  -> Plan
  -> Edit
  -> Verify
  -> Handoff
```

每个 phase 是一个 contract：

```ts
type AgentPhase = "intake" | "understand" | "plan" | "edit" | "verify" | "handoff";

type PhaseContract<Output> = {
  phase: AgentPhase;
  allowedTools: string[];
  maxReactSteps: number;
  outputSchemaName: string;
  validateOutput(value: unknown): Output;
  next(output: Output, run: TaskRunState): AgentPhase | "completed" | "blocked";
};
```

phase 内部仍然是 ReAct：

```text
build phase prompt
  -> model.generate
  -> tool calls
  -> permission / schema / path / shell gates
  -> tool results
  -> next model turn
  -> structured phase output
```

phase 完成条件不是“模型说完成了”，而是对应 artifact 通过 schema 校验。

| Phase | 允许工具 | 必须产物 | 失败方式 |
| --- | --- | --- | --- |
| `Intake` | 无或只读配置 | `TaskSpec` | 目标 / 范围不清则 ask user |
| `Understand` | `list_directory`、`read_file`、`search`、`read_project_manifest` | `ContextPack` | 找不到上下文则 block / ask |
| `Plan` | read-only tools | `ChangePlan` | 需要 scaffold / dependency 决策则 ask |
| `Edit` | `read_file`、`edit_file`、`write_file`、可选 `apply_patch` | `PatchSummary` | edit gate 不通过则 block |
| `Verify` | `shell_exec`、read-only tools | `VerificationResult[]` | 验证失败则 failed handoff，不得伪成功 |
| `Handoff` | 无或 `git_diff` | `AgentHandoff` | 必须列出未验证和阻塞 |

TUI 不能作为任何 phase 的完成条件。phase artifact 仍以 schema 校验为准，renderer 只能展示 `StepTrace`、runtime events、permission prompt 状态和最终 `AgentHandoff`。

## 4. 数据契约

```ts
type TaskRunState = {
  id: string;
  sessionId: string;
  status: "queued" | "running" | "waiting_permission" | "blocked" | "failed" | "completed";
  currentPhase: AgentPhase;
  task?: TaskSpec;
  context?: ContextPack;
  plan?: ChangePlan;
  patch?: PatchSummary;
  verification: VerificationResult[];
  handoff?: AgentHandoff;
  steps: StepTrace[];
  resume?: ResumeMarker;
  contextSnapshot: ContextSnapshot;
};

type StepTrace = {
  id: string;
  phase: AgentPhase;
  status: "pending" | "running" | "done" | "blocked" | "failed";
  promptId: string;
  promptVersion: string;
  summary: string;
  toolCallIds: string[];
  evidenceIds: string[];
  reactBudget: { maxSteps: number; usedSteps: number };
  error?: string;
};

type ResumeMarker =
  | { type: "permission"; permissionId: string }
  | { type: "blocked"; questionId: string }
  | { type: "failed"; failedStepId: string }
  | { type: "completed"; handoffId: string };

type ContextSnapshot = {
  taskInput: string;
  messageRefs: string[];
  decisionRefs: string[];
  compactedSummary: string;
  pinnedConstraints: string[];
  agentsMd?: { path: string; hash: string; summary: string };
  skills?: { name: string; path: string; hash: string; summary: string }[];
  commands?: { name: string; path: string; hash: string; description: string }[];
  mcpTools?: { server: string; tool: string; toolName: string }[];
};

type PendingInput =
  | {
      kind: "permission";
      permissionId: string;
      toolCallId: string;
      phase: AgentPhase;
      action: "write_file" | "edit_file" | "shell_exec" | "mcp_call" | "external_path";
      reason: string;
      command?: string;
      path?: string;
      options: ["approve", "deny"];
    }
  | {
      kind: "question";
      questionId: string;
      phase: AgentPhase;
      prompt: string;
      expectedAnswer: "text" | "json";
      schemaName?: string;
    };

type ResumeInput =
  | { kind: "permission"; permissionId: string; decision: "approve" | "deny"; reason?: string }
  | { kind: "question"; questionId: string; answerText?: string; answerJson?: unknown };

type HeadlessRunEnvelope = {
  runId: string;
  sessionId: string;
  status: TaskRunState["status"];
  pendingInput?: PendingInput;
  handoff?: AgentHandoff;
};
```

`PendingInput`、`ResumeInput` 和 `HeadlessRunEnvelope` 是 `v0.1` headless run / resume 的 canonical contract。`waiting_permission` 必须携带 `pendingInput.kind = "permission"`，`blocked` 必须携带 `pendingInput.kind = "question"`。`AgentHandoff.blockers`、`requiredDecisions`、`TaskRunState.resume` 和 event log 必须复用同一个 `permissionId` / `questionId`，避免 CLI resume 和 handoff 出现双套 id。

旧 `AgentResult.status` / `SessionState.status` 只保留 compatibility 语义；其中旧 `denied` 不再是 canonical `TaskRunState.status`，在 headless envelope、`graph-ready` wrapper 和后续 run-state 中必须映射为 `blocked`，并通过 blocker / required decision 说明权限拒绝或外部决策缺口。

### 4.1 Session management 最小范围

`v0.1` 的 session management 是 local-first run recovery，不是长期协作或记忆系统。

| 范围 | 最小要求 |
| --- | --- |
| Lifecycle | 支持 create / list / show / resume；session id 稳定；cwd 和 repo root 在 session 创建时固定 |
| `TaskRunState.status` | 仅使用 `queued`、`running`、`waiting_permission`、`blocked`、`failed`、`completed` |
| Resume semantics | `waiting_permission -> approve/deny -> continue`；`blocked -> user input -> continue`；`failed -> repair/retry`；`completed -> follow-up/fork later` |
| Context snapshot | 保存 task input、messages、decisions、compacted summary、pinned constraints、`AGENTS.md` snapshot/hash |
| Trace/evidence binding | 绑定 `StepTrace`、`Evidence`、tool invocation、shell output summary、file edit summary、verification result |

明确非目标：不做 cloud sync、多设备、多用户协作、完整 branch / fork graph、long-term memory 或 full `ActionGraph` persistence。

关键 artifact：

| Artifact | 用途 |
| --- | --- |
| `TaskSpec` | 用户目标、范围、验收、非目标、约束 |
| `ContextPack` | 已读取文件、相关片段、命令来源、开放问题 |
| `ChangePlan` | 修改文件、步骤、验证命令、风险 |
| `PatchSummary` | changed files、diff refs、修改理由 |
| `VerificationResult` | command、status、exit code、summary、output refs |
| `AgentHandoff` | 最终交付摘要、验证、风险、阻塞、下一步 |

当前 `v0.1` 实现切片采用以下最小字段集合，后续版本可在不破坏 headless contract 的前提下扩展：

| Artifact | 最小字段 |
| --- | --- |
| `TaskSpec` | `objective`、`scope`、`acceptance`、`nonGoals`、`constraints`、`blockers` |
| `ContextPack` | `summary`、`filesRead`、`relevantSnippets`、`commandSources`、`openQuestions` |
| `ChangePlan` | `summary`、`targetFiles`、`steps`、`verificationCommands`、`risks` |
| `PatchSummary` | `changedFiles`、`diffRefs`、`rationale`、`evidenceRefs` |
| `VerificationResult` | `command`、`status`、`summary`、`evidenceRefs`，可选 `exitCode`、`outputRefs` |
| `AgentHandoff` | `id`、`status`、`summary`、`changedFiles`、`verification`、`risks`、`blockers`、`requiredDecisions`、`traceRefs`、`evidenceRefs` |

## 5. Gate 完整列表

| Gate | 触发点 | 通过条件 | 失败语义 | 验收方式 |
| --- | --- | --- | --- | --- |
| `provider_gate` | run 启动 | default provider 存在、API key 可解析、模型支持 tools | fail fast | 缺 env 时 CLI 返回清晰错误 |
| `config_gate` | config load | schemaVersion、models、tools、permissions、context 合法 | fail fast | 无效配置单元测试 |
| `agents_gate` | context 构造前 | `AGENTS.md` snapshot/hash 已记录，且路径在 repo root / cwd 边界内 | block / ignore with reason | 外部或不可读 `AGENTS.md` 不静默进入 prompt |
| `task_gate` | `Intake` 完成 | `TaskSpec.objective` 非空，范围和非目标明确 | ask user | 模糊任务触发 ask |
| `context_budget_gate` | 每次 model call 前 | prompt 未超预算，必保留 lanes 未丢失 | block / compact | 超长 tool output 被摘要，验收条件保留 |
| `tool_schema_gate` | tool 执行前 | tool name 存在，input 符合 schema | tool error，继续或 block | invalid JSON / invalid args 测试 |
| `permission_gate` | mutating / shell / external path 前 | allow / ask / deny 决策明确 | ask / deny / block | write 和 shell 默认 ask |
| `path_boundary_gate` | file tool 前 | path 在 workspace 或 trusted external paths | deny / ask | `../`、`.git`、`.env` 被阻止 |
| `read_before_write_gate` | edit/write 前 | 修改文件已读取或有明确 create intent | block / ask | 未读直接 edit 被拒绝 |
| `stale_write_gate` | edit/write 前 | file hash / mtime 与读取时一致 | block / reread | 并发修改触发 reread |
| `edit_match_gate` | `edit_file` 前 | oldText 唯一匹配，或显式 replaceAll | block | 多匹配要求更多上下文 |
| `diff_review_gate` | mutating edit/write 后、落盘前或后 | diff summary 可审查，高风险需确认 | ask / deny | diff 进入 permission metadata |
| `shell_command_gate` | `shell_exec` 前 | 命令来自 allowlist、manifest scripts 或用户确认 | ask / deny | install/delete/network/git-write 默认阻止 |
| `mcp_gate` | MCP tool list / call 前 | server 显式启用，tool 映射到 Fluxcode contract，并经过 permission | ask / deny / block | disabled MCP server 对模型不可见 |
| `skill_gate` | skill 加载 / 注入前 | skill 来自本地允许路径，只注入 instruction / workflow / command spec | deny / block | skill 不能直接执行 side effects |
| `command_gate` | local command 执行前 | command 转成 `TaskSpec` / phase event，并进入 session | ask / deny / block | command 不能直接绕过 loop 调 tool |
| `verification_gate` | `Verify` 完成 | 声明验证已运行并记录结果，或明确 skipped 原因 | failed / skipped handoff | 测试失败时 status 非 completed-success |
| `handoff_gate` | 输出前 | changed files、verification、risks、blockers 完整 | fail internal | handoff snapshot 测试 |
| `recovery_gate` | resume 前 | event log 与 snapshot 可重建，无悬挂 tool call | repair / fail | interrupted tool call resume 测试 |

这些 gates 是 `v0.1` 必须验收的最小集合。它们不等价于完整 OS sandbox；如果没有平台级隔离，文档和 CLI 输出不能声称具备 sandbox。

## 6. 测试策略

### 6.1 单元测试

| 范围 | 测试内容 |
| --- | --- |
| Config | merge、env placeholder、invalid provider、shell allowlist |
| `AGENTS.md` | snapshot/hash、repo boundary、missing / unreadable behavior |
| Provider | fake、openai-compatible request mapping、missing API key、tool call parse error |
| Session | create / list / show / resume、status transitions、cwd / repo root 固定 |
| Prompt | 每个 phase prompt 有 id/version/schema，包含必要边界 |
| Context | compaction 保留 task / acceptance / blockers / verification |
| Tool schema | invalid tool name、invalid args、output truncation |
| Permission | allow / ask / deny、mutating、high risk、deny globs |
| Edit | unique replace、多匹配、stale file、line ending 保留 |
| Shell | allowlist、timeout、output cap、blocked install/delete/network/git-write |
| MCP / Skill / Command | disabled-by-default MCP、skill no-side-effect、command routes through `TaskSpec` |
| Handoff | failed verification 不会被标成成功 |

### 6.2 集成测试

集成测试必须用真实文件系统临时仓库，不只测纯函数。

| 场景 | Fixture | 断言 |
| --- | --- | --- |
| happy path | 小型 TypeScript / web app fixture | 修改源码、补测试、`npm test` 通过、handoff 完整 |
| snake game | 已有 Vite / vanilla app fixture | 生成游戏文件、测试或 build 通过 |
| permission ask | mutating edit / shell | 返回 waiting_permission 或 blocked，不写文件 |
| verification fail | 测试故意失败 | handoff 标记 failed，保留失败摘要 |
| context overflow | 大工具输出 | 输出被摘要，关键约束不丢 |
| interrupted resume | tool call 后中断 | resume 后无悬挂 tool call，可继续或安全失败 |
| stale write | 读取后外部修改文件 | edit 被阻止或重新读取 |
| local command / skill / MCP | 命令、skill 或 MCP tool 触发任务 | 全部进入 `TaskSpec -> Session -> PhaseRunner`，并产生 permission / evidence / trace |

### 6.3 Provider 集成测试

`v0.1` 不应把真实模型测试作为普通 CI 必需项。分层如下：

- CI 必跑：`FakeModelClient` scripted integration。
- 可选本地 smoke：配置真实 provider 后跑一个小 fixture。
- 禁止在 CI 中依赖外部 API key，除非有专门 secret 和显式 opt-in。

## 7. 与后续 runtime 的演进关系

| v0.1 对象 | 后续对象 | 演进方式 |
| --- | --- | --- |
| `TaskRunState` | `ActionGraph` seed | phase / step 变成 node / edge |
| `StepTrace` | `ActionNode` | 增加 dependsOn、readSet、writeSet、effect refs |
| tool evidence | `Evidence` | 增加 coverage、freshness、artifact refs |
| permission decisions | `PolicyDecision` / gate records | 结构化进入 policy layer |
| edit / shell records | `EffectRecord` | 增加 expected / observed / compensation |
| failed handoff | `ReconcileDecision` | 增加 affected refs 和 repair action |

## 8. v0.1 Done Definition

- `fluxcode run "实现一个贪吃蛇游戏"` 在已有 web app fixture 中能产出真实代码变更。
- 至少一个 scripted fake-model 集成测试覆盖完整 loop。
- 至少一个真实 provider smoke test 可手动运行。
- 所有 P0 gates 有单元或集成测试。
- CLI 输出包含 `AgentHandoff`，并能定位 event log / evidence / changed files。
- session lifecycle、`AGENTS.md` snapshot、minimal MCP bridge、local skill loader 和 local command specs 都进入最小 contract-first 闭环。
- 失败、阻塞、跳过验证都不会被报告为成功。
