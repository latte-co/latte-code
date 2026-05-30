# 基础 Code Agent 落地方案

## 目标与边界

本文件定义 Fluxcode 第一阶段的基础 code agent 技术方案。第一阶段目标是先交付一个单进程、本地优先、可测试的完整 code agent，而不是直接实现完整 harness-native runtime。

设计边界如下：

- **当前阶段**：实现基础 code agent 的模型调用、工具调用、权限控制、session、event log、evidence、恢复和 CLI 入口。
- **预留方向**：让核心模块在接口上保持 graph-ready，后续可以接入 `GraphState`、`NodeExecutor`、`Gate` 和 `Reconciler`。
- **非目标**：本文件不表示当前仓库已经存在源码、包管理器或测试配置；本节点只产出设计文档，不引入源码实现。

## 与 Harness-native 目标的关系

最终目标仍是 harness-native code agent，即以 graph lifecycle 作为执行、恢复、审计和人工接管的主状态。第一阶段先完成基础 agent，是为了稳定以下可复用能力：

| 第一阶段能力 | Harness-native 预留点 |
| --- | --- |
| `AgentLoop` 执行一次用户任务 | 后续包裹成单个 `NodeExecutor` 的执行体 |
| 工具调用与权限控制 | 后续由 node contract 和 scheduler 下发工具范围 |
| `EventLog` | 后续成为 graph event log 的底层追加记录 |
| `EvidenceStore` | 后续挂接到 node、gate 和 reconcile history |
| `SessionStore` | 后续退居为模型上下文和交互记录，不替代 `GraphState` |
| 覆盖率与验证门槛 | 后续绑定 `Gate` 的自动验收条件 |

核心原则：第一阶段可以没有完整 graph scheduler，但不能把 session、transcript 或 CLI 状态设计成未来唯一真相源。

## 第一阶段能力边界

基础 code agent 需要覆盖以下完整能力：

1. 读取用户请求并维护一次本地 session。
2. 构造模型上下文，调用模型生成文本或工具调用请求。
3. 根据工具契约校验工具输入。
4. 在执行 mutating 或高风险工具前执行权限判断。
5. 执行工具并记录结构化工具结果。
6. 将关键工具结果映射为 `Tool evidence`。
7. 追加 event log，支持失败定位和中断恢复。
8. 把模型响应、工具证据和最终结果写入 session。
9. 支持 fake model 和 fake tools，用于稳定测试 agent loop。
10. 用 Vitest 覆盖模型、工具、权限、session、evidence、恢复等基础场景。

第一阶段不追求多 agent、远程服务、插件市场或完整 TUI。CLI 可以先是最小入口，只要能驱动 agent loop、展示结果、暴露必要日志路径即可。

## 技术路线

### 语言与运行时

- 使用 TypeScript。
- 以 Node.js 本地进程为第一阶段运行形态。
- 模块之间通过显式接口协作，避免让模型 SDK、文件系统和 CLI 细节渗透到核心 loop。

### 测试框架

- 使用 Vitest。
- 覆盖率目标为 98%，覆盖率门槛从第一阶段开始写入配置约定。
- 使用 fake model、fake tool registry、临时 session store 和内存 event log 覆盖关键路径。

### 配置格式

- 使用 JSONC 作为用户配置格式。
- 默认配置文件名为 `fluxcode.config.jsonc`。
- 配置 schema 使用 TypeScript 类型与 JSON Schema 双轨维护：TypeScript 类型用于实现侧编译约束，JSON Schema 用于编辑器提示和运行时校验。

## 初始源码目录建议

后续引入源码时，建议保持以下目录职责。本文件只描述建议，不创建这些目录。

```text
src/
  cli/                  # CLI 参数解析、命令入口、输出渲染
  config/               # JSONC 读取、默认值合并、schema 校验
  core/                 # AgentLoop、运行上下文、错误模型
  model/                # 模型 provider 抽象、消息格式、fake model
  tools/                # Tool contract、registry、内置工具适配
  permissions/          # 权限策略、确认流程、risk level 判断
  evidence/             # evidence schema、mapper、store
  session/              # session schema、snapshot、恢复游标
  events/               # append-only event log、event schema
  graph-ready/          # NodeExecutor/Gate/Reconciler 适配接口草案
  testing/              # 测试夹具、fake providers、临时目录工具
tests/
  unit/                 # 纯函数、schema、策略、mapper 单测
  integration/          # agent loop、session/recovery、权限集成测试
```

模块依赖方向建议：`cli` 调用 `core`，`core` 编排 `model`、`tools`、`permissions`、`evidence`、`session` 和 `events`；底层模块不反向依赖 `cli`。

## JSONC 配置方案

### 文件名与查找顺序

默认配置文件名：`fluxcode.config.jsonc`。

建议查找顺序：

