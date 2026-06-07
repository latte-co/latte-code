import type { FluxcodeConfig, McpToolConfig } from "../config/types.js";
import type { JsonObject } from "../shared/types.js";
import { isJsonValue } from "../shared/types.js";
import type { RiskLevel, ToolDefinition, ToolResult } from "../tools/types.js";
import type { LightweightSchema } from "../tools/schema.js";

export interface McpBridgeClient {
  callTool(serverName: string, toolName: string, input: JsonObject): Promise<{ output?: JsonObject; summary: string; references?: string[]; truncated?: boolean }>;
}

export interface McpToolSnapshot {
  server: string;
  tool: string;
  toolName: string;
  enabled: boolean;
}

export function listConfiguredMcpTools(config: FluxcodeConfig): McpToolSnapshot[] {
  if (!config.mcp.enabled || !config.mcp.routeThroughPermission) return [];
  return Object.entries(config.mcp.servers).flatMap(([serverName, server]) => {
    if (config.mcp.requireExplicitEnable && server.enabled !== true) return [];
    return Object.keys(server.tools ?? {}).map((tool) => ({ server: serverName, tool, toolName: mcpToolName(serverName, tool), enabled: true }));
  });
}

export function createMcpToolDefinitions(config: FluxcodeConfig, client?: McpBridgeClient): ToolDefinition[] {
  if (!config.mcp.enabled || !config.mcp.routeThroughPermission) return [];
  return Object.entries(config.mcp.servers).flatMap(([serverName, server]) => {
    if (config.mcp.requireExplicitEnable && server.enabled !== true) return [];
    return Object.entries(server.tools ?? {}).map(([toolName, tool]) => mcpToolDefinition(serverName, toolName, tool, client));
  });
}

function mcpToolDefinition(serverName: string, remoteToolName: string, config: McpToolConfig, client: McpBridgeClient | undefined): ToolDefinition {
  const name = mcpToolName(serverName, remoteToolName);
  return {
    name,
    description: config.description ?? `Call MCP tool ${remoteToolName} on ${serverName}`,
    inputSchema: normalizeInputSchema(config.inputSchema),
    outputSchema: { type: "object" },
    riskLevel: config.riskLevel ?? "medium",
    mutating: config.mutating ?? true,
    permission: { reason: `Call enabled MCP tool ${serverName}.${remoteToolName}` },
    async execute(input): Promise<ToolResult> {
      if (client === undefined) throw new Error(`mcp_gate: MCP client is not configured for ${serverName}.${remoteToolName}`);
      const response = await client.callTool(serverName, remoteToolName, input);
      return {
        callId: "",
        toolName: name,
        ok: true,
        ...(response.output === undefined ? {} : { output: response.output }),
        summary: response.summary,
        references: response.references ?? [`mcp://${serverName}/${remoteToolName}`],
        truncated: response.truncated ?? false
      };
    }
  };
}

function mcpToolName(serverName: string, toolName: string): string {
  return `mcp_${sanitize(serverName)}_${sanitize(toolName)}`;
}

function sanitize(value: string): string {
  return value.replace(/[^a-zA-Z0-9_]/gu, "_");
}

function normalizeInputSchema(schema: Record<string, unknown> | undefined): LightweightSchema {
  if (schema === undefined) return { type: "object" };
  return isJsonValue(schema) ? (schema as unknown as LightweightSchema) : { type: "object" };
}
