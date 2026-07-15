# 本地开发指南

本文说明如何在本地构建、测试和验证 Rust 版本的 Latte Code。项目不依赖 Node.js、npm 或 TypeScript 工具链。

## 环境准备

需要安装 Rust（推荐使用 rustup）。仓库的 `rust-toolchain.toml` 会自动选择工具链，并声明 rustfmt 与 Clippy 组件。

首次开发时运行：

```bash
make setup
```

该命令会安装或检查：

- rustfmt
- Clippy
- llvm-tools-preview
- cargo-llvm-cov
- cargo-deny

脚本不会重复安装已经可用的 Cargo 工具。

## 常用命令

```bash
make build       # 编译整个 workspace
make test-unit   # 执行 crate-local UT（含 bin 测试编译）
make test-contract # 执行公开 contract/component targets
make test-e2e-portable # 执行三平台 final-binary CLI/Provider/SQLite E2E
make test-e2e-unix # 执行 Linux/macOS PTY/process E2E
make test-e2e    # 在 Unix 上组合以上两套 E2E
make test-doc    # 执行 doc tests
make test-all    # 执行 inventory 和以上全部测试层
make test        # test-all 的兼容入口
make check       # fmt + check + Clippy + Rustdoc
make lint-ci     # actionlint workflow syntax + ShellCheck shell analysis
make coverage-unit # UT-only 行覆盖率 >= 95%
make coverage-e2e  # 最终二进制 E2E 行覆盖率 >= 80%
make coverage-total # 全 targets 行覆盖率 >= 90%
make coverage    # 串行执行以上三个独立覆盖率卡点
make deny        # 检查安全公告、许可证和依赖来源
make ci          # 完整复现本地 CI
make release     # 构建 release 二进制
```

查看全部目标：

```bash
make help
```

也可以直接运行脚本：

```bash
./scripts/bootstrap.sh
./scripts/check.sh
./scripts/test.sh
./scripts/ci-local.sh
```

所有脚本都会先切换到仓库根目录，因此可以从任意工作目录调用。

## 运行程序

```bash
make tui
make run ARGS='--help'
make run ARGS='--json list'
```

没有配置文件时，CLI/TUI 会使用内置默认配置。自定义配置按“内置默认值 → `$HOME/.latte/latte-code.jsonc` → `<workspace>/.latte/latte-code.jsonc`”递归合并；相同 key 由工作区配置覆盖，数组和标量整体替换。执行真实的 `run`、`resume --allow` 或 TUI 前，只需导出最终配置所引用的密钥。若要自定义当前工作区：

```bash
mkdir -p .latte
cp latte-code.config.example.jsonc .latte/latte-code.jsonc
export OPENAI_API_KEY='...'
```

用户配置和工作区配置都可以只写需要覆盖的 key。在 `.latte/latte-code.jsonc` 中可配置 `default_provider`、`providers.<name>`、`database.path` 和 `verification.argv`。示例使用 `type: "openai-chat"`、`base_url`、`model`，以及 `api_key: { source: "env", name: "OPENAI_API_KEY" }`；CLI/TUI 只在实际调用 Provider 时于内存中解析该引用。不要把密钥直接写进 JSONC。无论相对路径来自哪一层，`database.path` 都以工作区根目录为基准。`latte-engine::config` 的 `${NAME}` 占位符和 `.latte/latte-engine.jsonc` 仅供嵌入式集成使用，不是 CLI/TUI 的配置接口。

`resume --deny`、等待态取消、查看运行记录和 Unknown reconciliation 不依赖 Provider 凭证。

## 测试层次

- crate 内单元测试：状态机、存储、权限、工具、进程监督和 TUI reducer，通过 `make test-unit` 运行。
- contract/component：公开 Engine 生命周期、协议与 repo contract，通过 `make test-contract` 运行。
- portable 最终二进制 E2E：三平台执行真实 CLI、loopback Provider 和 SQLite 持久化，通过 `make test-e2e-portable` 运行；Windows 不跳过整个 target。
- Unix 最终二进制 E2E：保留 75 个包含 PTY、process group、signal、symlink 和 Unix verification 的场景，通过 `make test-e2e-unix` 运行。
- doc tests：验证公开 authority API 的 compile-fail 边界。
- Markdown 链接测试：确保 README、AGENTS 和 `docs/` 内的本地链接有效。
- 覆盖率：UT-only、最终二进制 E2E、全 targets 分别独立统计，行覆盖率不得低于 95%、80%、90%。

