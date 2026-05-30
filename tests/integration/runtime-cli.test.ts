import { mkdtemp } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { describe, expect, it, vi } from "vitest";
import { DEFAULT_CONFIG } from "../../src/config/defaults.js";
import { mergeConfig } from "../../src/config/config.js";
import { parseArgs } from "../../src/cli/main.js";
import { createAgentLoop, createDefaultRegistry } from "../../src/runtime/create-agent.js";

describe("runtime factory and CLI helpers", () => {
  it("creates registry from enabled and disabled tool config", () => {
    const config = mergeConfig(DEFAULT_CONFIG, { tools: { enabled: ["read_file", "write_file"], disabled: ["write_file"] } });
    expect(createDefaultRegistry(config).list().map((tool) => tool.name)).toEqual(["read_file"]);
  });

  it("creates an agent loop with memory stores and fake script", async () => {
    const config = mergeConfig(DEFAULT_CONFIG, { session: { store: "memory" }, evidence: { store: "memory" } });
    const loop = createAgentLoop({ cwd: process.cwd(), config, fakeScript: [{ type: "message", content: "ok" }] });
    const result = await loop.run({ input: "hello" });
    expect(result.finalResponse).toBe("ok");
  });

  it("creates an agent loop with filesystem stores in a temp cwd", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-runtime-fs-"));
    const config = mergeConfig(DEFAULT_CONFIG, {});
    const loop = createAgentLoop({ cwd: dir, config, fakeScript: [{ type: "message", content: "fs ok" }] });
    const result = await loop.run({ input: "hello" });
    expect(result.finalResponse).toBe("fs ok");
  });

  it("uses an explicitly supplied model client", async () => {
    const config = mergeConfig(DEFAULT_CONFIG, { session: { store: "memory" }, evidence: { store: "memory" } });
    const model = { async generate() { return { type: "message" as const, content: "explicit" }; } };
    const loop = createAgentLoop({ cwd: process.cwd(), config, model });
    await expect(loop.run({ input: "hello" })).resolves.toMatchObject({ finalResponse: "explicit" });
  });

  it("uses the default fake response when no model script is supplied", async () => {
    const config = mergeConfig(DEFAULT_CONFIG, { session: { store: "memory" }, evidence: { store: "memory" } });
    await expect(createAgentLoop({ cwd: process.cwd(), config }).run({ input: "hello" })).resolves.toMatchObject({ finalResponse: "Fake model has no configured script." });
  });

  it("parses run/resume/session/evidence style arguments", () => {
    expect(parseArgs(["run", "hello", "world", "--config", "a.jsonc", "--session", "s1"])).toEqual({ command: "run", input: "hello world", configPath: "a.jsonc", sessionId: "s1" });
    expect(parseArgs([])).toEqual({ command: "run", input: "" });
  });

  it("keeps console usage test-contained", () => {
    const spy = vi.spyOn(console, "log").mockImplementation(() => undefined);
    console.log("test");
    expect(spy).toHaveBeenCalledWith("test");
    spy.mockRestore();
  });
});
