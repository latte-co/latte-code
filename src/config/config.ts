import { access, readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import { DEFAULT_CONFIG } from "./defaults.js";
import { parseJsonc } from "./jsonc.js";
import { providerTypeStatus } from "./types.js";
import type { FluxcodeConfig } from "./types.js";
import { isRecord, jsonClone } from "../shared/types.js";

export interface LoadConfigOptions {
  cwd: string;
  configPath?: string;
  homeDir?: string;
}

type PartialJson = Record<string, unknown>;

export interface LoadedConfig {
  config: FluxcodeConfig;
  path?: string;
  paths: string[];
}

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
  validateProvider(config.models.default, provider);
  for (const [id, candidate] of Object.entries(config.models.providers)) {
    validateProvider(id, candidate);
    if (candidate.apiKeyEnv !== undefined && candidate.apiKeyEnv.trim() === "") {
      throw new Error(`Provider '${id}' has empty apiKeyEnv`);
    }
  }
  validatePositiveInteger("runtime.maxPhaseSteps", config.runtime.maxPhaseSteps);
  validateNonNegativeInteger("runtime.maxRepairTurns", config.runtime.maxRepairTurns);
  validateBoolean("runtime.stopOnVerificationFailure", config.runtime.stopOnVerificationFailure);
  validateNonEmptyString("prompts.profile", config.prompts.profile);
  validateNonEmptyString("prompts.language", config.prompts.language);
  validateNonEmptyString("agents.agentsFile", config.agents.agentsFile);
  validateStringArray("agents.loadFrom", config.agents.loadFrom);
  for (const entry of config.agents.loadFrom) {
    if (entry !== "repoRoot" && entry !== "cwd") throw new Error(`Invalid agents.loadFrom entry '${entry}'`);
  }
  validateBoolean("agents.snapshot", config.agents.snapshot);
  if (config.agents.hashAlgorithm !== "sha256") throw new Error("agents.hashAlgorithm must be sha256");
  validatePositiveInteger("context.maxPromptBytes", config.context.maxPromptBytes);
  validatePositiveInteger("context.maxToolResultBytes", config.context.maxToolResultBytes);
  validateNonNegativeInteger("context.recentStepCount", config.context.recentStepCount);
  validateStringArray("context.preserve", config.context.preserve);
  for (const mode of [config.permissions.defaultMode, config.permissions.mutatingTools, config.permissions.highRiskTools]) {
    if (!["allow", "ask", "deny"].includes(mode)) throw new Error(`Invalid permission mode '${mode}'`);
  }
  validateStringArray("permissions.trustedDirectories", config.permissions.trustedDirectories);
  validateStringArray("permissions.denyGlobs", config.permissions.denyGlobs);
  validateStringArray("tools.enabled", config.tools.enabled);
  validateStringArray("tools.disabled", config.tools.disabled);
  validatePositiveInteger("tools.maxOutputBytes", config.tools.maxOutputBytes);
  validatePositiveInteger("tools.shell.defaultTimeoutMs", config.tools.shell.defaultTimeoutMs);
  validateStringArray("tools.shell.allowCommands", config.tools.shell.allowCommands);
  validateStringArray("tools.shell.requireApprovalFor", config.tools.shell.requireApprovalFor);
  validateStringArray("commands.enabled", config.commands.enabled);
  validateNonEmptyString("commands.localDirectory", config.commands.localDirectory);
  validateBoolean("commands.allowLocalCommands", config.commands.allowLocalCommands);
  validateStringArray("skills.enabled", config.skills.enabled);
  validateStringArray("skills.localDirectories", config.skills.localDirectories);
  validateBoolean("skills.allowSideEffects", config.skills.allowSideEffects);
  validateBoolean("mcp.enabled", config.mcp.enabled);
  if (!isRecord(config.mcp.servers)) throw new Error("mcp.servers must be an object");
  validateBoolean("mcp.requireExplicitEnable", config.mcp.requireExplicitEnable);
  validateBoolean("mcp.routeThroughPermission", config.mcp.routeThroughPermission);
  for (const [serverName, server] of Object.entries(config.mcp.servers)) {
    if (server.enabled !== undefined) validateBoolean(`mcp.servers.${serverName}.enabled`, server.enabled);
    if (server.command !== undefined) validateNonEmptyString(`mcp.servers.${serverName}.command`, server.command);
    if (server.args !== undefined) validateStringArray(`mcp.servers.${serverName}.args`, server.args);
    if (server.env !== undefined && !isRecord(server.env)) throw new Error(`mcp.servers.${serverName}.env must be an object`);
    if (server.tools !== undefined) {
      if (!isRecord(server.tools)) throw new Error(`mcp.servers.${serverName}.tools must be an object`);
      for (const [toolName, tool] of Object.entries(server.tools)) {
        if (tool.description !== undefined) validateNonEmptyString(`mcp.servers.${serverName}.tools.${toolName}.description`, tool.description);
        if (tool.mutating !== undefined) validateBoolean(`mcp.servers.${serverName}.tools.${toolName}.mutating`, tool.mutating);
        if (tool.riskLevel !== undefined && tool.riskLevel !== "low" && tool.riskLevel !== "medium" && tool.riskLevel !== "high") throw new Error(`Invalid risk level for mcp.servers.${serverName}.tools.${toolName}`);
      }
    }
  }
  for (const threshold of [config.coverage.statements, config.coverage.branches, config.coverage.functions, config.coverage.lines]) {
    if (threshold < 0 || threshold > 100) throw new Error("Coverage thresholds must be between 0 and 100");
  }
}