任何新增或修改产品行为的功能都必须同时增加最低责任层 UT 和至少一个最终二进制 E2E。具体目录、Harness、同步方式、断言清单和反例见 [E2E 编写手册](docs/zh-CN/testing/e2e-authoring-guide.md)。

UT 和 E2E 覆盖率使用互相独立的统计口径，不能合并或互相替代：

```bash
make coverage-unit
make coverage-e2e
```

全仓 UT-only 行覆盖率必须 `>= 95%`，最终二进制 E2E 行覆盖率必须 `>= 80%`。新增或修改的功能代码还必须由对应责任层测试直接覆盖，不能仅依赖既有测试维持全仓数字。全 targets 的 `>= 90%` 卡点继续保留；`make coverage` 会串行执行三项卡点并在每项前清理 profile，避免跨层数据污染。

## 与 CI 的关系

`make ci` 是本地最完整的提交前检查，对应 `.github/workflows/ci.yml` 中的格式、Clippy、测试、文档、三项独立覆盖率和依赖审计。所有变更必须从功能分支通过面向 `main` 的 PR 提交，禁止直接推送 `main`。

GitHub Actions 在 `main` push、面向 `main` 的 PR、merge queue 和手工触发时运行。PR 新提交会取消同一 PR 的旧运行，`main` 和 merge queue 的运行不会被新运行取消。底层 job 保持独立可见，包括：

- Linux、macOS、Windows 各自的 Cargo check、Clippy `-D warnings`、UT、Contract、portable 最终二进制 E2E 和 `latte-code` release build；
- Linux、macOS 的 75 个 Unix PTY/process 最终二进制 E2E；
- Rust 1.93 MSRV、actionlint 1.7.12、ShellCheck、Documentation tests 和 Dependency audit；
- `Coverage - UT (95%)`、`Coverage - E2E (80%)`、`Coverage - total (90%)` 三个独立状态；

稳定聚合状态 `PR Gate` 使用 fail-closed 语义检查上述每个 job 的结果；失败、取消或跳过任何一个依赖都不会产生假绿。仓库的 branch protection 或 ruleset 应只把 `PR Gate` 配置为 required check，底层 job 仍可用于定位失败。

`.github/workflows/ci.yml` 只实现了检查和聚合逻辑；branch protection/ruleset 是 GitHub 远端设置。在远端实际要求 `PR Gate` 之前，不能宣称 required 卡点已经激活。

三平台 release build 属于 `PR Gate`，但当前只证明产物可构建，不等同于尚未实现的 G5 release smoke。Cargo CI 命令均在适用处使用 `--locked`。

Windows 上的进程监督和安全文件变更目前运行时 fail-closed；portable E2E 因此覆盖 CLI、SQLite、HTTP 200 input request 和 terminal Provider failure，不伪造“验证成功”。完整成功 verification、PTY 和 process 场景仍由 Linux/macOS 卡点承担。

`make ci` 会调用 `scripts/lint-ci.sh`。本机已安装 actionlint/ShellCheck 时直接使用；否则使用 pinned Docker image（actionlint 1.7.12、ShellCheck 0.11.0）。Hosted CI 的 `Repository quality` job 直接执行同样的两类静态审计。

## 本地状态与清理

运行状态默认存放在 `.latte/latte-code.db`；可通过 `database.path` 配置其他位置。构建输出位于 `target/`。默认状态目录与构建目录已被 Git 忽略。

```bash
make clean
rm -rf .latte   # 仅在明确不需要本地运行历史时执行
```

不要把 Provider 密钥、`.env`、SQLite 状态、日志或覆盖率原始数据提交到仓库。
