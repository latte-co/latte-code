# Latte Code 能力 Roadmap

English counterpart: [Latte Code capability roadmap](../en-US/roadmap.md).

这个 Roadmap 是按能力依赖组织的完成清单，不是发布日期承诺。它记录当前
仓库真实支持的能力，以及下一步需要交付的边界；不会因为已有设计文档、局部
测试或原型就把能力标记为完成。

## 状态规则

- `[x]`：已由当前实现提供，并有相应的自动化验证。
- `[ ]`：尚未交付；设计文档、局部基础设施或计划都不构成完成。
- 一个条目从 `[ ]` 改为 `[x]` 前，必须明确支持边界，补齐最低责任层 UT 和
  最终二进制 E2E，并通过项目质量卡点。

涉及多平台的条目必须说明平台差异。当前 Unix 进程监管已实现；这不表示
Windows 已支持任意外部进程执行。

## 产品基础与交付底座

- [x] Rust workspace 分层：`latte-core`、`latte-engine`、`latte-headless`、
  `latte-tui` 与最终 `latte-code` 二进制。
- [x] 类型化状态、命令、事件与稳定的 Rust crate 边界。
- [x] TUI 终端生命周期：raw mode、panic/中断恢复、TTY 前置检查与受限尺寸渲染。
- [x] Linux、macOS、Windows 的构建、静态检查、UT、Contract、portable E2E 与
  release build CI 卡点；Unix 额外运行 PTY/进程 E2E。
- [x] 独立 UT、最终二进制 E2E、全目标覆盖率和文档链接检查规则。
- [ ] 可重复的性能基准、资源上限基准与回归告警。
- [ ] 安装后的跨平台 release smoke 与升级/降级兼容性验证。

## 工程与质量保障

- [ ] 机器可读的测试证据表：每项能力关联 owner、风险、UT、契约测试、回放、
  PTY E2E、live eval、平台范围、验证命令和已知缺口；Roadmap 记录交付范围，
  证据表记录如何持续证明该范围。
- [ ] 可复用的组合回放 Harness：在进程内组合 TUI/Headless/Engine/loopback
  Provider，断言公开事件、投影和持久化边界；最终二进制 PTY E2E 只承担终端、
  按键、子进程和真实组合入口的验证。
- [ ] 脱敏、版本化的 Provider/Harness fixture：覆盖 SSE、tool call、错误、取消、
  context、模型可见 tool schema 和实际 transport request；每个 Harness Profile
  都必须通过同一组适用的 conformance contract。
- [ ] Effect、JSONL 与 Session Catalog 的故障注入矩阵：覆盖写入中断、撕裂行、
  重启、lease 丢失、Provider 中断和 Effect `Unknown`；恢复后不得重复执行副作用。
- [ ] 失败证据包：测试失败时自动保留脱敏后的 ANSI/PTY 输出、事件轨迹、
  loopback Provider 请求、状态摘要、临时 workspace 与复现命令，并设置明确保留期。
- [ ] 独立的 live eval：临时仓库、明确任务判据、允许的模型/Harness 和可审计产物；
  仅在 nightly、手动或发布 smoke 运行，不能替代 PR 的离线确定性门禁。
- [ ] 发布物质量链：隔离环境的安装/启动/`--help`/最小 headless smoke、版本与
  checksum/签名/清单一致性、升级/降级和已发布包回归验证。
- [ ] 可消费的 CI 结果：长测试分片、JUnit/结构化报告、失败 artifact、稳定的
  重跑入口与超时诊断；在规模确有需要时再引入更复杂的测试归档/分发机制。
- [ ] 配置、Session 与扩展兼容迁移的转换契约：fixture 覆盖版本升级、字段丢失、
  warning、权限降级和拒绝路径。
- [ ] 性能与资源预算 smoke：启动、首帧、Session 加载、上下文构建、回放和
  cleanup 的可比基线与趋势告警；先监测回归，再在数据足够时设硬阈值。

## 配置、凭据与模型连接

