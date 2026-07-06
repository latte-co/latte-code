# Module Technical Design: Provider Compatibility Layer

## Document Status

This document records the future / near-term model provider compatibility constraints for Lattecode. It should guide later `src/model` and `src/config` implementation work; it does not mean the current `src/` already implements a full provider catalog, multi-protocol transport layer, automatic failover, or live smoke capability.

Chinese counterpart: [`docs/zh-CN/design/modules/provider-compatibility-layer.md`](../../../zh-CN/design/modules/provider-compatibility-layer.md).

This document is derived from comparative research inputs such as `opencode` and `openclaw`, but projects under `.tmp/` are research inputs only. They are not Lattecode formal source code, configuration format, or build evidence.

## 1. Goals and Non-goals

The Provider Compatibility Layer should keep `AgentLoop` provider-agnostic while allowing Lattecode to gradually support different model API modes, model capabilities, deployment environments, and authentication styles.

Core goals:

- `AgentLoop` depends only on Lattecode's internal model runtime protocol, not provider SDKs, HTTP details, or provider-specific event shapes.
- Provider compatibility logic lives under `src/model`; configuration parsing and credential references are supported by `src/config`.
- User-facing config uses `type` to express the provider / deployment class; implementation then resolves it into API mode / protocol dialect / transport adapter, and `apiMode` is not the main user-facing config key.
- User-facing config-layer `type`, provider identity/catalog, internal API mode/protocol dialect, model capability metadata, deployment/runtime environment, auth/runtime state/router, and provider-specific quirks are separate concepts.
- Streaming is runtime semantics: event order, tool-call assembly, cancellation, backpressure, usage accounting, event logs, and replay are compatibility-layer contracts, not only UI output concerns.

Non-goals:

- Do not rewrite or reuse existing formal `v0.2`-`v0.5` milestone labels in this document.
- Do not promise that every provider will be implemented in the same phase.
- Do not make third-party SDKs the core loop abstraction.
- Do not store plaintext secrets in config, docs, event logs, or test fixtures.
- Do not allow silent failover across unconfirmed data residency, security, or billing boundaries.

## 2. Boundary with Existing Modules

The Provider Compatibility Layer belongs inside the basic code agent's model loop boundary:

```text
AgentLoop / PhaseRunner
  -> ModelRuntime
  -> ProviderRuntimeAdapter
  -> protocol transport / SDK wrapper / local router
```

Boundary rules:

- `AgentLoop` receives only normalized model events and normalized final results.
- `AgentLoop` does not know provider names, HTTP routes, SSE chunk schemas, SDK response types, or authentication profiles.
- `src/model` owns provider catalog, capability registry, runtime adapters, stream normalization, error normalization, and retry policy.
- `src/config` reads JSONC config, resolves env refs, validates provider/model/deployment config, and provides `src/model` with runtime descriptors that do not expose secret values.
- `src/model` may refer to later runtime objects such as `Evidence`, event logs, and replay markers, but it must not require the current `v0.1` to have a complete `ActionGraph` or long-term runtime kernel implementation.

## 3. Internal Protocol Boundary

Lattecode must define its internal model protocol before defining the capability matrix. Provider adapters convert external APIs into these internal concepts; external API shapes must not spread inward.

### 3.1 `ModelRuntime`

`ModelRuntime` is the only model execution interface visible to `AgentLoop`:

```ts
type ModelRuntime = {
  readonly id: string;
  readonly capability: ProviderCapability;
  generate(request: ModelRequest, signal?: AbortSignal): Promise<ModelResult>;
  stream(request: ModelRequest, signal?: AbortSignal): AsyncIterable<NormalizedModelEvent>;
};
```

Design requirements:

- `generate` and `stream` receive the same canonical request model.
- `stream` must preserve recoverable event semantics under cancellation, provider errors, partial tool calls, usage-only chunks, and empty deltas.
- `ModelRuntime` does not expose SDK clients, fetch requests, SSE parsers, or provider-specific headers.
- `FakeModelRuntime` and fixture runtimes must implement the same interface for adapter conformance tests.

### 3.2 `NormalizedModelEvent`

`NormalizedModelEvent` is the stream unit consumed by event logs, tool-call assembly, UI renderers, replay, and verification:

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

Event semantics:

- Tool-call arguments are assembled from `tool_call_delta` and pass through JSON / schema validation before `tool_call_end` can drive tool execution.
- `content_delta`, `reasoning_delta`, and `tool_call_delta` may be interleaved; adapters must provide deterministic assembly.
- Usage may appear only at the end, in a standalone chunk, or in response metadata; it must normalize into a `usage` event or final `ModelResult.usage`.
- Cancellation must produce an auditable terminal state; it must not be reported as successful completion.
- Event logs record only redacted metadata and required deltas; they must not record secrets, Authorization headers, API keys, or full credential sources.

### 3.3 `ProviderCapability`

`ProviderCapability` describes normalized runtime capability rather than provider marketing labels:

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

The capability registry must distinguish what a provider claims from what the Lattecode adapter has normalized and tested. `AgentLoop` may depend only on the latter.

## 4. Concept Separation

Provider compatibility must not treat provider name or a user config key as the only source of truth. At minimum, it separates six classes of information:

| Concept | Examples | Owner | Design requirement |
| --- | --- | --- | --- |
| User-facing provider `type` | `openai-compatible`, `openai-responses`, `anthropic`, `gemini`, `vertex`, `bedrock`, `ollama`, `custom` | `src/config` | Primary user config classification key; selects config templates, default capability lookup, and the internal adapter resolution entry point; does not promise implemented support |
| Provider identity / catalog | `openai`, `anthropic`, `google`, `bedrock`, `ollama`, `local-router` | `src/model` catalog + `src/config` refs | Used for display, defaults, docs, and capability lookup; does not directly determine transport |
| Internal API mode / protocol dialect | `openai-compatible-chat`, `openai-responses`, `anthropic-messages`, `gemini`, `vertex`, `bedrock-converse`, `ollama-native` | `src/model` adapter | Internal transport discriminator; determines request/stream/error normalization; may be resolved from `type`, endpoint, model metadata, or an explicit internal descriptor |
| Model capability metadata | tools, JSON/schema, vision, reasoning, token window, streaming | `src/model` registry | Determines `AgentLoop` strategy and prompt/tool formatting |
| Deployment / runtime environment | public API, enterprise proxy, regional endpoint, local Ollama, router gateway | `src/config` + runtime descriptor | Determines base URL, data boundary, network/cost policy, and live smoke eligibility |
| Provider-specific quirks | event ordering, tool argument chunking, finish reason, rate-limit headers | adapter-local quirks table | Must not leak to `AgentLoop`; must have fixture coverage |

Configuration sketch:

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

After parsing, `src/config` may provide `src/model` with a runtime descriptor that contains an internal protocol/API mode or transport adapter discriminator. That field is an implementation detail; it must not replace user config `type`, and it does not need to appear directly in user-editable config.

Safety requirement: config may contain only secret references such as `apiKeyEnv`, credential profile names, or OS keychain references. It must not contain plaintext keys.

## 5. Provider `type` / Internal API Taxonomy

The user config layer should use `type` to express the provider / deployment class. `type` is a config classification and template-selection entry point, not a directly exposed protocol discriminator, and it does not mean Lattecode currently implements every provider in that class.

| User-facing `type` | Typical use | Default internal resolution direction |
| --- | --- | --- |
| `openai-compatible` | OpenAI Chat Completions, local or enterprise OpenAI-compatible routers | `openai-compatible-chat` or router-specific transport |
| `openai-responses` | OpenAI Responses API or compatible implementations | `openai-responses` |
| `anthropic` | Anthropic Messages or compatible implementations | `anthropic-messages` |
| `gemini` | Google Generative AI | `gemini` |
| `vertex` | Vertex AI Gemini / enterprise endpoints | `vertex` |
| `bedrock` | AWS Bedrock Converse / ConverseStream | `bedrock-converse` |
| `ollama` | Ollama local API | `ollama-native` |
| `custom` | Custom routers / adapters that need explicit endpoint, auth, and capability overrides | User-confirmed adapter or custom transport descriptor |

The internal implementation layer should still build adapters by API mode / protocol dialect / transport adapter. Provider identity and `type` select default adapters, capability metadata, and config templates only.