1. CLI 显式传入的 `--config` 路径；
2. 当前工作目录的 `fluxcode.config.jsonc`；
3. 用户级配置目录中的 `fluxcode/config.jsonc`；
4. 内置默认配置。

合并规则：越靠前的配置优先级越高；对象字段递归合并，数组字段整体替换；敏感字段不写入 event log。

### 顶层 schema

```ts
interface FluxcodeConfig {
  schemaVersion: 1;
  models: ModelConfig;
  permissions: PermissionConfig;
  tools: ToolConfig;
  session: SessionConfig;
  evidence: EvidenceConfig;
  coverage: CoverageConfig;
}
```

### 默认配置草案

```jsonc
{
  "schemaVersion": 1,
  "models": {
    "default": "primary",
    "providers": {
      "primary": {
        "type": "openai-compatible",
        "model": "gpt-5.5",
        "baseUrl": "${FLUXCODE_MODEL_BASE_URL}",
        "apiKeyEnv": "FLUXCODE_MODEL_API_KEY",
        "temperature": 0.2,
        "maxOutputTokens": 8192
      }
    }
  },
  "permissions": {
    "defaultMode": "ask",
    "allowReadOnlyTools": true,
    "mutatingTools": "ask",
    "highRiskTools": "deny",
    "trustedDirectories": ["."],
    "denyGlobs": ["**/.env*", "**/node_modules/**", "**/.git/**"]
  },
  "tools": {
    "enabled": ["read", "write", "edit", "bash"],
    "disabled": [],
    "bash": {
      "defaultTimeoutMs": 120000,
      "requireApprovalFor": ["network", "install", "delete", "git-write"]
    }
  },
  "session": {
    "store": "filesystem",
    "directory": ".fluxcode/sessions",
    "autosave": true,
    "maxTranscriptBytes": 1048576
  },
  "evidence": {
    "store": "filesystem",
    "directory": ".fluxcode/evidence",
    "captureToolInputs": "summary",
    "captureToolOutputs": "summary",
    "maxEvidenceBytes": 262144
  },
  "coverage": {
    "provider": "vitest",
    "statements": 98,
    "branches": 98,
    "functions": 98,
    "lines": 98,
    "exclude": ["tests/**", "src/testing/**"]
  }
}
```

说明：示例中的 `.fluxcode/` 是后续实现建议，不等同于当前仓库已有目录。若实际落地时需要持久化目录，应同时更新 `.gitignore` 与项目文档。

### 模型配置

`models` 需要支持：

- 多 provider 声明；
- 默认 provider；
- 模型名、endpoint、认证环境变量；
- temperature、max output token 等推理参数；
- fake model provider，用于测试和离线回放。

认证信息只通过环境变量引用，不允许直接写入配置文件。event log 只能记录 provider id 和模型名，不能记录密钥。

### 权限策略

`permissions` 需要区分三类决策：

| 决策 | 含义 |
| --- | --- |
| `allow` | 直接允许执行 |
| `ask` | 需要用户确认 |
| `deny` | 禁止执行 |

权限判断输入至少包括：工具名、`riskLevel`、`mutating`、目标路径、外部网络访问、命令类别和当前工作目录。权限结果需要写入 event log，并在产生 evidence 时附带权限摘要。

### 工具配置

`tools` 需要支持：

- 全局启用 / 禁用工具；
- 工具级默认 timeout；
- 工具级权限覆盖；
- bash 命令类别策略；
- 输出截断上限；
- evidence mapper 开关。

配置不能绕过工具自身 contract。即使配置允许某工具，工具输入仍必须通过 schema 校验。

### Session 与 Evidence 存储配置

`session` 保存模型交互、用户输入、最终响应和恢复游标。`evidence` 保存结构化工具证据。两者必须分离：

- session 面向模型上下文和用户体验；
- evidence 面向审计、验证和未来 gate/reconcile；
- 后续引入 `GraphState` 后，session 不是 source of truth。

恢复时建议按以下顺序处理：

1. 读取最近 session snapshot；
2. 回放未落入 snapshot 的 event log；
3. 校验 evidence 引用是否存在；
4. 恢复 agent loop 的下一步动作；
5. 若恢复状态不完整，进入 `NEEDS_CONTEXT` 或安全失败，而不是继续执行 mutating 工具。

### 覆盖率配置

`coverage` 的第一阶段默认门槛为 98%。覆盖率维度包括 statements、branches、functions、lines。若某些目录用于测试夹具或生成代码，需要显式加入 exclude 并说明理由。

覆盖率不应替代行为验收。关键 agent loop 场景必须有明确断言，而不是只追求行覆盖。

## Agent loop

基础 agent loop 建议如下：

