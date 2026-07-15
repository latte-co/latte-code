# Latte Code UT / E2E 测试卡点设计

> 状态：分层框架、三项独立覆盖率 job 和 fail-closed `PR Gate` workflow 已落地；GitHub 远端 branch protection/ruleset 尚需实际配置后才算 required 已激活。E2E-H-001 至 H-008、H-010/H-011、E2E-T-001 至 T-008 已实现，H-009 已覆盖公开恢复语义但仍缺真实 kill barrier，后续卡点仍按本文推进。
>
> 基线：2026-07-15，当前工作区，以 `make ci` 和三项独立 llvm-cov profile 实测。
>
> English counterpart: [Testing gates](../../en-US/design/testing-gates.md).

## 1. 结论

Latte Code 的合入测试不应只有“执行一次 `cargo test`”。目标测试模型分为三层：

1. **UT**：快速、确定性地证明单个模块的规则；
2. **Contract / Component**：证明 crate 边界、SQLite、文件系统、进程与 Provider 适配契约；
3. **E2E**：从最终 `latte-code` 二进制进入，经过真实配置、持久化、mock Provider 和 PTY，验证用户可观察结果。

用户要求的两个主卡点是 UT 和 E2E。中间的 Contract 层不是扩大范围，而是把当前已经存在、但被混称为 UT 的数据库、HTTP、进程测试放到正确位置。否则 UT 会越来越慢，而 E2E 失败时也无法快速定位责任边界。

最终规则：

- 每个 PR 都必须通过 UT、Contract、P0 E2E，以及 UT-only >= 95%、最终二进制 E2E >= 80%、全 targets >= 90% 三项独立行覆盖率卡点；新增或修改的功能代码还必须由对应 UT 和 E2E 直接覆盖；
- 阻断 CI 不访问真实 Provider，不依赖公网，不消耗真实 API key；
- Linux 与 macOS 在 PR 运行完整 E2E；Windows 在运行时能力仍 fail-closed 的阶段作为 PR 卡点只做编译；release build 仅在 `main` push 或手工触发时运行，不属于 PR 卡点；
- 安全、权限、恢复、验证类行为不能只靠覆盖率，必须有显式正向和负向断言；
- 新卡点按 `draft -> shadow -> required` 激活，不能把尚未执行的设计写成“已保护”。

## 2. 当前基线

### 2.1 已有能力

当前命令与 CI 已经具备：

- `make ci`；
- `cargo fmt`、Clippy、Rustdoc、`cargo deny`；
- `cargo test --workspace --all-targets --all-features`；
- `cargo llvm-cov ... --fail-under-lines 90`；
- Ubuntu、macOS 原生测试，Windows 编译检查；
- 面向 `main` 的 PR/merge queue 触发、三项独立 coverage job，以及聚合所有 G0-G4/Windows 必需 job 的 `PR Gate`；
- 最终二进制 CLI 测试、loopback mock HTTP、真实 PTY 测试；
- 真实 SQLite/temp workspace、进程组、权限、恢复和 TUI reducer 测试。

2026-07-15 的实测基线：

| 指标 | 当前值 | 说明 |
| --- | ---: | --- |
| Cargo 可发现测试 | 344 | 245 个 crate-local、13 个 contract、71 个最终二进制 E2E、15 个 doc tests |
| crate-local 测试 | 245 | 独立 `--lib --bins` profile；其中既有 inline tests 仍有待继续纯化的 component 行为 |
| Contract / component | 13 | 5 个 contract targets，由 inventory 防漏 |
| 最终二进制 E2E | 71 | 单一 `e2e` target；headless、Provider、tool/recovery、公开边界和真实 PTY |
| UT-only 行覆盖率 | 95.06% | 最新完整 CI 为 `26828 / 28223`，`make coverage-unit`；独立复跑的 missed lines 有 2 行波动，但显示值均为 95.06% |
| 最终二进制 E2E 行覆盖率 | 80.55% | `10657 / 13230`，独立运行均通过 80% 卡点；观测值为 80.54%–80.55% |
| 总行覆盖率 | 96.57%–96.58% | fresh `make coverage-total` 的独立运行观测为 `27256–27257 / 28223` |

