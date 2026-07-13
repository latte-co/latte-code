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
make test        # 执行单元、集成和 doc tests
make check       # fmt + check + Clippy + Rustdoc
make coverage    # 执行测试并检查行覆盖率 >= 90%
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

- crate 内单元测试：状态机、存储、权限、工具、进程监督和 TUI reducer。
- workspace 集成测试：公开 Engine 生命周期、CLI、多轮 Provider、恢复和 PTY 终端行为。
- doc tests：验证公开 authority API 的 compile-fail 边界。
- Markdown 链接测试：确保 README、AGENTS 和 `docs/` 内的本地链接有效。
- 覆盖率：对全部 workspace、features 和 targets 执行，行覆盖率不得低于 90%。

## 与 CI 的关系

`make ci` 是本地最完整的提交前检查，对应 `.github/workflows/ci.yml` 中的格式、Clippy、测试、文档、覆盖率和依赖审计。GitHub Actions 还会额外执行：

- Ubuntu 与 macOS 原生检查。
- Windows 编译检查。
- Linux、macOS 和 Windows release artifact 构建。

Windows 上的进程监督和安全文件变更目前运行时 fail-closed；CI 只保证 Windows 编译通过。

## 本地状态与清理

运行状态默认存放在 `.latte/latte-code.db`；可通过 `database.path` 配置其他位置。构建输出位于 `target/`。默认状态目录与构建目录已被 Git 忽略。

```bash
make clean
rm -rf .latte   # 仅在明确不需要本地运行历史时执行
```

不要把 Provider 密钥、`.env`、SQLite 状态、日志或覆盖率原始数据提交到仓库。
