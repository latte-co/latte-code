# 事件、投影与回放

状态：**核心已实现。** Engine 事务事件、Thread event stream、snapshot reload 与有界 transient progress 已落地；离线回放与审计导出仍是目标。


## 1. 决策

Latte Code 区分 durable domain event 与 transient progress。前者由 engine 和 state projection 在同一事务写入，并按 Thread 递增 sequence；后者只描述 streaming、mailbox、spinner 等进程内展示。TUI 不得把 transient event 当作权威 state。

现有 `ThreadSnapshot.revision`、`sequence` 与 `ThreadEventEnvelope` 是目标协议基础；新事件不得重用或改变 v1 serialization semantics。

## 2. 事件契约

durable event 至少含 `event_id`、`thread_id`、sequence、关联 Run、时间、typed redacted payload 与 source key。Run/Effect/Permission/Transcript 更新、revision 递增和 event append 必须原子提交；重复 source key 只产生一次状态变化和一次可见 event。

transient progress 可携带 Provider delta、`InputQueued`、waiting state 与 local reconnect hint，但不得有 raw credential、private descriptor 或未完成 assistant content。event buffer 有界；slow consumer、reconnect 或 sequence gap 时 adapter 丢弃 transient state 并重读权威 snapshot/page。

## 3. 回放与审计

conversation replay 从 JSONL 构造 model-visible history；SQLite snapshot、effect ledger、checkpoint 和 evidence 构造 control state，event log 不替代 effect recovery。replay 不得调用 Provider、执行 tool 或重新消费 approval。它应能解释用户看到的内容及 effect 为何 `Unknown`，但不能导出 private descriptor 或 secret。

event retention 和 pagination 有上界。Session detail 优先读取 snapshot 与最近 transcript page；旧内容用 cursor 获取。subscription 只是加速刷新，不能是唯一 read path。

## 4. 验收

- UT：state/projection/event 原子性、source-key dedup、sequence 单调性和 pagination boundary。
- UT：reconnect、slow consumer、gap 与 malformed payload 不会让 TUI 保留错误 transient state；replay 无 Provider/Tool side effect 且能从 JSONL 加 SQLite 重建同一公开 snapshot。
- E2E：杀掉并重启 TUI/client 后，它以 snapshot 恢复 permission、input 与 Unknown-effect card，不依赖错过的 progress event。
