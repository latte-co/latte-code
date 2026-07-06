import { join } from "node:path";
import { AgentLoop } from "../core/agent-loop.js";
import { InMemoryEventLog, FileEventLog } from "../events/event-log.js";
import { InMemoryEvidenceStore, FileEvidenceStore } from "../evidence/store.js";
import type { ModelClient, ModelTurn } from "../model/types.js";
import { createModelClient } from "../model/provider.js";
import { PermissionPolicy } from "../permissions/policy.js";
import { InMemorySessionStore, FileSessionStore } from "../session/session.js";
import { createBuiltinTools } from "../tools/builtin.js";
import { ToolRegistry } from "../tools/registry.js";
import type { LattecodeConfig } from "../config/types.js";
import { createMcpToolDefinitions, type McpBridgeClient } from "../mcp/bridge.js";
import { loadRuntimeContextSources } from "./context-sources.js";

export interface CreateAgentOptions {
  cwd: string;
  config: LattecodeConfig;
  model?: ModelClient;
  fakeScript?: readonly (ModelTurn | Error)[];
  env?: NodeJS.ProcessEnv;
  fetch?: typeof fetch;
  mcpClient?: McpBridgeClient;
}

export function createDefaultRegistry(config: LattecodeConfig, mcpClient?: McpBridgeClient): ToolRegistry {
  const registry = new ToolRegistry();
  const disabled = new Set(config.tools.disabled);
  const enabled = new Set(config.tools.enabled);
  for (const tool of createBuiltinTools()) {
    if (enabled.has(tool.name) && !disabled.has(tool.name)) registry.register(tool);
  }
  for (const tool of createMcpToolDefinitions(config, mcpClient)) {
    if (!disabled.has(tool.name)) registry.register(tool);
  }
  return registry;
}

export function createAgentLoop(options: CreateAgentOptions): AgentLoop {
  const model = options.model ?? createModelClient({
    config: options.config,
    ...(options.fakeScript === undefined ? {} : { fakeScript: options.fakeScript }),
    ...(options.env === undefined ? {} : { env: options.env }),
    ...(options.fetch === undefined ? {} : { fetch: options.fetch })
  });
  const sessions = options.config.session.store === "memory" ? new InMemorySessionStore() : new FileSessionStore(join(options.cwd, options.config.session.directory));
  const evidence = options.config.evidence.store === "memory" ? new InMemoryEvidenceStore() : new FileEvidenceStore(join(options.cwd, options.config.evidence.directory));
  const events = options.config.session.store === "memory" ? new InMemoryEventLog() : new FileEventLog(join(options.cwd, options.config.session.directory, "events.jsonl"));
  return new AgentLoop({
    cwd: options.cwd,
    config: options.config,
    model,
    registry: createDefaultRegistry(options.config, options.mcpClient),
    permissions: new PermissionPolicy(options.config.permissions, options.config.tools.shell),
    sessions,
    events,
    evidence,
    loadContextSources: () => loadRuntimeContextSources(options.cwd, options.config),
    maxTurns: options.config.runtime.maxTurns
  });
}
