# TUI Runtime 契约

状态：**设计中，尚未实现。**

English counterpart: [TUI runtime contract](../../../en-US/design/agent-harness/tui-runtime-contract.md).

## 1. 决策

`latte-tui` 是纯 presentation projection 和 reducer。它不解析 credential、不创建 Provider、不访问 SQLite、不读写 workspace、不执行 effect，也不把 key event 变成未类型化 engine call。composition root 提供 typed command sink、snapshot loader、event source、clock、terminal backend 和 shutdown handle。

## 2. 三层结构

1. **Reducer**：纯函数；输入 key/mouse/resize、snapshot、durable event 和 transient progress，输出 local model 与 typed UI action。
2. **Runtime adapter**：拥有 crossterm input、timer、event subscription、snapshot refresh 和 terminal lifecycle；不含 product decision。
3. **Renderer**：只读 model 并绘制 Ratatui frame；不得 I/O、spawn 或阻塞。

外部能力经 trait 注入，使 fake event source、fake clock、TestBackend 或 VT100 terminal 能替换真实终端。

## 3. 交互与恢复

composer 在 Session running 时仍可编辑。提交 prompt 后显示 local queue state；真实执行顺序由 asynchronous turn runner 的 mailbox receipt 决定。permission、input-request 与 reconciliation card 拥有完整 key event，不能被 slash popup、Enter 或 stale overlay 越权处理。

adapter 在 event gap、subscription failure 或 reconnect 后清空 transient progress，重读 snapshot 与当前 transcript page；仅在 model 改变时 redraw。normal exit、error、panic、SIGINT 和 terminal suspend 的所有路径都恢复 raw mode、alternate screen、keyboard enhancement 与 cursor state。

## 4. 可访问性与验收

输入按 grapheme cluster 编辑，CJK/emoji 宽度按 terminal cell 计算。来自 Provider、tool、path 和 error 的文本在 layout 前过滤 control character、限制字节数并遵守公开 redaction。小终端应退化布局，而非隐藏 composer、pending permission 或 exit path。

- UT：reducer 的 keyboard/focus/overlay/queue/gap recovery/approval-negative path 无 terminal I/O。
- 渲染测试：TestBackend/VT100 断言窄屏、Unicode、长 transcript、resize、permission card 和 terminal restoration frame。
- E2E：真实 PTY 覆盖 composer、queued prompt、cancel、permission、event reconnect 与退出后终端复原；持续 drain 输出，以明确 readiness 而非 sleep 等待。
