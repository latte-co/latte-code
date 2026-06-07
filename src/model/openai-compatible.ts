import type { ModelClient, ModelMessage, ModelRequest, ModelTurn } from "./types.js";
import type { ModelProviderConfig } from "../config/types.js";
import { isRecord, toJsonObject } from "../shared/types.js";
import type { JsonObject, JsonValue } from "../shared/types.js";
import type { ToolDefinition } from "../tools/types.js";

export interface OpenAICompatibleModelClientOptions {
  providerId: string;
  config: ModelProviderConfig;
  env?: NodeJS.ProcessEnv;
  fetch?: typeof fetch;
}

interface ResolvedOpenAICompatibleConfig {
  providerId: string;
  model: string;
  baseUrl: string;
  apiKey: string;
  temperature?: number;
  maxOutputTokens?: number;
}

interface OpenAIMessage {
  role: "system" | "user" | "assistant" | "tool";
  content: string;
  tool_call_id?: string;
}

export class OpenAICompatibleModelClient implements ModelClient {
  private readonly config: ResolvedOpenAICompatibleConfig;
  private readonly fetchImpl: typeof fetch;

  constructor(options: OpenAICompatibleModelClientOptions) {
    this.config = resolveOpenAICompatibleConfig(options.providerId, options.config, options.env ?? process.env);
    this.fetchImpl = options.fetch ?? fetch;
  }

  async generate(request: ModelRequest): Promise<ModelTurn> {
    const response = await this.fetchImpl(`${this.config.baseUrl}/chat/completions`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${this.config.apiKey}`,
        "content-type": "application/json"
      },
      body: JSON.stringify(this.requestBody(request))
    });

    if (!response.ok) {
      const body = await response.text().catch(() => "");
      if (response.status === 429) throw new Error(`Provider '${this.config.providerId}' rate limited (429)`);
      throw new Error(`Provider '${this.config.providerId}' request failed with HTTP ${response.status}${body.length === 0 ? "" : `: ${body}`}`);
    }

    const payload = await response.json();
    return parseOpenAICompatibleTurn(this.config.providerId, payload);
  }

  private requestBody(request: ModelRequest): Record<string, unknown> {
    const body: Record<string, unknown> = {
      model: this.config.model,
      messages: [...request.messages.map(toOpenAIMessage), ...request.toolResults.map((toolResult): OpenAIMessage => ({ role: "tool", tool_call_id: toolResult.callId, content: JSON.stringify(toolResult) }))],
      tools: request.tools.map(toOpenAITool),
      tool_choice: "auto"
    };
    if (this.config.temperature !== undefined) body.temperature = this.config.temperature;
    if (this.config.maxOutputTokens !== undefined) body.max_tokens = this.config.maxOutputTokens;
    return body;
  }
}

export function resolveOpenAICompatibleConfig(providerId: string, config: ModelProviderConfig, env: NodeJS.ProcessEnv = process.env): ResolvedOpenAICompatibleConfig {
  if (config.type !== "openai-compatible") throw new Error(`Provider '${providerId}' is not openai-compatible`);
  if (config.apiKeyEnv === undefined || config.apiKeyEnv.trim() === "") throw new Error(`Provider '${providerId}' requires apiKeyEnv`);
  const apiKey = env[config.apiKeyEnv];
  if (apiKey === undefined || apiKey.trim() === "") throw new Error(`Provider '${providerId}' requires missing env '${config.apiKeyEnv}'`);
  const baseUrl = resolveEnvTemplate(providerId, "baseUrl", config.baseUrl ?? "https://api.openai.com/v1", env).replace(/\/+$/, "");
  if (baseUrl.length === 0) throw new Error(`Provider '${providerId}' resolved empty baseUrl`);
  return {
    providerId,
    model: config.model,
    baseUrl,
    apiKey,
    ...(config.temperature === undefined ? {} : { temperature: config.temperature }),
    ...(config.maxOutputTokens === undefined ? {} : { maxOutputTokens: config.maxOutputTokens })
  };
}

function resolveEnvTemplate(providerId: string, field: string, value: string, env: NodeJS.ProcessEnv): string {
  return value.replace(/\$\{([A-Z0-9_]+)\}/gi, (_match, name: string) => {
    const resolved = env[name];
    if (resolved === undefined || resolved.trim() === "") throw new Error(`Provider '${providerId}' ${field} references missing env '${name}'`);
    return resolved;
  });
}

function toOpenAIMessage(message: ModelMessage): OpenAIMessage {
  if (message.role === "tool") {
    if (message.toolCallId === undefined) return { role: "user", content: `Tool result: ${message.content}` };
    return { role: "tool", tool_call_id: message.toolCallId, content: message.content };
  }
  return { role: message.role, content: message.content };
}

function toOpenAITool(tool: ToolDefinition): Record<string, unknown> {
  return {
    type: "function",
    function: {
      name: tool.name,
      description: tool.description,
      parameters: schemaToJson(tool.inputSchema)
    }
  };
}

function schemaToJson(value: unknown): JsonValue {
  if (value === null || typeof value === "string" || typeof value === "number" || typeof value === "boolean") return value;
  if (Array.isArray(value)) return value.map(schemaToJson);
  if (!isRecord(value)) return null;
  const result: JsonObject = {};
  for (const [key, entry] of Object.entries(value)) result[key] = schemaToJson(entry);
  return result;
}

function parseOpenAICompatibleTurn(providerId: string, payload: unknown): ModelTurn {
  const message = firstChoiceMessage(payload);
  const toolCalls = Array.isArray(message.tool_calls) ? message.tool_calls.map((entry, index) => parseToolCall(providerId, entry, index)) : [];
  const content = typeof message.content === "string" ? message.content : "";
  if (toolCalls.length > 0) return { type: "tool_calls", content, toolCalls };
  return { type: "message", content };
}

function firstChoiceMessage(payload: unknown): Record<string, unknown> {
  if (!isRecord(payload) || !Array.isArray(payload.choices) || payload.choices.length === 0) throw new Error("Provider response is missing choices");
  const [choice] = payload.choices;
  if (!isRecord(choice) || !isRecord(choice.message)) throw new Error("Provider response is missing choice.message");
  return choice.message;
}

function parseToolCall(providerId: string, value: unknown, index: number): { id: string; name: string; input: JsonObject } {
  if (!isRecord(value)) throw new Error(`Provider '${providerId}' returned invalid tool call at index ${index}`);
  const externalId = value.id;
  const functionPayload = value.function;
  if (!isRecord(functionPayload)) throw new Error(`Provider '${providerId}' returned tool call without function at index ${index}`);
  const name = functionPayload.name;
  const rawArguments = functionPayload.arguments;
  if (typeof name !== "string" || name.length === 0) throw new Error(`Provider '${providerId}' returned tool call without function.name at index ${index}`);
  const parsedArguments = rawArguments === undefined || rawArguments === "" ? {} : JSON.parse(String(rawArguments));
  return {
    id: typeof externalId === "string" && externalId.length > 0 ? externalId : `${name}_${index}`,
    name,
    input: toJsonObject(parsedArguments)
  };
}