| Internal API mode / protocol dialect | Representative provider / deployment | Main differences | Near-term priority |
| --- | --- | --- | --- |
| `openai-compatible-chat` | OpenAI Chat Completions, OpenAI-compatible routers, local gateways | messages, tools, SSE deltas, finish reasons, usage metadata | High: broadest basic compatibility surface |
| `openai-responses` | OpenAI Responses API and compatible implementations | item/event model, reasoning/tool events, response output array | Medium: needs independent event normalizer |
| `anthropic-messages` | Anthropic Messages and compatible implementations | content blocks, tool_use/tool_result, message_delta, stop_reason | Medium: different tool block semantics |
| `gemini` | Google Generative AI | contents/parts, function calling, safety feedback, candidate stream | Medium |
| `vertex` | Vertex AI Gemini / enterprise endpoints | project/location/auth, quota, regional boundary | Medium: more complex deployment environment |
| `bedrock-converse` | AWS Bedrock Converse / ConverseStream | AWS auth, model ARN, content blocks, throttling metadata | Medium / later |
| `ollama-native` | Ollama local API | local models, limited tool support, pull/list model lifecycle | High: relevant to local-first direction |
| `local-router-openai-compatible` | LiteLLM, OpenRouter, enterprise proxy, local router | OpenAI-like surface with inconsistent capability/headers/errors | High: requires explicit data boundary |

One provider or `type` may support multiple internal API modes; one API mode may be implemented by many providers or routers. Lattecode must not implicitly switch modes based on provider name; the mapping must be explicit through config parsing, capability metadata, or a user-confirmed adapter choice.

## 6. Capability Matrix

The capability matrix is an adapter conformance output, not a marketing compatibility table. Every entry must be marked as `supported`, `normalized`, `tested`, `degraded`, or `unsupported`.

| Capability | `openai-compatible-chat` | `openai-responses` | `anthropic-messages` | `gemini` / `vertex` | `bedrock-converse` | `ollama-native` | Router compatibility layer |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Text generation | normalized | normalized | normalized | normalized | normalized | normalized | normalized if route passes fixtures |
| Streaming deltas | normalized | normalized via item events | normalized via content blocks | normalized via candidates | normalized via ConverseStream | normalized if available | adapter-specific |
| Tool calling | basic/parallel depends on model | event/item based | block based | function declarations | tool config varies | limited / model-specific | must be probed or configured |
| JSON / schema output | response_format or prompted | schema-capable where supported | prompted / tool-mediated | schema/function mediated | provider-dependent | prompted | router-dependent |
| System prompt | native | native | native system field | merged or system instruction | provider-dependent | merged | route-dependent |
| Vision input | model-dependent | model-dependent | model-dependent | model-dependent | model-dependent | model-dependent | must not assume |
| Reasoning events | usually none | possible | provider/model-dependent | provider/model-dependent | provider-dependent | none | redacted by default |
| Usage accounting | provider metadata | provider metadata | provider metadata | metadata / estimated | metadata | estimated or none | unreliable unless verified |
| Rate-limit metadata | headers / error body | headers / error body | headers / error body | API-specific | AWS throttling | local resource errors | router-specific |

Capability use rules:

- `AgentLoop` decides whether to enable tool calling, JSON schema, vision input, or reasoning event display from normalized capabilities.
- Capability gaps must degrade explicitly. For example, falling back from schema output to prompted JSON must be recorded in events/evidence.
- Router providers must support capability override or startup probing; an OpenAI-compatible surface must not be assumed to equal OpenAI capability.

## 7. Tool / Function Calling Normalization

Lattecode's internal tool-calling contract is independent of providers:

```text
Lattecode ToolSpec
  -> adapter-specific tool declaration
  -> provider stream tool events
  -> NormalizedToolCall
  -> permission / schema gate
  -> Lattecode tool execution
  -> adapter-specific tool result message
```

Normalization requirements:

- `ToolSpec` contains only name, description, JSON schema, mutating/risk metadata, and permission hints; it does not contain provider SDK types.
- Tool arguments must not execute before full assembly.
- Partial JSON, duplicate tool ids, missing tool names, parallel tool calls, and tool-call cancellation must have deterministic handling.
- If a provider lacks native tools, Lattecode may use a prompted JSON/tool protocol, but it must mark the path as degraded and reduce automatic execution permission.
- Tool result injection must preserve `toolCallId` to avoid mismatches under parallel tool calls.
- Adapters normalize provider-specific finish reasons into internal values such as `tool_calls`, `stop`, `length`, `content_filter`, `cancelled`, and `error`.

## 8. Transcript and Model Event Normalization

The transcript is part of a recoverable agent run. Raw provider responses should not be the only source of truth.

Requirements:

- Input transcripts use Lattecode's canonical message/content/tool-result model.
- Output streams normalize into `NormalizedModelEvent` before being written to event logs, UI, tool assemblers, and final transcripts.
- Raw provider responses may be attached only in debug mode and only in redacted form; they are not persisted by default.
- Replay uses normalized events, not provider SDK objects.
- Compaction must not remove tool-call ids, permission decision ids, usage accounting, or error boundaries.
- Reasoning / thinking events are sensitive by default. Unless provider terms and user config permit retention, record only summaries or redacted markers.

## 9. Streaming Runtime Semantics

Streaming belongs to runtime semantics, not just terminal display optimization.

Required handling:

- **Tool-call assembly**: accumulate tool name, id, and arguments across chunks; do not execute before the end event.
- **Cancellation**: user cancellation, phase budget exhaustion, permission denial, and shell gate block propagate through `AbortSignal` and write cancelled events.
- **Backpressure**: slow UI, slow event log flush, or blocked tool assembly must not lead to unbounded adapter buffering; use a bounded queue or pull-based `AsyncIterable`.
- **Usage accounting**: streaming usage may arrive in the final chunk; allow late usage updates.
- **Event logs**: every message/event has a stable id for recovery, audit, and handoff references.
- **Replay**: normalized events are sufficient to rebuild the final message, tool calls, usage, and stop reason.
- **Error boundary**: provider stream interruption, malformed chunks, network retries, and rate limits distinguish retryable from non-retryable failures.

## 10. Auth, Credentials, and Runtime State

Credential safety is a hard boundary for provider compatibility.

Config and credential rules:

- Config may contain only `apiKeyEnv`, `tokenEnv`, profile ids, well-known auth sources, or OS keychain references.
- Docs, examples, fixtures, event logs, handoffs, error messages, and debug dumps must not contain real secrets.
- Secret values exist only briefly inside the runtime credential resolver; they do not enter normalized events, capability registries, or session snapshots.
- Logs and errors must be redacted: Authorization headers, API keys, bearer tokens, signed URLs, and sensitive fragments in credential file paths are hidden.
- OAuth, API key, cloud provider auth, and local unauthenticated endpoints require separate auth strategies. A single `apiKey` field must not cover all cases.

Failover and data boundary:

- Failover may occur only within the same user-confirmed data / security / compliance boundary.
- Switching from a local model to public cloud, from an enterprise endpoint to a public endpoint, or from a specified region to a cross-region endpoint must block and ask for user confirmation.
- Failover records reason, source model, target model, capability degradation, and cost/security boundary.
- Rate-limit fallback must not leak prompts, tool outputs, or repo content to unauthorized providers.

Runtime state:

- Token refresh, cooldown, rate-limit budget, endpoint health, and discovery cache are runtime state; they are not project config.
- Runtime state should be clearable, expirable, and partitioned by provider/deployment.
- Discovery cache may store only non-sensitive metadata. It must not cache secret-bearing responses.

## 11. Error, Retry, Rate Limit, and Router Behavior

Provider adapters must emit normalized errors:

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

Policy requirements:

- `auth`, `permission`, `invalid_request`, and `unsupported_capability` are non-retryable by default.
- `network`, `timeout`, and `provider_overloaded` may retry under a bounded retry policy.
- `rate_limit` / `quota` parses `retryAfterMs` from provider headers or bodies; if unavailable, use conservative backoff.
- Retry must be idempotent: it must not repeat already emitted tool calls or stitch a partial stream into a successful result.
- The actual provider/model route returned by a router must be recorded as metadata. If a router silently routes to a different data boundary, treat it as a policy violation.
- Cooldown/failover is a `src/model` runtime/router concern, not an `AgentLoop` concern.

## 12. SDK / HTTP Adapter Policy

The current implementation direction should prefer direct fetch / minimal HTTP adapters so Lattecode controls protocol boundaries, event normalization, and credential safety. Future SDKs or AI SDK-style provider wrappers are allowed only if:

- SDK wrappers exist only inside provider adapters and never become type dependencies of `AgentLoop`, phase contracts, or tool contracts.
- SDK responses, stream chunks, and error classes immediately convert into Lattecode normalized protocol.
- SDK automatic retry, telemetry, credential discovery, proxy behavior, file upload, and similar side effects are explicitly disabled or wrapped under Lattecode policy.
- SDK version and provider catalog metadata are testable, pinned, and replaceable.
- If Lattecode borrows Vercel AI SDK / provider catalog organization, it borrows only the catalog/wrapper pattern. `streamText` and SDK-specific message shapes must not leak into Lattecode's internal protocol.

