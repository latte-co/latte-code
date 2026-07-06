import type { TaskRunState } from "../core/contracts.js";
import type { ModelMessage } from "../model/types.js";
import type { ToolResult } from "../tools/types.js";

export interface CodeAgentPromptRenderInput {
  run: TaskRunState;
  allowedTools: string[];
  contextProjection: string;
  toolResults: ToolResult[];
}

export interface CodeAgentPromptTemplate {
  id: string;
  version: string;
  render(input: CodeAgentPromptRenderInput): ModelMessage[];
}

export function createDefaultCodeAgentPrompt(profile = "default-code-agent-v1", language = "en-US"): CodeAgentPromptTemplate {
  return {
    id: `${profile}:code-agent`,
    version: "v0.1",
    render(input) {
      return [
        {
          role: "system",
          content: [
            `You are Lattecode, a code agent for this workspace. Language: ${language}.`,
            "Operate as a direct ReAct code agent: reason over the task, call tools when useful, read before writing, use verification tools when appropriate, and continue from tool results.",
            "When the task is complete or blocked, reply with a normal assistant message for the user.",
            "All tool behavior routes through Lattecode permission decisions, evidence, trace, session, and handoff recording."
          ].join("\n")
        },
        {
          role: "system",
          content: [
            `Allowed tools: ${input.allowedTools.join(", ") || "none"}.`,
            "Final response: plain assistant text, not JSON unless the user explicitly asked for JSON.",
            input.contextProjection
          ].join("\n")
        }
      ];
    }
  };
}