```text
load config
  ↓
open or create session
  ↓
append user_input event
  ↓
build model context
  ↓
call model
  ↓
if model requests tool:
  validate tool input
  evaluate permission
  execute tool or request confirmation
  map tool result to evidence
  append tool/evidence events
  continue model loop
else:
  persist final response
  close or keep session resumable
```

### 模型层

模型层只负责把标准消息转换为 provider 请求，并把 provider 响应转换为内部 `ModelTurn`。它不直接读写文件、不执行工具、不决定权限。

### 工具调用层

工具调用层负责 schema 校验、执行、输出截断、错误规范化和 evidence mapping。工具失败需要返回结构化错误，供 agent loop 决定重试、降级、询问用户或终止。

### 权限层

权限层在工具执行前运行。mutating 工具、高风险命令、外部网络访问和越界路径必须进入 `ask` 或 `deny`，不能由模型响应直接放行。

### Event log

event log 使用 append-only 设计。建议事件类型包括：

- `session.created`；
- `user.input`；
- `model.requested`；
- `model.responded`；
- `tool.requested`；
- `permission.decided`；
- `tool.completed`；
- `evidence.recorded`；
- `session.snapshotted`；
- `loop.completed`；
- `loop.failed`。

event log 需要可回放，但不应保存密钥、完整大文件内容或未截断的长输出。

### Evidence

第一阶段的 evidence 至少包含：

| 字段 | 说明 |
| --- | --- |
| `id` | 稳定 evidence id |
| `sessionId` | 所属 session |
| `toolName` | 工具名称 |
| `inputSummary` | 输入摘要 |
| `outputSummary` | 输出摘要 |
| `references` | 文件路径、命令、URL 或其他可复查引用 |
| `permission` | 权限决策摘要 |
| `timestamp` | 产生时间 |
| `truncated` | 是否截断 |
| `graphHints` | 可选，未来挂接 node/gate 的提示 |

`graphHints` 只做预留，不在第一阶段承担调度语义。

### Session / Recovery

session 需要支持：

- 创建新 session；
- 从 session id 恢复；
- 保存 transcript 摘要；
- 保存最后一个稳定 event offset；
- 标记当前 loop 是否处于等待权限确认、等待用户输入或已完成状态。

恢复时不能自动重放 mutating 工具。若中断发生在 mutating 工具执行前，应重新进入权限判断；若中断发生在工具执行后但 evidence 未写入，应根据 event log 判断是否需要人工确认。

## Tool contract

每个工具必须声明稳定 contract：

```ts
interface ToolContract<Input, Output> {
  name: string;
  description: string;
  inputSchema: unknown;
  outputSchema: unknown;
  riskLevel: "low" | "medium" | "high";
  mutating: boolean;
  permission: PermissionRequirement;
  execute(input: Input, context: ToolExecutionContext): Promise<Output>;
  evidenceMapper?: EvidenceMapper<Input, Output>;
}
```

字段约定：

| 字段 | 要求 |
| --- | --- |
| `inputSchema` | 运行时校验模型生成的工具参数 |
| `outputSchema` | 规范工具输出，便于测试和 evidence mapping |
| `riskLevel` | 参与权限决策和默认审批策略 |
| `mutating` | 标记工具是否修改本地或外部状态 |
| `permission` | 声明工具执行前需要的权限条件 |
| `evidenceMapper` | 把工具输出转换为 evidence 摘要和引用 |

内置工具至少按以下策略分类：

| 工具类型 | `riskLevel` | `mutating` | 默认策略 |
| --- | --- | --- | --- |
| 读取文件 / 列目录 | `low` | `false` | `allow` |
| 搜索文件内容 | `low` | `false` | `allow` |
| 写文件 / 编辑文件 | `medium` | `true` | `ask` |
| 执行只读命令 | `medium` | `false` | `ask` |
| 删除文件 / 安装依赖 / git 写操作 | `high` | `true` | `deny` 或显式 `ask` |
| 网络访问 | `medium` 或 `high` | 视行为而定 | `ask` |

工具 contract 是后续 harness-native `NodeExecutor` 的关键复用点：scheduler 可以通过 contract 下发工具范围，reconciler 可以基于 evidence mapper 判断 gate 是否有足够证据。

## Graph-ready 预留接口

第一阶段可以先提供薄接口，避免未来重构核心 loop。

### `NodeExecutor`

`AgentLoop` 后续应能被适配成：

```ts
interface NodeExecutor {
  execute(input: NodeExecutionInput): Promise<NodeExecutionResult>;
}
```

其中 `NodeExecutionInput` 包含 node contract、上下文、工具范围和权限策略；`NodeExecutionResult` 包含状态、摘要、deliverables、concerns、evidence 引用和建议的 graph update。

### `Gate`

第一阶段的测试、覆盖率、权限确认和人工确认都应保留 gate 语义：

