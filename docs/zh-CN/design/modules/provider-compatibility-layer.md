# 模块技术设计：Provider Compatibility Layer

## 文档状态

本文记录 Lattecode 未来 / 近期的模型 provider 兼容层设计约束。它用于指导后续 `src/model` 与 `src/config` 的实现演进，不表示当前 `src/` 已经具备完整 provider catalog、多协议 transport、自动 failover 或 live smoke 能力。

英文对应文档：[`docs/en-US/design/modules/provider-compatibility-layer.md`](../../../en-US/design/modules/provider-compatibility-layer.md)。

本文基于对 `opencode`、`openclaw` 等临时输入的比较性调研结论整理，但 `.tmp/` 下项目只作为 research input，不作为 Lattecode 正式源码、配置格式或构建依据。

## 1. 目标与非目标

Provider Compatibility Layer 的目标是让 `AgentLoop` 保持 provider-agnostic，同时允许 Lattecode 逐步接入不同模型 API 模式、模型能力、部署环境和认证方式。

核心目标：

- `AgentLoop` 只依赖内部模型运行协议，不 import provider SDK、HTTP 细节或 provider-specific event shape。
- provider 兼容逻辑集中在 `src/model`，配置解析与凭据引用支持放在 `src/config`。
- 用户可见配置以 `type` 表达 provider / deployment 类别；内部实现再解析为 API mode / protocol dialect / transport adapter，不把 `apiMode` 作为主要用户配置键。
- 明确拆分用户配置层的 `type`、provider identity/catalog、内部 API mode/protocol dialect、model capability metadata、deployment/runtime environment、auth/runtime state/router 和 provider-specific quirks。
- 将 streaming 视为 runtime semantics：事件顺序、tool-call assembly、取消、背压、usage accounting、event log 和 replay 都属于兼容层契约，而不只是 UI 输出。

非目标：

- 不在当前文档中改写既有 `v0.2`-`v0.5` 正式里程碑标签。
- 不承诺所有 provider 在同一阶段实现。
- 不把第三方 SDK 作为 core loop 抽象。
- 不在配置、文档、事件日志或测试 fixture 中保存明文 secret。
- 不允许 silent failover 跨越用户未确认的数据驻留、安全边界或计费边界。

## 2. 与现有模块的边界

Provider Compatibility Layer 应放在基础 code agent 的 Model loop 边界内：

```text
AgentLoop / PhaseRunner
  -> ModelRuntime
  -> ProviderRuntimeAdapter
  -> protocol transport / SDK wrapper / local router
```

边界约束：

- `AgentLoop` 只接收 normalized model events 和 normalized final result。
- `AgentLoop` 不知道 provider name、HTTP route、SSE chunk schema、SDK response type 或认证 profile。
- `src/model` 负责 provider catalog、capability registry、runtime adapter、stream normalizer、error normalizer 和 retry policy。
- `src/config` 负责读取 JSONC 配置、解析 env refs、校验 provider/model/deployment 配置，并向 `src/model` 提供已解析但不泄露 secret value 的 runtime descriptor。
- `src/model` 可以引用后续 runtime 对象（如 `Evidence`、event log、replay marker），但不得要求当前 `v0.1` 已完整实现 `ActionGraph` 或长期 runtime kernel。

## 3. 内部协议边界

在 capability matrix 之前，必须先固化 Lattecode 内部模型协议。Provider adapter 的职责是把外部 API 转换为这些内部概念，而不是让外部 API shape 向内扩散。

### 3.1 `ModelRuntime`

`ModelRuntime` 是 `AgentLoop` 看到的唯一模型执行接口：

```ts
type ModelRuntime = {
  readonly id: string;
  readonly capability: ProviderCapability;
  generate(request: ModelRequest, signal?: AbortSignal): Promise<ModelResult>;
  stream(request: ModelRequest, signal?: AbortSignal): AsyncIterable<NormalizedModelEvent>;
};
```

设计要求：

- `generate` 与 `stream` 接收同一个 canonical request model。
- `stream` 必须能在 cancellation、provider error、partial tool call、usage-only chunk 和 empty delta 情况下维持可恢复事件语义。
- `ModelRuntime` 不暴露 SDK client、fetch request、SSE parser 或 provider-specific headers。
- `FakeModelRuntime` 与 fixture runtime 必须实现同一接口，用于 adapter conformance test。

