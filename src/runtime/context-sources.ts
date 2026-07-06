import type { LattecodeConfig } from "../config/types.js";
import { loadAgentsSnapshot, type AgentsSnapshot } from "../context/agents.js";
import { loadLocalCommandSpecs, type CommandSpec } from "../commands/registry.js";
import { listConfiguredMcpTools, type McpToolSnapshot } from "../mcp/bridge.js";
import { loadLocalSkills, type SkillContextEntry } from "../skills/loader.js";

export interface RuntimeContextSources {
  agentsMd?: AgentsSnapshot;
  skills: SkillContextEntry[];
  commands: CommandSpec[];
  mcpTools: McpToolSnapshot[];
}

export async function loadRuntimeContextSources(cwd: string, config: LattecodeConfig): Promise<RuntimeContextSources> {
  const [agentsMd, skills, commands] = await Promise.all([
    loadAgentsSnapshot({ cwd, config: config.agents }),
    loadLocalSkills({ cwd, config: config.skills }),
    loadLocalCommandSpecs(cwd, config.commands)
  ]);
  return { ...(agentsMd === undefined ? {} : { agentsMd }), skills, commands, mcpTools: listConfiguredMcpTools(config) };
}