### 2.2 当前剩余问题

1. **UT 仍待纯化**：`test-unit` 已独立可见，但现有 inline tests 中仍包含 SQLite、socket、子进程和真实 signal 的 component 行为。
2. **真实 crash barrier 仍有缺口**：公开 Engine authority + 最终 CLI/TUI 已覆盖 `Started -> Unknown -> reconcile` 语义，但 H-009 仍需由外部 barrier 在真实 effect Started 后终止最终二进制。
3. **Provider 协议保真层未落地**：scripted Provider 已能证明产品行为，但 cassette replay 与 live canary 仍待实现。
4. **release 只构建不启动**：artifact 存在不等于产物可以启动、解析配置和输出稳定 JSON。
5. **失败证据尚未打包成 artifact**：harness 已持有 stdout、stderr、PTY transcript、Provider request log 和最终投影，但 CI 还没有在失败时统一上传。
6. **required 激活仍是外部动作**：workflow 已配置 Linux/macOS E2E 和 fail-closed `PR Gate`，但还需要两平台各连续 10 次零 flake，并在 GitHub branch protection/ruleset 中实际把 `PR Gate` 设为 required；仓库内文件不能证明远端设置已启用。

## 3. 三层测试边界

### 3.1 UT

UT 只证明单个模块或纯规则，目标是失败定位直接、可并行、无环境波动。

UT 中允许：

- 纯数据构造和 deterministic fake clock / ID；
- reducer、parser、serializer、policy、redactor；
- Ratatui `TestBackend`；
- 小型内存 fake，不跨最终二进制。

UT 中不允许：

- 启动 `latte-code` 最终二进制；
- TCP listener 或任何外部网络；
- PTY、shell、子进程或真实 signal；
- 依赖 wall-clock sleep；
- SQLite reopen、崩溃恢复或多进程竞争。

允许 `tempfile` 只用于纯路径/文件内容算法；一旦测试依赖真实原子 rename、symlink、SQLite、进程组或 OS 权限语义，就归入 Contract / Component。

### 3.2 Contract / Component

这一层验证真实基础设施和公开边界，但不要求从最终二进制进入：

- `latte-core` 协议字节兼容、状态迁移表和 compile-fail authority boundary；
- `latte-engine` SQLite migration/reopen、lease fencing、effect ledger、文件系统 containment、symlink、进程组；
- `latte-headless` scripted HTTP/SSE、脱敏 cassette replay、Provider registry、agent loop、verification、resume；
- crate 公开 API 生命周期和 workspace dependency matrix；
- Markdown/link/repo structure checks。

这层可以使用真实 SQLite、temp workspace、loopback socket、子进程和 signal，但必须有 deadline、隔离目录和完整清理。

### 3.3 E2E

E2E 必须满足：

- 入口是 `CARGO_BIN_EXE_latte-code` 或刚构建出的 release artifact；
- 使用真实配置分层、真实 SQLite 和真实进程边界；
- Provider 必须使用本机 `127.0.0.1` 上的 deterministic harness：行为场景默认使用 scripted server，协议保真场景可以使用脱敏后的 cassette replay server；
- TUI 场景使用真实 PTY 和最终二进制；
- 断言优先使用 stdout/stderr、退出码、CLI JSON、公开 Engine projection 和工作区最终状态；
- 不直接写 SQLite 私有表来制造成功结果；只读探针仅用于当前 CLI 尚未暴露的 thread 投影证据；
- 每个场景独立 HOME、workspace、database、port 和 secret sentinel。

Provider E2E 的判定标准是生产 Provider adapter、序列化、HTTP/SSE parser、agent loop 和最终二进制都运行到本地网络边界，而不是必须访问真实远端。真实 Provider canary 只能是手工或定时非阻断任务，不能代替 deterministic merge gate。

## 4. 卡点模型