### 3.2 `NormalizedModelEvent`

`NormalizedModelEvent` 是 event log、tool-call assembly、UI renderer、replay 和 verification 共同消费的 stream 单元：

```ts
type NormalizedModelEvent =
  | { type: "message_start"; messageId: string; model: string }
  | { type: "content_delta"; messageId: string; index: number; text: string }
  | { type: "reasoning_delta"; messageId: string; index: number; text: string; redacted?: boolean }
  | { type: "tool_call_start"; messageId: string; toolCallId: string; name?: string }
  | { type: "tool_call_delta"; messageId: string; toolCallId: string; argumentsDelta: string }
  | { type: "tool_call_end"; messageId: string; toolCallId: string; name: string; argumentsText: string }
  | { type: "usage"; inputTokens?: number; outputTokens?: number; totalTokens?: number }
  | { type: "message_end"; messageId: string; finishReason: NormalizedFinishReason }
  | { type: "error"; error: NormalizedModelError; retryable: boolean };
```

事件语义要求：

- tool-call arguments 必须通过 `tool_call_delta` 组装，并在 `tool_call_end` 前通过 JSON / schema 校验入口。
- `content_delta`、`reasoning_delta` 和 `tool_call_delta` 可以交错到达；adapter 必须提供 deterministic assembly。
- usage 可能只在结尾、单独 chunk 或 response metadata 中出现；必须归一化为 `usage` event 或 final `ModelResult.usage`。
- cancellation 必须生成可审计的终止状态，不能被包装成成功完成。
- event log 只能记录 redacted metadata 和必要 delta；不得记录 secret、Authorization header、API key 或完整 credential source。

### 3.3 `ProviderCapability`

`ProviderCapability` 描述 normalized runtime 能力，而不是直接照搬 provider marketing label：

```ts
type ProviderCapability = {
  apiMode: ProviderApiMode;
  streaming: boolean;
  tools: "none" | "basic" | "parallel";
  toolChoice: "none" | "auto" | "required" | "named";
  jsonOutput: "none" | "prompted" | "response_format" | "schema";
  systemPrompt: "native" | "merged" | "unsupported";
  visionInput: boolean;
  reasoningEvents: boolean;
  usageAccounting: "none" | "estimated" | "provider";
  maxInputTokens?: number;
  maxOutputTokens?: number;
};
```

Capability registry 必须区分“provider 声称支持”和“Lattecode adapter 已归一化并测试通过”。`AgentLoop` 只能依赖后者。

## 4. 概念拆分

Provider compatibility 不应把 provider name 或用户配置键作为单一事实源。至少拆分为六类信息：

| 概念 | 示例 | 归属 | 设计要求 |
| --- | --- | --- | --- |
| User-facing provider `type` | `openai-compatible`、`openai-responses`、`anthropic`、`gemini`、`vertex`、`bedrock`、`ollama`、`custom` | `src/config` | 用户配置的主要 provider 分类键；用于选择配置模板、默认 capability lookup 和内部 adapter 解析入口；不等同于已实现能力承诺 |
| Provider identity / catalog | `openai`、`anthropic`、`google`、`bedrock`、`ollama`、`local-router` | `src/model` catalog + `src/config` refs | 用于展示、默认配置、文档和 capability lookup；不直接决定 transport |
| Internal API mode / protocol dialect | `openai-compatible-chat`、`openai-responses`、`anthropic-messages`、`gemini`、`vertex`、`bedrock-converse`、`ollama-native` | `src/model` adapter | 内部 transport discriminator；决定 request/stream/error normalization；可由 `type`、endpoint、model metadata 或显式内部 descriptor 解析得到 |
| Model capability metadata | tools、JSON/schema、vision、reasoning、token window、streaming | `src/model` registry | 决定 `AgentLoop` 可用策略和 prompt/tool formatting |
| Deployment / runtime environment | public API、enterprise proxy、regional endpoint、local Ollama、router gateway | `src/config` + runtime descriptor | 决定 base URL、data boundary、network/cost policy、live smoke eligibility |
| Provider-specific quirks | event ordering、tool arg chunking、finish reason、rate-limit headers | adapter-local quirks table | 不向 `AgentLoop` 泄漏；必须有 fixture 覆盖 |

