# 斜杠命令设计

状态：**第一批 Built-in 已实现。**

Latte Code 目前由同一个 Built-in Catalog 驱动 `Ctrl+P` 与 Composer 的精确
Slash Resolution。“当前实现状态”一节描述的 Session Command 已经落地；
最小 Built-in Suggestion Popup 也已经实现。Fuzzy Matching、Prompt Command
扩展、`/cancel` 与后续 Catalog Source 仍是目标设计。

## 1. 参考实现结论

本设计参考了以下本地源码快照。表中路径只作为设计证据，不会成为 Latte Code
的依赖。

| Agent | 快照 | 相关机制 |
| --- | --- | --- |
| Codex | `1f0566d3f59298d1bb88820a0d35294f1eeb07ea` | `codex-rs/tui/src/slash_command.rs` 使用强类型内建 Enum，集中定义 Alias、Description、Inline Argument、平台可见性和 Active Task 可用性。`bottom_pane/slash_commands.rs` 对 Lookup 与 Popup 复用同一套 Availability Filter。`chatwidget/slash_dispatch.rs` 把识别出的命令映射为显式 Application Action。 |
| OpenCode | `c69abee0c73253aebae65e87e4e1b9bfa8c38021` | `packages/tui/src/keymap.tsx` 从 Command Palette 使用的同一个 Reachable Command Registry 派生 Slash Entry。`packages/opencode/src/command/index.ts` 合并 Built-in、配置 Prompt Command、MCP Prompt 和 Skill。`packages/opencode/src/session/prompt.ts` 展开 Prompt Template，并通过正常 Session Prompt 路径执行。 |
| Claude Code 本地参考镜像 | `5a774a2b62d7949c1d94e0b726281554d7893cfd` | `src/types/command.ts` 区分 Local、Local UI 与 Prompt Command，并携带 Alias、Availability、Argument Hint、Source、Sensitivity 和 Invocation Policy。`src/utils/suggestions/commandSuggestions.ts` 按 Exact、Alias、Prefix、Description 与近期使用情况排序。`src/utils/processUserInput/processSlashCommand.tsx` 在 Dispatch 前执行 Remote Safety 和 User Invocation 检查。 |

可以复用的结论是：

- Command Metadata、Popup 可见性、精确 Lookup 和 Dispatch Availability 必须
  来自同一个 Catalog。
- 本地 UI Action、Typed Application Action 和 Provider 可见 Prompt Command
  是不同的执行类型。
- Availability 必须在 Dispatch 时再次检查，不能只在构建 Popup 时检查。
- 动态 Command Source 必须展示 Provenance 并处理名称冲突。
- 命令不能因为由 `/` 输入就自动获得额外权限。

Latte Code 明确**不复制** OpenCode 的 Command Template Shell Interpolation。
Prompt Template 只允许文本展开；解析期间不能执行 Shell、读取环境变量或进行
文件 I/O。

## 2. 目标与非目标

目标：

- 通过瞬态 New Session Draft 与按 Workspace 过滤的 Session Picker/Resume Path 完成最主要的
  Session 闭环。
- 在 Composer 开头输入 `/` 时发现命令。
- 让快捷键、`Ctrl+P` 与 Slash Alias 收敛到同一套 Command Identifier 和
  Availability Rule。
- 保持 TUI Reducer 纯净，并保留 `latte-engine` 的特权边界。
- 明确区分本地控制命令与会成为模型可见 Prompt 的命令。
- 支持确定性的匹配、参数、Alias、Disabled Reason 和未来可信 Prompt Command
  Source。
- Command Validation Error 与 Popup State 保持瞬态；Prompt 一旦被接受，Provider
  启动失败使用正常的持久 Run Failure 语义展示。

第一阶段不做：

- 任意 Shell Command、可执行 Plugin 或 Command Handler Script。
- Workspace 自定义命令覆盖内建命令。
- MCP Prompt Command 或模型可调用 Skill。
- 绕过 `ThreadRuntimeService` 或 Engine Effect 的第二套 Runtime API。
- 在 `latte-code --json run` 中解析斜杠命令；Headless Automation 继续使用显式
  CLI Subcommand，并把 Prompt String 当作字面文本。

