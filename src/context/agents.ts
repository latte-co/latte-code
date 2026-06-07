import { createHash } from "node:crypto";
import { access, readFile, stat } from "node:fs/promises";
import { dirname, isAbsolute, relative, resolve } from "node:path";
import type { AgentsConfig } from "../config/types.js";
import { truncateText } from "../shared/types.js";

export interface AgentsSnapshot {
  path: string;
  hash: string;
  summary: string;
  source: "repoRoot" | "cwd";
  bytes: number;
}

export interface LoadAgentsSnapshotOptions {
  cwd: string;
  config: AgentsConfig;
}

export async function loadAgentsSnapshot(options: LoadAgentsSnapshotOptions): Promise<AgentsSnapshot | undefined> {
  if (!options.config.snapshot) return undefined;
  if (isAbsolute(options.config.agentsFile)) throw new Error("agents_gate: agents.agentsFile must be relative to repo root or cwd");

  const repoRoot = await findRepoRoot(options.cwd);
  const roots = orderedRoots(options.config.loadFrom, repoRoot, resolve(options.cwd));
  const snapshots: AgentsSnapshot[] = [];
  for (const entry of roots) {
    const candidate = resolve(entry.root, options.config.agentsFile);
    assertWithinBoundary(entry.root, candidate);
    const snapshot = await snapshotFile(candidate, entry.source);
    if (snapshot !== undefined) snapshots.push(snapshot);
  }

  if (snapshots.length === 0) return undefined;
  if (snapshots.length === 1) return snapshots[0];
  return combineSnapshots(snapshots);
}

async function snapshotFile(path: string, source: AgentsSnapshot["source"]): Promise<AgentsSnapshot | undefined> {
  try {
    await access(path);
    const [content, info] = await Promise.all([readFile(path, "utf8"), stat(path)]);
    return {
      path,
      hash: sha256(content),
      summary: summarizeAgentsMd(content),
      source,
      bytes: info.size
    };
  } catch (error) {
    if (isMissingFileError(error)) return undefined;
    /* v8 ignore next -- fs/promises access/read/stat reject with Error instances in supported Node.js runtimes. */
    const message = error instanceof Error ? error.message : "unknown read error";
    throw new Error(`agents_gate: failed to read ${path}: ${message}`);
  }
}

function combineSnapshots(snapshots: AgentsSnapshot[]): AgentsSnapshot {
  const hash = sha256(snapshots.map((snapshot) => `${snapshot.path}\0${snapshot.hash}`).join("\n"));
  const summary = snapshots.map((snapshot) => `[${snapshot.source}] ${snapshot.path}\n${snapshot.summary}`).join("\n\n");
  return {
    path: snapshots.map((snapshot) => snapshot.path).join(","),
    hash,
    summary: truncateText(summary, 4000).text,
    /* v8 ignore next -- combined snapshots only occur when cwd contributes a distinct snapshot; single repoRoot snapshots return before combine. */
    source: snapshots.some((snapshot) => snapshot.source === "cwd") ? "cwd" : "repoRoot",
    bytes: snapshots.reduce((sum, snapshot) => sum + snapshot.bytes, 0)
  };
}

function summarizeAgentsMd(content: string): string {
  const normalized = content.split("\n").map((line) => line.trim()).filter((line) => line.length > 0).slice(0, 12).join("\n");
  return truncateText(normalized.length === 0 ? "Empty AGENTS.md" : normalized, 2000).text;
}

async function findRepoRoot(cwd: string): Promise<string> {
  let current = resolve(cwd);
  for (;;) {
    if (await exists(resolve(current, ".git"))) return current;
    const parent = dirname(current);
    if (parent === current) return resolve(cwd);
    current = parent;
  }
}

function orderedRoots(loadFrom: AgentsConfig["loadFrom"], repoRoot: string, cwd: string): { source: AgentsSnapshot["source"]; root: string }[] {
  const entries: AgentsConfig["loadFrom"] = loadFrom.length === 0 ? ["repoRoot", "cwd"] : loadFrom;
  const seen = new Set<string>();
  const result: { source: AgentsSnapshot["source"]; root: string }[] = [];
  for (const source of entries) {
    const root = source === "repoRoot" ? repoRoot : cwd;
    const key = root;
    if (seen.has(key)) continue;
    seen.add(key);
    result.push({ source, root });
  }
  return result;
}

function assertWithinBoundary(root: string, candidate: string): void {
  const diff = relative(resolve(root), resolve(candidate));
  if (diff.startsWith("..") || isAbsolute(diff)) throw new Error(`agents_gate: ${candidate} is outside ${root}`);
}

async function exists(path: string): Promise<boolean> {
  return stat(path).then(() => true).catch(() => false);
}

function isMissingFileError(error: unknown): boolean {
  return error instanceof Error && "code" in error && (error as { code?: unknown }).code === "ENOENT";
}

function sha256(content: string): string {
  return createHash("sha256").update(content).digest("hex");
}
