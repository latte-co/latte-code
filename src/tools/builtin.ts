import { exec, execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { readdir, readFile, stat, writeFile, mkdir } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { promisify } from "node:util";
import type { JsonObject } from "../shared/types.js";
import { isJsonValue, truncateText } from "../shared/types.js";
import type { PermissionDecision } from "../permissions/types.js";
import type { FileReadSnapshot, ToolDefinition, ToolExecutionContext, ToolResult } from "./types.js";

const execAsync = promisify(exec);
const execFileAsync = promisify(execFile);

function stringField(input: JsonObject, key: string): string {
  const value = input[key];
  if (typeof value !== "string") throw new Error(`${key} must be a string`);
  return value;
}

function numberField(input: JsonObject, key: string, fallback: number): number {
  const value = input[key];
  return typeof value === "number" ? value : fallback;
}

function absolutePath(cwd: string, path: string): string {
  return resolve(cwd, path);
}

function jsonOutput(value: Record<string, unknown>): JsonObject {
  const output: JsonObject = {};
  for (const [key, entry] of Object.entries(value)) {
    if (isJsonValue(entry)) output[key] = entry;
  }
  return output;
}

function result(callId: string, toolName: string, ok: boolean, summary: string, references: string[], output: JsonObject, truncated: boolean, error?: string): ToolResult {
  return { callId, toolName, ok, summary, references, output, truncated, ...(error === undefined ? {} : { error }) };
}

const baseEvidenceMapper = (input: JsonObject, toolResult: ToolResult, permission: PermissionDecision) => ({
  inputSummary: JSON.stringify(input),
  outputSummary: toolResult.summary,
  references: toolResult.references,
  truncated: toolResult.truncated,
  graphHints: permission.action === "allow" ? { gateId: "permission.allow" } : { gateId: `permission.${permission.action}` }
});

export function createBuiltinTools(): ToolDefinition[] {
  return [readFileTool(), listDirectoryTool(), searchTool(), editFileTool(), writeFileTool(), shellExecTool(), readProjectManifestTool(), gitDiffTool()];
}

export function readFileTool(): ToolDefinition {
  return {
    name: "read_file",
    description: "Read a UTF-8 text file from the local workspace.",
    inputSchema: { type: "object", required: ["path"], additionalProperties: false, properties: { path: { type: "string" }, maxBytes: { type: "number" } } },
    outputSchema: { type: "object", required: ["content", "path", "truncated"], properties: { content: { type: "string" }, path: { type: "string" }, truncated: { type: "boolean" } } },
    riskLevel: "low",
    mutating: false,
    permission: { reason: "Read file content", paths: ["path"] },
    async execute(input, context) {
      const path = stringField(input, "path");
      const fullPath = absolutePath(context.cwd, path);
      const maxBytes = numberField(input, "maxBytes", context.maxOutputBytes);
      const content = await readFile(fullPath, "utf8");
      const snapshot = await fileSnapshot(fullPath, content);
      recordSnapshot(context, snapshot);
      const truncated = truncateText(content, maxBytes);
      return result("", "read_file", true, `Read ${path}`, [fullPath], jsonOutput({ content: truncated.text, path: fullPath, truncated: truncated.truncated, sha256: snapshot.sha256, mtimeMs: snapshot.mtimeMs, size: snapshot.size }), truncated.truncated);
    },
    evidenceMapper: baseEvidenceMapper
  };
}

export function listDirectoryTool(): ToolDefinition {
  return {
    name: "list_directory",
    description: "List direct children of a local directory.",
    inputSchema: { type: "object", required: ["path"], additionalProperties: false, properties: { path: { type: "string" } } },
    outputSchema: { type: "object", required: ["entries", "path"], properties: { entries: { type: "array", items: { type: "string" } }, path: { type: "string" } } },
    riskLevel: "low",
    mutating: false,
    permission: { reason: "List directory", paths: ["path"] },
    async execute(input, context) {
      const path = stringField(input, "path");
      const fullPath = absolutePath(context.cwd, path);
      const entries = await readdir(fullPath, { withFileTypes: true });
      const names = entries.map((entry) => `${entry.name}${entry.isDirectory() ? "/" : ""}`).sort();
      return result("", "list_directory", true, `Listed ${names.length} entries in ${path}`, [fullPath], jsonOutput({ entries: names, path: fullPath }), false);
    },
    evidenceMapper: baseEvidenceMapper
  };
}

export function searchTool(): ToolDefinition {
  return {
    name: "search",
    description: "Search text files under a directory using a plain substring or regex.",
    inputSchema: { type: "object", required: ["path", "query"], additionalProperties: false, properties: { path: { type: "string" }, query: { type: "string" }, regex: { type: "boolean" }, maxResults: { type: "number" } } },
    outputSchema: { type: "object", required: ["matches", "truncated"], properties: { matches: { type: "array", items: { type: "string" } }, truncated: { type: "boolean" } } },
    riskLevel: "low",
    mutating: false,
    permission: { reason: "Search local files", paths: ["path"] },
    async execute(input, context) {
      const root = absolutePath(context.cwd, stringField(input, "path"));
      const query = stringField(input, "query");
      const regex = input.regex === true ? new RegExp(query) : undefined;
      const maxResults = numberField(input, "maxResults", 20);
      const matches = await collectMatches(root, query, regex, maxResults);
      return result("", "search", true, `Found ${matches.items.length} matches for ${query}`, [root], jsonOutput({ matches: matches.items, truncated: matches.truncated }), matches.truncated);
    },
    evidenceMapper: baseEvidenceMapper
  };
}

export function editFileTool(): ToolDefinition {
  return {
    name: "edit_file",
    description: "Apply a scoped UTF-8 local file edit using exact replace or unique anchor insertion.",
    inputSchema: {
      type: "object",
      required: ["path", "mode"],
      additionalProperties: false,
      properties: {
        path: { type: "string" },
        mode: { type: "string", enum: ["replace", "insert_after", "insert_before"] },
        oldText: { type: "string" },
        newText: { type: "string" },
        replaceAll: { type: "boolean" },
        anchor: { type: "string" },
        text: { type: "string" }
      }
    },
    outputSchema: { type: "object", required: ["path", "bytesChanged", "matchCount", "matchedRange"], properties: { path: { type: "string" }, bytesChanged: { type: "number" }, matchCount: { type: "number" }, matchedRange: { type: "string" } } },
    riskLevel: "medium",
    mutating: true,
    permission: { reason: "Edit local file", paths: ["path"] },
    async execute(input, context) {
      const path = stringField(input, "path");
      const mode = stringField(input, "mode");
      const fullPath = absolutePath(context.cwd, path);
      await assertFreshRead(context, fullPath, "edit_file");
      const before = await readFile(fullPath, "utf8");
      const edit = applyEdit(before, input, mode);
      await writeFile(fullPath, edit.content, "utf8");
      recordSnapshot(context, await fileSnapshot(fullPath, edit.content));
      const bytesChanged = Math.abs(Buffer.byteLength(edit.content, "utf8") - Buffer.byteLength(before, "utf8"));
      return result("", "edit_file", true, `Edited ${path}: ${edit.summary}`, [fullPath], jsonOutput({ path: fullPath, bytesChanged, matchCount: edit.matchCount, matchedRange: `${edit.start}:${edit.end}` }), false);
    },
    evidenceMapper: baseEvidenceMapper
  };
}

export function writeFileTool(): ToolDefinition {
  return {
    name: "write_file",
    description: "Write UTF-8 content to a local file, creating parent directories when requested.",
    inputSchema: { type: "object", required: ["path", "content"], additionalProperties: false, properties: { path: { type: "string" }, content: { type: "string" }, createDirs: { type: "boolean" }, createIntent: { type: "boolean" } } },
    outputSchema: { type: "object", required: ["path", "bytesWritten"], properties: { path: { type: "string" }, bytesWritten: { type: "number" } } },
    riskLevel: "medium",
    mutating: true,
    permission: { reason: "Write local file", paths: ["path"] },
    async execute(input, context) {
      const path = stringField(input, "path");
      const content = stringField(input, "content");
      const fullPath = absolutePath(context.cwd, path);
      await assertWriteAllowed(context, fullPath, input.createIntent === true);
      if (input.createDirs === true) await mkdir(dirname(fullPath), { recursive: true });
      await writeFile(fullPath, content, "utf8");
      recordSnapshot(context, await fileSnapshot(fullPath, content));
      return result("", "write_file", true, `Wrote ${Buffer.byteLength(content, "utf8")} bytes to ${path}`, [fullPath], jsonOutput({ path: fullPath, bytesWritten: Buffer.byteLength(content, "utf8") }), false);
    },
    evidenceMapper: baseEvidenceMapper
  };
}

export function readProjectManifestTool(): ToolDefinition {
  return {
    name: "read_project_manifest",
    description: "Read a summary of local package, TypeScript, test, and Lattecode manifest files.",
    inputSchema: { type: "object", additionalProperties: false, properties: { path: { type: "string" } } },
    outputSchema: { type: "object" },
    riskLevel: "low",
    mutating: false,
    permission: { reason: "Read project manifest", paths: ["path"] },
    async execute(input, context) {
      const rootPath = typeof input.path === "string" ? input.path : ".";
      const root = absolutePath(context.cwd, rootPath);
      const manifest = await readProjectManifest(root);
      return result("", "read_project_manifest", true, `Read project manifest for ${rootPath}`, manifest.references, manifest.summary, false);
    },
    evidenceMapper: baseEvidenceMapper
  };
}

export function gitDiffTool(): ToolDefinition {
  return {
    name: "git_diff",
    description: "Return a summary-only git working tree diff and changed-file list.",
    inputSchema: { type: "object", additionalProperties: false, properties: { path: { type: "string" }, maxBytes: { type: "number" } } },
    outputSchema: { type: "object", required: ["changedFiles", "status", "stat"], properties: { changedFiles: { type: "array", items: { type: "string" } }, status: { type: "string" }, stat: { type: "string" } } },
    riskLevel: "low",
    mutating: false,
    permission: { reason: "Summarize git diff", paths: ["path"] },
    async execute(input, context) {
      const rootPath = typeof input.path === "string" ? input.path : ".";
      const cwd = absolutePath(context.cwd, rootPath);
      const maxBytes = numberField(input, "maxBytes", context.maxOutputBytes);
      try {
        const status = await gitOutput(cwd, ["status", "--short"]);
        const unstagedStat = await gitOutput(cwd, ["diff", "--stat", "--no-ext-diff"]);
        const stagedStat = await gitOutput(cwd, ["diff", "--cached", "--stat", "--no-ext-diff"]);
        const statSummary = truncateText([unstagedStat, stagedStat].filter((entry) => entry.trim().length > 0).join("\n"), maxBytes);
        const changedFiles = parseGitStatusFiles(status);
        return result("", "git_diff", true, `Git diff summary: ${changedFiles.length} changed files`, [cwd], jsonOutput({ changedFiles, status, stat: statSummary.text }), statSummary.truncated);
      } catch (error) {
        /* v8 ignore next -- child_process rejects with Error instances in supported Node.js runtimes. */
        const message = error instanceof Error ? error.message : "Unknown git diff error";
        return result("", "git_diff", false, "Git diff summary failed", [cwd], jsonOutput({ changedFiles: [], status: "", stat: "" }), false, message);
      }
    },
    evidenceMapper: baseEvidenceMapper
  };
}

export function shellExecTool(): ToolDefinition {
  return {
    name: "shell_exec",
    description: "Execute a shell command after permission evaluation.",
    inputSchema: { type: "object", required: ["command"], additionalProperties: false, properties: { command: { type: "string" }, timeoutMs: { type: "number" }, cwd: { type: "string" } } },
    outputSchema: { type: "object", required: ["stdout", "stderr", "exitCode"], properties: { stdout: { type: "string" }, stderr: { type: "string" }, exitCode: { type: "number" } } },
    riskLevel: "medium",
    mutating: true,
    permission: { reason: "Execute shell command", commandCategories: ["shell"] },
    async execute(input, context) {
      const command = stringField(input, "command");
      const timeout = numberField(input, "timeoutMs", context.shellDefaultTimeoutMs ?? 120000);
      const commandCwd = typeof input.cwd === "string" ? resolve(context.cwd, input.cwd) : context.cwd;
      try {
        const { stdout, stderr } = await execAsync(command, { cwd: commandCwd, timeout, shell: "/bin/bash" });
        const combined = truncateText(`${stdout}${stderr}`, context.maxOutputBytes);
        return result("", "shell_exec", true, `Command succeeded: ${command}`, [command], jsonOutput({ stdout, stderr, exitCode: 0, combined: combined.text }), combined.truncated);
      } catch (error) {
        /* v8 ignore next -- exec rejects with Error instances in supported Node.js runtimes. */
        const message = error instanceof Error ? error.message : "Unknown shell error";
        return result("", "shell_exec", false, `Command failed: ${command}`, [command], jsonOutput({ stdout: "", stderr: message, exitCode: 1 }), false, message);
      }
    },
    evidenceMapper: baseEvidenceMapper
  };
}

async function collectMatches(root: string, query: string, regex: RegExp | undefined, maxResults: number): Promise<{ items: string[]; truncated: boolean }> {
  const items: string[] = [];
  async function visit(path: string): Promise<void> {
    if (items.length >= maxResults) return;
    const info = await stat(path);
    if (info.isDirectory()) {
      const entries = await readdir(path);
      for (const entry of entries) {
        if (["node_modules", ".git", "dist", "coverage"].includes(entry)) continue;
        await visit(join(path, entry));
        if (items.length >= maxResults) return;
      }
      return;
    }
    if (!info.isFile() || info.size > 1024 * 1024) return;
    const content = await readFile(path, "utf8").catch(() => "");
    const lines = content.split("\n");
    for (const [line, text] of lines.entries()) {
      const matched = regex === undefined ? text.includes(query) : regex.test(text);
      if (matched) items.push(`${path}:${line + 1}: ${text}`);
      if (items.length >= maxResults) return;
    }
  }
  await visit(root);
  return { items, truncated: items.length >= maxResults };
}

async function fileSnapshot(path: string, knownContent?: string): Promise<FileReadSnapshot> {
  const [info, content] = await Promise.all([stat(path), knownContent === undefined ? readFile(path, "utf8") : Promise.resolve(knownContent)]);
  return { path, sha256: sha256(content), mtimeMs: info.mtimeMs, size: info.size, readAt: new Date().toISOString() };
}

function sha256(content: string): string {
  return createHash("sha256").update(content).digest("hex");
}

function recordSnapshot(context: ToolExecutionContext, snapshot: FileReadSnapshot): void {
  if (context.fileSnapshots !== undefined) context.fileSnapshots[snapshot.path] = snapshot;
}

async function assertWriteAllowed(context: ToolExecutionContext, path: string, createIntent: boolean): Promise<void> {
  if (context.fileSnapshots === undefined) return;
  const existing = await fileSnapshot(path).catch((error: unknown) => {
    if (error instanceof Error && "code" in error && (error as { code?: unknown }).code === "ENOENT") return undefined;
    throw error;
  });
  const snapshot = context.fileSnapshots[path];
  if (existing === undefined) {
    if (!createIntent) throw new Error(`read_before_write_gate: ${path} does not exist; write_file requires createIntent=true for new files`);
    return;
  }
  if (snapshot === undefined) throw new Error(`read_before_write_gate: ${path} must be read before write_file overwrite`);
  assertSnapshotFresh(path, snapshot, existing);
}

async function assertFreshRead(context: ToolExecutionContext, path: string, toolName: string): Promise<void> {
  if (context.fileSnapshots === undefined) return;
  const snapshot = context.fileSnapshots[path];
  if (snapshot === undefined) throw new Error(`read_before_write_gate: ${path} must be read before ${toolName}`);
  assertSnapshotFresh(path, snapshot, await fileSnapshot(path));
}

function assertSnapshotFresh(path: string, expected: FileReadSnapshot, actual: FileReadSnapshot): void {
  if (actual.sha256 !== expected.sha256 || actual.mtimeMs !== expected.mtimeMs) throw new Error(`stale_write_gate: ${path} changed since it was read`);
}

function applyEdit(content: string, input: JsonObject, mode: string): { content: string; matchCount: number; start: number; end: number; summary: string } {
  if (mode === "replace") {
    const oldText = stringField(input, "oldText");
    const newText = stringField(input, "newText");
    const matches = rangesFor(content, oldText);
    if (matches.length === 0) throw new Error("edit_match_gate: oldText did not match file content");
    if (matches.length > 1 && input.replaceAll !== true) throw new Error("edit_match_gate: oldText matched multiple ranges; provide a more specific oldText or replaceAll=true");
    const first = matches[0]!;
    const next = input.replaceAll === true ? content.split(oldText).join(newText) : replaceRange(content, first.start, first.end, newText);
    return { content: next, matchCount: matches.length, start: first.start, end: first.end, summary: input.replaceAll === true ? "replaced all exact matches" : "replaced one exact match" };
  }
  if (mode === "insert_after" || mode === "insert_before") {
    const anchor = stringField(input, "anchor");
    const text = stringField(input, "text");
    const matches = rangesFor(content, anchor);
    if (matches.length === 0) throw new Error("edit_match_gate: anchor did not match file content");
    if (matches.length > 1) throw new Error("edit_match_gate: anchor matched multiple ranges; provide a more specific anchor");
    const first = matches[0]!;
    const insertAt = mode === "insert_after" ? first.end : first.start;
    return { content: replaceRange(content, insertAt, insertAt, text), matchCount: matches.length, start: first.start, end: first.end, summary: `${mode} unique anchor` };
  }
  /* v8 ignore next -- ToolRegistry schema validation rejects unsupported edit modes before execution. */
  throw new Error("mode must be one of replace, insert_after, insert_before");
}

function rangesFor(content: string, needle: string): { start: number; end: number }[] {
  if (needle.length === 0) throw new Error("edit_match_gate: match text must not be empty");
  const ranges: { start: number; end: number }[] = [];
  let index = content.indexOf(needle);
  while (index >= 0) {
    ranges.push({ start: index, end: index + needle.length });
    index = content.indexOf(needle, index + needle.length);
  }
  return ranges;
}

function replaceRange(content: string, start: number, end: number, replacement: string): string {
  return `${content.slice(0, start)}${replacement}${content.slice(end)}`;
}

async function readProjectManifest(root: string): Promise<{ summary: JsonObject; references: string[] }> {
  const configFiles: { path: string; kind: string }[] = [];
  const packageJson = await readJson(join(root, "package.json"));
  if (packageJson !== undefined) configFiles.push({ path: join(root, "package.json"), kind: "package.json" });
  for (const file of ["tsconfig.json", "vitest.config.ts", "vitest.config.js", "lattecode.config.jsonc", "lattecode.config.example.jsonc"]) {
    const path = join(root, file);
    if (await exists(path)) configFiles.push({ path, kind: file });
  }
  const scripts = isJsonObject(packageJson?.scripts) ? packageJson.scripts : {};
  const declaredCommands = Object.keys(scripts).flatMap((name) => (name === "test" ? ["npm test", "npm run test"] : [`npm run ${name}`]));
  const references = configFiles.map((entry) => entry.path);
  return { summary: jsonOutput({ root, package: packageJson === undefined ? undefined : { name: packageJson.name, type: packageJson.type, scripts }, declaredCommands, configFiles }), references };
}

async function readJson(path: string): Promise<JsonObject | undefined> {
  try {
    const value = JSON.parse(await readFile(path, "utf8")) as unknown;
    return isJsonObject(value) ? value : undefined;
  } catch {
    return undefined;
  }
}

async function exists(path: string): Promise<boolean> {
  return stat(path).then(() => true).catch(() => false);
}

function isJsonObject(value: unknown): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value) && isJsonValue(value);
}

async function gitOutput(cwd: string, args: string[]): Promise<string> {
  const { stdout } = await execFileAsync("git", args, { cwd, timeout: 30000 });
  return stdout;
}

function parseGitStatusFiles(status: string): string[] {
  return status.split("\n").map((line) => line.trimEnd()).filter((line) => line.length > 0).map((line) => {
    const path = line.slice(3);
    const renameIndex = path.indexOf(" -> ");
    return renameIndex >= 0 ? path.slice(renameIndex + 4) : path;
  });
}