| 卡点 | 触发时机 | 阻断 | 内容 | 目标预算 |
| --- | --- | --- | --- | ---: |
| G0 Static | 每个 PR | 是 | fmt、check、Clippy、Rustdoc、architecture/repo checks | 3 min |
| G1 UT | 每个 PR | 是 | 所有纯 crate-local UT；全仓 UT-only 行覆盖率 >= 95% | 2 min |
| G2 Contract | 每个 PR | 是 | SQLite、FS、process、scripted/cassette Provider、公开 API、doc tests | 5 min |
| G3 E2E | 每个 PR | 是 | 最终二进制 + loopback Provider 的 P0 headless，以及 TUI/PTY，Linux/macOS；独立行覆盖率 >= 80% | 5 min/OS |
| G4 Coverage | 每个 PR | 是 | workspace/all-features/all-targets，总行覆盖率 >= 90% | 5 min |
| G5 Release smoke | release workflow | 是 | 各平台 release artifact 启动、help、JSON list | 2 min/OS |
| Extended | nightly/manual | 否于 PR | 重复运行、长时取消、边界尺寸矩阵、live canary | 15 min |

预算是上限目标，不是通过 `sleep` 消耗掉的等待时间。任务应按 job 并行，单个测试默认 deadline 10 秒；明确测试 timeout/cancellation 的场景可以单独声明更长上限。

### 4.1 PR 聚合与并发语义

`.github/workflows/ci.yml` 仅由 `main` push、面向 `main` 的 PR（`opened`、`synchronize`、`reopened`、`edited`、`ready_for_review`）、`merge_group` 和手工触发。`edited` 防止 PR 修改目标分支并重新指向 `main` 后缺失 required check。每个底层 job 都有稳定名称和超时，required 路径没有 path filter、条件 skip 或自动 rerun：

- Linux/macOS 分别运行 Static、UT、Contract 和 E2E；Documentation、Dependency audit 和 Windows compile 也独立可见；
- `Coverage - UT (95%)`、`Coverage - E2E (80%)`、`Coverage - total (90%)` 是三个独立 job，不能互相补足；
- 稳定状态 `PR Gate` 以 job-level `always()` 等待所有 G0-G4/Windows 必需 job，并逐个要求 `needs.<job>.result == success`；任何 failure、cancelled 或 skipped 都失败；
- PR 的新提交取消同一 PR 的旧运行，`main` 和 merge queue 运行不取消，以免丢失主干/合并队列证据；
- release build 只在 `main` push 或手工触发时运行，不进入 `PR Gate`。它目前不是 G5 release smoke。

推荐 branch protection/ruleset 只要求稳定的 `PR Gate`，同时保留底层状态用于定位。该远端设置不由 workflow 文件创建；只有在 GitHub 实际配置 required check 后，本设计中的 PR 阻断才算激活。

### 4.2 已实现命令面

Makefile 当前提供以下稳定入口：

```text
make test-unit
make test-contract
make test-e2e
make test-doc
make test-all
make coverage
make ci
```

目标结构：

```text
crates/latte-code/tests/
  contract.rs
  contract/
    cli.rs
  e2e.rs
  e2e/
    support.rs
    headless.rs
    provider.rs
    tools.rs
    tui.rs
    recovery.rs
  architecture.rs
  markdown_links.rs
```

- `test-unit`：`cargo test --workspace --lib --bins --all-features`；
- `test-contract`：运行各 crate 的公开 contract/component targets；
- `test-e2e`：运行单一 `e2e` target；每个 headless、recovery 和 PTY 场景完全隔离，可由 Rust test harness 并行调度；
- `test-doc`：运行 workspace doc tests；
- `test-all`：组合以上所有层；
- `test-inventory`（由 repo check 承担）：保证新增 `tests/*.rs` target 被归入已知层，禁止悄悄遗漏。

迁移初期，现有 inline tests 先归为 `crate-local`。包含 socket、SQLite reopen、子进程和真实 signal 的测试随后移到 component target；完成迁移前不能宣称 `test-unit` 已完全满足纯 UT 定义。

## 5. UT 必测矩阵

