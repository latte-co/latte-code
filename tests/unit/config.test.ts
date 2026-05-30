import { mkdtemp, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { describe, expect, it } from "vitest";
import { DEFAULT_CONFIG } from "../../src/config/defaults.js";
import { findConfigPath, loadConfig, mergeConfig } from "../../src/config/config.js";
import { parseJsonc, stripJsonComments, removeTrailingCommas } from "../../src/config/jsonc.js";
import { FLUXCODE_CONFIG_SCHEMA } from "../../src/config/config-schema.js";

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
      tools: { enabled: ["read_file"], shell: { defaultTimeoutMs: 10 } }
    });
    expect(config.session.store).toBe("memory");
    expect(config.tools.enabled).toEqual(["read_file"]);
    expect(config.tools.shell.defaultTimeoutMs).toBe(10);
    expect(config.tools.shell.requireApprovalFor).toContain("install");
  });

  it("loads explicit JSONC file and validates provider references", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-config-"));
    const path = join(dir, "custom.jsonc");
    await writeFile(path, `{ "schemaVersion": 1, "models": { "default": "fake" }, "session": { "store": "memory" } }`, "utf8");
    const loaded = await loadConfig({ cwd: dir, configPath: path });
    expect(loaded.path).toBe(path);
    expect(loaded.config.session.store).toBe("memory");
    expect(() => mergeConfig(DEFAULT_CONFIG, { models: { default: "missing" } })).toThrow("not defined");
    expect(() => mergeConfig(DEFAULT_CONFIG, { schemaVersion: 2 })).toThrow("Unsupported");
    expect(() => mergeConfig(DEFAULT_CONFIG, { permissions: { defaultMode: "sometimes" } })).toThrow("Invalid permission");
    expect(() => mergeConfig(DEFAULT_CONFIG, { models: { providers: { fake: { apiKeyEnv: "" } } } })).toThrow("empty apiKeyEnv");
    expect(() => mergeConfig(DEFAULT_CONFIG, { coverage: { lines: 101 } })).toThrow("Coverage");
  });

  it("finds cwd config before home config and exposes JSON schema", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-find-"));
    const home = await mkdtemp(join(tmpdir(), "fluxcode-home-"));
    await writeFile(join(dir, "fluxcode.config.jsonc"), `{ "schemaVersion": 1 }`, "utf8");
    expect(await findConfigPath({ cwd: dir, homeDir: home })).toBe(join(dir, "fluxcode.config.jsonc"));
    expect((await loadConfig({ cwd: join(dir, "missing") })).path).toBeUndefined();
    expect(FLUXCODE_CONFIG_SCHEMA.properties).toBeDefined();
  });
});
