import { describe, expect, it } from "vitest";
import { DEFAULT_CONFIG } from "../../src/config/defaults.js";
import { mergeConfig } from "../../src/config/config.js";
import { OpenAICompatibleModelClient, resolveOpenAICompatibleConfig } from "../../src/model/openai-compatible.js";
import { createModelClient } from "../../src/model/provider.js";
import { readFileTool } from "../../src/tools/builtin.js";

describe("openai-compatible provider", () => {
  it("resolves credentials and baseUrl environment placeholders", () => {
    const resolved = resolveOpenAICompatibleConfig(
      "primary",
      { type: "openai-compatible", model: "gpt-test", baseUrl: "${MODEL_BASE}/", apiKeyEnv: "MODEL_KEY" },
      { MODEL_BASE: "https://gateway.example.test/v1", MODEL_KEY: "secret" }
    );
    expect(resolved.baseUrl).toBe("https://gateway.example.test/v1");
    expect(() => resolveOpenAICompatibleConfig("primary", { type: "openai-compatible", model: "gpt-test", baseUrl: "${MISSING_BASE}", apiKeyEnv: "MODEL_KEY" }, { MODEL_KEY: "secret" })).toThrow("MISSING_BASE");
  });

  it("fails fast on missing real-provider credentials", () => {
    const config = mergeConfig(DEFAULT_CONFIG, {
      models: {
        default: "primary",
        providers: {
          primary: { type: "openai-compatible", model: "gpt-test", apiKeyEnv: "MISSING_MODEL_KEY" }
        }
      }
    });
    expect(() => createModelClient({ config, env: {} })).toThrow("MISSING_MODEL_KEY");
    const fakeProvider = DEFAULT_CONFIG.models.providers.fake;
    if (fakeProvider === undefined) throw new Error("expected fake provider");
    expect(() => resolveOpenAICompatibleConfig("fake", fakeProvider, {})).toThrow("not openai-compatible");
    expect(() => resolveOpenAICompatibleConfig("primary", { type: "openai-compatible", model: "gpt-test" }, {})).toThrow("requires apiKeyEnv");
    expect(() => resolveOpenAICompatibleConfig("primary", { type: "openai-compatible", model: "gpt-test", apiKeyEnv: "MODEL_KEY" }, { MODEL_KEY: " " })).toThrow("MODEL_KEY");
    expect(() => resolveOpenAICompatibleConfig("primary", { type: "openai-compatible", model: "gpt-test", baseUrl: "${EMPTY}/", apiKeyEnv: "MODEL_KEY" }, { MODEL_KEY: "secret", EMPTY: "" })).toThrow("EMPTY");
    expect(() => resolveOpenAICompatibleConfig("primary", { type: "openai-compatible", model: "gpt-test", baseUrl: "///", apiKeyEnv: "MODEL_KEY" }, { MODEL_KEY: "secret" })).toThrow("empty baseUrl");
  });

  it("posts chat completions requests and parses message responses", async () => {
    let capturedUrl = "";
    let capturedBody = "";
    const fetchImpl: typeof fetch = async (input, init) => {
      capturedUrl = String(input);
      capturedBody = String(init?.body ?? "");
      return new Response(JSON.stringify({ choices: [{ message: { content: "hello" } }] }), { status: 200 });
    };
    const client = new OpenAICompatibleModelClient({ providerId: "primary", config: { type: "openai-compatible", model: "gpt-test", baseUrl: "https://gateway.example.test/v1", apiKeyEnv: "MODEL_KEY", temperature: 0.2, maxOutputTokens: 128 }, env: { MODEL_KEY: "secret" }, fetch: fetchImpl });
    const turn = await client.generate({ messages: [{ role: "user", content: "hi" }], tools: [readFileTool()], toolResults: [] });
    expect(turn).toEqual({ type: "message", content: "hello" });
    expect(capturedUrl).toBe("https://gateway.example.test/v1/chat/completions");
    expect(capturedBody).toContain('"model":"gpt-test"');
    expect(capturedBody).toContain('"tools"');
    expect(capturedBody).toContain('"max_tokens":128');

    const toolWithUnsupportedSchemaEntry = readFileTool();
    Object.assign(toolWithUnsupportedSchemaEntry.inputSchema, { unsupported: undefined });
    await client.generate({ messages: [], tools: [toolWithUnsupportedSchemaEntry], toolResults: [] });
    expect(capturedBody).toContain('"unsupported":null');
  });

  it("maps tool-role messages without tool_call_id as user-visible tool results", async () => {
    let capturedBody = "";
    const client = new OpenAICompatibleModelClient({
      providerId: "primary",
      config: { type: "openai-compatible", model: "gpt-test", apiKeyEnv: "MODEL_KEY" },
      env: { MODEL_KEY: "secret" },
      fetch: async (_input, init) => {
        capturedBody = String(init?.body ?? "");
        return new Response(JSON.stringify({ choices: [{ message: { content: "ok" } }] }), { status: 200 });
      }
    });
    await client.generate({ messages: [{ role: "tool", content: "result without id" }], tools: [], toolResults: [] });
    expect(capturedBody).toContain("Tool result: result without id");
    await client.generate({ messages: [{ role: "tool", toolCallId: "call_1", content: "result with id" }], tools: [], toolResults: [] });
    expect(capturedBody).toContain('"tool_call_id":"call_1"');
  });

  it("parses tool calls and preserves provider errors", async () => {
    const toolClient = new OpenAICompatibleModelClient({
      providerId: "primary",
      config: { type: "openai-compatible", model: "gpt-test", apiKeyEnv: "MODEL_KEY" },
      env: { MODEL_KEY: "secret" },
      fetch: async () =>
        new Response(
          JSON.stringify({
            choices: [
              {
                message: {
                  content: "using a tool",
                  tool_calls: [{ id: "call_1", function: { name: "read_file", arguments: '{"path":"README.md"}' } }]
                }
              }
            ]
          }),
          { status: 200 }
        )
    });
    await expect(toolClient.generate({ messages: [], tools: [], toolResults: [] })).resolves.toEqual({ type: "tool_calls", content: "using a tool", toolCalls: [{ id: "call_1", name: "read_file", input: { path: "README.md" } }] });

    const rateLimited = new OpenAICompatibleModelClient({ providerId: "primary", config: { type: "openai-compatible", model: "gpt-test", apiKeyEnv: "MODEL_KEY" }, env: { MODEL_KEY: "secret" }, fetch: async () => new Response("too many", { status: 429 }) });
    await expect(rateLimited.generate({ messages: [], tools: [], toolResults: [] })).rejects.toThrow("rate limited");

    const failed = new OpenAICompatibleModelClient({ providerId: "primary", config: { type: "openai-compatible", model: "gpt-test", apiKeyEnv: "MODEL_KEY" }, env: { MODEL_KEY: "secret" }, fetch: async () => new Response("server said no", { status: 500 }) });
    await expect(failed.generate({ messages: [], tools: [], toolResults: [] })).rejects.toThrow("server said no");
    const failedWithoutBody = new OpenAICompatibleModelClient({ providerId: "primary", config: { type: "openai-compatible", model: "gpt-test", apiKeyEnv: "MODEL_KEY" }, env: { MODEL_KEY: "secret" }, fetch: async () => new Response("", { status: 500 }) });
    await expect(failedWithoutBody.generate({ messages: [], tools: [], toolResults: [] })).rejects.toThrow("HTTP 500");
  });

  it("uses process env and global fetch defaults at construction time", () => {
    const previous = process.env.MODEL_KEY;
    process.env.MODEL_KEY = "secret";
    try {
      expect(() => new OpenAICompatibleModelClient({ providerId: "primary", config: { type: "openai-compatible", model: "gpt-test", apiKeyEnv: "MODEL_KEY" } })).not.toThrow();
    } finally {
      if (previous === undefined) delete process.env.MODEL_KEY;
      else process.env.MODEL_KEY = previous;
    }
  });

  it("rejects malformed provider payloads and fills safe tool-call fallbacks", async () => {
    const clientFor = (payload: unknown) => new OpenAICompatibleModelClient({
      providerId: "primary",
      config: { type: "openai-compatible", model: "gpt-test", apiKeyEnv: "MODEL_KEY" },
      env: { MODEL_KEY: "secret" },
      fetch: async () => new Response(JSON.stringify(payload), { status: 200 })
    });

    await expect(clientFor({ choices: [] }).generate({ messages: [], tools: [], toolResults: [] })).rejects.toThrow("missing choices");
    await expect(clientFor({ choices: [{}] }).generate({ messages: [], tools: [], toolResults: [] })).rejects.toThrow("choice.message");
    await expect(clientFor({ choices: [{ message: { content: null } }] }).generate({ messages: [], tools: [], toolResults: [] })).resolves.toEqual({ type: "message", content: "" });
    await expect(clientFor({ choices: [{ message: { tool_calls: [null] } }] }).generate({ messages: [], tools: [], toolResults: [] })).rejects.toThrow("invalid tool call");
    await expect(clientFor({ choices: [{ message: { tool_calls: [{ function: null }] } }] }).generate({ messages: [], tools: [], toolResults: [] })).rejects.toThrow("without function");
    await expect(clientFor({ choices: [{ message: { tool_calls: [{ function: { name: "", arguments: "{}" } }] } }] }).generate({ messages: [], tools: [], toolResults: [] })).rejects.toThrow("function.name");
    await expect(clientFor({ choices: [{ message: { tool_calls: [{ function: { name: "read_file", arguments: "" } }] } }] }).generate({ messages: [], tools: [], toolResults: [] })).resolves.toMatchObject({ type: "tool_calls", toolCalls: [{ id: "read_file_0", input: {} }] });
    await expect(clientFor({ choices: [{ message: { tool_calls: [{ id: "", function: { name: "read_file" } }] } }] }).generate({ messages: [], tools: [], toolResults: [] })).resolves.toMatchObject({ type: "tool_calls", toolCalls: [{ id: "read_file_0", input: {} }] });
    await expect(clientFor({ choices: [{ message: { tool_calls: [{ function: { name: "read_file", arguments: "[]" } }] } }] }).generate({ messages: [], tools: [], toolResults: [] })).rejects.toThrow("Expected JSON object");
  });
});
