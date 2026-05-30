import { exec } from "node:child_process";
import { readdir, readFile, stat, writeFile, mkdir } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { promisify } from "node:util";
import type { JsonObject } from "../shared/types.js";
import { isJsonValue, truncateText } from "../shared/types.js";
import type { PermissionDecision } from "../permissions/types.js";
import type { ToolDefinition, ToolResult } from "./types.js";

const execAsync = promisify(exec);

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
    if (entry === null || typeof entry === "string" || typeof entry === "number" || typeof entry === "boolean") output[key] = entry;
    else if (Array.isArray(entry)) output[key] = entry.filter(isJsonValue);
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
  return [readFileTool(), listDirectoryTool(), searchTool(), writeFileTool(), shellExecTool()];
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
      const truncated = truncateText(content, maxBytes);
      return result("", "read_file", true, `Read ${path}`, [fullPath], jsonOutput({ content: truncated.text, path: fullPath, truncated: truncated.truncated }), truncated.truncated);
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

export function writeFileTool(): ToolDefinition {
  return {
    name: "write_file",
    description: "Write UTF-8 content to a local file, creating parent directories when requested.",
    inputSchema: { type: "object", required: ["path", "content"], additionalProperties: false, properties: { path: { type: "string" }, content: { type: "string" }, createDirs: { type: "boolean" } } },
    outputSchema: { type: "object", required: ["path", "bytesWritten"], properties: { path: { type: "string" }, bytesWritten: { type: "number" } } },
    riskLevel: "medium",
    mutating: true,
    permission: { reason: "Write local file", paths: ["path"] },
    async execute(input, context) {
      const path = stringField(input, "path");
      const content = stringField(input, "content");
      const fullPath = absolutePath(context.cwd, path);
      if (input.createDirs === true) await mkdir(dirname(fullPath), { recursive: true });
      await writeFile(fullPath, content, "utf8");
      return result("", "write_file", true, `Wrote ${Buffer.byteLength(content, "utf8")} bytes to ${path}`, [fullPath], jsonOutput({ path: fullPath, bytesWritten: Buffer.byteLength(content, "utf8") }), false);
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
        /* c8 ignore next */
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