- 覆盖率 gate：Vitest 覆盖率是否达到 98%；
- 权限 gate：mutating / high-risk 工具是否得到确认；
- 验收 gate：基础场景是否通过；
- 人工 gate：用户是否确认继续。

实现上可以先是普通策略对象，但命名和事件类型应便于后续迁移到 graph gate。

### `Reconciler`

第一阶段不需要完整 reconciler，但 agent loop 结束后应有一个结果归一化步骤，把成功、失败、阻塞、需要用户输入等状态统一成结构化结果。后续该步骤可以替换为真正的 `Reconciler`。

### `GraphState`

第一阶段不把 `GraphState` 作为主状态，但 session 和 evidence 记录需要保留 future mapping：

- session id 可映射到 run id；
- evidence 可映射到 node id / gate id；
- event log offset 可映射到 graph event cursor；
- final result 可映射到 node result。

## 测试和验证方案

### Vitest 分层

| 层级 | 覆盖对象 | 示例 |
| --- | --- | --- |
| Unit | schema、默认值合并、权限策略、evidence mapper | 配置缺字段时使用默认值；高风险工具默认拒绝 |
| Integration | agent loop、tool registry、session/recovery | fake model 请求工具后产出 evidence 并继续 loop |
| Contract | tool contract 与模型工具调用格式 | 模型生成非法参数时工具不执行 |
| Recovery | event log 回放和 session 恢复 | 中断后不会重复执行 mutating 工具 |

### Fake model

fake model 需要支持脚本化响应：

1. 返回普通文本；
2. 返回单个工具调用；
3. 返回连续工具调用；
4. 返回非法工具参数；
5. 模拟 provider 错误；
6. 模拟上下文过长。

fake model 是 agent loop 测试的主工具，避免单测依赖真实模型和网络。

### 基础验收场景

第一阶段至少覆盖：

1. 只读任务：读取文件、生成回答、记录 evidence。
2. 写入任务：模型请求写文件，权限为 `ask` 时暂停等待确认。
3. 工具失败：工具返回结构化错误，agent loop 给出可恢复结果。
4. 非法参数：schema 校验失败，工具不执行。
5. 中断恢复：event log 回放后继续安全状态。
6. 覆盖率 gate：Vitest 覆盖率未达到 98% 时验证失败。
7. fake model：无网络环境下可完整测试 agent loop。

## MVP 切法

### 必做

- TypeScript 工程骨架；
- JSONC 配置读取、默认值合并与 schema 校验；
- 模型 provider 抽象与 fake model；
- tool contract、registry、schema 校验；
- 权限策略和确认状态；
- append-only event log；
- session snapshot 与恢复游标；
- evidence schema、mapper 和 store；
- 最小 CLI agent loop；
- Vitest 测试与 98% 覆盖率门槛。

### 后置

- 完整 TUI；
- MCP adapter；
- 多 provider 动态切换；
- 真实 `GraphState` scheduler；
- 多 `NodeExecutor`；
- remote server；
- plugin marketplace；
- 长期 memory。

### 不做

- 第一阶段不实现多 agent 并发；
- 不把 transcript 当作唯一状态源；
- 不做自动 git commit / push；
- 不默认允许安装依赖、删除文件或修改仓库历史；
- 不把用户密钥写入配置、日志、session 或 evidence；
- 不用真实模型调用作为单元测试前提。

## 风险与反模式

| 风险 / 反模式 | 问题 | 缓解 |
| --- | --- | --- |
| 直接从 CLI 调工具 | 权限、evidence 和恢复绕过核心 loop | 所有工具调用必须经过 `AgentLoop` 编排 |
| session 过度膨胀 | 恢复慢且难以审计 | session 存摘要，完整证据进入 `EvidenceStore` |
| evidence 只是长文本 | 后续 gate/reconcile 无法稳定引用 | evidence 必须有 id、摘要、引用和权限信息 |
| 权限由模型自觉遵守 | 高风险动作可能被误执行 | 权限层在工具执行前硬拦截 |
| fake model 缺失 | 测试依赖真实模型，结果不稳定 | fake model 是必做模块 |
| 只追求覆盖率数字 | 关键行为未被断言 | 覆盖率 gate 与基础验收场景同时存在 |
| 过早引入 graph 全量模型 | 第一阶段复杂度失控 | 只预留 `NodeExecutor`、`Gate`、`Reconciler`、`GraphState` 映射 |
| 配置中保存密钥 | 安全风险和日志泄漏 | 只允许环境变量引用，日志和 evidence 自动脱敏 |

## 结论

第一阶段应先交付一个基础但完整的本地 code agent：可配置、可测试、可恢复、可审计，并且所有工具调用都经过权限和 evidence 规范化。它不需要立即成为完整 harness-native runtime，但必须避免形成与 `GraphState`、`NodeExecutor`、`Tool evidence`、`Gate` 和 `Reconciler` 冲突的状态模型。