配置示意：

```jsonc
{
  "models": {
    "default": {
      "provider": "openai",
      "type": "openai-compatible",
      "model": "gpt-4.1",
      "baseUrl": "https://api.openai.com/v1",
      "apiKeyEnv": "OPENAI_API_KEY",
      "dataBoundary": "public-openai"
    },
    "local": {
      "provider": "ollama",
      "type": "ollama",
      "model": "qwen2.5-coder",
      "baseUrl": "http://127.0.0.1:11434",
      "dataBoundary": "local-device"
    }
  }
}
```

解析后，`src/config` 可以向 `src/model` 提供包含内部 protocol/API mode 或 transport adapter discriminator 的 runtime descriptor；该字段属于内部实现细节，不应替代用户配置中的 `type`，也不要求直接出现在用户可编辑配置中。

安全要求：配置只允许 secret 引用，例如 `apiKeyEnv`、credential profile name 或 OS keychain reference；不得保存明文 key。

## 5. Provider `type` / internal API taxonomy

用户配置层应优先使用 `type` 表达 provider / deployment 类别。`type` 是配置分类和模板选择入口，不是直接对外暴露的 protocol discriminator，也不表示当前 Lattecode 已经实现该类别下的所有 provider。

| User-facing `type` | 典型用途 | 默认内部解析方向 |
| --- | --- | --- |
| `openai-compatible` | OpenAI Chat Completions、本地或企业 OpenAI-compatible router | `openai-compatible-chat` 或 router-specific transport |
| `openai-responses` | OpenAI Responses API 或兼容实现 | `openai-responses` |
| `anthropic` | Anthropic Messages 或兼容实现 | `anthropic-messages` |
| `gemini` | Google Generative AI | `gemini` |
| `vertex` | Vertex AI Gemini / enterprise endpoint | `vertex` |
| `bedrock` | AWS Bedrock Converse / ConverseStream | `bedrock-converse` |
| `ollama` | Ollama 本地 API | `ollama-native` |
| `custom` | 需要显式配置 endpoint、auth、capability override 的自定义 router / adapter | 用户确认的 adapter 或 custom transport descriptor |

内部实现层仍应按 API mode / protocol dialect / transport adapter 建 adapter。Provider identity 和 `type` 只选择默认 adapter、capability metadata 和配置模板。

| Internal API mode / protocol dialect | 代表 provider / deployment | 主要差异 | 近期优先级 |
| --- | --- | --- | --- |
| `openai-compatible-chat` | OpenAI Chat Completions、OpenAI-compatible routers、本地网关 | messages、tools、SSE delta、finish reason、usage metadata | 高：基础兼容面最大 |
| `openai-responses` | OpenAI Responses API、兼容实现 | item/event model、reasoning/tool events、response output array | 中：需要独立 event normalizer |
| `anthropic-messages` | Anthropic Messages、兼容实现 | content blocks、tool_use/tool_result、message_delta、stop_reason | 中：tool block 语义不同 |
| `gemini` | Google Generative AI | contents/parts、function calling、safety feedback、candidate stream | 中 |
| `vertex` | Vertex AI Gemini / enterprise endpoints | project/location/auth、quota、regional boundary | 中：部署环境更复杂 |
| `bedrock-converse` | AWS Bedrock Converse / ConverseStream | AWS auth、model ARN、content blocks、throttling metadata | 中 / 后续 |
| `ollama-native` | Ollama 本地 API | local models、limited tool support、pull/list model lifecycle | 高：local-first 路线相关 |
| `local-router-openai-compatible` | LiteLLM、OpenRouter、企业代理、本地 router | OpenAI-like surface 但 capability/headers/error 不一致 | 高：必须显式 data boundary |

注意：同一 provider 或 `type` 可能支持多个内部 API mode；同一 API mode 也可能由多个 provider 或 router 实现。Lattecode 不能通过 provider name 隐式切换 mode；需要通过配置解析、capability metadata 或用户确认的 adapter 选择明确建立映射。

## 6. Capability matrix

Capability matrix 是 adapter conformance 的输出，不是营销兼容表。每个 entry 必须标明 `supported`、`normalized`、`tested`、`degraded` 或 `unsupported`。