## 3. Command Model

Catalog 暴露类似下面的无密钥 Descriptor：

```rust
pub struct CommandDescriptor {
    pub id: CommandId,
    pub name: String,
    pub aliases: Vec<String>,
    pub description: String,
    pub category: CommandCategory,
    pub kind: CommandKind,
    pub arguments: ArgumentPolicy,
    pub concurrency: ConcurrencyPolicy,
    pub source: CommandSource,
}

pub enum CommandKind {
    LocalUi,
    TypedAction,
    PromptTemplate,
}

pub enum CommandAvailability {
    Enabled,
    Disabled { reason: String },
    Hidden,
}
```

三种 Kind 的权限刻意不同：

| Kind | 结果 | Provider 可见 | 持久化 |
| --- | --- | --- | --- |
| `LocalUi` | 纯 Reducer State Change，例如打开 Help 或进入 Navigation | 否 | 否 |
| `TypedAction` | 由 Composition Root 处理的既有或新增 `ThreadUiAction` | 默认否；只有正常 Domain Operation 本身产生 Conversation Content 时才可见 | 只保存目标 Service 权威拥有的 Domain State |
| `PromptTemplate` | 展开为有界文本，通过普通 Start/Follow-up 路径提交 | 是 | 精确展开后的 User Content 与有界 Invocation Metadata |

Catalog 只包含 Metadata 与 Identifier，不包含任意 Callback。Built-in Command
解析为封闭 Rust Enum。未来的动态 Prompt Command 只能解析为
`PromptCommandId`，不能制造通用 Engine Action。

## 4. Palette 与 Slash Input 共用一个 Catalog

`Ctrl+P` 和 Slash Popup 使用同一个 `CommandContext` 查询同一个 Catalog。
Context 只包含判断 Availability 所需的状态：

- 当前 Focus 与 Overlay Ownership。
- Connection State。
- 是否已经存在 Session。
- 当前 Session Lifecycle 与 Pending Request Kind。
- 是否存在正在提交的内容或一条 Queued Follow-up。
- Platform 与已启用的 Product Feature。
- 未来加载 Workspace Command Source 时，Workspace 是否受信任。

`Hidden` 表示命令不适用于当前 Build、Platform、Feature 或 User；`Disabled`
表示命令有关联，但在当前状态执行不安全，UI 会展示有界原因。Dispatch 会重新
计算 Context，因此过期 Popup State 不能在 Session 状态变化后执行命令。

当前私有 `PaletteCommand` 列表要改为 Catalog 驱动。Command 可以只有 Palette
Entry 而没有 Slash Alias，但不能再维护第二份 Slash-only Built-in Handler
列表。

## 5. 解析与识别

Slash Recognition 使用以下确定性规则：

1. 只有第一个 Byte 是 `/` 的 Composer Content 才是 Candidate。前面存在空白时
   按普通 Prompt 处理。
2. Command Name 是第一 Logical Line 的第一个 Token；第一行剩余内容与后续行
   一起组成不透明 Argument String。
3. Canonical Name 和 Alias 使用小写 ASCII，满足
   `[a-z0-9][a-z0-9:_-]{0,63}`。
4. Lookup 区分大小写，并要求精确匹配 Canonical Name 或 Alias。
5. Argument 内部换行保持不变，只裁剪外围空白；绝不使用平台 Shell Parser。
6. 已知命令携带非法或禁止参数时返回本地 Validation Error，并完整保留 Composer
   Draft。
7. 未知或语法非法 Candidate 继续作为普通 Prompt。因此 `/tmp/file`、`/a/b`
   和未知 `/example` 不会被 Command System 截获。

Recognition 与 Suggestion Matching 分离。Popup 可以提供 Exact、Prefix、Alias
Prefix 与有界 Fuzzy Match，但除非用户显式选择，否则 Dispatch 不能执行 Fuzzy
Result。

