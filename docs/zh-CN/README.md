# Fluxcode 文档

本目录收录 Fluxcode 相关的工程调研、架构设计与阶段性路线图。

## 文档索引

### Code Agent / Harness-native Code Agent

- [Code Agent 横向调研](./research/code-agent-survey.md)
  - 覆盖 `claude-code`、`codex`、`CodeWhale`、`opencode`、`oh-my-openagent` 五个系统。
  - 重点记录各系统的架构设计、能力边界、核心工具、agent loop、可借鉴能力和 graph-native 缺口。
- [Harness-native Code Agent 设计建议](./design/harness-native-code-agent.md)
  - 基于调研结论，给出 harness-native code agent 的目标架构、核心模型、执行生命周期、接口边界、风险和反模式。
- [基础 Code Agent 落地方案](./design/basic-code-agent-implementation-plan.md)
  - 定义第一阶段 TypeScript + Vitest 基础 code agent 的能力边界、JSONC 配置方案、agent loop、tool contract、测试验证和 graph-ready 预留接口。
- [MVP 切法与路线图](./design/mvp-roadmap.md)
  - 定义 MVP 必须覆盖的 graph lifecycle 能力、非目标、阶段性验收标准和后置能力路线图。

## 维护约定

- `research/`：记录调研事实、横向对比和可复查的观察结论，避免混入尚未验证的设计承诺。
- `design/`：记录设计建议、架构分层、接口模型和阶段性路线图。
- 新增文档时优先补充本 README 的索引，确保索引只指向实际存在且需要维护的正式文档。
- 面向读者的名称使用 `CodeWhale` 和 `oh-my-openagent`；旧路径名称只在"原路径"说明语境中出现。

## 术语边界

- **调研事实**：来自横向调研和可复查观察，用于描述已有系统的能力和限制。
- **设计建议**：基于调研事实推导出的 harness-native runtime 设计，不表示既有系统已经具备该能力。
- **CodeWhale**：指 `CodeWhale（原路径：.tmp/codeagent/DeepSeek-TUI）`。
- **oh-my-openagent**：指 `oh-my-openagent（原路径：.tmp/codeagent/oh-my-opencode）`。