| Capability | `openai-compatible-chat` | `openai-responses` | `anthropic-messages` | `gemini` / `vertex` | `bedrock-converse` | `ollama-native` | Router 兼容层 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Text generation | normalized | normalized | normalized | normalized | normalized | normalized | normalized if route passes fixtures |
| Streaming deltas | normalized | normalized via item events | normalized via content blocks | normalized via candidates | normalized via ConverseStream | normalized if available | adapter-specific |
| Tool calling | basic/parallel depends on model | event/item based | block based | function declarations | tool config varies | limited / model-specific | must be probed or configured |
| JSON / schema output | response_format or prompted | schema-capable where supported | prompted / tool-mediated | schema/function mediated | provider-dependent | prompted | router-dependent |
| System prompt | native | native | native system field | merged or system instruction | provider-dependent | merged | route-dependent |
| Vision input | model-dependent | model-dependent | model-dependent | model-dependent | model-dependent | model-dependent | must not assume |
| Reasoning events | usually none | possible | provider/model-dependent | provider/model-dependent | provider-dependent | none | must be redacted by default |
| Usage accounting | provider metadata | provider metadata | provider metadata | metadata / estimated | metadata | estimated or none | unreliable unless verified |
| Rate-limit metadata | headers / error body | headers / error body | headers / error body | API-specific | AWS throttling | local resource errors | router-specific |

Capability use rules：

- `AgentLoop` 根据 normalized capability 决定是否启用 tool calling、JSON schema、vision input 或 reasoning event display。
- capability 不足时必须显式 degrade：例如从 schema output 降级到 prompted JSON 时，要在 event/evidence 中记录。
- router provider 必须支持 capability override 或 startup probe；不能假设其 OpenAI-compatible surface 等于 OpenAI capability。

## 7. Tool / function calling normalization

Lattecode tool calling 的内部契约必须独立于 provider：

```text
Lattecode ToolSpec
  -> adapter-specific tool declaration
  -> provider stream tool events
  -> NormalizedToolCall
  -> permission / schema gate
  -> Lattecode tool execution
  -> adapter-specific tool result message
```

规范化要求：

- `ToolSpec` 只包含 name、description、JSON schema、mutating/risk metadata 和 permission hint；不包含 provider SDK type。
- tool arguments 在完全组装前不得执行。
- partial JSON、重复 tool id、缺失 tool name、parallel tool calls、tool-call cancellation 都必须有 deterministic handling。
- provider 不支持 native tools 时，可以采用 prompted JSON/tool protocol，但必须标记为 degraded，并降低自动执行权限。
- tool result 回填必须保留 `toolCallId`，避免多工具并发时错配。
- adapter 必须把 provider-specific finish reason 归一化为 `tool_calls`、`stop`、`length`、`content_filter`、`cancelled`、`error` 等内部枚举。

## 8. Transcript 与 model event normalization

Transcript 是可恢复 agent run 的一部分，不应保存 provider 原始响应作为唯一事实源。

要求：

- 输入 transcript 使用 Lattecode canonical message/content/tool-result model。
- 输出 stream 先归一化为 `NormalizedModelEvent`，再写入 event log、UI、tool assembler 和 final transcript。
- 原始 provider response 只可在 debug mode 下以 redacted form 作为 evidence attachment，默认不持久化。
- replay 基于 normalized events，不依赖 provider SDK object。
- compaction 不得删除 tool-call id、permission decision id、usage accounting 和 error boundary。
- reasoning / thinking events 默认视为敏感内容；除非 provider terms 和用户配置允许，否则只记录摘要或 redacted marker。

## 9. Streaming runtime semantics

Streaming 必须纳入 runtime 语义，而不只是终端显示优化。

必须处理：

- **Tool-call assembly**：跨 chunk 累积 tool name、id、arguments；在 end event 前禁止执行。
- **Cancellation**：用户取消、phase budget 耗尽、permission denied、shell gate block 都应通过 `AbortSignal` 下传，并写入 cancelled event。
- **Backpressure**：UI 慢、event log flush 慢或 tool assembler 阻塞时，adapter 不能无限缓存；需要 bounded queue 或 pull-based `AsyncIterable`。
- **Usage accounting**：streaming 下 usage 可能延迟到 final chunk；必须允许 late usage update。
- **Event logs**：每个 message/event 有 stable id，便于恢复、审计和 handoff 引用。
- **Replay**：normalized events 应足以重建 final message、tool calls、usage 和 stop reason。
- **Error boundary**：provider stream 中断、格式错误、网络重试和 rate-limit 必须区分 retryable / non-retryable。

