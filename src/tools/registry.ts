import { validateSchema } from "./schema.js";
import type { ToolCall, ToolDefinition, ToolExecutionContext, ToolResult } from "./types.js";

export class ToolRegistry {
  private readonly tools = new Map<string, ToolDefinition>();

  register(tool: ToolDefinition): void {
    if (this.tools.has(tool.name)) throw new Error(`Tool '${tool.name}' is already registered`);
    this.tools.set(tool.name, tool);
  }

  get(name: string): ToolDefinition {
    const tool = this.tools.get(name);
    if (tool === undefined) throw new Error(`Unknown tool '${name}'`);
    return tool;
  }

  list(): ToolDefinition[] {
    return [...this.tools.values()];
  }

  validate(call: ToolCall): ToolDefinition {
    const tool = this.get(call.name);
    validateSchema(tool.inputSchema, call.input);
    return tool;
  }

  async execute(call: ToolCall, context: ToolExecutionContext): Promise<ToolResult> {
    const tool = this.validate(call);
    const result = await tool.execute(call.input, context);
    if (result.output !== undefined) validateSchema(tool.outputSchema, result.output);
    return result;
  }
}
