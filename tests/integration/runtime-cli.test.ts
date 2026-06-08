import { mkdtemp, readFile } from "node:fs/promises";
import { join, relative, sep } from "node:path";
import { tmpdir } from "node:os";
import { describe, expect, it, vi } from "vitest";
import { DEFAULT_CONFIG } from "../../src/config/defaults.js";
import { mergeConfig } from "../../src/config/config.js";
import { parseArgs, runCli } from "../../src/cli/main.js";
import { createQuestionPendingInput } from "../../src/core/contracts.js";
import { buildBlockedHandoff, createTaskRunState, setRunPendingInput } from "../../src/core/run-state.js";
import { createAgentLoop, createDefaultRegistry } from "../../src/runtime/create-agent.js";
import { FileSessionStore } from "../../src/session/session.js";
import type { ModelTurn } from "../../src/model/types.js";

interface PackageMetadata {
  bin?: {
    fluxcode?: string;
  };
  scripts?: Record<string, string>;
}

interface PackageLockMetadata {
  packages?: {
    ""?: PackageMetadata;
  };
}

interface TypeScriptConfig {
  compilerOptions?: {
    rootDir?: string;
    outDir?: string;
  };
  include?: string[];
}

function artifact(value: unknown): ModelTurn {
  return { type: "message", content: JSON.stringify(value) };
}

function happyScript(summary = "done"): ModelTurn[] {
  return [
    artifact({ objective: "hello", scope: ["workspace"], acceptance: ["complete"], nonGoals: [], constraints: [], blockers: [] }),
    artifact({ summary: "context", filesRead: [], relevantSnippets: [], commandSources: [], openQuestions: [] }),
    artifact({ summary: "plan", targetFiles: [], steps: ["handoff"], verificationCommands: [], risks: [] }),
    artifact({ changedFiles: [], diffRefs: [], rationale: "no changes", evidenceRefs: [] }),
    artifact([{ command: "not run", status: "skipped", summary: "not required", evidenceRefs: [] }]),
    artifact({ id: "handoff_test", status: "completed", summary, changedFiles: [], verification: [{ command: "not run", status: "skipped", summary: "not required", evidenceRefs: [] }], risks: [], blockers: [], requiredDecisions: [], traceRefs: [], evidenceRefs: [] })
  ];
}

