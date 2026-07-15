# Latte Code E2E 编写手册

> 本手册规定功能开发时如何配套编写 UT 与最终二进制 E2E。测试卡点的整体设计和场景编号见 [UT / E2E 测试卡点设计](../design/testing-gates.md)。
>
> English counterpart: [E2E authoring guide](../../en-US/testing/e2e-authoring-guide.md).

## 1. 强制规则

任何新增或修改产品行为的功能都必须同时具备：

1. **UT**：在最低责任模块直接证明规则、边界和失败路径；
2. **E2E**：从最终 `latte-code` 二进制进入，证明用户可观察行为；
3. **独立覆盖率卡点**：全仓 UT-only 行覆盖率 `>= 95%`，最终二进制 E2E 行覆盖率 `>= 80%`；
4. **变更直接覆盖**：新增或修改的功能代码必须由对应 UT 和 E2E 直接命中，不能只依赖既有测试维持全仓数字；
5. **回归保护**：修复用户可见缺陷时，必须增加能在修复前失败、修复后通过的回归测试。

纯文档、注释、格式化或不改变运行行为的构建元数据修改可以不增加 E2E，但必须在交付说明中明确理由。重构只有在能够证明没有改变行为时才适用此例外；否则仍按功能变更处理。

E2E 不能代替 UT，Contract/Component 测试也不能冒充 E2E。没有对应 UT 和 E2E 的功能不视为完成。

## 2. 测试放置位置

| 测试类型 | 位置 | 入口 |
| --- | --- | --- |
| UT | 对应 crate 的 `src/**/*.rs` 内 `#[cfg(test)]` 模块 | `make test-unit` |
| Contract/Component | `crates/*/tests/*.rs` | `make test-contract` |
| portable 最终二进制 E2E | `crates/latte-code/tests/e2e/portable.rs` | `make test-e2e-portable` |
| Unix 最终二进制 E2E | 由 `e2e/mod.rs` 注册的 `crates/latte-code/tests/e2e/*.rs` | `make test-e2e-unix` |

E2E 按用户旅程放置：

- `portable.rs`：必须在 Linux、macOS、Windows 执行的跨平台 CLI、loopback Provider 和 SQLite 旅程；
- `headless.rs`、`headless_matrix.rs`、`runtime_convergence.rs`：CLI、配置、JSON envelope、多轮只读工具、跨进程收敛和 secret non-egress；
- `provider.rs`：HTTP 状态、重试、超时、SSE、stream fallback 和 wire 兼容失败；
- `tools.rs`、`permission_chain.rs`、`runtime.rs`：最终二进制驱动的工具矩阵、alias、权限链、进程监督和 durable tool round；
- `recovery.rs`、`legacy.rs`、`legacy_lifecycle_matrix.rs`：权限、跨进程 resume、旧 schema 迁移、验证、持久化和恢复；
- `public_lifecycle_matrix.rs`、`public_boundary_matrix.rs`、`v2_boundary_matrix.rs`：通过公开 Engine authority 构造生命周期/边界 fixture，再由最终 CLI/TUI 验收公开投影；
- `tui.rs`、`interactive_matrix.rs`、`projection.rs`、`ui.rs`：真实 PTY、键盘协议、阻断卡片、交互流、投影、取消和终端恢复；
- `support.rs`：`Scenario`、`ScriptedProvider`、`PtySession`、有界等待和进程清理；
- `mod.rs`：模块注册，不在这里堆叠场景实现。

新增场景应放入最接近的现有文件。只有整个行为都受三平台支持时才能放入 `portable.rs`；PTY、Unix signal/process group、symlink 语义、可执行 verification 或其他 Unix 假设必须进入 Unix suite。只有形成新的稳定 Unix 用户旅程类别时才新增模块，并同步 `e2e/mod.rs`。

公开 Engine fixture 只用于制造当前最终二进制尚无命令可创建的合法生命周期状态。fixture 必须只调用公开 authority API，不能写私有 SQLite 表；测试结论仍必须由新的最终 CLI/TUI 进程及其用户可见输出验收。旧 schema migration fixture 可以创建历史 schema 数据，但只能用于兼容性场景，不能用来绕过当前 authority 规则。

## 3. 什么才算 E2E

Latte Code E2E 必须同时满足：

- 使用 `env!("CARGO_BIN_EXE_latte-code")` 启动 Cargo 构建的最终二进制；
- 使用隔离的临时 Git workspace、HOME、配置和 SQLite；
- 经过真实 CLI/TUI composition root，而不是直接调用内部 service 或 Provider trait；
- Provider 场景通过 loopback HTTP/SSE 进入生产 adapter、序列化和 parser；
- TUI 场景运行在真实 PTY 中，并验证 terminal lifecycle；
- 只通过公开输出、持久化 projection、文件系统结果、Provider 请求或进程状态判断结果；
- 全程有明确 deadline，失败和 Drop 路径会终止并 reap child/process group。

