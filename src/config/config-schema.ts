import type { JsonObject } from "../shared/types.js";

export const LATTECODE_CONFIG_SCHEMA: JsonObject = {
  $schema: "https://json-schema.org/draft/2020-12/schema",
  title: "LattecodeConfig",
  type: "object",
  required: ["schemaVersion"],
  properties: {
    schemaVersion: { const: 1 },
    models: { type: "object" },
    runtime: { type: "object" },
    prompts: { type: "object" },
    agents: { type: "object" },
    context: { type: "object" },
    permissions: { type: "object" },
    tools: { type: "object" },
    commands: { type: "object" },
    skills: { type: "object" },
    mcp: { type: "object" },
    session: { type: "object" },
    evidence: { type: "object" }
  },
  additionalProperties: false
};
