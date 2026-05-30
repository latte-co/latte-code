import type { JsonObject } from "../shared/types.js";

export const FLUXCODE_CONFIG_SCHEMA: JsonObject = {
  $schema: "https://json-schema.org/draft/2020-12/schema",
  title: "FluxcodeConfig",
  type: "object",
  required: ["schemaVersion"],
  properties: {
    schemaVersion: { const: 1 },
    models: { type: "object" },
    permissions: { type: "object" },
    tools: { type: "object" },
    session: { type: "object" },
    evidence: { type: "object" },
    coverage: { type: "object" }
  },
  additionalProperties: false
};