Paste 永远不会执行命令；它只会修改 Composer，用户仍需显式提交或选择结果。

## 6. Composer Interaction

当 Composer 以 `/` 开头且 Caret 仍在第一个 Token 内时，在 Composer 正上方
打开 Popup。最多展示十行，每行包含：

- Canonical `/name`。
- Description。
- 非 Built-in 的 Source Badge。
- Argument Hint。
- 适用时的 Disabled Reason。

交互规则：

- Up/Down 循环选择；PageUp/PageDown 按页移动。
- `Tab` 补全选中项的 Canonical Name；支持参数时追加一个空格。
- `Enter` 执行精确匹配且 Enabled 的无参数命令，或用户显式选中的 Enabled
  Result。
- 选择必须提供参数的命令时，只补全到 Composer，不执行不完整 Invocation。
- `Esc` 首先关闭 Slash Popup；之后再次按 `Esc` 才继续当前进入 Transcript
  Navigation 的行为。
- Backspace 删除开头 `/` 后关闭 Popup，不改变其他 Composer Content。
- Disabled Selection 不可执行，只显示原因，不清空 Draft。

Popup 不是居中 Modal，也不会替代 `Ctrl+P`。`Ctrl+P` 仍是完整 Action Palette；
Slash Input 是 Composer 中的快速路径。

## 7. Dispatch 与权限边界

Dispatch 使用以下流程：

```text
composer text
→ parse candidate
→ exact catalog resolution
→ re-evaluate availability
→ validate arguments
→ dispatch by closed CommandKind
```

`LocalUi` Command 调用纯 Reducer Transition。`TypedAction` Command 发出显式
`ThreadUiAction` Variant。`latte-code` Composition Root 把该 Variant 映射为
具体 `ThreadRuntimeService` Method 或本地 Terminal Action。`latte-engine` 永远
不会接收 Command Name String，也不提供通用 `execute_slash_command` Method。

未来的 `PromptTemplate` Command 向 `latte-headless` 发出有界 Prompt Command
Request。Resolver 展开文本并返回普通 Prompt；Start 或 Follow-up 随后复用手写
文本相同的 Submission Identity、Queue、Provider、Tool、Permission、Effect 和
Recovery Path。

因此：

- `/refresh` 只能发出 `RefreshSnapshots`。
- `/cancel` 只能发出现有 Typed `Cancel { thread_id }` Action。
- `/help` 不能创建 Session 或调用 Provider。
- Prompt Command 可以影响模型，但后续所有 Tool 仍必须经过 Engine Prepare、
  Permission、Fencing 与 Observation。
- Command 不能从 TUI Reducer 直接写文件、启动进程、修改 SQLite 或追加 JSONL。

## 8. Lifecycle 与并发

每个 Descriptor 显式声明 Concurrency Policy：

- `Always`：安全的本地查看或 Terminal Action。
- `SessionRequired`：要求选中 Session，但不一定要求 Run Idle。
- `IdleOnly`：修改 Session Configuration 或 Lifecycle，要求 `Ready`。
- `RunningOnly`：只对 Active Run 有意义，例如 `/cancel`。
- `PromptLike`：使用普通 Composer Text 相同的当前 Submission 与单条 Follow-up
  Queue Contract。

Permission、Input Request 与 Reconciliation State 继续拥有完整 Key Event。
Slash Popup 不能在这些状态打开。尤其不能通过 Command 把 Enter 变成 Approval
或 Reconciliation Shortcut。

Local 与 Typed Action Command 不会进入 Provider Follow-up Queue。Prompt Command
使用现有单条 Queued Follow-up；系统不会建立平行 Command Queue。

## 9. 持久化、History 与 Telemetry

持久化契约遵守数据存储设计：

- Popup Filter、Selection、Validation Error、Disabled Reason 和 Local Command
  Output 都是内存中的 Presentation State。
- `LocalUi` Invocation 不创建 Session、Run、SQLite 或 JSONL Record。
- `TypedAction` Command 只持久化它调用的权威 Domain Transition；字面
  `/command` 文本不是 Conversation Message。
