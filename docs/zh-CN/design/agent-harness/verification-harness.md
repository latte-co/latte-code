# 验证 Harness 与确定性测试

状态：**设计中，尚未实现。**

English counterpart: [Verification harness and deterministic testing](../../../en-US/design/agent-harness/verification-harness.md).

## 1. 决策

Harness 是可替换的 production-boundary dependency 和 test fixture，不是另一套 agent runtime，也不是绕过 `latte-engine` 的 test backdoor。它驱动最终 `latte-code` binary、scripted Provider、temporary global state home、SQLite、event subscription 和真实 PTY，同时保持无公网、无真实 credential、有界且可清理。

UT 95%、final-binary E2E 80%、all-target 90% 的独立 coverage floor 不变。本文定义达到这些门槛的底座；现有 testing gates 与 E2E authoring guide 继续是交付规则。

## 2. 可替换依赖

production composition root 应通过 trait 或显式 constructor 注入：Provider transport/stream 和可控 request recorder；clock、random ID、cancellation/timeout scheduler；runtime event source 与 snapshot loader；state-home/path resolver；terminal input/output/backend；以及只在受支持平台有真实实现的 process supervisor。

test double 必须遵循相同 production schema、policy 和 capacity；产品代码不能借 `cfg(test)` 接收更宽 command 或跳过 policy。fixture 只用 loopback `127.0.0.1` Provider，严格断言 model、messages、tools、request order 和精确次数；协议保真场景可使用 redacted cassette，但绝不录制 Authorization 或 secret。

## 3. 测试层

| 层 | 目标 | 主要替身 |
| --- | --- | --- |
| Core/Engine UT | state machine、policy、storage、Effect、recovery | fake clock/ID、temporary SQLite、constrained workspace |
| Headless UT | turn runner、history、Provider/tool 协调 | scripted Provider、event collector、fake timer |
| TUI UT/渲染 | reducer 与 cell-level 输出 | fake event source、TestBackend、VT100 |
| Final-binary E2E | 用户可见跨 crate 旅程 | loopback Provider、temporary home、真实 binary/PTY |

行为变更必须有最低责任层 UT 和至少一条 final-binary E2E。portable Provider/SQLite/CLI 旅程在 Linux、macOS、Windows 跑；PTY、Unix signal 和 process-group 旅程只在 Unix job 跑，Windows 不伪造这些能力。

## 4. 失败证据、清理与验收

每场景限制 wall-clock、output、request count 和 resource 数量。失败时 fixture 收集 redacted stdout/stderr、PTY transcript、Provider request log、snapshot 和 event summary；无论成败都关闭 listener、child process group、database 和 temporary directory。readiness 使用 Provider call、event、rendered text 或 child exit，绝不固定 sleep。

- UT：每个 fake 与 production boundary 遵守同一 schema、policy 与 capacity。
- E2E：timeout 后清理 child process 与 loopback server，重复运行无 port/state 泄漏。
- CI：三平台 check/Clippy/UT/contract/portable E2E/release build、Unix PTY/process E2E、独立 coverage 与文档链接检查均 fail closed。