function validateProvider(id: string, provider: FluxcodeConfig["models"]["providers"][string]): void {
  if (Object.prototype.hasOwnProperty.call(provider, "apiMode")) {
    throw new Error(`Provider '${id}' uses unsupported apiMode; set models.providers.${id}.type explicitly instead`);
  }

  const status = providerTypeStatus(provider.type);
  if (status === "future") throw new Error(`Provider '${id}' type '${provider.type}' is recognized but not implemented in this runtime`);
  if (status === "unsupported") throw new Error(`Provider '${id}' has unsupported provider type '${String(provider.type)}'`);
  validateNonEmptyString(`Provider '${id}' model`, provider.model);
  if (provider.type === "openai-compatible" && provider.apiKeyEnv === undefined) throw new Error(`Provider '${id}' requires apiKeyEnv`);
  if (provider.baseUrl !== undefined) validateNonEmptyString(`Provider '${id}' baseUrl`, provider.baseUrl);
}

function validateNonEmptyString(name: string, value: unknown): void {
  if (typeof value !== "string" || value.trim() === "") throw new Error(`${name} must be a non-empty string`);
}

function validateBoolean(name: string, value: unknown): void {
  if (typeof value !== "boolean") throw new Error(`${name} must be a boolean`);
}

function validatePositiveInteger(name: string, value: unknown): void {
  if (!Number.isInteger(value) || typeof value !== "number" || value <= 0) throw new Error(`${name} must be a positive integer`);
}

function validateNonNegativeInteger(name: string, value: unknown): void {
  if (!Number.isInteger(value) || typeof value !== "number" || value < 0) throw new Error(`${name} must be a non-negative integer`);
}

function validateStringArray(name: string, value: unknown): void {
  if (!Array.isArray(value) || !value.every((entry) => typeof entry === "string")) throw new Error(`${name} must be a string array`);
}

export async function loadConfig(options: LoadConfigOptions): Promise<LoadedConfig> {
  const paths = await findConfigPaths(options);
  if (paths.length === 0) {
    validateConfig(DEFAULT_CONFIG);
    return { config: jsonClone(DEFAULT_CONFIG), paths: [] };
  }

  let override: unknown = undefined;
  for (const path of paths) {
    const raw = await readFile(path, "utf8");
    override = mergeRawConfig(override, parseJsonc(raw));
  }

  const effectivePath = paths[paths.length - 1];
  if (effectivePath === undefined) throw new Error("Internal config resolution error: expected at least one config path");
  return { config: mergeConfig(DEFAULT_CONFIG, override), path: effectivePath, paths };
}

export async function findConfigPath(options: LoadConfigOptions): Promise<string | undefined> {
  const paths = await findConfigPaths(options);
  return paths[paths.length - 1];
}

export async function findConfigPaths(options: LoadConfigOptions): Promise<string[]> {
  if (options.configPath !== undefined) {
    const explicitPath = await findFirstExistingFile([options.configPath]);
    if (explicitPath !== undefined) return [explicitPath];
  }

  const selectedPaths = await Promise.all([
    findFirstExistingFile(globalConfigCandidates(options.homeDir ?? homedir())),
    findFirstExistingFile(projectConfigCandidates(options.cwd))
  ]);

  return selectedPaths.filter((entry): entry is string => entry !== undefined);
}

function globalConfigCandidates(homeDir: string): string[] {
  return [join(homeDir, ".fluxcode", "fluxcode.jsonc"), join(homeDir, ".fluxcode", "fluxcode.json")];
}

function projectConfigCandidates(cwd: string): string[] {
  return [join(cwd, ".fluxcode", "fluxcode.jsonc"), join(cwd, ".fluxcode", "fluxcode.json")];
}

async function findFirstExistingFile(candidates: string[]): Promise<string | undefined> {
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

function mergeRawConfig(base: unknown, override: unknown): unknown {
  if (base === undefined) return override;
  return mergeObjects(base, override);
}
