# Effect Authority、策略与隔离

状态：**设计中，尚未实现。**

English counterpart: [Effect authority, policy, and isolation](../../../en-US/design/agent-harness/effect-authority-and-policy.md).

## 1. 决策

`latte-engine` 是唯一能观察或改变外部世界的 authority。Provider、headless agent loop、TUI、slash command、extension 和 delegated agent 只可提交 typed request 并读取 redacted projection；它们不能获得 SQLite writer、workspace directory capability、process spawn 或 general shell capability。

## 2. Effect 协议

每个 effect 绑定 `thread_id`、`run_id`、精确 Run revision、lease fencing token、输入 digest 和单次 approval。状态机固定：

```text
Declared -> Prepared -> Started -> ObservedSuccess | ObservedFailed | Unknown
```

engine-private table 保存 executable descriptor、precondition 与 observation detail；公开 transcript/event 仅有 redacted operation、target、scope、result summary 与 stable effect ID。`Prepared` 在执行前持久化；approval 消费与 `Prepared -> Started` 必须在同一数据库事务。

## 3. Policy、批准与取消

policy 在执行前 fail closed：allow、require one-time approval 或 deny。approval card 绑定精确 descriptor digest，不能由 Enter、stale UI、不同 revision/lease/effect 重用。permission answer 是 runner control input，但最终验证与消费仍在 engine。

取消只能请求停止可取消的 Provider/process work，不能推断 effect 没发生。process、filesystem 或 network observation 不明确时为 `Unknown`；reconciliation 独立、显式、可审计，不能被自动 retry 或后续 prompt 绕过。

## 4. 隔离与验收

filesystem 必须通过已持有的 workspace-relative capability，拒绝 path escape、link replacement 与不支持的安全原语。process argv-first；显式 shell 是独立高风险 action。输出、timeout、取消和 process-group supervision 有上界；无完整监督的平台在 `Started` 前失败关闭。Provider output、repo/tool/reminder text 都是不可信数据，不能改变 classification、approval scope、workspace root 或 policy。

- UT：所有状态转移、重复 command、过期 lease、错误 revision 与重复 approval 都 fail closed；descriptor 不经 snapshot/event/log/transcript 泄露。
- UT：取消、timeout、observer failure 有不同可证明状态，绝不猜测成功。
- E2E：最终二进制展示精确 approval；拒绝、approval replay、path/link attack 与中断均不能执行未授权 effect。