## 13. Provider Compatibility Sub-roadmap

To avoid conflict with the current formal `v0.1`-`v0.5` roadmap labels, this document uses provider compatibility sub-roadmap ids. They may map to existing milestone lanes, but they do not rename formal milestones.

| Sub-roadmap | Goal | Mappable existing lane | Exit criteria |
| --- | --- | --- | --- |
| `PCL-0` | Stabilize `ModelRuntime`, `NormalizedModelEvent`, `ProviderCapability`, and config credential refs | `v0.1` Model loop / Config | Fake runtime + one direct-fetch adapter fixture pass conformance |
| `PCL-1` | Minimal `openai-compatible-chat` and `ollama-native` adapters | Basic local-first agent | Text streaming, tool-call fixtures, redacted auth, and mocked errors pass |
| `PCL-2` | Router / OpenAI-compatible local gateway support | Provider extensibility lane | Data boundary, capability override, and rate-limit normalization pass |
| `PCL-3` | `openai-responses`, `anthropic-messages`, and `gemini` adapter PoCs | Post-v0.1 provider expansion | Golden transcripts, tool calls, and stream event assembly pass |
| `PCL-4` | Enterprise deployment: Vertex / Bedrock / auth profiles / failover policy | Later deployment hardening | Opt-in live smoke, cloud auth redaction, and no silent boundary switch |

Every stage must keep `AgentLoop` provider-agnostic. If a provider needs special prompts, special tool result formatting, or special retry behavior, that difference stays in the adapter/config layer.

## 14. Testing and Smoke Strategy

The Provider Compatibility Layer must prioritize offline, repeatable, secret-free tests.

Required test layers:

- **Adapter conformance fixtures**: each API mode has request/response/stream fixtures that verify normalized events, final transcript, usage, finish reason, and normalized error.
- **Golden transcripts**: cover multi-turn text, tool calls, parallel tool calls, tool result injection, schema output, and degraded prompted JSON.
- **Mocked streaming**: simulate chunk splitting, empty deltas, usage-only chunks, partial JSON, provider metadata arriving late, stream interruption, cancellation, and backpressure.
- **Tool-call tests**: verify tool id retention, argument assembly, schema failure, permission block, and tool result correlation.
- **Error / rate-limit / retry tests**: cover auth failure, 429 with retry-after, quota exhaustion, network reset, timeout, provider overload, content filter, and unsupported capability.
- **Credential redaction tests**: ensure config parsing, error messages, event logs, handoffs, and debug output do not contain secret values.
- **Router / failover tests**: verify data boundary mismatches block instead of silently switching.

Live smoke strategy:

- Live smoke is disabled by default and can only run through explicit environment variables or CLI flags.
- Live smoke uses minimal non-sensitive prompts, does not read private repo content, and does not call mutating tools.
- Live smoke verifies only endpoint reachability, basic text, stream shape, and redaction. It is not a prerequisite for unit tests.
- Live smoke failure should not invalidate offline test results, but it must report provider, `type`, internal API mode / transport adapter, deployment boundary, and redacted request id.

## 15. Implementation Checklist

When implementing Provider Compatibility Layer adapters, each adapter should at least satisfy:

- [ ] Define the user-visible provider `type` and resolve it to an internal `ProviderApiMode` / transport adapter entry; do not make `apiMode` the main user config key.
- [ ] Provide request builder, stream parser, event normalizer, error normalizer, and usage normalizer.
- [ ] Mark capability as `supported` / `normalized` / `tested`.
- [ ] Use env/profile/keychain references; do not store secrets in config or tests.
- [ ] Cover golden transcripts, tool-call assembly, mocked streams, error/retry/rate-limit, and redaction tests.
- [ ] Record deployment data boundary and prohibit silent cross-boundary failover.
- [ ] Ensure `AgentLoop` does not import provider SDKs, HTTP response types, or provider-specific chunk types.

## 16. Research Input Boundary

Comparative research notes:

- `opencode` shows an AI SDK/provider catalog style, branded provider/model identity, capability metadata, `streamText`, and OpenAI-compatible provider catalog organization that Lattecode can study.
- `openclaw` shows explicit API modes, provider config, discovery/runtime auth/stream wrapping/capability hooks, custom transports, auth profiles, failover, and cooldown boundaries that Lattecode can study.

These facts describe observable choices in external systems. Lattecode's formal design authority remains this document, `Code Agent Loop`, the architecture overview, and the current roadmap.
