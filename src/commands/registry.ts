import { createHash } from "node:crypto";
import { readFile, readdir } from "node:fs/promises";
import { isAbsolute, relative, resolve } from "node:path";
import type { CommandsConfig } from "../config/types.js";
import type { AgentTaskContext } from "../core/contracts.js";
import { isAgentTaskContext } from "../core/contracts.js";
import { isRecord, truncateText } from "../shared/types.js";

export interface CommandSpec {
  name: string;
  description: string;
  path: string;
  hash: string;
  context: AgentTaskContext;
}

export interface RoutedCommand {
  command: CommandSpec;
  args: string;
  context: AgentTaskContext;
}

const COMMAND_SIDE_EFFECT_KEYS = new Set(["toolCalls", "tools", "shell", "writeFiles", "scripts", "sideEffects"]);

export async function loadLocalCommandSpecs(cwd: string, config: CommandsConfig): Promise<CommandSpec[]> {
  if (!config.allowLocalCommands) return [];
  const root = resolveLocal(cwd, config.localDirectory, "command_gate");
  const files = await readdir(root).catch(() => []);
  const enabled = new Set(config.enabled);
  const specs = await Promise.all(files.filter((file) => file.endsWith(".json") && enabled.has(file.replace(/\.json$/u, ""))).map((file) => loadCommandSpec(root, file)));
  return specs.filter((spec) => enabled.has(spec.name));
}

export function routeCommandInput(input: string, specs: readonly CommandSpec[]): RoutedCommand | undefined {
  const trimmed = input.trim();
  if (!trimmed.startsWith("/")) return undefined;
  const [rawName = "", ...rest] = trimmed.slice(1).split(/\s+/u);
  const spec = specs.find((candidate) => candidate.name === rawName);
  if (spec === undefined) return undefined;
  const args = rest.join(" ");
  return { command: spec, args, context: contextWithArgs(spec.context, args) };
}

async function loadCommandSpec(root: string, file: string): Promise<CommandSpec> {
  const path = resolve(root, file);
  assertInside(root, path, "command_gate");
  const content = await readFile(path, "utf8");
  const parsed = JSON.parse(content) as unknown;
  if (!isRecord(parsed)) throw new Error(`command_gate: ${path} must contain a JSON object`);
  assertNoSideEffects(parsed, path);
  const context = parsed.context;
  if (!isAgentTaskContext(context)) throw new Error(`command_gate: ${path} must provide a valid agent context under context`);
  const name = typeof parsed.name === "string" ? parsed.name : file.replace(/\.json$/u, "");
  const description = typeof parsed.description === "string" ? parsed.description : context.objective;
  return { name, description: truncateText(description, 500).text, path, hash: sha256(content), context };
}

function contextWithArgs(context: AgentTaskContext, args: string): AgentTaskContext {
  if (args.trim().length === 0) return JSON.parse(JSON.stringify(context)) as AgentTaskContext;
  return {
    objective: context.objective.replaceAll("{{args}}", args),
    scope: context.scope.map((entry) => entry.replaceAll("{{args}}", args)),
    acceptance: context.acceptance.map((entry) => entry.replaceAll("{{args}}", args)),
    nonGoals: context.nonGoals.map((entry) => entry.replaceAll("{{args}}", args)),
    constraints: context.constraints.map((entry) => entry.replaceAll("{{args}}", args)),
    blockers: context.blockers.map((entry) => entry.replaceAll("{{args}}", args))
  };
}

function assertNoSideEffects(value: Record<string, unknown>, path: string): void {
  for (const key of Object.keys(value)) {
    if (COMMAND_SIDE_EFFECT_KEYS.has(key)) throw new Error(`command_gate: ${path} declares side-effect key '${key}'`);
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
  /* v8 ignore next -- command candidates are resolved from files returned by readdir under root; resolveLocal covers user-provided directory escapes. */
  if (diff.startsWith("..") || isAbsolute(diff)) throw new Error(`${gate}: ${candidate} is outside ${root}`);
}

function sha256(content: string): string {
  return createHash("sha256").update(content).digest("hex");
}
