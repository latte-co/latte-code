# Session 存储与恢复

状态：**已实现。** 全局 SQLite + 每 Session JSONL 的存储模型已落地，详见 [全局 Session 与数据存储](../data-storage.md)。


## 1. 决策

Session 是用户可见的 conversation；内容以 JSONL 只追加保存，SQLite 只保存可查询
Session metadata 与需要事务/CAS 的运行 control state。二者都在用户全局 Latte Code
home，而不在 workspace。workspace configuration 可以改变行为，但不能决定 history
和数据库位置。

这取代当前按 workspace `.latte/latte-code.db` 的实现方向。迁移期间 `ThreadId` 就是
Session ID，不引入第二套 conversation identity。

## 2. 存储模型

```text
$LATTE_CODE_HOME/
  state.db
  sessions/
    <canonical-workspace-key>/<session-id>.jsonl
```

SQLite 至少保存 Project、Workspace、Session、Run、Effect、Permission、Lease、
Checkpoint、Evidence 和 deduplication key。它保存 title、最近活动时间、无密钥 binding
fingerprint 与 JSONL 定位信息，但不复制 conversation transcript。

JSONL 首行是最小自描述 header；后续只追加有稳定 `entry_id`、单调 `seq`、可选
`run_id` 与有界内容的 `message`、完整 tool-call/tool-result、checkpoint 或 compaction。
它不保存 credential、request header、raw Provider error、partial delta、cancellation
token 或 engine-private effect descriptor。

## 3. 物化和恢复

新 Session 与 follow-up 在本地校验阶段保持为内存 Draft。Prompt 一旦被接受，其
Session/Run 与精确 User Content 会在解析 Credential、构造 Provider 或发起网络
I/O 之前持久化。

接受提交就是物化点：

1. 插入不可发现的 `materializing` Session metadata；
2. 追加并 sync JSONL header 与已消费输入；
3. 创建 Child Run/Control State 并将 Session 标为可发现；
4. 追加完整 Provider Outcome 或有界、已脱敏的 Failure Card。

接受前的 Validation 或 Storage Failure 不创建 Session/Run Row 或 JSONL，并精确
恢复 Draft。接受后的 Configuration、Credential、Model、Authentication、
Transport 或启动失败会保留 User Record，并追加已脱敏 Failure Record。Provider
构造失败可重试；原始 Provider Error 与 Credential 绝不持久化。

启动时只能裁剪一条撕裂的 JSONL 尾行，绝不改写有效 history；随后从 header 修复
catalog，或删除没有有效 JSONL 的 `materializing` row。已 `Started` 而观察不确定的
effect 必须为 `Unknown`，只能经 reconciliation 终结。

## 4. 并发、隐私与验收

canonical workspace root 决定 bucket key。不同 Git worktree 有独立 Workspace record，
即使关联相同 Project。每 Session 一把 lease、一个 writer 和递增 fencing token；接管
会推进 token，旧 owner 不能开始或观察 effect。

Session mailbox、Provider stream 和 retry 为进程内 runtime。将来若要在崩溃后保留未
消费 prompt，必须单独设计 durable inbox、用户语义与清理策略；不能悄悄放进 JSONL
或 Provider Attempt。

- UT：workspace bucket、materialization crash point、撕裂尾行、lease takeover 与
  fencing 均确定性验证。
- UT：credential、Provider error、partial delta 和 descriptor 永不进入 JSONL。
- E2E：新进程可列出、打开、replay 完成 Session；Provider 启动失败会保留已接受
  Input、追加一个有界 Failure 并允许重试；`Started` Effect 后中断只能显示
  Reconciliation。