- [x] 内置默认值、用户级和工作区级 JSONC 配置的确定性合并。
- [x] 内联与环境变量凭据；密钥只在内存解析，正常日志和持久化状态不写入密钥。
- [x] OpenAI Chat Completions 兼容 Provider、`base_url`、有界 HTTP 超时与重试。
- [x] 有界 SSE 流式输出、tool-call 聚合，以及受限的非流式回退。
- [x] Provider binding、工具别名，以及内部派生凭据/data scope 的 resume 校验。
- [x] 从已配置 Catalog 显式选择模型/Provider，并通过 TUI model picker 修改每个
  Session 的 binding；切换从下一个不可变 child 生效，并记录在公开 transcript 中。
- [ ] 版本化的 Harness Profile：按模型/Provider 成组解析 context strategy、
  system/developer prompt、工具 schema/命名、Plan/Stop 语义与能力开关。它不是
  Provider adapter，也不能依赖不透明的模型名猜测来放宽权限。
- [ ] Harness Profile 的 capability negotiation、未知 profile 的 fail-closed fallback，
  以及按 profile 运行的契约/E2E conformance suite。
- [ ] 模型目录、能力声明、fallback chain、成本/预算和 rate-limit 可见性。
- [ ] 用户账户、登录/登出、凭据轮换与受控的凭据存储策略。
- [ ] 其他 Provider 协议适配器；Responses API、Anthropic 等并未实现。
- [ ] 可观测且可控的 Provider 限流、退避、配额与本地健康诊断。

## 工作区、Session 与历史

- [x] 工作区发现、路径约束、工作区内工具执行与 Git/文件 manifest 读取。
- [x] 用户级全局 SQLite 中的 v1 Run 状态与 Thread v2 控制状态；Session Transcript
  Content 只以每 Session JSONL 为权威。
- [x] v1 `run`、`resume`、`show`、`list` CLI 兼容路径；既有 v1 Run 不被回填成 Thread。
- [x] Thread v2 的不可变 follow-up child、分页投影和有界 history 预算校验。
- [x] 用户级全局 `LATTE_CODE_HOME`、可识别 Git Worktree 的稳定
  Project/Workspace Identity、全局 Catalog 注册与 Session 分区 Lease。
- [x] TUI Catalog 中当前 Workspace 的 Session 发现与有界搜索。
- [x] 每个 Session 一份追加式 JSONL，作为 Transcript 读取权威，并支持有界 Record
  与撕裂末行恢复；SQLite 只保留同步 JSONL 前的事务 Outbox。
- [x] 幂等导入当前 Workspace 的旧数据库、不修改源文件，并生成 JSONL。
- [ ] 从孤立 JSONL 重建 Catalog，以及修复缺失的已观察 Tool Result Record。
- [x] 基于全局 SQLite Catalog 的 `/new`、`/sessions`、`/resume` 临时 Draft 新建、
  当前 Workspace 选择与恢复闭环。
- [x] 当前 Workspace 搜索、Session 标题与安全分叉。
- [ ] Session 的外部导入、分享与 handoff；每种操作都需保留权限和敏感内容边界。
- [ ] 外部 Agent 的 Session/config/Skill/Plugin 兼容导入：保留 source、版本和
  lossy conversion warning，绝不把不支持的权限或 hook 静默当成已生效。

## Agent Runtime 与上下文

- [x] Headless Provider → tool → Provider continuation loop。
- [x] 有界仓库上下文收集，包括受路径约束的 `AGENTS.md` 内容。
- [x] 模型工具调用、非密钥 input request、验证命令、handoff/evidence 的运行路径。
- [x] Provider 工具调用 ID、history 语法和请求字节预算的 fail-closed 校验。
- [x] Thread v2 snapshot、事件订阅和瞬态流式进度的基础协议。
- [x] 已验证的 TUI 基础 loop：首条 Prompt 经选定 Provider、工具/权限、持久化和
  transcript 展示完整走通。
- [x] 每 Session 一个进程内 Runner、有界 FIFO 用户输入 Mailbox 与跨 Session 并行。
- [x] 在下一个可运行 Child 边界接收第二条用户 Prompt，不篡改进行中的 Effect。
- [ ] 可信 Reminder Producer、排序与取消语义。
- [ ] Context compaction、摘要、选择性上下文与可解释 token 预算。
- [ ] 用户可控的 Session/Workspace memory、provenance、过期与 reset；不能把
  未经确认的模型结论伪装为事实。
- [ ] 跨模型/Provider handoff：验证消息、tool-result、reasoning/附件和 context
  是否能安全转换；不能重放只对原模型合法的受保护内容。
