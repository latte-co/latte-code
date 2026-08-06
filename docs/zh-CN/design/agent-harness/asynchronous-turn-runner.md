# 异步 Turn Runner 与 Session Mailbox

状态：**部分实现：用户 Prompt Runner 与 Mailbox 已启用。**

Runtime 现在保证每 Session 一个进程内 Runner、八条 FIFO 用户 Prompt Mailbox，
不同 Session 使用独立 Runner。Active Child 期间 TUI 按 Enter 会入队，Runner 只在
下一个 `accepts_follow_up` 边界物化输入。可信 Reminder、Input Sequence/Progress
类型、去重/过期与显式 Steer 仍属于提案范围。

English counterpart: [Asynchronous turn runner and session mailbox](../../../en-US/design/agent-harness/asynchronous-turn-runner.md).

## 1. 决策

一个 Session 同时只能有一个 agent loop，但 loop 必须异步运行。TUI、CLI 或可信
运行时来源可在运行期间提交新用户输入或 reminder；输入进入每 Session 一个有界
mailbox，由 runner 在安全边界消费。

异步不表示同一 Session 并发发起多个 Provider Request，也不表示修改已发出的 HTTP
stream。不同 Session 可以并发；单 Session 的 Provider context、Run revision、tool
round 和 JSONL 顺序始终串行。这演进 v2 `ThreadRuntimeService` 的进程内 active map
及 TUI 的单条 follow-up 槽位，不改变 v1 协议。

## 2. 责任与输入

`latte-headless` 拥有 `TurnSupervisor`：构造 Provider history、驱动 stream、协调
tool continuation 并消费 mailbox。它没有直接 filesystem、process、SQLite 写入或
approval 消费能力；这些继续经 `latte-engine` 的受限句柄完成。`latte-engine` 仍是
lease、Run revision、Effect、Permission 与 durable projection 的权威；`latte-tui`
只维护 composer/queued 展示并提交 typed command。

```rust
enum RuntimeInput {
    UserPrompt { input_id: InputId, text: String },
    TrustedReminder {
        input_id: InputId,
        source: ReminderSource,
        text: String,
        dedupe_key: Option<String>,
        expires_at_ms: Option<u64>,
    },
}

enum ControlInput {
    Cancel,
    PermissionDecision { request_id: RequestId, decision: Decision },
    RequestedInputAnswer { request_id: RequestId, value: String },
}
```

只有 composition root 注册的来源能创建 `TrustedReminder`。它带来源、大小限制、
可选去重键与过期时间；TUI、Provider、工具输出和扩展文本不能把任意字符串伪装成
高权限 reminder。

## 3. 顺序与安全注入

每个 mailbox entry 有单调 `input_seq`。用户输入严格 FIFO；有效 reminder 也按接受
顺序使用同一数据队列。只有控制输入可以越过它：取消、权限答复和显式 input-request
答复必须及时唤醒 runner。

数据输入只可在下列安全点进入 Provider history：当前 Provider outcome 已完整组装、
一轮 tool result 已全部观察并入上下文、没有 `Started` effect（或它已到可恢复的观察
状态）、且没有 pending permission/input/reconciliation。故 tool effect 运行中收到的
prompt 不会改变 descriptor、approval、revision 或执行顺序，而会保持 `Queued`。

普通 Enter 使用 `Queue`。未来可提供显式 `Steer`：仅在 Provider streaming 且没有
`Started` effect 时，取消当前 stream、丢弃 partial delta，再以最后确认的 context 和
队首输入继续。`Steer` 不会隐式取消外部 effect；停止 effect 必须显式 `Cancel`，并
遵守 `Unknown` 与 reconciliation 契约。

## 4. 生命周期、容量与恢复

mailbox、partial delta、stream handle、timer 与 cancellation token 都是进程内状态。
接受 entry 时 TUI 可显示 `InputQueued` progress，但不写 JSONL、SQLite 或 telemetry。
Runner 在安全注入点消费 Entry 时，会先按会话存储的物化点追加精确输入并创建
Child Run/Control State，再构造或调用 Provider，完整 Outcome 随后追加。Provider
启动失败会向这个已接受的 Conversation Record 追加有界、已脱敏的 Failure；满队列、
过期 Reminder 或消费前进程退出不会创建 Record。

容量当前固定为八条，单 Entry 字节数复用现有 Thread Input/History Budget；满时返回无密钥
`MailboxFull` 并保留 composer，绝不丢弃旧输入。只有当前 Session lease owner 可
运行 supervisor；失去 lease 后停止消费、取消可取消 Provider work、关闭 writer，已
`Started` effect 交由 engine 记录 `Unknown` 并 reconciliation。未物化 mailbox 在
进程崩溃时可丢失，这是不持久化 Provider attempt/Draft 的有意取舍。

`ThreadSnapshot` 保持 durable lifecycle 权威。`ThreadTransientProgress` 可显示
`queued`、`consumed`、`expired`，但事件缺口或重连后 TUI 必须丢弃这些瞬态条目并重新
读取 snapshot。

## 5. 验收

- [x] UT：同一 Session 至多一个 Runner；不同 Session 可并行。
- [x] UT：Scripted Provider 验证用户输入 FIFO 与容量拒绝。
- [ ] UT：Fake Clock 与 Scripted Provider 验证 Reminder 去重/过期、
  取消优先级和安全注入点。
- [ ] UT：tool/effect 运行时的输入不能修改 prepared descriptor、approval digest 或 Run
  revision。
- [x] E2E：最终二进制在 Provider stream 期间接受第二条 prompt，并在下一个安全 request
  中恰好发送一次。
- [ ] E2E：tool 执行中收到 reminder，只能在 tool result 后注入；取消和 Provider 启动
  失败都不产生虚假 Transcript Entry，其中 Provider 启动失败会无重复地保留已接受
  User Entry 与一个有界 Failure。
