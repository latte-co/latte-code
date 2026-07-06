import type { AgentPhase, TaskRunState } from "../core/contracts.js";
import type { ModelMessage } from "../model/types.js";
import type { ToolResult } from "../tools/types.js";

export interface PromptRenderInput {
  run: TaskRunState;
  phase: AgentPhase;
  allowedTools: string[];
  contextProjection: string;
  toolResults: ToolResult[];
}

export interface PromptTemplate {
  id: string;
  version: string;
  phase: AgentPhase;
  expectedOutput: string;
  render(input: PromptRenderInput): ModelMessage[];
}

export class PromptRegistry {
  private readonly templates = new Map<AgentPhase, PromptTemplate>();

  register(template: PromptTemplate): void {
    this.templates.set(template.phase, template);
  }

  get(phase: AgentPhase): PromptTemplate {
    const template = this.templates.get(phase);
    if (template === undefined) throw new Error(`Prompt template for phase '${phase}' is not registered`);
    return template;
  }
}

export function createDefaultPromptRegistry(profile = "default-code-agent-v1", language = "en-US"): PromptRegistry {
  const registry = new PromptRegistry();
  for (const phase of ["intake", "understand", "plan", "edit", "verify", "handoff"] as const) {
    registry.register(defaultTemplate(profile, language, phase));
  }
  return registry;
}

function defaultTemplate(profile: string, language: string, phase: AgentPhase): PromptTemplate {
  const expectedOutput = outputSchemaName(phase);
  return {
    id: `${profile}:${phase}`,
    version: "v0.1",
    phase,
    expectedOutput,
    render(input) {
      return [
        {
          role: "system",
          content: [
            `You are Lattecode, a local-first code agent. Language: ${language}.`,
            "All CLI, command, skill, MCP, and built-in tool behavior must route through the unified phase loop, permission decisions, evidence, trace, session, and handoff.",
            "Skills may inject instructions/workflows/specs only; commands become TaskSpec or phase events; MCP tools are regular Lattecode tools and never bypass permission.",
            `Current phase: ${input.phase}. Return only JSON matching ${expectedOutput} when the phase artifact is ready.`
          ].join("\n")
        },
        {
          role: "system",
          content: [`Allowed tools: ${input.allowedTools.join(", ") || "none"}.`, `Expected output schema: ${expectedOutput}.`, input.contextProjection].join("\n")
        }
      ];
    }
  };
}

function outputSchemaName(phase: AgentPhase): string {
  if (phase === "intake") return "TaskSpec";
  if (phase === "understand") return "ContextPack";
  if (phase === "plan") return "ChangePlan";
  if (phase === "edit") return "PatchSummary";
  if (phase === "verify") return "VerificationResult[]";
  return "AgentHandoff";
}