- [ ] 面向嵌入式前端的 Agent Runtime SDK：类型化 message/event stream、受控的
  context transformation 与生命周期；SDK 不暴露 Engine authority。
- [ ] 后台/长运行任务的恢复语义与用户可见控制面。
- [ ] 暂停、后台化、排队、定时与通知的任务系统；任务结果只能通过显式的、
  有 provenance 的输入路径回到 Session。

## 工具、Effect 与验证

- [x] 内建只读工具：读文件、列目录、搜索、读取项目 manifest 与 Git diff。
- [x] 内建写入工具：精确 edit、受约束 write/create，以及内容过期检查。
- [x] argv-first 验证/进程执行、输出上限、超时、取消和 Unix 进程组监管。
- [x] 变更后的验证证据与“验证失败、缺失或未运行不能完成”的约束。
- [ ] Effect-aware tool scheduler：只读、互不冲突的工具可并行；任何 mutation、
  approval、Effect ledger 和外部进程仍保持可证明的串行/隔离顺序。
- [ ] Windows 上安全、受监管的外部进程执行；当前明确 fail closed。
- [ ] 更丰富的内建开发工具、结构化代码修改与可预览的 patch workflow。
- [ ] 附件、图像与工具产物的输入/输出模型、大小限制、provenance 与持久化边界。
- [ ] 可插拔但受模式/资源限制的外部工具集成。
- [ ] 运行时 sandbox、网络访问控制与可配置的隔离 profile。

## 计划、任务与变更管理

- [ ] 用户可见的 Goal、Plan、Todo 与任务依赖；它们是可检查的执行承诺，而不是
  隐藏 prompt。
- [ ] Plan-first/Review 模式：计划、执行、验证和交付使用不同权限与 UI 状态，
  但不产生绕过 Engine 的 Effect 路径。
- [ ] Git 变更生命周期：状态、选择性暂存、commit、分支、PR/Issue 和代码托管
  集成；任何外部写入仍须用户明确授权。
- [ ] 独立于用户仓库 `.git` 的可审计快照/rollback，以及安全的 worktree 创建、
  进入、退出和清理。
- [ ] 可重复的 code review、security review、测试建议和结构化 findings；结果应
  绑定文件/行、证据和严重级别。
- [ ] 人机交接包：当前目标、计划、修改、验证、风险、下一步与可恢复位置。

## 安全、权限与恢复

- [x] `latte-engine` 是文件、进程、SQLite 控制状态和特权 Effect 的唯一 authority。
- [x] `Declared → Prepared → Started → Observed/Unknown` 的持久化 Effect 生命周期。
- [x] 精确绑定 revision、lease、fencing、请求 digest 的单次权限批准。
- [x] 中断或观察不确定时记录 `Unknown`，必须显式 reconciliation，绝不猜测成功。
- [x] 工作区包含性、handle-relative 安全写入、deny glob 与非安全平台 fail closed。
- [x] Provider、TUI 和投影只能读取脱敏公开信息，不能获得私有 descriptor 或密钥。
- [ ] Workspace trust：对 workspace 提供的命令、MCP、Skill、Hook 与远端配置按
  来源确认和最小权限授权。
- [ ] 面向用户的策略编辑、策略解释、审计导出与组织级 policy 分发。
- [ ] 细粒度网络、凭据、数据域与外部服务授权策略。

## TUI、CLI 与交互体验

- [x] Transcript-first Ratatui TUI、composer、Unicode 编辑、导航和受限 viewport 降级。
- [x] Permission、input request、Unknown reconciliation 的独立交互路径；Enter 不会隐式批准。
- [x] 快照重载处理事件缺口；本地 progress 不作为持久化 authority。
- [x] `Ctrl+P` 的 help、navigation、refresh、quit 本地 command palette。
- [x] 与 `Ctrl+P` 共享一个封闭的内建 Slash command Catalog，包含 composer
  suggestion、参数校验与 dispatch 时 availability 重检；动态来源和 fuzzy matching
  仍属于后续能力。
- [x] `/new`、分级 Session Picker/Search、`/sessions`/`/resume`、生命周期治理命令，
  并在切换时安全保留或显式替换当前 Composer Draft。