以下测试不是产品 E2E：

- 直接调用 reducer、parser、runtime 或 mock Provider trait；
- 绕过最终二进制直接构造 service；
- 访问真实 Provider、公网或使用开发者 API key；
- 使用 `#[ignore]`、条件 skip 或只在本机手工运行；
- 只断言“进程退出成功”，没有证明用户可观察结果。

## 4. 编写流程

### 4.1 先写验收矩阵

开始编码前先列出一个最小矩阵：

| 维度 | 必须回答的问题 |
| --- | --- |
| happy path | 用户最终看见什么？持久化状态是什么？ |
| rejection | deny、非法输入或配置错误时，什么绝不能发生？ |
| interruption | timeout、cancel、进程退出后，状态和 child 如何收敛？ |
| durability | 新进程重开后是否保持同一结果？ |
| security | secret 可能经过哪些输出、请求和持久化表面？ |
| exactness | approval、effect、tool result 和 Provider 重入是否恰好一次？ |

一个功能至少需要一个覆盖主用户旅程的 E2E。权限、安全、恢复、验证等高风险行为还必须覆盖对应负向路径。

### 4.2 先补最低责任层 UT

UT 应直接覆盖：

- 正常输入；
- 边界值；
- 非法输入和错误类型；
- 状态不可变式；
- 安全负向断言；
- bug 的最小复现。

不要为了提高覆盖率把 SQLite、socket、真实子进程或 PTY 塞进 UT；这些属于 Contract/Component 或 E2E。

### 4.3 先选择平台边界

只依赖 CLI JSON、SQLite 和 loopback HTTP 即可证明的旅程优先进入 portable suite。由于非 Unix 进程监督会主动 fail closed，portable Provider 场景必须在 Windows 进入进程 verification 前结束，例如持久化 input request 或 typed terminal Provider failure。禁止用 `cfg`、ignored test 或运行时 skip 隐藏 portable target 的失败。

行为本身依赖 PTY、signal、process group、symlink 或可执行 verification 时进入 Unix suite。下面的 completion 示例会执行 `/usr/bin/true` 作为 verification，因此属于 Unix-only 场景。

### 4.4 再补最终二进制 E2E

Headless 场景的基本结构：

```rust
use super::support::{ProviderReply, Scenario, ScriptedProvider, json};

#[test]
fn feature_name_describes_user_visible_outcome() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::completion("done"),
    ]);

    let output = scenario.output(&["--json", "run", "perform task"], |command| {
        scenario.configure_provider(
            command,
            provider.endpoint(),
            r#"["/usr/bin/true"]"#,
            "test-secret",
        );
    });

    assert!(output.status.success());
    assert_eq!(json(&output)["status"], "completed");
    provider.assert_consumed();
    assert_eq!(provider.requests().len(), 1);
}
```

模板只是起点。实际场景还要断言功能特有的持久化状态、副作用、负向结果和安全边界。

## 5. Harness 使用规范

### 5.1 Scenario

必须使用 `Scenario` 创建隔离环境。它负责：

- 临时 workspace 和 `.git` 根；
- 隔离 HOME；
- 清除可能污染测试的 Provider/verification 环境变量；
- 使用 `CARGO_BIN_EXE_latte-code`；
- 给最终二进制设置有界执行时间；
- 在 coverage 运行中为 child 保留唯一 `LLVM_PROFILE_FILE`。

不得依赖开发者当前目录、真实 HOME、已有 `.latte` 状态或系统全局配置。

### 5.2 ScriptedProvider

Provider E2E 使用 `ScriptedProvider`：

- 按顺序声明所有预期响应；
- 使用 `wait_for_calls` 等待明确网络事件；
- 使用 `requests()` 检查 method、path、headers、model、messages、tools 和 tool result；
- 使用 `assert_consumed()` 保证没有漏掉响应或出现额外 Provider 请求；
- tool loop 必须验证结果确实进入下一次生产 Provider 请求。

不要用“mock 被调用过”代替 wire contract 断言，也不要让 fixture 在意外请求时静默返回成功。

### 5.3 PTY

TUI E2E 使用 `PtySession`：

- 先等待明确的 TUI readiness 或渲染文本，再输入按键；
- 使用真实 Crossterm key encoding；
- 对权限、reconciliation 等受保护动作，分别证明惰性键不产生状态变化，再发送确认键；
- 退出后验证 alternate screen、keyboard enhancement 等模式成对恢复；
- reader 必须持续 drain 到 EOF，不能等待 child 退出时停止读取；
- Drop 和超时路径必须终止整个进程组并 reap child。