| ID | 所属 crate | 必测规则 | 断言重点 |
| --- | --- | --- | --- |
| UT-COR-001 | `latte-core` | run/thread 状态迁移表 | 每个状态的合法/非法迁移、revision 单调、completed immutable |
| UT-COR-002 | `latte-core` | v1/v2 protocol serialization | version、字段名、未知/非法输入关闭失败、字节兼容 |
| UT-COR-003 | `latte-core` | redaction 和边界 | secret/control 不保留，安全结构不被误删，文本有界 |
| UT-ENG-001 | `latte-engine` | policy/classification | argv-first、shell/high-risk、deny 优先、无隐式 allow |
| UT-ENG-002 | `latte-engine` | effect/permission validator | revision、lease token、digest、single-use、错误不部分提交 |
| UT-ENG-003 | `latte-engine` | path/manifest 算法 | component 编码、非 UTF-8 fail-closed、glob 与输出上限 |
| UT-HDL-001 | `latte-headless` | Provider parser/SSE state machine | 分块、CRLF、tool call、fallback、retry 分类、取消 |
| UT-HDL-002 | `latte-headless` | history/budget/redaction | 顺序、最小保留段、超限失败、secret 不进入 history |
| UT-HDL-003 | `latte-headless` | registry/binding | alias 稳定、scope/generation 精确、secret lookup 之前校验 |
| UT-TUI-001 | `latte-tui` | reducer action matrix | 每个 active state 只产生类型化 action |
| UT-TUI-002 | `latte-tui` | protected keys | Enter/Shift+Enter 不批准权限或 reconciliation |
| UT-TUI-003 | `latte-tui` | render/layout | 三档尺寸、Unicode grapheme/display width、固定 blocking card |
| UT-CLI-001 | `latte-code` | config merge/root discovery | defaults -> HOME -> workspace，数组/标量替换，最近 Git root |
| UT-CLI-002 | `latte-code` | parser/exit/JSON mapping | 所有命令形态、稳定 code、versioned envelope、错误脱敏 |

UT 卡点除行覆盖率外还要求：

- 状态机、权限、secret、reconciliation 必须包含负向断言；
- 不允许 `#[ignore]` 隐藏 P0 行为；
- 不允许自动 rerun 后把第一次失败算作通过；
- 时间与 ID 必须可控，不能用 sleep 证明顺序；
- bug 修复至少在最低责任层新增一个能先失败后通过的回归测试。

## 6. E2E 场景矩阵

### 6.1 Headless / final binary

| ID | 优先级 | 场景 | 关键证据 | 当前状态 |
| --- | --- | --- | --- | --- |
| E2E-H-001 | P0 | 无配置从嵌套目录启动并 `--json list` | exit 0、versioned JSON、DB 位于 Git root | 已有 |
| E2E-H-002 | P0 | scripted Provider 完成只读任务 | 请求契约、最终 completed、无 mutation/permission | 已有 |
| E2E-H-003 | P0 | mutation 请求后 deny | 文件未变、effect failed、Provider 不被错误重入 | 已有 |
| E2E-H-004 | P0 | mutation allow，跨进程 resume，验证通过 | 文件只改一次、approval single-use、evidence/handoff 完整 | 已有 |
| E2E-H-005 | P0 | mutation 后验证失败 | 永不 completed、failure typed、变更和 evidence 可审计 | 已有 |
| E2E-H-006 | P0 | HOME/workspace 配置覆盖 | workspace 胜出、相对 DB 路径仍基于 workspace | 已有 |
| E2E-H-007 | P0 | secret non-egress | stdout/stderr/JSON/transcript/持久化无 sentinel | 已有 |
| E2E-H-008 | P0 | 子进程 timeout/cancel | 整个进程组退出、单一 terminal observation、无孤儿 | 已有 |
| E2E-H-009 | P0 | `Started` 时 kill，重启进入 Unknown，再 reconcile | 不猜成功、不自动重试、只终结精确 child/effect | 部分：公开恢复语义已有，真实 kill barrier 缺失 |
| E2E-H-010 | P1 | Provider malformed/timeout/retry matrix | retry 有界、非法 success 不重试、错误 typed | 已有 |
| E2E-H-011 | P1 | legacy v1 `show/list/resume` | 兼容 envelope、退出码、不会回填成 thread | 已有 |
| E2E-H-012 | P1 | 每种受支持 wire protocol 的 cassette replay tool loop | 录制请求逐步精确消费、tool result 回传、最终 answer、无公网访问 | 缺失 |

