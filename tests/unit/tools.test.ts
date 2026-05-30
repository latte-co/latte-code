import { mkdir, mkdtemp, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { describe, expect, it } from "vitest";
import { createBuiltinTools, readFileTool } from "../../src/tools/builtin.js";
import { ToolRegistry } from "../../src/tools/registry.js";
import { SchemaValidationError, validateSchema } from "../../src/tools/schema.js";

function registry(): ToolRegistry {
  const toolRegistry = new ToolRegistry();
  for (const tool of createBuiltinTools()) toolRegistry.register(tool);
  return toolRegistry;
}

describe("ToolRegistry and builtin tools", () => {
  it("validates lightweight schemas", () => {
    expect(() => validateSchema({ type: "object", required: ["path"], additionalProperties: false, properties: { path: { type: "string" } } }, { missing: true })).toThrow(SchemaValidationError);
    expect(() => validateSchema({ type: "object" }, "not-object")).toThrow("expected object");
    expect(() => validateSchema({ type: "array" }, "not-array")).toThrow("expected array");
    expect(() => validateSchema({ type: "number" }, "not-number")).toThrow("expected number");
    expect(() => validateSchema({ type: "array", items: { type: "string" } }, ["a", "b"])).not.toThrow();
    expect(() => validateSchema({ type: "object" }, { extra: "allowed" })).not.toThrow();
    expect(() => validateSchema({ type: "object", properties: { ok: { type: "boolean" } } }, { ok: true, extra: "allowed" })).not.toThrow();
    expect(() => validateSchema({ type: "string", enum: ["a"] }, "b")).toThrow("one of");
  });

  it("executes read, list, search, write and shell tools", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-tools-"));
    await mkdir(join(dir, "sub"));
    await mkdir(join(dir, "node_modules"));
    await writeFile(join(dir, "input.txt"), "alpha\nbeta\n", "utf8");
    await writeFile(join(dir, "node_modules", "skip.txt"), "alpha", "utf8");
    await writeFile(join(dir, "big.txt"), "x".repeat(1024 * 1024 + 1), "utf8");
    const toolRegistry = registry();
    const context = { cwd: dir, sessionId: "s1", maxOutputBytes: 1024 };
    const read = await toolRegistry.execute({ id: "read", name: "read_file", input: { path: "input.txt" } }, context);
    expect(read.ok).toBe(true);
    expect(read.output?.content).toContain("alpha");
    const listed = await toolRegistry.execute({ id: "list", name: "list_directory", input: { path: "." } }, context);
    expect(listed.output?.entries).toContain("input.txt");
    expect(listed.output?.entries).toContain("sub/");
    const searched = await toolRegistry.execute({ id: "search", name: "search", input: { path: ".", query: "beta" } }, context);
    expect(searched.summary).toContain("1 matches");
    const regexSearch = await toolRegistry.execute({ id: "search2", name: "search", input: { path: ".", query: "alp.*", regex: true, maxResults: 1 } }, context);
    expect(regexSearch.truncated).toBe(true);
    const zeroSearch = await toolRegistry.execute({ id: "search3", name: "search", input: { path: ".", query: "alpha", maxResults: 0 } }, context);
    expect(zeroSearch.truncated).toBe(true);
    await toolRegistry.execute({ id: "write", name: "write_file", input: { path: "nested/out.txt", content: "ok", createDirs: true } }, context);
    await expect(readFile(join(dir, "nested/out.txt"), "utf8")).resolves.toBe("ok");
    await toolRegistry.execute({ id: "write2", name: "write_file", input: { path: "out2.txt", content: "ok" } }, context);
    const shell = await toolRegistry.execute({ id: "shell", name: "shell_exec", input: { command: "printf hello" } }, context);
    expect(shell.ok).toBe(true);
    expect(shell.output?.stdout).toBe("hello");
    const shellWithCwd = await toolRegistry.execute({ id: "shell2", name: "shell_exec", input: { command: "pwd", cwd: ".", timeoutMs: 1000 } }, context);
    expect(shellWithCwd.output?.stdout).toContain(dir);
    const tiny = await toolRegistry.execute({ id: "read2", name: "read_file", input: { path: "input.txt", maxBytes: 3 } }, context);
    expect(tiny.truncated).toBe(true);
  });

  it("normalizes shell failures and duplicate registration", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-tools-fail-"));
    const toolRegistry = registry();
    const firstTool = createBuiltinTools()[0];
    if (firstTool === undefined) throw new Error("expected at least one builtin tool");
    expect(() => toolRegistry.register(firstTool)).toThrow("already registered");
    const failed = await toolRegistry.execute({ id: "shell", name: "shell_exec", input: { command: "exit 7" } }, { cwd: dir, sessionId: "s1", maxOutputBytes: 1024 });
    expect(failed.ok).toBe(false);
    expect(failed.error).toBeDefined();
    expect(() => toolRegistry.get("missing")).toThrow("Unknown tool");
    await expect(readFileTool().execute({}, { cwd: dir, sessionId: "s", maxOutputBytes: 10 })).rejects.toThrow("path must be a string");
  });
});
