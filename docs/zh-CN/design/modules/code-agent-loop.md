# 模块技术设计：Code Agent Loop

## 文档状态

本文定义 Lattecode `v0.1` 面向本地代码仓库任务的最小 ReAct loop。设计优先级是先成为可用的 local code agent：读取仓库上下文，做受控文件修改，运行验证，并交付可审查结果。

英文对应文档：[`docs/en-US/design/modules/code-agent-loop.md`](../../../en-US/design/modules/code-agent-loop.md)。

## 1. 目标与非目标

`v0.1` 的目标是实现一个最小、可恢复、可验证的 code agent loop，足以完成本地仓库里的小型开发、修复和文档任务。

| 类别 | 内容 |
| --- | --- |
| 目标 | 接收任务输入，加载仓库指令和相关文件，执行 ReAct 工具循环，修改文件，运行验证，输出 handoff |
| 目标 | 对所有写入、shell、外部路径访问保留明确权限边界 |
| 目标 | 保留可恢复的 run record：消息、工具调用、工具结果、修改文件、验证结果、阻塞原因 |
| 非目标 | 不在 `v0.1` 前置完整 `ActionGraph`、高级 `Scheduler`、`Reconciler` 或完整 `runtime kernel` |
| 非目标 | 不做多智能体、插件市场、TUI / IDE cockpit、事务回滚、云端协作或长期记忆系统 |

如果任务需要新建项目脚手架、安装依赖、删除文件、提交代码、改变 git 状态或发布内容，agent 必须请求用户确认；不能静默执行。

## 2. 最小 Loop 形态

主链路固定为：

```text
TaskInput
  -> ContextPack
  -> ReAct turn loop
  -> Tool results / Changed files
  -> Verification
  -> Handoff
```

| 环节 | 责任 | 最小产物 |
| --- | --- | --- |
| `TaskInput` | 记录用户目标、范围、约束、验收标准 | `taskId`、原始输入、可选 acceptance |
| `ContextPack` | 汇总仓库指令、相关文件、最近工具结果、开放问题 | 文件引用、摘要、token 预算状态 |
| `ReAct turn loop` | 模型在“思考 -> 调工具 -> 观察结果”之间迭代 | 消息、工具调用、工具结果 |
| `Tool results / Changed files` | 持久化读写结果和变更摘要 | changed files、diff refs、permission refs |
| `Verification` | 运行声明的验证命令或记录不能验证的原因 | command、status、summary、evidence refs |
| `Handoff` | 输出可审查交付物 | summary、changed files、verification、risks、blockers |

ReAct turn 的最小步骤：

1. 用 `TaskInput`、`ContextPack`、最近工具结果构造模型输入。
2. 模型返回普通消息或工具调用。
3. runtime 校验工具 schema、路径边界、权限和预算。
4. 执行工具，记录结果、摘要和 evidence reference。
5. 更新 loop state；如果需要权限或用户输入，进入等待状态。
6. 当模型认为修改完成时，必须进入验证和 handoff；不能只靠自然语言结束任务。

## 3. Loop State 与记录

`v0.1` 不需要复杂图结构，但必须有一个可恢复的 `TaskRunState`。

```ts
type TaskRunStatus =
  | "running"
  | "waiting_permission"
  | "blocked"
  | "failed"
  | "completed";

type TaskRunState = {
  taskId: string;
  status: TaskRunStatus;
  taskInput: string;
  messages: MessageRecord[];
  context: ContextPack;
  toolCalls: ToolCallRecord[];
  toolResults: ToolResultRecord[];
  changedFiles: ChangedFileRecord[];
  permissions: PermissionRecord[];
  verification: VerificationRecord[];
  handoffStatus?: "not_ready" | "ready" | "delivered";
  compactedSummary?: string;
  resumeReason?: string;
};
```

