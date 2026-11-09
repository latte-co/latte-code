# Agent Runtime 运行契约

状态：**描述已实现行为。** 本文合并了原 `agent-harness/` 设计方案中仍然成立的
部分（异步 Turn Runner、Session 存储与恢复、Effect Authority、事件与投影、
TUI Runtime、验证 Harness）。未实现的提案（可信 Reminder Producer、Steer、
durable inbox、扩展 Registry、delegated child Turn、cassette replay、live canary
等）不再作为设计文档维护，以 [Roadmap](../roadmap.md) 的未完成条目为准。
存储细节的权威是 [全局 Session 与数据存储](data-storage.md)。

## 1. 异步 Turn Runner 与 Mailbox（已实现）

- 每个 Session 同时至多一个 agent loop（进程内 Runner）；不同 Session 可并发。
- 每个 Session 一个有界 FIFO 用户 Prompt Mailbox（容量 8）。Active Child 期间
  TUI 按 Enter 入队，Runner 只在下一个可运行边界物化输入；满队列返回无密钥
  `MailboxFull` 并保留 composer，绝不丢弃旧输入。
- mailbox、partial delta、stream handle、timer 与 cancellation token 都是进程内
  状态；未物化 mailbox 在进程崩溃时可丢失，这是不持久化 Provider attempt/Draft
  的有意取舍。权限门停泊（park）的 turn 保留 mailbox 条目，恢复路径按队列采纳。
- 只有当前 Session lease owner 可运行 Runner；失去 lease 后停止消费、取消可取消
  的 Provider work、关闭 writer，已 `Started` effect 交由 engine 记录 `Unknown`
  并 reconciliation。
- 数据输入只在安全点进入 Provider history：当前 Provider outcome 已完整组装、
  tool result 已全部观察、没有未决 `Started` effect 与 pending
  permission/input/reconciliation。进行中的 effect 不会因排队输入改变 descriptor、
  approval、revision 或执行顺序。

## 2. Session 存储与恢复（已实现）

权威契约见 [全局 Session 与数据存储](data-storage.md)。运行时要点：

- 内容以每 Session 一份 append-only JSONL 保存；SQLite 只保存可查询 metadata 与
  需要事务/CAS 的 control state。二者都在用户全局 Latte Code home，绝不在
  workspace；workspace 配置不能重定向全局存储。
- 接受即物化：Prompt 通过本地校验被接受时，Session/Turn 与精确 User Content 在
  解析 Credential、构造 Provider 或发起网络 I/O 之前持久化。接受后的 Provider
  启动失败保留 User Card 并追加有界、已脱敏 Failure，可重试；Credential 与原始
  Provider Diagnostic 绝不持久化。
- 启动时只裁剪一条撕裂 JSONL 尾行；`materializing` 残行按有效文件修复或删除。
  `Started` 而观察不确定的 effect 必须为 `Unknown`，只能经显式 reconciliation 终结。

## 3. Effect Authority、策略与隔离（已实现）

- `latte-engine` 是唯一特权 Effect 权威；Provider、headless loop、TUI、slash
  command 只可提交 typed request 并读取 redacted projection，不能获得 SQLite
  writer、workspace directory capability、process spawn 或 general shell capability。
- Effect 状态机：`Declared -> Prepared -> Started -> ObservedSuccess |
  ObservedFailed | Unknown`（`Declared` 仅测试路径使用）。执行前持久化意图与
  pre-effect hash；approval 消费与 `Prepared -> Started` 在同一事务。精确
  descriptor 只存 engine 私有表；公开 transcript/event 仅有 redacted operation、
  target、scope、result summary 与 stable effect ID。
- approval 精确绑定 descriptor digest、turn revision、lease fencing token，
  单次使用；Enter、stale UI、不同 revision/lease/effect 都不能重用。
- 取消只停止可取消的 Provider/process work，不推断 effect 未发生；观察不明确即
  `Unknown`，reconciliation 独立、显式、可审计，不能被自动 retry 或后续 prompt
  绕过。
- filesystem 经 workspace-relative capability 执行，拒绝 path escape、link
  replacement；process argv-first，显式 shell 是独立高风险 action；输出/超时/
  取消/进程组监管有上界，无完整监督的平台在 `Started` 前失败关闭。Provider
  output、repo/tool 文本都是不可信输入。

## 4. 事件、投影与回放（已实现）

- durable domain event 由 engine 与 state projection 在同一事务写入，按 Session
  递增 sequence；transient progress 只描述 streaming/spinner 等进程内展示。TUI
  不得把 transient event 当作权威 state。
- Turn/Effect/Permission/Transcript 更新、revision 递增与 event append 原子提交；
  重复 source key 只产生一次状态变化与一次可见 event。
- event buffer 有界；slow consumer、reconnect 或 sequence gap 时，前端丢弃
  transient state 并重读权威 snapshot/page。
- 回放从 JSONL 构造 model-visible history，从 SQLite 构造 control state；回放不
  调用 Provider、不执行 tool、不重新消费 approval。

## 5. TUI Runtime（已实现）

- `latte-tui` 是纯 presentation projection 与 reducer：不解析 credential、不创建
  Provider、不访问 SQLite、不读写 workspace、不执行 effect，也不把 key event 变成
  未类型化 engine call。composition root 提供 typed command sink、snapshot
  loader、event source、clock、terminal backend 与 shutdown handle。
- 三层结构：Reducer（纯函数）→ Runtime adapter（crossterm input、timer、订阅、
  terminal lifecycle，不含产品决策）→ Renderer（只读 model 绘制 Ratatui frame，
  不得 I/O、spawn 或阻塞）。
- permission、input-request 与 reconciliation card 拥有完整 key event，不能被
  slash popup、Enter 或 stale overlay 越权处理；adapter 在 event gap/reconnect
  后清空 transient progress 并重读 snapshot；所有退出路径恢复 raw mode、
  alternate screen、keyboard enhancement 与 cursor state。
- 输入按 grapheme cluster 编辑，CJK/emoji 按终端显示宽度计算；来自
  Provider/tool/error 的文本在 layout 前过滤控制字符、限制长度并遵守 redaction；
  小终端退化布局而非隐藏 composer、pending permission 或退出路径。

## 6. 验证 Harness（已实现）

- 测试 harness 是可替换的 production-boundary dependency 与 fixture，不是另一套
  runtime，也不是绕过 `latte-engine` 的后门。它驱动最终 `latte-code` binary、
  scripted Provider、临时 state home、SQLite、event subscription 与真实 PTY，
  无公网、无真实 credential、有界、可清理。
- test double 遵循与 production 相同的 schema、policy 与 capacity；fixture 只用
  loopback `127.0.0.1` Provider，严格断言 model、messages、tools、请求顺序与
  精确次数，绝不录制 Authorization 或 secret。
- 分层与覆盖率门禁、场景编写规则见 [E2E 编写手册](../testing/e2e-authoring-guide.md)。

## 关联文档

- [全局 Session 与数据存储](data-storage.md)
- [Rust 架构](architecture-overview.md)
- [UT / E2E 编写手册](../testing/e2e-authoring-guide.md)
- [能力 Roadmap](../roadmap.md)