- `PromptTemplate` Command 持久化精确展开、Provider 可见的 User Message；有界
  Metadata 可以记录 Canonical Name、Source Class 和 Template SHA-256，使 Replay
  不依赖当前 Template File。
- Template Validation 或 Expansion 失败不留下 Session Content，并恢复原始
  Composer Invocation。
- PromptTemplate Expansion 被接受后，Provider 启动失败会按照数据存储设计保留
  展开的 User Message，并追加有界、已脱敏的 Failure。

Composer Recall 可以在进程内历史中保留已提交 Invocation，但它不属于 Session
History。Sensitive Command Argument 绝不进入 Telemetry。Telemetry 只允许记录
Allowlist 中的 Built-in Name 或 `user`/`workspace`/`mcp` 等粗粒度 Source，不能
记录 Raw Argument、Template Content、Absolute Path 或 Credential。

## 10. Prompt Command 扩展契约

动态 Prompt Command 要等 Built-in Catalog 稳定后再实现。目标 Source 为：

```text
~/.latte/latte-code/commands/<name>.md
<workspace>/.latte/commands/<name>.md
```

可以先支持 User Command。Latte Code 具备显式 Workspace Trust Decision 之前，
Workspace Command 必须保持 Hidden。仅仅打开 Repository 不代表同意加载其中的
Prompt Command。

扩展要求：

- 只允许 UTF-8 Regular File，并以 No-follow 方式打开。
- 最多发现 128 个 Command，每个 Source File 最大 64 KiB。
- Front Matter 严格定义 Name、Description、Argument Hint 与可选 Model Policy；
  未知字段直接拒绝。
- Built-in Name 与 Alias 保留，不能被 Shadow。
- 动态 Source 之间任何 Canonical Name 或 Alias 冲突都会禁用冲突 Command，并
  报告本地 Diagnostic；Source Order 不能静默选择 Winner。
- `$ARGUMENTS` 和有界 `$1` 至 `$9` 只执行纯文本替换，并使用跨平台一致 Lexer。
- Shell Block、Command Substitution、Environment Expansion、Command Root 之外的
  Include 与 Executable Hook 一律拒绝。
- Catalog Reload 必须原子化。非法 Dynamic File 不能移除 Built-in，也不能只
  替换上一份有效 Catalog 的一部分。

MCP Prompt 与 Skill 未来可以适配为 `PromptTemplate` Descriptor，但必须携带
Source Badge、显式 User Invocation Capability、有界 Content，以及相同的冲突与
Trust Check。它们不会获得新的 Execution Kind。

## 11. 初始命令清单

第一批产品能力要先完成 Session 闭环，而不是优先选择当前 Palette 中最容易复用
的命令：

| Command | Alias | Kind | Availability | Mapping |
| --- | --- | --- | --- | --- |
| `/new` | – | `LocalUi` | 不存在 Active Run 或 Blocking Request | 切换到瞬态 `NewSessionDraft`，在第一条 Prompt 被接受前不创建 Durable Session。 |
| `/sessions [query]` | `/resume` | `TypedAction` 加本地 Picker | 不存在 Active Run 或 Blocking Request | 无参数时加载并打开当前 Workspace 的 Session Picker；携带 ID 或标题 Query 时直接解析并打开该 Session。 |

`/new` 不会 Clear、Archive 或 Delete 当前 Session。TUI 需要显式的 Active
Conversation Target，例如：

```rust
pub enum ActiveConversation {
    NewSessionDraft,
    Session(ThreadId),
}
```

进入 `NewSessionDraft` 只清空新 Draft Composer 与本地 Selection State；原
Session 仍然可以发现。它的第一个 Prompt 使用正常 Start Path，并遵守数据存储
设计的 Commit Point：Prompt 通过本地校验并被接受时，先创建 Session Row 与 User
Content，再构造或调用 Provider。

