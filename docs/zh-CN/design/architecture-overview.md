# Rust 架构

完成操作在持有引擎 operation gate 时执行稳定的 A/B 工作区快照。快照 B 是进入原子数据库提交前的验证线性化点。Handoff 会保存 B 的 manifest 摘要与验证时间；B 之后发生的变化不属于已验证内容，下游可通过重新计算 manifest 检测漂移。符号链接以拓扑记录参与摘要，不安全或采样期间不稳定的链接会失败关闭。

工作区 manifest 的路径和符号链接 target 必须是有效 UTF-8。无法表示为 UTF-8 的操作系统路径字节会失败关闭，绝不会通过替换字符折叠成有损的 manifest key。
Manifest key 使用 JSON 序列化精确的 UTF-8 路径 component 数组，不会归一化分隔符。因此 Unix 文件名中的字面反斜杠与嵌套路径始终不同。

English counterpart: [English architecture](../../en-US/design/architecture-overview.md).

## 组成

```text
lattecode CLI/TUI
  |-- latte-tui -------- 投影与类型化 UI 动作
  |-- latte-headless --- Provider、上下文、Agent Loop 与验证
        |-- latte-engine -- 存储、策略、Effect、工具与进程
              |-- latte-core -- ID、命令、事件与状态迁移
```

`latte-core` 不依赖存储、Provider 或 UI。`latte-engine` 是所有特权副作用的唯一执行主体，只暴露受限句柄。前端读取持久化投影并提交类型化命令，不直接修改仓库或运行时状态。

## 持久化状态

每个工作区使用 `.latte/lattecode.db`。SQLite 启用 WAL、外键、busy timeout 与 full synchronous。单写入者原子提交事件、revision 和 projection，命令 ID 在重启后仍可去重。

运行写入必须携带有效 owner lease 与单调递增的 fencing token。lease 过期后即使同一 owner 重新获取也会推进 epoch；接管后旧 owner 不能再写入。

## Effect 与权限

Effect 状态为：

```text
Declared -> Prepared -> Started -> ObservedSuccess | ObservedFailed | Unknown
```

执行前持久化意图与 pre-effect hash。批准精确绑定 effect ID、run revision、lease 和请求摘要，且只能使用一次。消费批准和 `Prepared -> Started` 在同一事务完成。崩溃或观察不明确时记录 `Unknown`，必须先协调，不能猜测成功后自动重试。

文件工具执行工作区边界、deny glob、内容过期检查和精确 edit/create 约束。变更在支持的平台通过已持有目录句柄和同目录原子 rename 完成；缺少安全原语的平台会在消费权限和进入 `Started` 前失败。

## 进程监管

命令默认使用 argv，不经过 shell；shell 语法单独分类并视为高风险。stdout/stderr 并发读取且有容量上限，超时与取消具有有限 grace period。Unix 创建并监管整个进程组，依次发送 `TERM`、`KILL` 并确认子孙进程退出。当前非 Unix 平台在创建 effect 前 fail closed；CI 会编译检查 Windows，但不宣称支持 Windows 进程执行。

## Agent 运行时

Headless 运行时收集仓库上下文（包括 `AGENTS.md`），调用 OpenAI-compatible chat-completions Provider，通过 engine 执行类型化工具请求，运行配置的验证 argv，并持久化 evidence 与 handoff。需要验证的运行在验证失败、缺失或未执行时不能完成。

CLI 支持 `run`、`resume`、`show`、`list` 与版本化 JSON 输出。TUI 是同一 engine 状态上的 Ratatui 投影：通过 snapshot refresh 处理事件滞后，明确呈现权限、输入和 Unknown 状态，并在正常退出、错误、panic 与中断时恢复终端。

## 信任边界

- Provider 凭据只在内存解析，Debug 输出会脱敏。
- 仓库文本、模型输出与工具输出都是不可信输入。
- 模型不能直接调用文件系统或进程 API。
- 权限默认拒绝，普通 Enter 不代表批准。
- 恢复流程不会把缺失 evidence 当作成功。
