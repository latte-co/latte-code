import { mkdir, mkdtemp, readFile, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { tmpdir } from "node:os";
import { describe, expect, it } from "vitest";
import { DEFAULT_CONFIG } from "../../src/config/defaults.js";
import { findConfigPath, findConfigPaths, loadConfig, mergeConfig } from "../../src/config/config.js";
import { parseJsonc, stripJsonComments, removeTrailingCommas } from "../../src/config/jsonc.js";
import { LATTECODE_CONFIG_SCHEMA } from "../../src/config/config-schema.js";

describe("JSONC config", () => {
  it("parses comments and trailing commas without touching strings", () => {
    const raw = `{
      // comment
      "url": "https://example.com/a//b",
      "arr": [1, 2,],
    }`;
    expect(stripJsonComments(raw)).toContain("https://example.com/a//b");
    expect(removeTrailingCommas(stripJsonComments(raw))).not.toContain("2,");
    expect(parseJsonc(raw)).toEqual({ url: "https://example.com/a//b", arr: [1, 2] });
    expect(parseJsonc(`{ /* block\ncomment */ "text": "quote \\\" // no comment", }`)).toEqual({ text: "quote \" // no comment" });
  });

  it("deep merges objects and replaces arrays", () => {
    expect(mergeConfig(DEFAULT_CONFIG, undefined).schemaVersion).toBe(1);
    const config = mergeConfig(DEFAULT_CONFIG, {
      session: { store: "memory" },
      runtime: { maxPhaseSteps: 3 },
      context: { preserve: ["task"] },
      tools: { enabled: ["read_file"], shell: { defaultTimeoutMs: 10, allowCommands: ["npm test"] } }
    });
    expect(config.session.store).toBe("memory");
    expect(config.runtime.maxPhaseSteps).toBe(3);
    expect(config.context.preserve).toEqual(["task"]);
    expect(config.tools.enabled).toEqual(["read_file"]);
    expect(config.tools.shell.defaultTimeoutMs).toBe(10);
    expect(config.tools.shell.allowCommands).toEqual(["npm test"]);
    expect(config.tools.shell.requireApprovalFor).toContain("install");
  });

  it("loads explicit JSONC file and validates provider references", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-config-"));
    const path = join(dir, "custom.jsonc");
    await writeConfig(join(dir, ".lattecode", "lattecode.jsonc"), `{ "schemaVersion": 1, "session": { "store": "filesystem" } }`);
    await writeFile(path, `{ "schemaVersion": 1, "models": { "default": "fake" }, "session": { "store": "memory" } }`, "utf8");
    const loaded = await loadConfig({ cwd: dir, configPath: path });
    expect(loaded.path).toBe(path);
    expect(loaded.paths).toEqual([path]);
    expect(loaded.config.session.store).toBe("memory");
    expect(() => mergeConfig(DEFAULT_CONFIG, { models: { default: "missing" } })).toThrow("not defined");
    expect(() => mergeConfig(DEFAULT_CONFIG, { schemaVersion: 2 })).toThrow("Unsupported");
    expect(() => mergeConfig(DEFAULT_CONFIG, { permissions: { defaultMode: "sometimes" } })).toThrow("Invalid permission");
    expect(() => mergeConfig(DEFAULT_CONFIG, { models: { providers: { fake: { apiKeyEnv: "" } } } })).toThrow("empty apiKeyEnv");
    expect(() => mergeConfig(DEFAULT_CONFIG, { models: { providers: { primary: { type: "openai-compatible", model: "gpt-test" } } } })).toThrow("requires apiKeyEnv");
    expect(() => mergeConfig(DEFAULT_CONFIG, { runtime: { maxPhaseSteps: 0 } })).toThrow("runtime.maxPhaseSteps");
    expect(() => mergeConfig(DEFAULT_CONFIG, { tools: { shell: { allowCommands: [1] } } })).toThrow("tools.shell.allowCommands");
    expect(() => mergeConfig(DEFAULT_CONFIG, { agents: { loadFrom: ["outside"] } })).toThrow("Invalid agents.loadFrom");
    expect(() => mergeConfig(DEFAULT_CONFIG, { coverage: { lines: 101 } })).toThrow("Coverage");
    expect(() => mergeConfig(DEFAULT_CONFIG, null)).toThrow("Config root");
    expect(() => mergeConfig(DEFAULT_CONFIG, { models: { providers: { fake: { type: "unsupported" } } } })).toThrow("unsupported provider type");
    expect(() => mergeConfig(DEFAULT_CONFIG, { models: { providers: { fake: { model: " " } } } })).toThrow("model");
    expect(() => mergeConfig(DEFAULT_CONFIG, { models: { providers: { fake: { baseUrl: " " } } } })).toThrow("baseUrl");
    expect(() => mergeConfig(DEFAULT_CONFIG, { agents: { hashAlgorithm: "sha1" } })).toThrow("sha256");
    expect(() => mergeConfig(DEFAULT_CONFIG, { runtime: { stopOnVerificationFailure: "yes" } })).toThrow("runtime.stopOnVerificationFailure");
    expect(() => mergeConfig(DEFAULT_CONFIG, { runtime: { maxRepairTurns: -1 } })).toThrow("runtime.maxRepairTurns");
    expect(() => mergeConfig(DEFAULT_CONFIG, { context: { maxToolResultBytes: 0 } })).toThrow("context.maxToolResultBytes");
    expect(() => mergeConfig(DEFAULT_CONFIG, { mcp: { servers: [] } })).toThrow("mcp.servers");
    expect(() => mergeConfig(DEFAULT_CONFIG, { mcp: { servers: { s: { enabled: "yes" } } } })).toThrow("mcp.servers.s.enabled");
    expect(() => mergeConfig(DEFAULT_CONFIG, { mcp: { servers: { s: { command: " " } } } })).toThrow("mcp.servers.s.command");
    expect(() => mergeConfig(DEFAULT_CONFIG, { mcp: { servers: { s: { args: [1] } } } })).toThrow("mcp.servers.s.args");
    expect(() => mergeConfig(DEFAULT_CONFIG, { mcp: { servers: { s: { env: [] } } } })).toThrow("mcp.servers.s.env");
    expect(() => mergeConfig(DEFAULT_CONFIG, { mcp: { servers: { s: { tools: [] } } } })).toThrow("mcp.servers.s.tools");
    expect(() => mergeConfig(DEFAULT_CONFIG, { mcp: { servers: { s: { tools: { t: { description: " " } } } } } })).toThrow("description");
    expect(() => mergeConfig(DEFAULT_CONFIG, { mcp: { servers: { s: { tools: { t: { mutating: "yes" } } } } } })).toThrow("mutating");
    expect(() => mergeConfig(DEFAULT_CONFIG, { mcp: { servers: { s: { tools: { t: { riskLevel: "critical" } } } } } })).toThrow("risk level");
  });

  it("rejects apiMode and distinguishes future provider taxonomy from typos", () => {
    expect(() => mergeConfig(DEFAULT_CONFIG, { models: { providers: { fake: { apiMode: "openai-compatible-chat" } } } })).toThrow(/apiMode.*type/);
    expect(() => mergeConfig(DEFAULT_CONFIG, { models: { default: "primary", providers: { primary: { apiMode: "openai-compatible-chat", model: "gpt-test", apiKeyEnv: "MODEL_KEY" } } } })).toThrow(/apiMode.*type/);
    expect(() => mergeConfig(DEFAULT_CONFIG, { models: { default: "primary", providers: { primary: { type: "anthropic", model: "claude-test" } } } })).toThrow("recognized but not implemented in this runtime");
    expect(() => mergeConfig(DEFAULT_CONFIG, { models: { providers: { fake: { type: "opneai-compatible" } } } })).toThrow("unsupported provider type");
  });

  it("keeps the example config on user-facing provider type", async () => {
    const raw = await readFile(join(process.cwd(), "lattecode.config.example.jsonc"), "utf8");
    expect(raw).not.toContain("apiMode");
    expect(raw).toContain('"type": "fake"');
    expect(raw).toContain('"type": "openai-compatible"');
    expect(() => mergeConfig(DEFAULT_CONFIG, parseJsonc(raw))).not.toThrow();
  });

  it("covers JSONC scanner edge branches", () => {
    expect(stripJsonComments("plain")).toBe("plain");
    expect(stripJsonComments("/* block without close\nnext */ {\"x\":1}")).toContain("\n");
    expect(stripJsonComments("{\"slash\":\"\\\\\",\"quote\":\"\\\"\"}")).toContain("slash");
    expect(removeTrailingCommas("[1,2]")).toBe("[1,2]");
    expect(removeTrailingCommas("[\"comma, inside\",]")).toBe("[\"comma, inside\"]");
  });

  it("loads global JSONC config", async () => {
    const { cwd, home } = await configFixture();
    const path = join(home, ".lattecode", "lattecode.jsonc");
    await writeConfig(path, `{ // global config
      "schemaVersion": 1,
      "session": { "store": "memory" },
    }`);
    const loaded = await loadConfig({ cwd, homeDir: home });
    expect(loaded.paths).toEqual([path]);
    expect(loaded.path).toBe(path);
    expect(loaded.config.session.store).toBe("memory");
  });

  it("loads global JSON config", async () => {
    const { cwd, home } = await configFixture();
    const path = join(home, ".lattecode", "lattecode.json");
    await writeConfig(path, `{ "schemaVersion": 1, "runtime": { "maxPhaseSteps": 3 } }`);
    const loaded = await loadConfig({ cwd, homeDir: home });
    expect(loaded.paths).toEqual([path]);
    expect(loaded.config.runtime.maxPhaseSteps).toBe(3);
  });

  it("loads project JSONC config", async () => {
    const { cwd, home } = await configFixture();
    const path = join(cwd, ".lattecode", "lattecode.jsonc");
    await writeConfig(path, `{ "schemaVersion": 1, "prompts": { "language": "zh-CN" } }`);
    const loaded = await loadConfig({ cwd, homeDir: home });
    expect(loaded.paths).toEqual([path]);
    expect(loaded.config.prompts.language).toBe("zh-CN");
  });

  it("loads project JSON config", async () => {
    const { cwd, home } = await configFixture();
    const path = join(cwd, ".lattecode", "lattecode.json");
    await writeConfig(path, `{ "schemaVersion": 1, "commands": { "allowLocalCommands": false } }`);
    const loaded = await loadConfig({ cwd, homeDir: home });
    expect(loaded.paths).toEqual([path]);
    expect(loaded.config.commands.allowLocalCommands).toBe(false);
  });

  it("selects JSONC before JSON at the same level", async () => {
    const { cwd, home } = await configFixture();
    const jsoncPath = join(cwd, ".lattecode", "lattecode.jsonc");
    const jsonPath = join(cwd, ".lattecode", "lattecode.json");
    await writeConfig(jsonPath, `{ "schemaVersion": 1, "session": { "store": "memory" } }`);
    await writeConfig(jsoncPath, `{ "schemaVersion": 1, "runtime": { "maxPhaseSteps": 4 } }`);
    const loaded = await loadConfig({ cwd, homeDir: home });
    expect(loaded.paths).toEqual([jsoncPath]);
    expect(loaded.path).toBe(jsoncPath);
    expect(loaded.config.runtime.maxPhaseSteps).toBe(4);
    expect(loaded.config.session.store).toBe(DEFAULT_CONFIG.session.store);
  });

  it("reads global and project configs and deep merges them", async () => {
    const { cwd, home } = await configFixture();
    const globalPath = join(home, ".lattecode", "lattecode.jsonc");
    const projectPath = join(cwd, ".lattecode", "lattecode.json");
    await writeConfig(globalPath, `{ "schemaVersion": 1, "tools": { "shell": { "allowCommands": ["npm test"], "defaultTimeoutMs": 1000 } } }`);
    await writeConfig(projectPath, `{ "schemaVersion": 1, "tools": { "shell": { "defaultTimeoutMs": 2000 } } }`);
    const loaded = await loadConfig({ cwd, homeDir: home });
    expect(loaded.paths).toEqual([globalPath, projectPath]);
    expect(loaded.path).toBe(projectPath);
    expect(loaded.config.tools.shell.allowCommands).toEqual(["npm test"]);
    expect(loaded.config.tools.shell.defaultTimeoutMs).toBe(2000);
    expect(await findConfigPaths({ cwd, homeDir: home })).toEqual([globalPath, projectPath]);
    expect(await findConfigPath({ cwd, homeDir: home })).toBe(projectPath);
  });

  it("lets project keys override global keys", async () => {
    const { cwd, home } = await configFixture();
    await writeConfig(join(home, ".lattecode", "lattecode.jsonc"), `{ "schemaVersion": 1, "prompts": { "language": "zh-CN" }, "runtime": { "maxPhaseSteps": 2 } }`);
    await writeConfig(join(cwd, ".lattecode", "lattecode.jsonc"), `{ "schemaVersion": 1, "prompts": { "language": "en-US" } }`);
    const loaded = await loadConfig({ cwd, homeDir: home });
    expect(loaded.config.prompts.language).toBe("en-US");
    expect(loaded.config.runtime.maxPhaseSteps).toBe(2);
  });

  it("falls back to defaults when discovered config files are missing", async () => {
    const { cwd, home } = await configFixture();
    const loaded = await loadConfig({ cwd, homeDir: home });
    expect(loaded.path).toBeUndefined();
    expect(loaded.paths).toEqual([]);
    expect(loaded.config).toEqual(DEFAULT_CONFIG);
    expect(LATTECODE_CONFIG_SCHEMA.properties).toBeDefined();
  });

  it("falls back to discovered config when an explicit path is absent", async () => {
    const { cwd, home } = await configFixture();
    const projectPath = join(cwd, ".lattecode", "lattecode.jsonc");
    await writeConfig(projectPath, `{ "schemaVersion": 1, "session": { "store": "memory" } }`);
    const loaded = await loadConfig({ cwd, homeDir: home, configPath: join(cwd, "missing.jsonc") });
    expect(loaded.paths).toEqual([projectPath]);
    expect(loaded.config.session.store).toBe("memory");
  });
});

async function configFixture(): Promise<{ cwd: string; home: string }> {
  const root = await mkdtemp(join(tmpdir(), "lattecode-config-fixture-"));
  return { cwd: join(root, "project"), home: join(root, "home") };
}

async function writeConfig(path: string, content: string): Promise<void> {
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, content, "utf8");
}