禁止用固定 `sleep` 证明 readiness 或事件顺序。只有产品协议本身定义的时间窗口，例如双 `Ctrl+C` 去抖窗口，才可以使用带注释的固定等待。

## 6. 必须断言的结果

根据场景选择并组合以下断言，不能只检查退出码：

- CLI exit code、JSON version/status/error code；
- thread/run lifecycle 与 pending request；
- effect 精确状态和 single-use approval；
- 文件只修改一次，deny/失败路径不修改；
- verification evidence 与 handoff；
- Provider 请求次数、顺序和 tool result；
- stdout、stderr、Provider body、transcript、SQLite 中不存在纯 secret value；
- timeout/cancel 后进程组不存在；
- 不确定执行只能进入 `Unknown`，不能猜测成功或自动重试；
- TUI 受保护按键的惰性、精确确认键和终端模式恢复。

失败消息应包含脱敏后的 stdout、stderr 或 PTY transcript，帮助本地和 CI 定位问题。

## 7. UT 95% / E2E 80% 覆盖率规则

### 7.1 统计口径

UT 覆盖率只运行 crate-local lib/bin tests，不包含 Contract、E2E 或 doc tests；E2E 覆盖率只运行 portable 与 Unix 两个最终二进制 target。两个 profile 每次独立清理和采集：

```bash
make coverage-unit # --lib --bins --fail-under-lines 95
make coverage-e2e  # --test e2e_portable --test e2e_unix --fail-under-lines 80
```

Unix PTY/process E2E 在 Makefile 和 CI 中固定使用单测试线程，避免不同场景争用终端、signal 和 process-group 资源；单个场景内部仍必须使用可观测事件同步和有界超时，不能改回固定 sleep。

需要定位未覆盖行时生成 HTML：

```bash
cargo llvm-cov clean --workspace
cargo llvm-cov --workspace --all-features --lib --bins --html
```

E2E 命中的代码不能计入 UT 95% 指标，UT、Contract 或直接调用内部 API 的测试也不能计入 E2E 80% 指标。禁止通过扩大排除列表、删除断言、移动测试层级或只统计容易覆盖的 package 来制造达标。

### 7.2 Required 卡点

三项覆盖率卡点互相独立，全部通过才算完成：

1. `make coverage-unit`：全仓 UT-only lines `>= 95%`；
2. `make coverage-e2e`：最终二进制 E2E lines `>= 80%`；
3. `make coverage-total`：全 targets lines `>= 90%`。

`make coverage` 串行执行三项卡点，并在每项前清理 profile，避免上一层测试命中污染下一层数字。新增或修改功能除了保持全仓卡点通过，还必须直接覆盖 success、boundary、typed failure 和适用的 safety-negative 路径。

评审时应附上 UT-only summary，并从 HTML 报告核对新增/修改可执行行。若工具无法自动计算 diff coverage，必须逐行检查 touched functional code；“没有数据”不等于达标。

## 8. 命名与稳定性

- 测试名描述用户旅程和结果，例如 `write_file_deny_never_mutates_and_never_reenters_the_provider`；
- 不使用 `test_1`、`happy_path` 等缺少行为信息的名字；
- 每个测试独立创建 Scenario，不共享端口、HOME、SQLite 或可变全局状态；
- 使用事件、公开 projection、文件出现或 Provider call 作为 barrier；
- 每个等待都有上限，失败时输出足够证据；
- required E2E 不允许 `#[ignore]`、条件 skip、真实网络和真实凭证；
- 新增 portable E2E 在升级为 required 前，应在 Linux、macOS、Windows 各连续运行 10 次零 flake；新增 Unix E2E 则在 Linux/macOS 各连续运行 10 次零 flake。

## 9. 完成前检查清单

功能只有全部满足以下项目才算完成：

- [ ] 功能验收矩阵已明确 happy path 和负向路径；
- [ ] 最低责任模块有对应 UT；
- [ ] 全仓 UT-only 行覆盖率达到 95% 以上；
- [ ] 全仓最终二进制 E2E 行覆盖率达到 80% 以上；
- [ ] 新增/修改功能代码由对应 UT 和 E2E 直接覆盖；
- [ ] 至少一个最终二进制 E2E 覆盖主用户旅程；
- [ ] 权限、安全、恢复、验证行为有显式负向断言；
- [ ] E2E 不依赖公网、真实 key、固定 sleep 或 `#[ignore]`；
- [ ] child/process group、PTY 和 Provider fixture 能在失败时清理；
- [ ] `make test-unit`、`make test-contract` 与适用的 portable/Unix E2E target 通过；
- [ ] `make coverage` 和 `make ci` 通过；
- [ ] 中英文行为文档同步更新。