- [ ] 变更 diff、验证结果、Effect 历史的可浏览详情与文件跳转。
- [ ] 辅助功能、主题、键盘映射、国际化和可配置的终端体验。

## 事件、回放与可观测性

- [x] Engine 事务性事件、Thread event stream、snapshot reload 与有界 transient progress。
- [x] 持久化的 Run/Effect/permission/checkpoint/verification 控制信息。
- [ ] 以 JSONL Conversation 为权威的离线回放；回放不调用 Provider 或执行 Effect。
- [ ] 可查询的 Session/Run/Effect 时间线、结构化诊断与脱敏审计导出。
- [ ] 按 Provider、模型、任务、工具与上下文归因的 token/成本/延迟使用报告。
- [ ] opt-in 遥测、隐私边界、崩溃报告与运行指标。
- [ ] 事件保留、分页、版本演进与迁移策略。

## 扩展、委派与多 Agent

- [ ] 统一的能力 Registry：稳定 ID、版本、provenance、schema、资源限制与 availability。
- [ ] 可信纯文本 Prompt Command；不允许 shell interpolation 或任意 callback。
- [ ] 显式 Provider adapter、外部 Tool/MCP 接入的权限和 schema 边界。
- [ ] MCP 的 tool、resource、prompt、authorization/elicitation 和连接生命周期；
  每个远端能力都要单独纳入 policy/approval。
- [ ] 可安装 Plugin/Skill 的发现、版本、签名、依赖、生命周期与可撤销权限。
- [ ] Hook/automation 的声明式生命周期事件、输入输出 schema、超时和失败策略；
  不为 Hook 绕开 Engine 的权限、sandbox 或审计。
- [ ] 受限 delegated child Run：预算、deadline、tool allowlist、取消和结果摘要。
- [ ] 多 Agent 的 parent/child 可视化、批准隔离、资源治理与恢复。
- [ ] Agent 间消息、任务分配、共享受限上下文与用户可见的协作记录。

## 代码智能、IDE 与远程能力

- [ ] LSP/语义索引、符号导航、代码搜索与结构化编辑。
- [ ] IDE bridge：编辑器选择、诊断、diff、终端和 Session 的受权互操作。
- [ ] 本地 app-server/API：多前端共享 Session 所需的连接认证、版本化 RPC、
  event backpressure、snapshot reload 与并发/权限契约。
- [ ] Web、桌面、移动和 CLI 等多端表面复用相同的公开协议，不复制 Engine authority。
- [ ] 远程执行、远程 workspace、队列、断线重连与凭据隔离。
- [ ] 浏览器、计算机使用、多模态输入输出等可选体验能力。

## Roadmap 关联设计

以下文档定义尚未完成能力的架构边界，不能单独改变 checklist 状态：

- [全局 Session 与数据存储](design/data-storage.md)
- [斜杠命令](design/slash-commands.md)
- [异步 Turn Runner](design/agent-harness/asynchronous-turn-runner.md)
- [Session 存储与恢复](design/agent-harness/session-store-and-recovery.md)
- [Effect Authority、策略与隔离](design/agent-harness/effect-authority-and-policy.md)
- [扩展与委派能力](design/agent-harness/extensions-and-delegation.md)
- [事件、投影与回放](design/agent-harness/event-projection-and-replay.md)
- [TUI Runtime 契约](design/agent-harness/tui-runtime-contract.md)
- [验证 Harness 与确定性测试](design/agent-harness/verification-harness.md)

## 横向参考后的范围校准

本清单在扫描 `references/` 的 CodeWhale、Codex、OpenCode 与 Claude Code 后更新。
随后纳入 Pi 与 Trae-X。它们在产品取舍上不同，但共同证明完整 Code Agent 还
需要模型运营、Harness Profile、会话治理、上下文/记忆、计划/任务、变更管理、
受控扩展、代码智能、多端协议与可观测性。Pi 说明可嵌入的 event-driven Agent
Runtime 与 Provider handoff 的边界；它默认不提供权限系统，不能作为安全设计的
参考。Trae-X 说明 Provider adapter 与 per-model Harness Profile 应分离：后者选择
prompt/context、工具 schema 和运行语义，而不是直接运行另一个 Agent binary。
Ink 和 OpenTUI 只提供终端 UI 基础设施参考，不会把其 widget 或渲染实现当作
Latte Code 的产品能力。
