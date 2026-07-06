import { mkdir, mkdtemp, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { describe, expect, it } from "vitest";
import { DEFAULT_CONFIG } from "../../src/config/defaults.js";
import { mergeConfig } from "../../src/config/config.js";
import { loadAgentsSnapshot } from "../../src/context/agents.js";
import { loadLocalSkills } from "../../src/skills/loader.js";
import { loadLocalCommandSpecs, routeCommandInput } from "../../src/commands/registry.js";
import { createDefaultRegistry } from "../../src/runtime/create-agent.js";
import { buildContextMessages, buildContextProjection } from "../../src/context/compactor.js";
import { createDefaultCodeAgentPrompt } from "../../src/prompts/registry.js";
import { createTaskRunState } from "../../src/core/run-state.js";

describe("direct-loop runtime adapters", () => {
  it("loads AGENTS.md through repo/cwd boundary with snapshot hash and summary", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-agents-"));
    await mkdir(join(dir, ".git"));
    await writeFile(join(dir, "AGENTS.md"), "# Rules\n\n- Keep tests passing.\n- Do not bypass gates.\n", "utf8");
    const snapshot = await loadAgentsSnapshot({ cwd: dir, config: DEFAULT_CONFIG.agents });
    expect(snapshot?.path).toBe(join(dir, "AGENTS.md"));
    expect(snapshot?.hash).toHaveLength(64);
    expect(snapshot?.summary).toContain("Do not bypass gates");

    const child = join(dir, "child");
    await mkdir(child);
    await expect(loadAgentsSnapshot({ cwd: child, config: { ...DEFAULT_CONFIG.agents, loadFrom: ["cwd"], agentsFile: "../AGENTS.md" } })).rejects.toThrow("agents_gate");
  });

  it("combines repo and cwd AGENTS snapshots and handles disabled or invalid AGENTS config", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-agents-combined-"));
    await mkdir(join(dir, ".git"));
    const child = join(dir, "child");
    await mkdir(child);
    await writeFile(join(dir, "AGENTS.md"), "# Root\n\n- Root rule.\n", "utf8");
    await writeFile(join(child, "AGENTS.md"), "# Child\n\n- Child rule.\n", "utf8");
    const combined = await loadAgentsSnapshot({ cwd: child, config: DEFAULT_CONFIG.agents });
    expect(combined?.source).toBe("cwd");
    expect(combined?.summary).toContain("Root rule");
    expect(combined?.summary).toContain("Child rule");
    await expect(loadAgentsSnapshot({ cwd: dir, config: { ...DEFAULT_CONFIG.agents, snapshot: false } })).resolves.toBeUndefined();
    await expect(loadAgentsSnapshot({ cwd: dir, config: { ...DEFAULT_CONFIG.agents, agentsFile: join(dir, "AGENTS.md") } })).rejects.toThrow("agents_gate");

    const empty = await mkdtemp(join(tmpdir(), "lattecode-agents-empty-"));
    await writeFile(join(empty, "AGENTS.md"), "\n\n", "utf8");
    await expect(loadAgentsSnapshot({ cwd: empty, config: { ...DEFAULT_CONFIG.agents, loadFrom: [] } })).resolves.toMatchObject({ summary: "Empty AGENTS.md", source: "repoRoot" });
    await expect(loadAgentsSnapshot({ cwd: empty, config: { ...DEFAULT_CONFIG.agents, agentsFile: "MISSING.md", loadFrom: ["cwd", "cwd"] } })).resolves.toBeUndefined();

    const unreadable = await mkdtemp(join(tmpdir(), "lattecode-agents-unreadable-"));
    await mkdir(join(unreadable, "AGENTS.md"));
    await expect(loadAgentsSnapshot({ cwd: unreadable, config: { ...DEFAULT_CONFIG.agents, loadFrom: ["cwd"] } })).rejects.toThrow("failed to read");
  });

  it("loads local skills as instruction-only context and rejects side effects", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-skills-"));
    await mkdir(join(dir, ".lattecode", "skills", "safe"), { recursive: true });
    await mkdir(join(dir, ".lattecode", "skills", "unsafe"), { recursive: true });
    await writeFile(join(dir, ".lattecode", "skills", "safe", "skill.json"), JSON.stringify({ name: "safe", instructions: "Prefer small changes." }), "utf8");
    await writeFile(join(dir, ".lattecode", "skills", "unsafe", "skill.json"), JSON.stringify({ name: "unsafe", instructions: "bad", sideEffects: true }), "utf8");

    await expect(loadLocalSkills({ cwd: dir, config: { ...DEFAULT_CONFIG.skills, enabled: ["safe"] } })).resolves.toMatchObject([{ name: "safe", instructions: "Prefer small changes." }]);
    await expect(loadLocalSkills({ cwd: dir, config: { ...DEFAULT_CONFIG.skills, enabled: ["unsafe"] } })).rejects.toThrow("skill_gate");
  });

  it("loads markdown skills and fails closed for missing or invalid skill paths", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-skills-markdown-"));
    await mkdir(join(dir, ".lattecode", "skills"), { recursive: true });
    await writeFile(join(dir, ".lattecode", "skills", "doc.md"), "Use documented procedure.", "utf8");
    await expect(loadLocalSkills({ cwd: dir, config: { ...DEFAULT_CONFIG.skills, enabled: [] } })).resolves.toEqual([]);
    await expect(loadLocalSkills({ cwd: dir, config: { ...DEFAULT_CONFIG.skills, enabled: ["doc"] } })).resolves.toMatchObject([{ name: "doc", instructions: "Use documented procedure." }]);
    await expect(loadLocalSkills({ cwd: dir, config: { ...DEFAULT_CONFIG.skills, enabled: ["missing"] } })).rejects.toThrow("was not found");
    await expect(loadLocalSkills({ cwd: dir, config: { ...DEFAULT_CONFIG.skills, enabled: ["doc"], localDirectories: [join(dir, ".lattecode", "skills")] } })).rejects.toThrow("skill_gate");

    await writeFile(join(dir, ".lattecode", "skills", "fallback.json"), JSON.stringify({ commands: [{ run: "x" }, null] }), "utf8");
    await expect(loadLocalSkills({ cwd: dir, config: { ...DEFAULT_CONFIG.skills, enabled: ["fallback"], allowSideEffects: true } })).resolves.toMatchObject([{ name: "fallback", instructions: "", commandSpecs: [expect.stringContaining("run"), "null"] }]);
    await writeFile(join(dir, ".lattecode", "skills", "bad.json"), "[]", "utf8");
    await expect(loadLocalSkills({ cwd: dir, config: { ...DEFAULT_CONFIG.skills, enabled: ["bad"] } })).rejects.toThrow("JSON object");
    await mkdir(join(dir, ".lattecode", "skills", "directory", "skill.json"), { recursive: true });
    await expect(loadLocalSkills({ cwd: dir, config: { ...DEFAULT_CONFIG.skills, enabled: ["directory"] } })).rejects.toThrow();
  });

  it("loads local commands as direct agent context without tool calls", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-commands-"));
    await mkdir(join(dir, ".lattecode", "commands"), { recursive: true });
    await writeFile(join(dir, ".lattecode", "commands", "fix.json"), JSON.stringify({
      name: "fix",
      description: "Fix {{args}}",
      context: { objective: "Fix {{args}}", scope: ["workspace"], acceptance: ["tests pass"], nonGoals: ["no install"], constraints: ["use gates"], blockers: [] }
    }), "utf8");
    await writeFile(join(dir, ".lattecode", "commands", "bad.json"), JSON.stringify({ name: "bad", shell: "rm -rf .", context: { objective: "bad", scope: [], acceptance: [], nonGoals: [], constraints: [], blockers: [] } }), "utf8");

    const specs = await loadLocalCommandSpecs(dir, { ...DEFAULT_CONFIG.commands, enabled: ["fix"] });
    const routed = routeCommandInput("/fix issue 1", specs);
    expect(routed?.context).toMatchObject({ objective: "Fix issue 1", acceptance: ["tests pass"] });
    await expect(loadLocalCommandSpecs(dir, { ...DEFAULT_CONFIG.commands, enabled: ["bad"] })).rejects.toThrow("command_gate");
  });

  it("handles disabled local commands, unmatched routes, and invalid command specs", async () => {
    const dir = await mkdtemp(join(tmpdir(), "lattecode-commands-invalid-"));
    await mkdir(join(dir, ".lattecode", "commands"), { recursive: true });
    await writeFile(join(dir, ".lattecode", "commands", "plain.json"), JSON.stringify({ context: { objective: "Plain", scope: [], acceptance: [], nonGoals: [], constraints: [], blockers: [] } }), "utf8");
    await writeFile(join(dir, ".lattecode", "commands", "invalid.json"), JSON.stringify({ name: "invalid", context: { objective: "missing arrays" } }), "utf8");
    await expect(loadLocalCommandSpecs(dir, { ...DEFAULT_CONFIG.commands, allowLocalCommands: false, enabled: ["plain"] })).resolves.toEqual([]);
    const [plain] = await loadLocalCommandSpecs(dir, { ...DEFAULT_CONFIG.commands, enabled: ["plain"] });
    if (plain === undefined) throw new Error("expected plain command");
    expect(plain.name).toBe("plain");
    expect(plain.description).toBe("Plain");
    expect(routeCommandInput("plain", [plain])).toBeUndefined();
    expect(routeCommandInput("/missing", [plain])).toBeUndefined();
    expect(routeCommandInput("/plain", [plain])?.context.objective).toBe("Plain");
    await expect(loadLocalCommandSpecs(dir, { ...DEFAULT_CONFIG.commands, enabled: ["invalid"] })).rejects.toThrow("valid agent context");
    await expect(loadLocalCommandSpecs(dir, { ...DEFAULT_CONFIG.commands, enabled: ["plain"], localDirectory: join(dir, ".lattecode", "commands") })).rejects.toThrow("command_gate");
    await writeFile(join(dir, ".lattecode", "commands", "array.json"), "[]", "utf8");
    await expect(loadLocalCommandSpecs(dir, { ...DEFAULT_CONFIG.commands, enabled: ["array"] })).rejects.toThrow("JSON object");
  });

  it("keeps MCP disabled by default and exposes enabled tools as permission-routed ToolRegistry entries", async () => {
    const disabled = createDefaultRegistry(DEFAULT_CONFIG);
    expect(disabled.list().some((tool) => tool.name.startsWith("mcp_"))).toBe(false);
    const config = mergeConfig(DEFAULT_CONFIG, {
      mcp: {
        enabled: true,
        servers: {
          local: { enabled: true, tools: { lookup: { description: "Lookup data", mutating: false, riskLevel: "low" } } }
        }
      }
    });
    const registry = createDefaultRegistry(config, { async callTool() { return { summary: "lookup ok", output: { ok: true } }; } });
    expect(registry.list().map((tool) => tool.name)).toContain("mcp_local_lookup");
    await expect(registry.execute({ id: "mcp", name: "mcp_local_lookup", input: {} }, { cwd: process.cwd(), sessionId: "s", maxOutputBytes: 1024 })).resolves.toMatchObject({ ok: true, summary: "lookup ok" });
  });

  it("renders direct code-agent prompt context through compaction without dropping required lanes", () => {
    const run = createTaskRunState("s", "task", "run_context");
    run.agentContext = { objective: "change", scope: ["src"], acceptance: ["must pass"], nonGoals: ["no release"], constraints: ["keep gates"], blockers: [] };
    run.changedFiles = ["src/a.ts"];
    run.verification = [{ command: "npm test", status: "failed", exitCode: 1, summary: "failed", evidenceRefs: [] }];
    const template = createDefaultCodeAgentPrompt();
    const baseMessages = template.render({ run, allowedTools: ["shell_exec"], contextProjection: buildContextProjection(run, []), toolResults: [] });
    const transcript = Array.from({ length: 20 }, (_, index) => ({ role: "tool" as const, content: `long output ${index} ${"x".repeat(200)}` }));
    const built = buildContextMessages({ run, transcript, config: { ...DEFAULT_CONFIG.context, maxPromptBytes: 2500, recentTurnCount: 2, maxToolResultBytes: 80 }, baseMessages, toolResults: [] });
    expect(built.compacted).toBe(true);
    expect(built.blockedReason).toBeUndefined();
    expect(built.messages.map((message) => message.content).join("\n")).toContain("must pass");
    expect(built.messages.map((message) => message.content).join("\n")).toContain("npm test");
  });

  it("blocks context building when preserved base lanes exceed the budget", () => {
    const run = createTaskRunState("s", "task", "run_context_blocked");
    const template = createDefaultCodeAgentPrompt();
    const baseMessages = template.render({ run, allowedTools: [], contextProjection: "x".repeat(500), toolResults: [] });
    const built = buildContextMessages({ run, transcript: [{ role: "user", content: "older" }], config: { ...DEFAULT_CONFIG.context, maxPromptBytes: 20, recentTurnCount: 1, maxToolResultBytes: 5 }, baseMessages, toolResults: [] });
    expect(built.blockedReason).toContain("context_budget_gate");
  });

  it("drops older transcript when compacted transcript still exceeds budget but base lanes fit", () => {
    const run = createTaskRunState("s", "task", "run_context_drop");
    const baseMessages = [{ role: "system" as const, content: "short" }];
    const transcript = Array.from({ length: 8 }, (_, index) => ({ role: "tool" as const, content: `tool ${index} ${"x".repeat(200)}` }));
    const built = buildContextMessages({ run, transcript, config: { ...DEFAULT_CONFIG.context, maxPromptBytes: 80, recentTurnCount: 8, maxToolResultBytes: 80 }, baseMessages, toolResults: [] });
    expect(built.blockedReason).toBeUndefined();
    expect(built.messages).toEqual(baseMessages);
    expect(run.contextSnapshot.compactedSummary).toContain("Dropped older transcript entries");
  });
});