### 6.2 TUI / real PTY

| ID | 优先级 | 场景 | 关键证据 | 当前状态 |
| --- | --- | --- | --- | --- |
| E2E-T-001 | P0 | 启动与显式退出 | raw/alternate/keyboard/paste 模式成对恢复 | 已有 |
| E2E-T-002 | P0 | Shift+Enter 多行，Enter 单次提交 | durable user card 恰好一条，内容精确 | 已有 |
| E2E-T-003 | P0 | permission card | Enter/Shift+Enter 惰性；仅精确 Ctrl+A 或 deny key 生效 | 已有 |
| E2E-T-004 | P0 | active run Ctrl+C，再次 Ctrl+C 退出 | 先取消任务、再确认退出，终端恢复 | 已有 |
| E2E-T-005 | P0 | Unknown reconciliation | Ctrl+R 打开，Enter 惰性，Ctrl+A 只确认精确 effect | 已有 |
| E2E-T-006 | P1 | input request | 输入持久化且只恢复一次，不混入 permission | 已有 |
| E2E-T-007 | P1 | resize、窄终端、Unicode、bracketed paste | 不 panic、不丢输入、布局仍保留 blocking surface | 已有 |
| E2E-T-008 | P1 | Provider 配置/transport 失败 | prompt 已持久化并恢复，secret 不显示 | 已有 |

### 6.3 Release artifact

| ID | 优先级 | 场景 | 平台 |
| --- | --- | --- | --- |
| E2E-R-001 | P0 | release binary `--help` | Linux/macOS/Windows |
| E2E-R-002 | P0 | 临时 HOME/workspace 下 `--json list` | Linux/macOS/Windows |
| E2E-R-003 | P0 | PTY 启动后退出并恢复 | Linux/macOS |

Windows 仍不宣称支持安全 mutation/process supervision；当前 release job 只证明构建成功，上述启动 smoke 尚未激活。

## 7. E2E Harness 设计约束

### 7.1 `Scenario` fixture

统一 fixture 至少持有：

- 独立 `TempDir` workspace 与 HOME；
- `.git` root、配置和 database 路径；
- 唯一 secret sentinel；
- scripted Provider 与 cassette replay server；
- child/PTY handle；
- stdout、stderr、terminal transcript、Provider request log；
- deadline 和 cleanup guard。

fixture drop 必须终止并 reap 全部 child/process group。覆盖率运行时给每个子进程保留带 `%p` 的 `LLVM_PROFILE_FILE`，PTY reader 要持续 drain 到 EOF，不能在等待 child 退出时停止读取。

### 7.2 Provider 三层验证模型

| 机制 | 网络边界 | 主要证明 | 所属卡点 |
| --- | --- | --- | --- |
| Scripted Provider | 本机 loopback，测试生成响应 | 行为、错误、时序、重试、取消 | G2/G3 required |
| Cassette Replay Provider | 本机 loopback，回放脱敏真实交互 | 生产协议栈与真实 wire shape 兼容 | G2 required；每种受支持 wire protocol 至少一个 G3 场景 |
| Live Provider Canary | 真实远端 | 凭据、供应商可用性和远端协议漂移 | Extended，非 PR 阻断 |

三层不能互相替代。Scripted Provider 负责可控故障和状态路径，cassette replay 负责真实协议保真，live canary 只负责发现测试夹具之外的远端漂移。

#### 7.2.1 Scripted Provider

mock server 不是“返回一个固定 200”即可。每一步包含：

```text
expected request predicate
response / streamed chunks / connection action
maximum call count
next step
```

它必须验证：

- Authorization 只发往配置的 endpoint；
- model、messages、tools、tool order 和 alias 精确；
- resume 后历史只包含允许持久化的数据；
- 不期望的额外请求立即失败；
- retry、fallback 和 cancellation 次数有界。