## 10. Auth、凭据与 runtime state

凭据安全是 provider compatibility 的硬边界。

配置与凭据规则：

- 配置只允许 `apiKeyEnv`、`tokenEnv`、profile id、well-known auth source 或 OS keychain reference。
- 文档、示例、fixture、event log、handoff、error message 和 debug dump 不得包含真实 secret。
- secret value 只在 runtime credential resolver 中短暂存在，不进入 normalized event、capability registry 或 session snapshot。
- 所有日志和错误必须经过 redaction：Authorization header、API key、bearer token、signed URL、credential file path 中的敏感片段都应脱敏。
- OAuth / API key / cloud provider auth / local unauthenticated endpoint 应分别建 auth strategy，不得用单一 `apiKey` 字段覆盖全部情况。

Failover 与 data boundary：

- failover 只能在同一用户确认的数据 / 安全 / 合规边界内发生。
- 从 local model 切到 public cloud、从 enterprise endpoint 切到 public endpoint、从指定区域切到跨区域 endpoint 都必须阻塞并请求用户确认。
- failover 必须记录 reason、source model、target model、capability degradation 和 cost/security boundary。
- rate-limit fallback 不得泄露 prompt、tool output 或 repo content 到未授权 provider。

Runtime state：

- token refresh、cooldown、rate-limit budget、endpoint health 和 discovery cache 属于 runtime state，不写入项目 config。
- runtime state 应可清理、可失效、可按 provider/deployment 分区。
- discovery cache 只能缓存非敏感 metadata；不得缓存 secret-bearing response。

## 11. Error、retry、rate-limit 与 router behavior

Provider adapter 必须输出 normalized error：

```ts
type NormalizedModelError = {
  category:
    | "auth"
    | "permission"
    | "rate_limit"
    | "quota"
    | "network"
    | "timeout"
    | "invalid_request"
    | "unsupported_capability"
    | "provider_overloaded"
    | "content_filter"
    | "internal";
  message: string;
  retryAfterMs?: number;
  providerRequestId?: string;
  redactedDetails?: Record<string, unknown>;
};
```

策略要求：

- auth、permission、invalid request、unsupported capability 默认 non-retryable。
- network、timeout、provider overloaded 可按 bounded retry policy 重试。
- rate limit / quota 使用 provider header 或 body 解析 `retryAfterMs`；无法解析时使用保守退避。
- retry 必须 idempotent：不能重复执行已发出的 tool call，也不能把 partial stream 拼接为成功结果。
- router 返回的 provider/model 实际路由信息必须作为 metadata 记录；如果 router silently routes 到不同 data boundary，应视为 policy violation。
- cooldown/failover 是 `src/model` runtime/router concern，不属于 `AgentLoop`。

## 12. SDK / HTTP adapter policy

当前实现方向应优先保持 direct fetch / minimal HTTP adapter，以便 Lattecode 控制协议边界、事件归一化和凭据安全。未来可以引入 SDK 或 AI SDK 风格 provider wrapper，但必须满足：

- SDK wrapper 只能存在于 provider adapter 内部，不能成为 `AgentLoop`、phase contract 或 tool contract 的类型依赖。
- SDK response、stream chunk、error class 必须立即转换为 Lattecode normalized protocol。
- SDK 自动 retry、telemetry、credential discovery、proxy、file upload 等 side effect 必须显式关闭或包装到 Lattecode policy 下。
- SDK version 与 provider catalog metadata 必须可测试、可 pin、可替换。
- 如果采用 Vercel AI SDK / provider catalog 风格，只能借鉴 catalog/wrapper 组织方式；不能让 `streamText` 或 SDK-specific message shape 泄漏为 Lattecode 内部协议。

## 13. Provider compatibility sub-roadmap

为避免与当前正式 `v0.1`-`v0.5` roadmap 标签冲突，本文使用 provider compatibility sub-roadmap 编号。它可以映射到现有 milestone lanes，但不重命名既有正式里程碑。

