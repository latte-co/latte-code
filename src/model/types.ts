import type { ToolCall, ToolDefinition, ToolResult } from "../tools/types.js";

export interface ModelMessage {
  role: "system" | "user" | "assistant" | "tool";
  content: string;
  toolCallId?: string;
}

export type ModelTurn =
  | { type: "message"; content: string }
  | { type: "tool_calls"; content?: string; toolCalls: ToolCall[] };

export interface ModelRequest {
  messages: ModelMessage[];
  tools: ToolDefinition[];
  toolResults: ToolResult[];
}

export interface ModelClient {
  generate(request: ModelRequest): Promise<ModelTurn>;
}
