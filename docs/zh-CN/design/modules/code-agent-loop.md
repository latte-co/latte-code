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
- 能在目标仓库中读取、搜索、编辑、创建文件。
- 能对写入和 shell 执行做 gate。
- 能运行仓库已声明的验证命令。
- 能把结果交付为 `AgentHandoff`，包括 changed files、verification、risks、blockers。

如果目标仓库没有应用框架、测试框架或依赖决策，Fluxcode 必须明确阻塞并请求用户确认，不能静默 scaffold、安装依赖或发布。

## 2. Basic Code Agent 最小功能集

| 能力 | v0.1 最小要求 | 不做什么 |
| --- | --- | --- |
| CLI entry | `fluxcode run <task>` 启动真实 agent loop，输出 JSON / text handoff | 不做 TUI / IDE cockpit |
| Session / TaskRun | 创建可恢复 `TaskRunState`，保存 phase、steps、artifacts、tool calls、events | 不做多会话协同 |
| Model loop | 支持真实 `ModelClient` + `FakeModelClient` 测试；phase 内部允许 ReAct | 不把 provider SDK 泄漏到 core loop |
| Provider abstraction | 先支持 `fake` 与 `openai-compatible` | 不做完整 provider catalog |
| Tool contract | 每个 tool 有 schema、mutating、risk、permission、summary、references | 不允许裸函数工具 |
| Read/search | 支持目录、文件读取、文本搜索，输出可截断摘要和 references | 不做大型索引系统 |
| Edit/write | 支持 `edit_file` 严格 old/new 局部替换和受控 `write_file` 创建新文件 | 不鼓励整文件无差别重写 |
| Shell verify | 仅运行声明命令、配置 allowlist 或用户确认命令 | 不开放任意 shell |
| Prompt registry | system prompt + phase prompt 有 id/version/schema | 不把 prompt 散落在代码字符串中 |
| Context budget | 使用 `TaskSpec`、artifacts、evidence summary、recent steps 构造 prompt | 不无限拼 transcript |
| Evidence/event | 每次 tool call、permission decision、phase artifact 写入事件或 evidence | 不只靠自然语言总结 |
| Handoff | 输出 changed files、verification、risks、blockers、next steps | 不把失败包装成成功 |
| Recovery | append-only event log + session snapshot 可恢复运行状态 | 不做复杂 graph recovery |

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

## 4. 数据契约

```ts
type TaskRunState = {
  id: string;
  sessionId: string;
  currentPhase: AgentPhase;
  task?: TaskSpec;
  context?: ContextPack;
  plan?: ChangePlan;
  patch?: PatchSummary;
  verification: VerificationResult[];
  handoff?: AgentHandoff;
  steps: StepTrace[];
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
```

关键 artifact：

| Artifact | 用途 |
| --- | --- |
| `TaskSpec` | 用户目标、范围、验收、非目标、约束 |
| `ContextPack` | 已读取文件、相关片段、命令来源、开放问题 |
| `ChangePlan` | 修改文件、步骤、验证命令、风险 |
| `PatchSummary` | changed files、diff refs、修改理由 |
| `VerificationResult` | command、status、exit code、summary、output refs |
| `AgentHandoff` | 最终交付摘要、验证、风险、阻塞、下一步 |

## 5. Gate 完整列表

| Gate | 触发点 | 通过条件 | 失败语义 | 验收方式 |
| --- | --- | --- | --- | --- |
| `provider_gate` | run 启动 | default provider 存在、API key 可解析、模型支持 tools | fail fast | 缺 env 时 CLI 返回清晰错误 |
| `config_gate` | config load | schemaVersion、models、tools、permissions、context 合法 | fail fast | 无效配置单元测试 |
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
| `verification_gate` | `Verify` 完成 | 声明验证已运行并记录结果，或明确 skipped 原因 | failed / skipped handoff | 测试失败时 status 非 completed-success |
| `handoff_gate` | 输出前 | changed files、verification、risks、blockers 完整 | fail internal | handoff snapshot 测试 |
| `recovery_gate` | resume 前 | event log 与 snapshot 可重建，无悬挂 tool call | repair / fail | interrupted tool call resume 测试 |

这些 gates 是 `v0.1` 必须验收的最小集合。它们不等价于完整 OS sandbox；如果没有平台级隔离，文档和 CLI 输出不能声称具备 sandbox。

## 6. 测试策略

### 6.1 单元测试

| 范围 | 测试内容 |
| --- | --- |
| Config | merge、env placeholder、invalid provider、shell allowlist |
| Provider | fake、openai-compatible request mapping、missing API key、tool call parse error |
| Prompt | 每个 phase prompt 有 id/version/schema，包含必要边界 |
| Context | compaction 保留 task / acceptance / blockers / verification |
| Tool schema | invalid tool name、invalid args、output truncation |
| Permission | allow / ask / deny、mutating、high risk、deny globs |
| Edit | unique replace、多匹配、stale file、line ending 保留 |
| Shell | allowlist、timeout、output cap、blocked install/delete/network/git-write |
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
- 失败、阻塞、跳过验证都不会被报告为成功。
