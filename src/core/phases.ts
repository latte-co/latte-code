import type { AgentHandoff, AgentPhase, ChangePlan, ContextPack, PatchSummary, TaskRunState, TaskSpec, VerificationResult } from "./contracts.js";
import { isAgentHandoff, isChangePlan, isContextPack, isPatchSummary, isTaskSpec, isVerificationResult } from "./contracts.js";

export interface PhaseContract<Output> {
  phase: AgentPhase;
  allowedTools: string[];
  maxReactSteps: number;
  outputSchemaName: string;
  validateOutput(value: unknown): Output;
  next(output: Output, run: TaskRunState): AgentPhase | "completed" | "blocked" | "failed";
}

export type PhaseArtifact = TaskSpec | ContextPack | ChangePlan | PatchSummary | VerificationResult[] | AgentHandoff;

const READ_TOOLS = ["list_directory", "read_file", "search", "read_project_manifest"];

export function createDefaultPhaseContracts(maxReactSteps: number): Record<AgentPhase, PhaseContract<PhaseArtifact>> {
  return {
    intake: {
      phase: "intake",
      allowedTools: [],
      maxReactSteps,
      outputSchemaName: "TaskSpec",
      validateOutput(value) {
        if (!isTaskSpec(value)) throw new Error("Invalid TaskSpec artifact");
        return value;
      },
      next(output) {
        const task = output as TaskSpec;
        return task.blockers.length > 0 ? "blocked" : "understand";
      }
    },
    understand: {
      phase: "understand",
      allowedTools: READ_TOOLS,
      maxReactSteps,
      outputSchemaName: "ContextPack",
      validateOutput(value) {
        if (!isContextPack(value)) throw new Error("Invalid ContextPack artifact");
        return value;
      },
      next(output) {
        const context = output as ContextPack;
        return context.openQuestions.length > 0 ? "blocked" : "plan";
      }
    },
    plan: {
      phase: "plan",
      allowedTools: READ_TOOLS,
      maxReactSteps,
      outputSchemaName: "ChangePlan",
      validateOutput(value) {
        if (!isChangePlan(value)) throw new Error("Invalid ChangePlan artifact");
        return value;
      },
      next() {
        return "edit";
      }
    },
    edit: {
      phase: "edit",
      allowedTools: ["read_file", "edit_file", "write_file"],
      maxReactSteps,
      outputSchemaName: "PatchSummary",
      validateOutput(value) {
        if (!isPatchSummary(value)) throw new Error("Invalid PatchSummary artifact");
        return value;
      },
      next() {
        return "verify";
      }
    },
    verify: {
      phase: "verify",
      allowedTools: ["shell_exec", ...READ_TOOLS],
      maxReactSteps,
      outputSchemaName: "VerificationResult[]",
      validateOutput(value) {
        if (!Array.isArray(value) || !value.every(isVerificationResult)) throw new Error("Invalid VerificationResult[] artifact");
        return value;
      },
      next(output) {
        const verification = output as VerificationResult[];
        return verification.some((entry) => entry.status === "failed") ? "failed" : "handoff";
      }
    },
    handoff: {
      phase: "handoff",
      allowedTools: ["git_diff"],
      maxReactSteps,
      outputSchemaName: "AgentHandoff",
      validateOutput(value) {
        if (!isAgentHandoff(value)) throw new Error("Invalid AgentHandoff artifact");
        return value;
      },
      next(output) {
        const handoff = output as AgentHandoff;
        return handoff.status;
      }
    }
  };
}