`/sessions` 是规范 Discovery Command，`/resume` 是精确 Alias。无参数形式打开
由当前配置的 SQLite 数据库提供、按 Canonical Workspace 过滤的有界 Picker。Row
只包含 Title、Workspace、Lifecycle、Provider、Model 与 Timestamp；只有用户选中后
才加载 Transcript Row。可选参数先尝试精确 Session ID，再跨完整 Workspace Catalog
执行 Exact Title Query，而不是只搜索最近的 Picker Page。Title 存在歧义时继续停留
在 Picker，不能静默选择。

选中 Row 后发出显式 Typed Open/Resume Action。它重新加载所选 Session 的 JSONL
Conversation 与 SQLite Control Projection，不调用 Provider，也不增加 Conversation
Entry。如果原 Workspace 不可用，则按照既有数据存储 Rebinding Contract，要求
用户显式选择有效 Workspace。

Background Session Ownership 完成设计前，Active Run 或 Permission、Input、
Reconciliation Request 拥有交互时，这两个命令都保持 Disabled，绝不隐式 Detach
Active Run。

下面的命令可以随后复用同一个 Catalog，但它们不再定义首个产品里程碑：

| Command | Alias | Kind | Mapping |
| --- | --- | --- | --- |
| `/help` | – | `LocalUi` | 打开现有 Help Overlay。 |
| `/navigation` | `/nav` | `LocalUi` | 进入 Transcript Navigation。 |
| `/refresh` | – | `TypedAction` | 发出 `RefreshSnapshots`。 |
| `/cancel` | – | `TypedAction` | 对可取消 Active Run 发出 `Cancel { thread_id }`。 |
| `/quit` | `/exit`、`/q` | `TypedAction` | 发出 `Quit`。 |

后续 Built-in 可以增加 `/status`、`/fork`、`/rename`、`/compact`、
`/permissions` 和 `/diff`，但前提是每个命令都有 Typed Service Contract 与
Lifecycle Policy。Prompt Expansion 可用后，`/init` 和 `/review` 应作为第一批
Built-in `PromptTemplate` Command。

这份清单表示交付顺序，不是兼容性承诺。只有完成实现、文档和 Final-Binary E2E
覆盖后，Command Name 才成为 Public Contract。

## 12. Module Placement

目标实现按以下方式保持职责收敛：

```text
latte-tui/src/command.rs
  built-in identifiers, descriptors, parser, catalog matching, availability

latte-tui/src/thread.rs
  popup state, ActiveConversation, Session picker, reducer integration,
  rendering, typed action emission

latte-core/src/command.rs             (future PromptTemplate phase)
  secret-free dynamic descriptor and PromptCommandId wire types

latte-headless/src/command.rs         (future PromptTemplate phase)
  trusted discovery, validation, pure bounded template expansion

latte-code/src/lib.rs
  composition-root mapping from explicit ThreadUiAction variants to services,
  typed global Session catalog/open adapter
```

`latte-engine` 中不存在 Slash Command Router。Privileged Work 继续留在现有
Engine API 与 Typed Service Method 中。

## 13. Error 与恢复行为

- Parse 或 Argument Validation Error 会逐 Byte 保留 Draft，并显示有界本地消息。
- Command 在 Popup Selection 与 Enter 之间变为 Disabled 时，第二次 Availability
  Check 会在本地拒绝执行。
- Local UI Command 失败不能关闭无关 Overlay，也不能改变 Durable State。
- Typed Action 失败使用现有 Secret-safe `ThreadUiFeedback` Channel，并在需要时
 重新加载 Authoritative Snapshot。
- PromptTemplate Load 或 Expansion 失败会恢复 Invocation，并且不调用 Provider。
- PromptTemplate 一旦转化为普通 Submitted Prompt，恢复就完全遵守普通
  Start/Follow-up Contract；Command Code 不得推断成功。

所有 Error Text 在展示前都要过滤控制字符并限制长度。

## 14. 必须验证的场景

UT 至少覆盖：

