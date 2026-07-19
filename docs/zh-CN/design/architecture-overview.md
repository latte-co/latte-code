# Rust 架构

完成操作在持有引擎 operation gate 时执行稳定的 A/B 工作区快照。快照 B 是进入原子数据库提交前的验证线性化点。Handoff 会保存 B 的 manifest 摘要与验证时间；B 之后发生的变化不属于已验证内容，下游可通过重新计算 manifest 检测漂移。符号链接以拓扑记录参与摘要，不安全或采样期间不稳定的链接会失败关闭。

工作区 manifest 的路径和符号链接 target 必须是有效 UTF-8。无法表示为 UTF-8 的操作系统路径字节会失败关闭，绝不会通过替换字符折叠成有损的 manifest key。
Manifest key 使用 JSON 序列化精确的 UTF-8 路径 component 数组，不会归一化分隔符。因此 Unix 文件名中的字面反斜杠与嵌套路径始终不同。

English counterpart: [English architecture](../../en-US/design/architecture-overview.md).

## 组成

```text
latte-code CLI/TUI
  |-- latte-tui -------- 投影与类型化 UI 动作
  |-- latte-headless --- Provider、上下文、Agent Loop 与验证
        |-- latte-engine -- 存储、策略、Effect、工具与进程
              |-- latte-core -- ID、命令、事件与状态迁移
```

`latte-core` 不依赖存储、Provider 或 UI。`latte-engine` 是所有特权副作用的唯一执行主体，只暴露受限句柄。前端读取持久化投影并提交类型化命令，不直接修改仓库或运行时状态。

`latte-code` 按顺序递归合并内置默认值、`$HOME/.latte/latte-code.jsonc` 和工作区 `.latte/latte-code.jsonc`；配置文件缺失是合法状态，相同 key 由工作区值覆盖。新运行使用命名的默认 Provider，CLI 与 TUI 共用 headless Provider 注册表。Thread binding 会在解析 secret 或发送 history 前，结构化固定所有 v1 语义 binding 字段（包括 alias），以及非 secret credential reference、credential generation 与 data scope；不匹配会关闭失败，避免静默漂移。

配置 `streaming` 后，OpenAI Chat Completions 支持有边界的 SSE：可处理 CRLF、注释、多 data 行、UTF-8 分块、tool 聚合、`[DONE]` 和取消。只有真实 delta 会被呈现；内联 JSON 只呈现最终结果，不伪造 delta。仅当 streaming 请求以零 body 的不支持响应失败时，才允许一次非流式回退；Responses API 仍不在范围内。

## 持久化状态

CLI 和 TUI 共用一个产品级 SQLite 数据库：`$HOME/.latte/latte-code/state.db`；当 `LATTE_CODE_HOME` 是绝对路径时，使用 `LATTE_CODE_HOME/state.db`。Workspace 只提供 Provider 和 Verification 配置，绝不拥有产品状态。SQLite 启用 WAL、外键、busy timeout 与 full synchronous。单写入者原子提交事件、revision 和 projection，命令 ID 在重启后仍可去重。遗留的 `database.path` 仅为迁移兼容而接受，不能选择状态数据库。

Thread v2 是增量协议：v1 的 `RunState`、命令、事件和协议版本保持字节兼容。迁移 7 新增独立的 `threads_v2`、关联 child run、唯一的 `thread_active_runs_v2` active authority、可分页的类型化 transcript card、独立的 thread 事件流，以及脱敏后的 command/source 去重；迁移 8 新增仅 engine 可读的 canonical effect descriptor 表；迁移 9 新增有界 Session Catalog Metadata（`title` 和规范化的 `workspace_root`）。关联 child 不能调用 legacy transition、checkpoint、process 或 tool mutation API；其状态、事件与 transcript 必须通过带 lease/fence 的 `CommitThreadRunUpdate` 原子提交。旧 run 仍可由 CLI 读取，绝不回填为 thread。

运行写入必须携带有效 owner lease 与单调递增的 fencing token。lease 过期后即使同一 owner 重新获取也会推进 epoch；接管后旧 owner 不能再写入。

## Effect 与权限

Effect 状态为：

```text
Declared -> Prepared -> Started -> ObservedSuccess | ObservedFailed | Unknown
```

执行前持久化意图与 pre-effect hash。批准精确绑定 effect ID、run revision、lease 和请求摘要，且只能使用一次。消费批准和 `Prepared -> Started` 在同一事务完成。精确 descriptor 只保存在 engine 私有表；effect ledger、checkpoint、transcript、事件、Provider history 重建和 TUI 只能获得独立的脱敏投影。崩溃或观察不明确时记录 `Unknown`，必须先协调，不能猜测成功后自动重试。

文件工具执行工作区边界、deny glob、内容过期检查和精确 edit/create 约束。变更在支持的平台通过已持有目录句柄和同目录原子 rename 完成；缺少安全原语的平台会在消费权限和进入 `Started` 前失败。

## 进程监管

命令默认使用 argv，不经过 shell；shell 语法单独分类并视为高风险。stdout/stderr 并发读取且有容量上限，超时与取消具有有限 grace period。Unix 创建并监管整个进程组，依次发送 `TERM`、`KILL` 并确认子孙进程退出。当前非 Unix 平台在创建 effect 前 fail closed；CI 会在 Windows 运行 check、Clippy、UT、Contract、portable 最终二进制 E2E 和 release build 卡点，但不宣称支持 Windows 进程执行。

