import { join } from "node:path";
import { AgentLoop } from "../core/agent-loop.js";
import { InMemoryEventLog, FileEventLog } from "../events/event-log.js";
import { InMemoryEvidenceStore, FileEvidenceStore } from "../evidence/store.js";
import { FakeModelClient } from "../model/fake.js";
import type { ModelClient, ModelTurn } from "../model/types.js";
import { PermissionPolicy } from "../permissions/policy.js";
import { InMemorySessionStore, FileSessionStore } from "../session/session.js";
import { createBuiltinTools } from "../tools/builtin.js";
import { ToolRegistry } from "../tools/registry.js";
import type { FluxcodeConfig } from "../config/types.js";

export interface CreateAgentOptions {
  cwd: string;
  config: FluxcodeConfig;
  model?: ModelClient;
  fakeScript?: readonly (ModelTurn | Error)[];
}

export function createDefaultRegistry(config: FluxcodeConfig): ToolRegistry {
  const registry = new ToolRegistry();
  const disabled = new Set(config.tools.disabled);
  const enabled = new Set(config.tools.enabled);
  for (const tool of createBuiltinTools()) {
    if (enabled.has(tool.name) && !disabled.has(tool.name)) registry.register(tool);
  }
  return registry;
}

export function createAgentLoop(options: CreateAgentOptions): AgentLoop {
  const model = options.model ?? new FakeModelClient(options.fakeScript ?? [{ type: "message", content: "Fake model has no configured script." }]);
  const sessions = options.config.session.store === "memory" ? new InMemorySessionStore() : new FileSessionStore(join(options.cwd, options.config.session.directory));
  const evidence = options.config.evidence.store === "memory" ? new InMemoryEvidenceStore() : new FileEvidenceStore(join(options.cwd, options.config.evidence.directory));
  const events = options.config.session.store === "memory" ? new InMemoryEventLog() : new FileEventLog(join(options.cwd, options.config.session.directory, "events.jsonl"));
  return new AgentLoop({
    cwd: options.cwd,
    config: options.config,
    model,
    registry: createDefaultRegistry(options.config),
    permissions: new PermissionPolicy(options.config.permissions, options.config.tools.shell),
    sessions,
    events,
    evidence
  });
}