- Candidate Parsing、Multi-line Argument、Alias 与 Command Name Limit。
- 未知 Slash Text 与 Absolute Path 继续作为普通 Prompt。
- Exact Recognition 与 Fuzzy Suggestion 相互独立。
- Deterministic Ranking 与 Filter 后稳定 Selection。
- Catalog Collision、保留 Built-in Name、Source Badge 与 Atomic Reload。
- Hidden/Disabled/Enabled Availability 与 Dispatch-time Revalidation。
- Argument Policy，以及所有 Validation Failure 的 Draft Preservation。
- PromptTemplate Resolution 不执行 Shell、Environment 或 Include Expansion。
- Unicode Display Width、Narrow Terminal Popup Layout 与 Bounded Rendering。
- Permission、Input Request 与 Reconciliation Event Ownership。

Final-Binary E2E 至少覆盖：

- 输入 `/` 会显示 Command Popup，且不会调用 Provider。
- `/new` 保持原 Session 不变、切换到空的瞬态 Draft，并且在第一条 Prompt 通过
  本地校验并被接受前不创建 Persistent Session。
- `/sessions` 从 Workspace Catalog 读取有界 Metadata，且不会加载每个 Transcript 或
  调用 Provider。
- `/resume <session-id>` 打开精确 Session；Title Match 存在歧义时停留在 Picker，
  要求用户显式选择。
- Session Selection 会重新加载 JSONL Content 与 SQLite Control State；缺失
  Workspace 时要求显式 Rebinding。
- Active Run 或 Blocking Request 存在时，`/new` 与 `/sessions` 会拒绝执行且不
  丢失 Draft。
- `/help` 在没有 Session 时可用，并保持 Persistent Storage 为空。
- `/refresh` 经过 Projection Adapter。
- `/cancel` 对 Active Run 使用 Typed Cancellation Path。
- `/quit` 退出并恢复 Terminal Mode。
- `/tmp/file` 与未知 Slash Prefix 作为普通 Prompt 提交。
- 非法参数保留 Composer，并且不调用 Provider。
- Slash Command 不能批准 Permission 或确认 Unknown Effect。
- 未来 PromptTemplate 发送精确展开后的 Prompt，其后续 Tool Request 仍经过
  Engine Permission 与 Effect Gate。
- Popup Rendering 与 Keyboard Behavior 在 Linux、macOS 和受支持的 Windows
  Terminal Harness 上通过。

这些测试属于仓库现有的独立 UT 95%、Final-Binary E2E 90% 和 All-Target 90%
覆盖率卡点。

## 15. 当前实现状态

`latte-tui::command::BUILTINS` 是 Composer Slash Resolution 与 `Ctrl+P`
Palette 共用的唯一 Catalog。它只保存 Identifier 与无密钥 Metadata，不携带
任意 Callback 或 Engine Capability。

| 命令 | Alias | Kind | 当前结果 |
| --- | --- | --- | --- |
| `/new` | – | `LocalUi` | 启动新的瞬态 Draft。 |
| `/sessions [id 或精确 title]` | `/resume` | `TypedAction` | 打开 Session Picker，或恢复唯一精确匹配。 |
| `/model` | – | `TypedAction` | 打开可搜索、按 Provider 分组的 Model Picker，并切换下一个 Child 使用的 Binding。 |
| `/help` | – | `LocalUi` | 打开键盘帮助。 |
| `/navigation` | `/nav` | `LocalUi` | 进入 Transcript Navigation。 |
| `/refresh` | – | `TypedAction` | 重新加载权威 Projection。 |
| `/quit` | `/exit`、`/q` | `LocalUi` | 退出 Terminal UI。 |

只有第一个 Byte 是 `/` 的 Composer Text 才是 Command Candidate。Built-in
Name 和 Alias 使用小写 ASCII 并精确匹配；Argument 保留内部换行，只裁剪外层
空白。已知命令携带禁止的 Argument 时产生本地 Validation Error 并保留 Draft；
未知或非法 Candidate 仍是普通 Prompt。Local Command 不调用 Provider，也不把
Command Text 写入 Transcript。