请求断言使用语义 inspector，而不是只比较整段原始 JSON snapshot。harness 至少暴露 messages、tools、tool result、model、Authorization 目标、请求顺序和精确 call count，并提供 `wait_for_call(n)` 一类公开 readiness barrier。

#### 7.2.2 Cassette replay

Cassette 只录制 Provider HTTP/SSE transport 边界；生产 Provider adapter、序列化、parser、request executor 和 agent loop 保持真实。录制/回放遵循以下规则：

- 录制只能由显式的本地或手工命令触发；CI 永远 replay-only，fixture 缺失时关闭失败，禁止回退公网；
- Authorization、API key、token、account/user identifier、带 secret 的 query/header/body 必须脱敏；检测到疑似 secret 时拒绝写盘；
- request id、时间戳等非语义动态字段先归一化，model、messages、tools、tool result 和协议字段不得被宽松忽略；
- 交互按请求开始顺序逐条消费；请求不匹配、并发重复消费、游标耗尽或测试结束仍有未消费交互都失败；
- fixture 按 wire protocol 和场景版本化，更新必须在评审中说明上游协议变化或生产序列化变化；
- cassette 只证明已录制路径的协议兼容性；malformed stream、hang、disconnect、429/500、retry/cancel 仍由 Scripted Provider 覆盖。

同一份 replay server 必须既能被 G2 的 crate-level Provider contract 使用，也能通过 endpoint 配置被 G3 的最终 `latte-code` 二进制使用；后者才证明配置、持久化、agent loop 与 Provider 协议在产品边界闭环。

#### 7.2.3 Live Provider canary

- 默认只运行最小 read-only 或无副作用 tool loop，不允许隐式 mutation；
- 仅从 CI secret store 或开发者显式环境取得凭据，设置 token、费用、调用次数和 wall-clock 上限；
- 失败记录供应商、协议、状态码和脱敏诊断，但不阻断 PR，也不能自动改写 cassette；
- 只有 scripted 或 cassette gate 也失败时，才能初步归因为代码回归；单独的 canary 失败优先视为远端、凭据或网络信号。

### 7.3 等待与失败证据

- 不用固定 sleep 判断 readiness；轮询明确的 terminal marker、`wait_for_call(n)`、cassette interaction consumption、CLI JSON 或公开 projection；
- 所有轮询有 deadline，超时时输出当前证据；
- CI 失败时上传或打印 stdout、stderr、PTY transcript、mock request log、最终 workspace tree 和脱敏后的 projection；
- secret sentinel 检查失败时只能报告命中的表面与位置，不能再次打印 secret 本身。

### 7.4 无生产后门的故障注入

不在 release binary 中加入隐藏环境变量或 test-only subcommand。

`Started -> Unknown` 可以使用外部可观察 barrier 完成：

1. scripted Provider 请求一个受监督的进程；
2. 被执行进程创建 workspace barrier 文件，记录自身 PID/PGID 并保持运行；
3. E2E runner 等待 barrier 和公开 `Started` 投影；
4. runner 对最终 `latte-code` 进程执行 SIGKILL；
5. 用同一 workspace/database 重启最终二进制；
6. 断言 effect 为 Unknown、没有 tool success、没有自动重试；
7. 再通过 CLI/TUI 完成精确 reconciliation。

fixture 的 finally/Drop 路径必须独立终止该 PGID 并等待其消失，避免父进程被 SIGKILL 后留下孤儿。这样既能控制崩溃窗口，也不会把测试控制面带入生产协议。

### 7.5 E2E 名称约束

- 只调用 mock Provider trait、绕过生产 adapter/serializer/parser 的测试属于 Component，不得命名为 Provider E2E；
- 生产 Provider 栈运行到 loopback HTTP/SSE 边界属于 Provider Contract；再从最终二进制进入，才属于产品 E2E；
- PTY/UI E2E 只证明用户输入、渲染和 terminal lifecycle；除非真实完成 Provider/tool loop，否则不能代替 Provider E2E；
- `#[ignore]`、条件 skip 或依赖真实 API key 的测试不能计入 required coverage，也不能用于宣称 P0 已覆盖。

