import { access, readFile } from "node:fs/promises";
import { join } from "node:path";
import { DEFAULT_CONFIG } from "./defaults.js";
import { parseJsonc } from "./jsonc.js";
import type { FluxcodeConfig } from "./types.js";
import { isRecord, jsonClone } from "../shared/types.js";

export interface LoadConfigOptions {
  cwd: string;
  configPath?: string;
  homeDir?: string;
}

type PartialJson = Record<string, unknown>;

export function mergeConfig(base: FluxcodeConfig, override: unknown): FluxcodeConfig {
  if (override === undefined) return jsonClone(base);
  if (!isRecord(override)) throw new Error("Config root must be an object");
  const merged = mergeObjects(base, override) as FluxcodeConfig;
  validateConfig(merged);
  return merged;
}

function mergeObjects(base: unknown, override: unknown): unknown {
  if (Array.isArray(override)) return [...override];
  if (!isRecord(base) || !isRecord(override)) return override;
  const result: PartialJson = { ...base };
  for (const [key, value] of Object.entries(override)) {
    result[key] = mergeObjects(result[key], value);
  }
  return result;
}

export function validateConfig(config: FluxcodeConfig): void {
  if (config.schemaVersion !== 1) throw new Error("Unsupported schemaVersion");
  const provider = config.models.providers[config.models.default];
  if (provider === undefined) throw new Error(`Default model provider '${config.models.default}' is not defined`);
  for (const [id, candidate] of Object.entries(config.models.providers)) {
    if (candidate.apiKeyEnv !== undefined && candidate.apiKeyEnv.trim() === "") {
      throw new Error(`Provider '${id}' has empty apiKeyEnv`);
    }
  }
  for (const mode of [config.permissions.defaultMode, config.permissions.mutatingTools, config.permissions.highRiskTools]) {
    if (!["allow", "ask", "deny"].includes(mode)) throw new Error(`Invalid permission mode '${mode}'`);
  }
  for (const threshold of [config.coverage.statements, config.coverage.branches, config.coverage.functions, config.coverage.lines]) {
    if (threshold < 0 || threshold > 100) throw new Error("Coverage thresholds must be between 0 and 100");
  }
}

export async function loadConfig(options: LoadConfigOptions): Promise<{ config: FluxcodeConfig; path?: string }> {
  const path = await findConfigPath(options);
  if (path === undefined) {
    validateConfig(DEFAULT_CONFIG);
    return { config: jsonClone(DEFAULT_CONFIG) };
  }
  const raw = await readFile(path, "utf8");
  const parsed = parseJsonc(raw);
  return { config: mergeConfig(DEFAULT_CONFIG, parsed), path };
}

export async function findConfigPath(options: LoadConfigOptions): Promise<string | undefined> {
  const candidates = [
    options.configPath,
    join(options.cwd, "fluxcode.config.jsonc"),
    options.homeDir === undefined ? undefined : join(options.homeDir, "fluxcode", "config.jsonc")
  ].filter((entry): entry is string => entry !== undefined);

  for (const candidate of candidates) {
    try {
      await access(candidate);
      return candidate;
    } catch {
      continue;
    }
  }
  return undefined;
}