TUI 每次启动都进入全新的瞬态 Draft。已有 Session 只会在用户显式执行
`/sessions` 或 `/resume` 后载入；无参数 `/sessions` 打开 Picker，携带参数时解析
UUID 或精确 Title。不会打开持久化 Workspace 不同的 Session。存在 Submission、Active Child、Pending
Request 或 Reconciliation 时禁用 Session 切换，并在 Dispatch 时再次检查
Availability。`Failed` 与 `Interrupted` 已没有 Active Child，可以执行 `/new` 或
`/sessions`。
`/model` 会按 Provider 分组展示各自完整的 `models` 目录。每个 map key 是实际
发送给该 Provider 的模型 ID；可选 `name` 只用于展示与搜索，嵌套 `options` 由对应
Provider 实现强类型解析，因此不同 Provider 不必共享 Options key。选中 Options
会固定进 Binding，展示名不会。唯一的全局 `default_model` 使用 `provider/model`
标识并标记初始选择，Provider 自身没有默认模型。New Draft
仅在本地保留选择直到 Start；Ready Session 会先持久化精确 Provider/Model Binding
与一条 System Card，Composer 在权威 Snapshot 确认切换前保持锁定。Credential
仍只在下一个 Child 启动时解析，因此错误 Provider 会成为该 Child 的持久、可重试
Failure，而不是让 Picker 闪屏并吞掉提交。
在这两种 Terminal Session 中，普通 Enter 不会创建本地 Queue，也不会消费
Composer。若 Follow-up 在 Child Active 时进入 Queue，但 Child 在它被持久接受前
终结，则精确恢复 Draft。Submission Reconciliation 使用 Durable Card Source 与
已脱敏 Content，而不是只比较展示文本。

Composer 现在会针对 `/` 与单 Token 前缀打开锚定在 Composer 上方的 Built-in
Suggestion Popup。它按 Canonical Name 与 Alias Prefix 匹配，并以稳定的
Exact/Canonical/Alias 顺序排列；Up/Down 在有界结果内选择，Enter 复用正常的
Availability 二次检查后 Dispatch，Esc 只关闭 Popup 而不修改 Draft，后续编辑会
重新打开。Blocking Request State 继续拥有完整 Key Event；其他 Overlay 也会隐藏
Popup。

Fuzzy Matching、Dynamic Prompt Command、Workspace Command File、MCP Prompt、
Skill、可执行 Plugin、显式 Cross-workspace Rebinding 与 Slash `/cancel` 尚未实现。

Reducer Test 覆盖 Popup Filter、键盘选择、Dismiss、Blocking、Local、Typed 与
普通 Prompt Path、Alias、精确 Argument、禁用的切换和 Draft 保留。最终二进制
PTY E2E 覆盖 Popup Rendering、方向键 Navigation、Prefix Filter、Session 创建、
`/resume <thread-id>`、`/new`、Provider/Model 切换、下一次 Wire Request 使用所选
Model，以及纯本地路径不会触发额外 Provider Request。

## 16. 交付阶段

1. 增加单一 Built-in Command Catalog、Parser、Availability Evaluation、Alias，
   以及可以表示瞬态 New Session Draft 的显式 `ActiveConversation` Target。
2. 增加 Typed Session Catalog/Open Boundary 与有界 Session Picker；实现
   `/new` 和 `/sessions`，并把 `/resume` 作为 Alias。
3. 增加覆盖 New、Discovery、Direct Resume、Workspace Rebinding、Active Run
   Blocking 与跨平台 Popup/Picker Rendering 的 Final-Binary E2E。
4. 把现有 Help、Navigation、Refresh、Cancel 与 Quit Action 收敛进同一个 Catalog
   和 Slash Popup。
5. 增加可信 User PromptTemplate Discovery 和纯文本有界 Expansion；在普通
   Prompt Path 上实现 `/init` 与 `/review`。
6. 增加 Workspace Trust 与 Workspace PromptTemplate Source。
7. 只有在 Provenance、Collision、Limit、Persistence Metadata 与 E2E Gate 完成
   后，才评估 MCP Prompt 与 Skill Adapter。