## 8. 覆盖率、flake 与平台规则

### 8.1 Coverage

- 全仓 UT-only 行覆盖率必须 `>= 95%`，由 `make coverage-unit` 独立统计；
- 最终二进制 E2E 行覆盖率必须 `>= 80%`，由 `make coverage-e2e` 独立统计；
- E2E、Contract 和 doc test 命中不能计入 UT 指标，UT、Contract 或直接调用内部 API 的测试也不能计入 E2E 指标；不得通过排除功能代码制造达标；
- 保留现有总行覆盖率 `>= 90%`，不得降低；
- 先在 CI 产出 crate/file 级报告，再记录稳定基线；
- 单 crate/file floor 采用基线 ratchet，不能先拍一个无法解释的数字；
- 新增或修改的关键安全分支必须有显式测试，即使总覆盖率没有下降；
- branch coverage 当前为无数据状态，先采集和观察，再决定是否阻断。

具体编写和验收流程见 [E2E 编写手册](../testing/e2e-authoring-guide.md)。

### 8.2 Flake

- required gate 不自动 rerun；第一次失败即失败；
- 新 E2E 在进入 required 前，Linux/macOS 各连续运行至少 10 次且零 flake；
- quarantine 必须带 issue、owner、失效日期和替代保护，安全/权限/恢复 P0 场景不得 quarantine；
- 任何 hang 都按测试失败处理，不能无限等待；
- 禁止以扩大 sleep 作为默认 flake 修复。

### 8.3 平台

- Linux/macOS：UT、Contract、完整 headless/TUI E2E；
- Windows：目标状态运行纯 UT；当前 PR/merge queue 保留 all-target compile，release build 仅在 `main` push 或手工触发；
- Unix-only process/PTY 场景必须明确 `cfg(unix)`，并分别在 Linux/macOS 执行；
- 平台未测试的能力不能在文档中宣称支持。

## 9. 变更类型与最低测试要求

| 变更类型 | 最低要求 |
| --- | --- |
| 纯 parser/reducer/policy | 同模块 UT，包含错误/负向路径 |
| protocol/state schema | UT + byte/JSON contract + migration/compat contract |
| SQLite/effect/lease | UT validator + reopen/fencing component + 对用户可见恢复 E2E |
| filesystem/process tool | policy UT + 真实 OS component + allow/deny/cancel E2E |
| Provider 协议 | parser UT + scripted HTTP contract + 脱敏 cassette replay + final-binary loopback E2E；真实远端只做非阻断 canary |
| CLI config/exit/JSON | UT + final binary headless E2E |
| TUI key/reducer | reducer UT + protected action 的 PTY E2E |
| TUI 纯视觉调整 | TestBackend UT；只有影响输入/阻断操作/terminal lifecycle 时才要求 PTY E2E |
| bug fix | 最低责任层回归；若 bug 逃逸到用户表面，再加对应 E2E |

## 10. 分阶段落地

### Phase 1：分层和可见性

1. 增加 `test-unit`、`test-contract`、`test-e2e`、`test-doc`、`test-all`；
2. 把 `cli.rs` 拆为 contract 与 `e2e` target，共享统一 fixture；
3. GitHub Actions 拆成可见的 UT、Contract、E2E 和三个独立 Coverage job，并由稳定 `PR Gate` fail-closed 聚合；
4. 对 integration target 做 inventory check；
5. 保持 `make ci` 为唯一完整本地卡点。

完成状态：仓库内实现已完成。测试 target 已按层拆分并由 inventory 防漏；最终二进制 E2E 和三项 coverage 已进入独立 job，`PR Gate` 严格聚合所有 G0-G4/Windows 必需状态，`make ci` 保持完整本地卡点。GitHub branch protection/ruleset 仍需在远端实际启用 `PR Gate`，不能仅凭本文件宣称 required 已激活。当前测试数与覆盖率以本节上方的最新实测基线为准。

### Phase 2：补齐 P0 用户旅程

按以下顺序补齐：