| 记录 | 必需字段 |
| --- | --- |
| `MessageRecord` | role、content summary、token estimate、createdAt |
| `ToolCallRecord` | id、tool name、input summary、mutating、permission id、createdAt |
| `ToolResultRecord` | tool call id、status、output summary、evidence refs、error |
| `ChangedFileRecord` | path、operation、read revision、write revision、diff ref |
| `PermissionRecord` | id、action、path / command、reason、decision、decidedAt |
| `VerificationRecord` | command、status、exit code、summary、evidence refs |

这些记录用于三件事：继续下一轮模型输入、恢复中断任务、生成最终 handoff。

## 4. 最小工具集

| 工具 | 是否修改状态 | 用途 | 最小约束 |
| --- | --- | --- | --- |
| `list_directory` | 否 | 查看目录结构 | 只能访问仓库内路径 |
| `read_file` | 否 | 读取文件内容 | 大文件截断并保留引用 |
| `search_text` | 否 | 搜索文件和文本 | 默认忽略依赖目录、构建产物和敏感文件 |
| `edit_file` | 是 | 基于已读取内容做局部替换 | 必须 read-before-write，old text 默认唯一匹配 |
| `write_file` | 是 | 创建新文件或受控整文件写入 | 创建意图必须明确；覆盖已有文件需要确认 |
| `shell_exec` | 可能 | 运行验证命令 | 仅允许验证类命令，其他命令需确认 |
| `git_diff` | 否 | 读取当前 diff 作为 handoff evidence | 只读；不执行 `git add`、`commit`、`push` |

`v0.1` 可以先不提供大型工具生态。内置工具要覆盖本地仓库任务的最小闭环：读、找、改、写、验证、看 diff。

## 5. 工具契约与权限

每个工具必须声明：

| 字段 | 含义 |
| --- | --- |
| `name` | 稳定工具名 |
| `inputSchema` | 可校验输入结构 |
| `mutating` | 是否可能修改文件、进程、网络或外部状态 |
| `risk` | `low`、`medium`、`high` |
| `permission` | `allow`、`ask`、`deny` 或 allowlist 规则 |
| `resultSummary` | 面向下一轮模型和 handoff 的短摘要 |
| `evidenceRefs` | 文件、diff、命令输出或 tool result 引用 |

权限规则：

- 只读工具默认可执行，但必须受仓库路径边界和敏感文件规则限制。
- `edit_file` 和 `write_file` 必须先读取目标文件；新文件必须有明确 create intent。
- 写入前需要校验路径、旧内容匹配、文件是否过期；高风险 diff 需要用户确认。
- `shell_exec` 只默认允许运行项目声明、配置 allowlist 或用户明确确认的验证命令。
- 不允许静默安装依赖、删除文件、发起网络请求、修改 git 状态或执行发布命令。
- `git_diff` 是只读 evidence 工具；`git add`、`git commit`、`git push` 不属于默认工具集。

权限被拒绝时，任务应进入 `blocked` 或输出带 blocker 的 handoff，而不是把拒绝包装成成功。

## 6. Context Policy

`ContextPack` 是每轮模型输入的工程边界。它不应无限拼接完整 transcript。

| Context lane | 内容 |
| --- | --- |
| Repo instructions | `AGENTS.md`、项目 README、用户显式约束 |
| Task input | 原始任务、范围、验收标准、非目标 |
| Relevant files | 已读取文件摘要、关键片段、路径引用 |
| Recent tool results | 最近工具结果、错误、权限决策、diff 摘要 |
| Verification plan | 可运行命令、已运行命令、跳过原因 |
| Open questions | 需要用户确认的问题和阻塞项 |

Token 预算策略：

- 永远保留任务目标、验收标准、权限决策、changed files 和 blocker。
- 长工具输出必须摘要；摘要保留 evidence ref，必要时可重新读取原始文件。
- 旧消息可压缩成 `compactedSummary`，但不能丢失用户明确约束。
- 如果上下文不足以安全修改文件，agent 应读取更多文件或进入 `blocked`。

## 7. Verification 与 Handoff

验证命令来源优先级：

