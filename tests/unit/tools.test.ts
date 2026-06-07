import { execFile } from "node:child_process";
import { mkdir, mkdtemp, readFile, utimes, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { promisify } from "node:util";
import { describe, expect, it } from "vitest";
import { createBuiltinTools, readFileTool } from "../../src/tools/builtin.js";
import { ToolRegistry } from "../../src/tools/registry.js";
import { SchemaValidationError, validateSchema } from "../../src/tools/schema.js";

const execFileAsync = promisify(execFile);

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

  it("executes read, list, search, edit, manifest, diff, write and shell tools", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-tools-"));
    await mkdir(join(dir, "sub"));
    await mkdir(join(dir, "node_modules"));
    await execFileAsync("git", ["init"], { cwd: dir });
    await writeFile(join(dir, "input.txt"), "alpha\nbeta\n", "utf8");
    await writeFile(join(dir, "package.json"), JSON.stringify({ name: "fixture", scripts: { test: "vitest run", build: "tsc -p tsconfig.json" } }), "utf8");
    await writeFile(join(dir, "node_modules", "skip.txt"), "alpha", "utf8");
    await writeFile(join(dir, "big.txt"), "x".repeat(1024 * 1024 + 1), "utf8");
    const toolRegistry = registry();
    const context = { cwd: dir, sessionId: "s1", maxOutputBytes: 1024, fileSnapshots: {} };
    const read = await toolRegistry.execute({ id: "read", name: "read_file", input: { path: "input.txt" } }, context);
    expect(read.ok).toBe(true);
    expect(read.output?.content).toContain("alpha");
    expect(read.output?.sha256).toEqual(expect.any(String));
    const listed = await toolRegistry.execute({ id: "list", name: "list_directory", input: { path: "." } }, context);
    expect(listed.output?.entries).toContain("input.txt");
    expect(listed.output?.entries).toContain("sub/");
    const searched = await toolRegistry.execute({ id: "search", name: "search", input: { path: ".", query: "beta" } }, context);
    expect(searched.summary).toContain("1 matches");
    const regexSearch = await toolRegistry.execute({ id: "search2", name: "search", input: { path: ".", query: "alp.*", regex: true, maxResults: 1 } }, context);
    expect(regexSearch.truncated).toBe(true);
    const zeroSearch = await toolRegistry.execute({ id: "search3", name: "search", input: { path: ".", query: "alpha", maxResults: 0 } }, context);
    expect(zeroSearch.truncated).toBe(true);
    const edited = await toolRegistry.execute({ id: "edit", name: "edit_file", input: { path: "input.txt", mode: "replace", oldText: "beta", newText: "gamma" } }, context);
    expect(edited.summary).toContain("Edited");
    await expect(readFile(join(dir, "input.txt"), "utf8")).resolves.toContain("gamma");
    const manifest = await toolRegistry.execute({ id: "manifest", name: "read_project_manifest", input: {} }, context);
    expect(manifest.output?.declaredCommands).toEqual(expect.arrayContaining(["npm test", "npm run build"]));
    const diff = await toolRegistry.execute({ id: "diff", name: "git_diff", input: {} }, context);
    expect(diff.output?.changedFiles).toEqual(expect.arrayContaining(["input.txt", "package.json"]));
    await toolRegistry.execute({ id: "write", name: "write_file", input: { path: "nested/out.txt", content: "ok", createDirs: true, createIntent: true } }, context);
    await expect(readFile(join(dir, "nested/out.txt"), "utf8")).resolves.toBe("ok");
    await toolRegistry.execute({ id: "write2", name: "write_file", input: { path: "out2.txt", content: "ok", createIntent: true } }, context);
    await toolRegistry.execute({ id: "insert-read", name: "read_file", input: { path: "input.txt" } }, context);
    await toolRegistry.execute({ id: "insert-after", name: "edit_file", input: { path: "input.txt", mode: "insert_after", anchor: "gamma", text: " after" } }, context);
    await toolRegistry.execute({ id: "insert-before", name: "edit_file", input: { path: "input.txt", mode: "insert_before", anchor: "gamma", text: "before " } }, context);
    await writeFile(join(dir, "replace-all.txt"), "x x", "utf8");
    await toolRegistry.execute({ id: "replace-all-read", name: "read_file", input: { path: "replace-all.txt" } }, context);
    await toolRegistry.execute({ id: "replace-all", name: "edit_file", input: { path: "replace-all.txt", mode: "replace", oldText: "x", newText: "y", replaceAll: true } }, context);
    const shell = await toolRegistry.execute({ id: "shell", name: "shell_exec", input: { command: "printf hello" } }, context);
    expect(shell.ok).toBe(true);
    expect(shell.output?.stdout).toBe("hello");
    const shellWithCwd = await toolRegistry.execute({ id: "shell2", name: "shell_exec", input: { command: "pwd", cwd: ".", timeoutMs: 1000 } }, context);
    expect(shellWithCwd.output?.stdout).toContain(dir);
    const tiny = await toolRegistry.execute({ id: "read2", name: "read_file", input: { path: "input.txt", maxBytes: 3 } }, context);
    expect(tiny.truncated).toBe(true);
    await writeFile(join(dir, "invalid-package", "package.json"), "not json", "utf8").catch(async () => {
      await mkdir(join(dir, "invalid-package"));
      await writeFile(join(dir, "invalid-package", "package.json"), "not json", "utf8");
    });
    const invalidManifest = await toolRegistry.execute({ id: "manifest-invalid", name: "read_project_manifest", input: { path: "invalid-package" } }, context);
    expect(invalidManifest.output?.declaredCommands).toEqual([]);
    const nonGitDir = await mkdtemp(join(tmpdir(), "fluxcode-tools-nongit-"));
    const diffFailure = await toolRegistry.execute({ id: "diff-fail", name: "git_diff", input: {} }, { ...context, cwd: nonGitDir });
    expect(diffFailure.ok).toBe(false);
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

  it("enforces read-before-write, stale-write, and edit-match gates when snapshots are tracked", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-tools-gates-"));
    await writeFile(join(dir, "input.txt"), "same\nsame\n", "utf8");
    const toolRegistry = registry();
    const context = { cwd: dir, sessionId: "s1", maxOutputBytes: 1024, fileSnapshots: {} };
    await expect(toolRegistry.execute({ id: "write", name: "write_file", input: { path: "input.txt", content: "overwrite" } }, context)).rejects.toThrow("read_before_write_gate");
    await toolRegistry.execute({ id: "read", name: "read_file", input: { path: "input.txt" } }, context);
    await expect(toolRegistry.execute({ id: "edit", name: "edit_file", input: { path: "input.txt", mode: "replace", oldText: "same", newText: "once" } }, context)).rejects.toThrow("edit_match_gate");
    await expect(toolRegistry.execute({ id: "edit-empty", name: "edit_file", input: { path: "input.txt", mode: "replace", oldText: "", newText: "once" } }, context)).rejects.toThrow("edit_match_gate");
    await expect(toolRegistry.execute({ id: "edit-mode", name: "edit_file", input: { path: "input.txt", mode: "append", oldText: "same", newText: "once" } }, context)).rejects.toThrow("must be one of");
    await writeFile(join(dir, "input.txt"), "external\n", "utf8");
    await expect(toolRegistry.execute({ id: "edit2", name: "edit_file", input: { path: "input.txt", mode: "insert_after", anchor: "external", text: " change" } }, context)).rejects.toThrow("stale_write_gate");
  });

  it("blocks write and edit when only file mtime changed since read", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-tools-mtime-gates-"));
    const toolRegistry = registry();
    const editContext = { cwd: dir, sessionId: "s1", maxOutputBytes: 1024, fileSnapshots: {} };
    const writeContext = { cwd: dir, sessionId: "s2", maxOutputBytes: 1024, fileSnapshots: {} };
    const editPath = join(dir, "edit.txt");
    const writePath = join(dir, "write.txt");
    await writeFile(editPath, "same\n", "utf8");
    await writeFile(writePath, "same\n", "utf8");

    await toolRegistry.execute({ id: "read-edit", name: "read_file", input: { path: "edit.txt" } }, editContext);
    await utimes(editPath, new Date("2030-01-01T00:00:00.000Z"), new Date("2030-01-01T00:00:00.000Z"));
    await expect(toolRegistry.execute({ id: "edit", name: "edit_file", input: { path: "edit.txt", mode: "replace", oldText: "same", newText: "changed" } }, editContext)).rejects.toThrow("stale_write_gate");

    await toolRegistry.execute({ id: "read-write", name: "read_file", input: { path: "write.txt" } }, writeContext);
    await utimes(writePath, new Date("2030-01-02T00:00:00.000Z"), new Date("2030-01-02T00:00:00.000Z"));
    await expect(toolRegistry.execute({ id: "write", name: "write_file", input: { path: "write.txt", content: "changed" } }, writeContext)).rejects.toThrow("stale_write_gate");
  });

  it("covers builtin tool edge branches without changing tool semantics", async () => {
    const dir = await mkdtemp(join(tmpdir(), "fluxcode-tools-edges-"));
    await mkdir(join(dir, "sub"));
    await writeFile(join(dir, "sub", "input.txt"), "anchor\n", "utf8");
    await writeFile(join(dir, "large.txt"), "x".repeat(1024 * 1024 + 2), "utf8");
    const toolRegistry = registry();
    const untrackedContext = { cwd: dir, sessionId: "s1", maxOutputBytes: 20 };

    const noSnapshotWrite = await toolRegistry.execute({ id: "write", name: "write_file", input: { path: "new.txt", content: "created without snapshot" } }, untrackedContext);
    expect(noSnapshotWrite.ok).toBe(true);
    const noSnapshotEdit = await toolRegistry.execute({ id: "edit", name: "edit_file", input: { path: "sub/input.txt", mode: "insert_before", anchor: "anchor", text: "before " } }, untrackedContext);
    expect(noSnapshotEdit.ok).toBe(true);

    const trackedContext = { cwd: dir, sessionId: "s2", maxOutputBytes: 20, fileSnapshots: {} };
    await expect(toolRegistry.execute({ id: "write-missing", name: "write_file", input: { path: "missing.txt", content: "x" } }, trackedContext)).rejects.toThrow("createIntent=true");
    await expect(toolRegistry.execute({ id: "write-dir", name: "write_file", input: { path: "sub", content: "x" } }, trackedContext)).rejects.toThrow();
    await expect(toolRegistry.execute({ id: "edit-before-read", name: "edit_file", input: { path: "sub/input.txt", mode: "replace", oldText: "anchor", newText: "x" } }, trackedContext)).rejects.toThrow("read before edit_file");
    await toolRegistry.execute({ id: "read-input", name: "read_file", input: { path: "sub/input.txt" } }, trackedContext);
    await expect(toolRegistry.execute({ id: "edit-missing-old", name: "edit_file", input: { path: "sub/input.txt", mode: "replace", oldText: "missing", newText: "x" } }, trackedContext)).rejects.toThrow("oldText did not match");
    await expect(toolRegistry.execute({ id: "edit-missing-anchor", name: "edit_file", input: { path: "sub/input.txt", mode: "insert_after", anchor: "missing", text: "x" } }, untrackedContext)).rejects.toThrow("anchor did not match");
    await writeFile(join(dir, "duplicate-anchor.txt"), "a a", "utf8");
    await expect(toolRegistry.execute({ id: "edit-multi-anchor", name: "edit_file", input: { path: "duplicate-anchor.txt", mode: "insert_after", anchor: "a", text: "x" } }, untrackedContext)).rejects.toThrow("anchor matched multiple");

    const fileSearch = await toolRegistry.execute({ id: "search-file", name: "search", input: { path: "sub/input.txt", query: "anchor" } }, untrackedContext);
    expect(fileSearch.output?.matches).toHaveLength(1);
    const largeSearch = await toolRegistry.execute({ id: "search-large", name: "search", input: { path: "large.txt", query: "x" } }, untrackedContext);
    expect(largeSearch.output?.matches).toEqual([]);

    const packageArrayDir = await mkdtemp(join(tmpdir(), "fluxcode-tools-package-array-"));
    await writeFile(join(packageArrayDir, "package.json"), "[]", "utf8");
    await writeFile(join(packageArrayDir, "tsconfig.json"), "{}", "utf8");
    const manifest = await toolRegistry.execute({ id: "manifest", name: "read_project_manifest", input: { path: "." } }, { cwd: packageArrayDir, sessionId: "s3", maxOutputBytes: 100 });
    expect(manifest.output?.declaredCommands).toEqual([]);
    expect(manifest.output?.configFiles).toEqual(expect.arrayContaining([expect.objectContaining({ kind: "tsconfig.json" })]));
    const packageNoScriptsDir = await mkdtemp(join(tmpdir(), "fluxcode-tools-package-no-scripts-"));
    await writeFile(join(packageNoScriptsDir, "package.json"), JSON.stringify({ name: "fixture" }), "utf8");
    const manifestNoScripts = await toolRegistry.execute({ id: "manifest-no-scripts", name: "read_project_manifest", input: { path: "." } }, { cwd: packageNoScriptsDir, sessionId: "s4", maxOutputBytes: 100 });
    expect(manifestNoScripts.output?.declaredCommands).toEqual([]);

    await execFileAsync("git", ["init"], { cwd: dir });
    await writeFile(join(dir, "tracked.txt"), "before", "utf8");
    await execFileAsync("git", ["add", "tracked.txt"], { cwd: dir });
    await execFileAsync("git", ["commit", "-m", "initial"], { cwd: dir, env: { ...process.env, GIT_AUTHOR_NAME: "Test", GIT_AUTHOR_EMAIL: "test@example.com", GIT_COMMITTER_NAME: "Test", GIT_COMMITTER_EMAIL: "test@example.com" } });
    await execFileAsync("git", ["mv", "tracked.txt", "renamed.txt"], { cwd: dir });
    const renamedDiff = await toolRegistry.execute({ id: "diff", name: "git_diff", input: { path: ".", maxBytes: 5 } }, untrackedContext);
    expect(renamedDiff.output?.changedFiles).toContain("renamed.txt");

    const shell = await toolRegistry.execute({ id: "shell", name: "shell_exec", input: { command: "printf edge" } }, untrackedContext);
    expect(shell.output?.stdout).toBe("edge");
  });
});
