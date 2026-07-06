import { createHash } from "node:crypto";
import { readFile, stat } from "node:fs/promises";
import { isAbsolute, relative, resolve } from "node:path";
import type { SkillsConfig } from "../config/types.js";
import { isRecord, truncateText } from "../shared/types.js";

export interface SkillContextEntry {
  name: string;
  path: string;
  hash: string;
  instructions: string;
  commandSpecs: string[];
}

export interface LoadSkillsOptions {
  cwd: string;
  config: SkillsConfig;
}

const SIDE_EFFECT_KEYS = new Set(["sideEffects", "scripts", "shell", "toolCalls", "hooks", "install", "postInstall", "writeFiles"]);

export async function loadLocalSkills(options: LoadSkillsOptions): Promise<SkillContextEntry[]> {
  if (options.config.enabled.length === 0) return [];
  return Promise.all(options.config.enabled.map((name) => loadSkillByName(options.cwd, options.config, name)));
}

async function loadSkillByName(cwd: string, config: SkillsConfig, name: string): Promise<SkillContextEntry> {
  for (const directory of config.localDirectories) {
    const root = resolveLocal(cwd, directory, "skill_gate");
    for (const candidate of [resolve(root, name, "skill.json"), resolve(root, `${name}.json`), resolve(root, name, "SKILL.md"), resolve(root, `${name}.md`)]) {
      assertInside(root, candidate, "skill_gate");
      const content = await readIfExists(candidate);
      if (content !== undefined) return parseSkill(candidate, name, content, config.allowSideEffects);
    }
  }
  throw new Error(`skill_gate: enabled skill '${name}' was not found in localDirectories`);
}

function parseSkill(path: string, fallbackName: string, content: string, allowSideEffects: boolean): SkillContextEntry {
  if (path.endsWith(".json")) {
    const parsed = JSON.parse(content) as unknown;
    if (!isRecord(parsed)) throw new Error(`skill_gate: ${path} must contain a JSON object`);
    if (!allowSideEffects) assertNoSideEffects(parsed, path);
    const name = typeof parsed.name === "string" ? parsed.name : fallbackName;
    const instructions = typeof parsed.instructions === "string" ? parsed.instructions : "";
    const commandSpecs = Array.isArray(parsed.commands) ? parsed.commands.map((entry) => JSON.stringify(entry)).filter((entry) => entry.length > 0) : [];
    return {
      name,
      path,
      hash: sha256(content),
      instructions: truncateText(instructions, 2000).text,
      commandSpecs
    };
  }
  return { name: fallbackName, path, hash: sha256(content), instructions: truncateText(content, 2000).text, commandSpecs: [] };
}

function assertNoSideEffects(value: Record<string, unknown>, path: string): void {
  for (const key of Object.keys(value)) {
    if (SIDE_EFFECT_KEYS.has(key)) throw new Error(`skill_gate: ${path} declares side-effect key '${key}'`);
  }
}

function resolveLocal(cwd: string, directory: string, gate: string): string {
  if (isAbsolute(directory)) throw new Error(`${gate}: local directory must be relative to cwd`);
  const resolved = resolve(cwd, directory);
  assertInside(resolve(cwd), resolved, gate);
  return resolved;
}

function assertInside(root: string, candidate: string, gate: string): void {
  const diff = relative(resolve(root), resolve(candidate));
  /* v8 ignore next -- skill candidates are built from the already bounded local skill root; resolveLocal covers user-provided directory escapes. */
  if (diff.startsWith("..") || isAbsolute(diff)) throw new Error(`${gate}: ${candidate} is outside ${root}`);
}

async function readIfExists(path: string): Promise<string | undefined> {
  try {
    await stat(path);
    return await readFile(path, "utf8");
  } catch (error) {
    if (error instanceof Error && "code" in error && (error as { code?: unknown }).code === "ENOENT") return undefined;
    throw error;
  }
}

function sha256(content: string): string {
  return createHash("sha256").update(content).digest("hex");
}