1. 用户明确指定的命令。
2. 项目配置或 package manifest 中声明的测试 / 构建 /检查命令。
3. agent 根据仓库事实提出的命令，并经过 allowlist 或用户确认。

不能运行验证时，必须记录 skipped reason。验证失败时，最终状态不能是成功。

`AgentHandoff` 最小字段：

| 字段 | 含义 |
| --- | --- |
| `status` | `completed`、`failed` 或 `blocked` |
| `summary` | 完成了什么 |
| `changedFiles` | 文件路径、操作类型、diff ref |
| `commandsRun` | 命令、退出码、摘要、evidence ref |
| `risks` | 未覆盖风险、行为变化、兼容性担忧 |
| `blockers` | 权限拒绝、信息缺口、验证失败 |
| `evidenceRefs` | 文件片段、diff、工具结果、命令输出引用 |

handoff 面向代码审查和后续接手，不应只输出“已完成”。

## 8. Failure 与 Resume 状态

| 状态 | 含义 | Resume 行为 |
| --- | --- | --- |
| `waiting_permission` | 工具调用需要用户批准 | 用户 approve 后继续；deny 后进入 `blocked` 或 handoff |
| `blocked` | 缺少用户决策、上下文或权限 | 用户补充输入后继续；否则保持 blocker |
| `failed` | 工具执行或验证失败，且无法自动修复 | 可从失败点重试，或输出失败 handoff |
| `completed` | handoff 已生成，任务闭环结束 | 后续请求应创建新 task 或 follow-up task |

对 `waiting_permission`、`blocked` 和 `failed` 的继续恢复必须复用同一个 `taskId`，读取已有 messages、tool results、changed files、permissions 和 verification 记录，避免重复写入或重复执行高风险 shell。`completed` run 只能读取或复盘已完成状态；如需继续推进，应创建 follow-up task。

## 9. 延后事项

以下内容属于后续演进，不进入 `v0.1` 的最小 ReAct loop：

- 完整图执行模型，例如把每个步骤提升为 `ActionGraph` 节点。
- 高级 `Scheduler`、并发执行、优先级队列和跨任务调度。
- `Reconciler` 驱动的自动修复、复杂事实系统和长期状态推理。
- 完整 `runtime kernel`、事务 / 回滚、effect ledger 或 overlay revision。
- 多智能体协作、角色编排、插件市场、远程工具生态。
- TUI / IDE cockpit 作为核心交互面。

这些方向可以从 `v0.1` 的 messages、tool records、permissions、verification 和 handoff evidence 中自然演进；不应先于可工作的本地 code agent loop。

## 10. `v0.1` 实现切片与验收

实现切片：

- `lattecode run <task>` 创建 `TaskRunState`，写入 `taskId` 和原始任务输入。
- 加载仓库指令、相关文件和最近工具结果，构造 `ContextPack`。
- 接入模型 client，支持多轮 ReAct tool call。
- 提供最小内置工具：list、read、search、edit、write、shell verification、git diff。
- 对 mutating tool、shell、外部路径执行 permission gate。
- 强制 read-before-write、路径边界、stale write 检查和 diff evidence。
- 运行验证命令或记录不能验证的原因。
- 输出 `AgentHandoff`，包含 changed files、commands run、results、risks、blockers、evidence refs。
- 支持 `waiting_permission`、`blocked`、`failed` 的本地 resume；`completed` run 只能读取、复盘或派生 follow-up task，不能继续原任务的 mutating loop。

验收标准：

- 在一个已有小型仓库 fixture 中，agent 能完成简单代码或文档修改并生成 diff。
- scripted fake-model 集成测试覆盖完整 `TaskInput -> ContextPack -> ReAct -> Verification -> Handoff`。
- 写入前未读取目标文件会被阻止。
- 未确认的 install、delete、network、git write 命令会被阻止或进入 `waiting_permission`。
- 验证失败时 handoff 状态为 `failed` 或 `blocked`，不会报告成功。
- handoff 能列出 changed files、验证命令、结果摘要、风险和 evidence refs。