describe("runtime factory and CLI helpers", () => {
  it("creates registry from enabled and disabled tool config", () => {
    const config = mergeConfig(DEFAULT_CONFIG, { tools: { enabled: ["read_file", "write_file"], disabled: ["write_file"] } });
    expect(createDefaultRegistry(config).list().map((tool) => tool.name)).toEqual(["read_file"]);
  });

  it("creates an agent loop with memory stores and fake script", async () => {
    const config = mergeConfig(DEFAULT_CONFIG, { session: { store: "memory" }, evidence: { store: "memory" } });
    const loop = createAgentLoop({ cwd: process.cwd(), config, fakeScript: happyScript("ok") });
    const result = await loop.run({ input: "hello" });
    expect(result.finalResponse).toBe("ok");
  });

  it("creates an agent loop with filesystem stores in a temp cwd", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-runtime-fs-"));
    const config = mergeConfig(DEFAULT_CONFIG, {});
    const loop = createAgentLoop({ cwd: dir, config, fakeScript: happyScript("fs ok") });
    const result = await loop.run({ input: "hello" });
    expect(result.finalResponse).toBe("fs ok");
  });

  it("uses an explicitly supplied model client", async () => {
    const config = mergeConfig(DEFAULT_CONFIG, { session: { store: "memory" }, evidence: { store: "memory" } });
    const script = happyScript("explicit");
    const model = { async generate() { return script.shift() ?? artifact({ id: "handoff_test", status: "completed", summary: "explicit", changedFiles: [], verification: [], risks: [], blockers: [], requiredDecisions: [], traceRefs: [], evidenceRefs: [] }); } };
    const loop = createAgentLoop({ cwd: process.cwd(), config, model });
    await expect(loop.run({ input: "hello" })).resolves.toMatchObject({ finalResponse: "explicit" });
  });

  it("rejects the default fake provider when no model script is supplied", () => {
    const config = mergeConfig(DEFAULT_CONFIG, { session: { store: "memory" }, evidence: { store: "memory" } });
    expect(() => createAgentLoop({ cwd: process.cwd(), config })).toThrow("no fakeScript was supplied");
  });

  it("fails fast on default CLI run without a real provider or fakeScript", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-cli-default-provider-"));
    const logged: string[] = [];
    const errored: string[] = [];
    const logSpy = vi.spyOn(console, "log").mockImplementation((value: string) => {
      logged.push(value);
    });
    const errorSpy = vi.spyOn(console, "error").mockImplementation((value: string) => {
      errored.push(value);
    });
    try {
      const runCode = await runCli(["run", "读取 package.json 并总结项目", "--output", "json"], { cwd: dir });
      expect(runCode).toBe(2);
      expect(logged).toEqual([]);
      expect(JSON.parse(errored.at(-1) ?? "{}")).toMatchObject({ error: expect.stringContaining("Configure a real model provider") });
      expect(errored.at(-1)).toContain("fakeScript");
    } finally {
      logSpy.mockRestore();
      errorSpy.mockRestore();
    }
  });

  it("fails fast instead of falling back to fake when real provider credentials are missing", () => {
    const config = mergeConfig(DEFAULT_CONFIG, {
      models: {
        default: "primary",
        providers: {
          primary: { type: "openai-compatible", model: "gpt-test", apiKeyEnv: "FLUXCODE_TEST_MISSING_KEY" }
        }
      }
    });
    expect(() => createAgentLoop({ cwd: process.cwd(), config, fakeScript: [{ type: "message", content: "must not run" }], env: {} })).toThrow("FLUXCODE_TEST_MISSING_KEY");
  });

  it("parses headless run/resume/show/list arguments", () => {
    expect(parseArgs(["run", "hello", "world", "--config", "a.jsonc", "--session", "s1", "--output", "text"])).toEqual({ command: "run", input: "hello world", configPath: "a.jsonc", sessionId: "s1", output: "text" });
    expect(parseArgs(["resume", "run_1", "--input", "{\"kind\":\"question\",\"questionId\":\"q1\",\"answerText\":\"ok\"}"])).toEqual({ command: "resume", input: "run_1", output: "json", resumeInput: "{\"kind\":\"question\",\"questionId\":\"q1\",\"answerText\":\"ok\"}" });
    expect(parseArgs([])).toEqual({ command: "run", input: "", output: "json" });
    expect(() => parseArgs(["list", "--output", "yaml"])).toThrow("--output");
  });

  it("keeps the package bin aligned with the emitted CLI entrypoint", async () => {
    const [packageMetadata, packageLockMetadata, tsconfig] = await Promise.all([
      readJsonFile<PackageMetadata>("package.json"),
      readJsonFile<PackageLockMetadata>("package-lock.json"),
      readJsonFile<TypeScriptConfig>("tsconfig.json")
    ]);
    const rootDir = tsconfig.compilerOptions?.rootDir ?? ".";
    const outDir = tsconfig.compilerOptions?.outDir ?? "dist";
    const emittedCliPath = join(outDir, relative(rootDir, "bin/fluxcode.ts")).replace(/\.ts$/, ".js").split(sep).join("/");

    expect(packageMetadata.bin?.fluxcode).toBe(emittedCliPath);
    expect(packageLockMetadata.packages?.[""]?.bin?.fluxcode).toBe(emittedCliPath);
    expect(tsconfig.include).toContain("bin/**/*.ts");
    expect(packageMetadata.scripts?.["smoke:provider"]).toBe("node scripts/provider-smoke.mjs");
  });

  it("shows and lists a seeded run through the CLI headless JSON contract", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-cli-headless-"));
    const seeded = await seedBlockedRun(dir);
    const logged: string[] = [];
    const errored: string[] = [];
    const logSpy = vi.spyOn(console, "log").mockImplementation((value: string) => {
      logged.push(value);
    });
    const errorSpy = vi.spyOn(console, "error").mockImplementation((value: string) => {
      errored.push(value);
    });
    try {
      const showCode = await runCli(["show", seeded.runId, "--output", "json"], { cwd: dir });
      expect(showCode).toBe(21);
      expect(JSON.parse(logged.at(-1) ?? "{}")).toMatchObject({ runId: seeded.runId, sessionId: seeded.sessionId, status: "blocked" });

      const listCode = await runCli(["list", "--output", "json"], { cwd: dir });
      expect(listCode).toBe(0);
      expect(JSON.parse(logged.at(-1) ?? "{}")).toMatchObject({ runs: [{ runId: seeded.runId, status: "blocked" }] });
      expect(errored).toEqual([]);
    } finally {
      logSpy.mockRestore();
      errorSpy.mockRestore();
    }
  });

  it("renders text output and validates resume input errors without legacy session/evidence commands", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-cli-text-"));
    const seeded = await seedBlockedRun(dir);
    const logged: string[] = [];
    const errored: string[] = [];
    const logSpy = vi.spyOn(console, "log").mockImplementation((value: string) => {
      logged.push(value);
    });
    const errorSpy = vi.spyOn(console, "error").mockImplementation((value: string) => {
      errored.push(value);
    });
    try {
      expect(await runCli(["show", seeded.runId, "--output", "text"], { cwd: dir })).toBe(21);
      expect(logged.at(-1)).toContain("Status blocked");
      expect(await runCli(["resume", "missing", "--input", "{}", "--output", "json"], { cwd: dir })).toBe(2);
      expect(errored.at(-1)).toContain("Invalid ResumeInput contract");
      expect(await runCli(["session", "--output", "json"], { cwd: dir })).toBe(2);
      expect(errored.join("\n")).toContain("Unsupported command 'session'");
    } finally {
      logSpy.mockRestore();
      errorSpy.mockRestore();
    }
  });

  it("keeps console usage test-contained", () => {
    const spy = vi.spyOn(console, "log").mockImplementation(() => undefined);
    console.log("test");
    expect(spy).toHaveBeenCalledWith("test");
    spy.mockRestore();
  });
});

async function readJsonFile<T>(path: string): Promise<T> {
  return JSON.parse(await readFile(join(process.cwd(), path), "utf8")) as T;
}

async function seedBlockedRun(dir: string): Promise<{ runId: string; sessionId: string }> {
  const store = new FileSessionStore(join(dir, DEFAULT_CONFIG.session.directory));
  const session = await store.create("session_seeded");
  const run = createTaskRunState(session.id, "seeded task", "run_seeded");
  const pendingInput = createQuestionPendingInput({ questionId: "question_seeded", phase: "intake", prompt: "Seeded blocked fixture", expectedAnswer: "json", schemaName: "TaskSpec" });
  setRunPendingInput(run, pendingInput);
  run.handoff = buildBlockedHandoff(run, pendingInput.prompt, pendingInput);
  session.status = run.status;
  session.pendingInput = pendingInput;
  session.runState = run;
  await store.save(session);
  return { runId: run.id, sessionId: session.id };
}