| Sub-roadmap | 目标 | 可映射到的现有 lane | Exit criteria |
| --- | --- | --- | --- |
| `PCL-0` | 固化 `ModelRuntime`、`NormalizedModelEvent`、`ProviderCapability` 和 config credential refs | `v0.1` Model loop / Config | fake runtime + one direct-fetch adapter fixture 通过 conformance |
| `PCL-1` | `openai-compatible-chat` 与 `ollama-native` 最小 adapter | 基础 local-first agent | text streaming、tool-call fixture、redacted auth、mocked errors 通过 |
| `PCL-2` | router / OpenAI-compatible local gateway support | provider extensibility lane | data boundary、capability override、rate-limit normalization 通过 |
| `PCL-3` | `openai-responses`、`anthropic-messages`、`gemini` adapter PoC | post-v0.1 provider expansion | golden transcripts、tool calls、stream event assembly 通过 |
| `PCL-4` | enterprise deployment：Vertex / Bedrock / auth profiles / failover policy | later deployment hardening | opt-in live smoke、cloud auth redaction、no silent boundary switch |

每个阶段都必须保持 `AgentLoop` provider-agnostic；如果某个 provider 需要特殊提示、特殊 tool result 或特殊 retry，该差异只能留在 adapter/config 层。

## 14. Testing 与 smoke strategy

Provider Compatibility Layer 必须优先使用离线、可重复、无 secret 的测试。

必需测试层：

- **Adapter conformance fixtures**：每个 API mode 有 request/response/stream fixture，验证 normalized events、final transcript、usage、finish reason 和 normalized error。
- **Golden transcripts**：覆盖 multi-turn text、tool call、parallel tool call、tool result 回填、schema output 和 degraded prompted JSON。
- **Mocked streaming**：模拟 chunk split、empty delta、usage-only chunk、partial JSON、out-of-order-ish provider metadata、stream interruption、cancellation 和 backpressure。
- **Tool-call tests**：验证 tool id 保留、arguments assembly、schema failure、permission block、tool result correlation。
- **Error / rate-limit / retry tests**：覆盖 auth failure、429 with retry-after、quota exhausted、network reset、timeout、provider overloaded、content filter 和 unsupported capability。
- **Credential redaction tests**：确保 config parse、error message、event log、handoff 和 debug output 都不出现 secret value。
- **Router / failover tests**：验证 data boundary mismatch 会 block，而不是 silent switch。

Live smoke 策略：

- live smoke 默认 disabled，只能通过显式环境变量或 CLI flag opt in。
- live smoke 使用最小无敏感 prompt，不读取 repo private content，不调用 mutating tools。
- live smoke 只验证 endpoint reachability、basic text、stream shape 和 redaction，不作为 unit test 前置条件。
- live smoke 失败不应阻塞离线测试结论，但必须报告 provider、`type`、内部 api mode / transport adapter、deployment boundary 和 redacted request id。

## 15. Implementation checklist

后续实现 Provider Compatibility Layer 时，每个 adapter 至少需要满足：

- [ ] 定义用户可见 provider `type`，并为其解析到内部 `ProviderApiMode` / transport adapter entry；不要把 `apiMode` 作为主要用户配置键。
- [ ] 提供 request builder、stream parser、event normalizer、error normalizer 和 usage normalizer。
- [ ] 标明 capability 的 `supported` / `normalized` / `tested` 状态。
- [ ] 使用 env/profile/keychain reference，不在 config 或 tests 中保存 secret。
- [ ] 覆盖 golden transcript、tool-call assembly、mocked stream、error/retry/rate-limit 和 redaction tests。
- [ ] 记录 deployment data boundary，并禁止 silent cross-boundary failover。
- [ ] 确保 `AgentLoop` 不 import provider SDK、HTTP response type 或 provider-specific chunk type。

## 16. Research input boundary

比较性调研说明：

- `opencode` 展示了 AI SDK/provider catalog 风格、branded provider/model identity、capability metadata、`streamText` 和 OpenAI-compatible provider catalog 等可参考组织方式。
- `openclaw` 展示了显式 API modes、provider config、discovery/runtime auth/stream wrapping/capability hooks、自定义 transports、auth profiles、failover 和 cooldown 等可参考边界。

这些事实只说明外部系统的可观察设计选择；Lattecode 的正式设计以本文、`Code Agent Loop`、架构总览和现有 roadmap 为准。
