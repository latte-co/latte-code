# 扩展与委派能力

状态：**设计中，尚未实现。**


## 1. 决策

扩展点是声明式 capability contract，不是把 callback、shell script 或任意 native plugin 装进 agent runtime。每项 capability 有 stable ID、version、provenance、visibility、input/output schema、resource bound 与 required effect class；composition root 在启动时构建 immutable registry。

第一阶段只允许 built-in typed tool、built-in slash action、可信纯文本 prompt command 和显式 Provider adapter。dynamic local code、shell interpolation、workspace 覆盖内建命令、未验证 MCP tool 和 executable plugin 均不在范围内。

## 2. 调用路径

```text
catalog descriptor
-> typed request
-> headless orchestration
-> engine policy / approval / effect
-> redacted result projection
```

catalog 只含 metadata，不能携带任意 executable callback。TUI popup 与 dispatch 使用同一 registry，并在 dispatch 时重查 availability。prompt command 只展开为 bounded text 并走正常 user submission；解析期间不读 file/environment/shell，也没有额外权限。非内建 capability 在 TUI、log 和 transcript projection 显示 provenance；名称冲突固定 reject 或显式消歧，绝不静默覆盖安全敏感内建项。

## 3. Delegation

delegated agent 是 primary Session 的受限 child Run，不是并发写同一 Session 的第二 owner。它有独立 input budget、deadline、cancellation token、tool allowlist 与 provenance，通过 engine 申请所有 effect，并以 bounded redacted result summary 回父 runner。父 runner 串行决定 start/await/merge/cancel。child effect 绑定自己的 Run revision、lease 与 approval，不能继承父 Run 已消费 approval。主 conversation 只追加用户可见 delegation summary，不写 private scratchpad、partial stream 或 credential。

## 4. 验收

- UT：registry schema/version/name conflict/provenance 及 build/dispatch 两次 availability 检查。
- UT：extension 无法获得 private descriptor、file handle、credential 或 general effect capability；child cancellation、resource limit、approval isolation 与 result boundary 均确定性验证。
- E2E：slash/prompt command/child 统一经过 Engine approval 与公开 projection；禁用或未知 capability 不能影响 Session。