## Transcript 运行时

受限尺寸终端仍继续渲染产品布局，而不会用“请调整窗口”整屏替换：装饰性欢迎内容与次要元数据优先折叠，同时尽量保留 composer、阻断操作和可用的 transcript 行。

终端将一个聚焦 conversation 呈现为单一 transcript viewport，并固定显示多行 composer；不再提供 session 侧栏或 session overlay。空状态在宽且高的 viewport 中并排展示产品标识与已解析环境，在更小宽度下才有意堆叠；首次输入后，紧凑 header 展示已解析的工作区路径，不虚构 branch 或仓库指标。纯展示投影按 child run 对持久化 card 分组，并在存在公开 `tool_call_id` 时配对 tool-call/tool-result card。活动层级最多三层：真实的 run/status 标题、tool action 和可选 result detail；展开 detail 只展示有界且脱敏的 target/query/command 结构化元数据。权限展示从持久化的脱敏 descriptor 与摘要中分别派生 operation、target 和 scope。Completion card 携带脱敏 handoff payload，使 changed files 与 verification evidence 在重启后仍可由 TUI 展示。投影不渲染私有 checkpoint 数据或原始 payload JSON。Thread 投影携带最近的、最多 500 条 card，而不是最早的一页；若更早 card 被省略，transcript 会明确提示。

Composer 模式拥有包括 `q`、`s`、`j`、`k` 和 `?` 在内的全部可打印字符。Enter 发送非空 composer 或待补充输入，Shift+Enter 插入换行；空状态 composer 还会展示 Ctrl+Enter 这一兼容发送组合键，F5 则仍不对外展示。Unicode grapheme cluster 按整体编辑，CJK 与 emoji 的换行、对齐和光标位置按终端显示宽度计算。Ctrl+P 打开本地命令面板，其中 help、navigation、refresh 与 quit 复用对应快捷键使用的安全 UI 状态或类型化 action。终端会在支持时启用渐进式键盘消歧与 bracketed paste，不启用未使用的鼠标捕获，并在所有退出路径恢复原模式。按 Esc 才会显式进入 transcript Navigation 模式，此时 `j`/`k` 选择 action，PageUp/PageDown 滚动，Enter/Space 展开或折叠选中 action，可打印的 `q` 退出；F10 是两个模式都可使用的显式退出键。第一次 Ctrl+C 会中断活动任务并进入退出确认，2 秒内再次按 Ctrl+C 才退出；超时或按下其他键会解除确认。待决权限会以内联 card 展示有边界、过滤控制字符且脱敏的操作摘要（写入目标/内容意图、进程 argv/cwd 或读取/调用目标）。权限与 reconciliation 分支会消费完整 key event：只有精确聚焦请求上的 `d` 或 Ctrl+A 能作出决定，Enter 和 Shift+Enter 都不能批准、确认或修改任何文本缓冲。事件缺口或重连会清空瞬态进度，并由 projection adapter 内部重新加载权威 snapshot。事件循环仅在投影或本地状态变化后重绘。每个客户端最多保留一条本地 follow-up 队列，且只在新读取到 `Ready` snapshot 后派发。

当前 thread 协调器支持 Provider 对话、类型化 user/assistant/input/failure/completion card、不可变 follow-up child 和精确有界 history。Provider 发出的 tool-call 与 input-request ID 必须先满足小型 opaque 语法 `[A-Za-z0-9_-]{1,256}`，才可以成为持久化 source、request 或 deduplication key。Provider 和 TUI 没有直接 effect authority。Provider 请求的 v2 tool 只能由 engine 以带 fencing 的持久化 effect 执行：`Prepare -> Started -> Observe`。engine 会在执行前持久化 descriptor，在 Provider 调用或 effect 已 `Started` 时续租；重启或 `Started` 后失去 lease 会记录为 `Unknown`，且只能由显式 reconciliation 解决。TUI 将该确认作为独立流程：Ctrl+R 打开已脱敏的 unknown-effect card，Ctrl+A 确认；Enter 永远不能确认。确认会把 effect 记为失败，并终结精确关联的 child。

## Agent 运行时

Headless 运行时收集仓库上下文（包括 `AGENTS.md`），调用 OpenAI-compatible chat-completions Provider，通过 engine 执行类型化工具请求，运行配置的验证 argv，并持久化 evidence 与 handoff。需要验证的运行在验证失败、缺失或未执行时不能完成。

CLI 支持 `run`、`resume`、`show`、`list` 与版本化 JSON 输出。TUI 是同一 engine 状态上的 Ratatui 投影：通过 snapshot refresh 处理事件滞后，明确呈现权限、输入和 Unknown 状态，并在正常退出、错误、panic 与中断时恢复终端。

## 信任边界

- Provider 凭据只在内存解析，Debug 输出会脱敏。
- 仓库文本、模型输出与工具输出都是不可信输入。
- 模型不能直接调用文件系统或进程 API。
- 权限默认拒绝，普通 Enter 不代表批准。
- 恢复流程不会把缺失 evidence 当作成功。