1. headless mutation deny/allow + verification pass/fail；
2. secret non-egress；
3. TUI permission protected keys；
4. Ctrl+C cancel/exit；
5. process timeout/cancel 与无孤儿证明。

完成标准：E2E-H-001 至 H-008、E2E-T-001 至 T-004 成为 required，Linux/macOS 各 10 次零 flake。

### Phase 3：崩溃恢复与发布产物

1. 外部 barrier 驱动 `Started -> kill -> Unknown`；
2. headless/TUI reconciliation；
3. release artifact smoke；
4. 每种受支持 wire protocol 的 cassette replay；
5. per-crate/file coverage baseline ratchet；
6. nightly live Provider canary 与 extended matrix。

完成标准：E2E-H-009、E2E-H-012、E2E-T-005、E2E-R-001 至 R-003 required；release workflow 不再只是“文件构建成功”。

## 11. Gate 激活条件

一个测试或 job 只有同时满足以下条件，才能从 shadow 变成 required：

1. 本地有稳定、文档化的单命令入口；
2. Linux/macOS 目标平台各连续运行至少 10 次无 flake；
3. 超时、child cleanup 和 PTY drain 有明确上限；
4. 失败时能给出足够的脱敏证据；
5. 不依赖公网、真实 Provider 或开发者个人配置；
6. required cassette 已脱敏、版本化，且 CI 明确 replay-only；
7. GitHub 远端 branch protection/ruleset 已实际将稳定状态 `PR Gate` 配置为 required check；
8. `make ci` 与云端执行的测试集合一致。

## 12. 第一轮实现结果

本轮已完成 Phase 1、Phase 2 的场景实现，并继续覆盖公开恢复边界：

- 已重组测试目录并增加 Scenario、严格 scripted Provider 和 Drop-safe PTY harness；
- 已增加 Makefile/CI 分层入口和 integration target inventory；
- 已将 CI coverage 拆为三个独立状态，并增加 fail-closed `PR Gate`、PR 并发取消与 merge queue 触发契约；远端 required 设置仍待实际配置；
- 已迁移现有测试且保留断言；最终二进制场景现覆盖配置、Provider、工具、权限链、跨进程 resume、验证、公开 lifecycle/boundary、legacy migration 和真实 PTY；
- 只读工具循环暴露出 v1 checkpoint 会把文件 secret 原样回传 Provider；现已在持久化和 Provider 重入前复用统一 redactor，并增加 legacy checkpoint normalization 回归 UT；
- required 路径没有 `#[ignore]` 或条件 skip；
- 修复一次由测试只读探针过早打开 SQLite 引起的 PTY flake 后，最终 E2E target 已在 macOS 连续 10 轮零失败；Linux 10 轮证据由 CI 补齐；
- TUI reconciliation/input 已落地；H-009 的真实 kill barrier、cassette replay 与 release smoke 仍是下一轮缺口。

这能先让测试卡点“看得见、跑得准、不会漏”，再逐项补齐真正的行为保护。

## 13. 可行性复核

本设计不是从空白假设测试能力：

- `cargo test --workspace --lib --bins --all-features -- --list` 当前可独立发现 245 个 crate-local 测试，UT-only profile 为 95.06%；
- 当前独立 `contract` 与 `e2e` targets 共 84 个 integration tests，其中最终二进制 E2E 71 个；
- 三个 fresh llvm-cov profile 均通过：UT-only 95.06%、最终二进制 E2E 80.55%、全 targets 96.57%–96.58%；
- 最终二进制、loopback Provider、真实 PTY、跨进程 SQLite resume 和 terminal mode restore 都已有可复用实现；
- Provider endpoint 已可指向 loopback harness，因此 cassette replay 可以复用同一最终二进制路径，不要求生产后门；
- runtime 已有 process `Started`、Unknown、restart recovery 和 reconciliation 的 component tests，外部 barrier E2E 是把既有语义提升到最终二进制边界，不要求新增生产后门；
- 当前完整 `make ci` 已在本机通过；分层入口仍可分别执行，覆盖率 profile 在每一层前独立清理。
