import type { AgentResult, RunAgentInput } from "../core/agent-loop.js";

export interface NodeContract {
  nodeId: string;
  goal: string;
  allowedTools: string[];
  acceptance: string[];
}

export interface Gate {
  id: string;
  label: string;
  status: "pending" | "passed" | "failed" | "blocked";
}

export interface GraphState {
  runId: string;
  nodes: Record<string, NodeContract>;
  gates: Record<string, Gate>;
  evidenceIds: string[];
  eventCursor: number;
}

export interface NodeExecutionInput extends RunAgentInput {
  contract: NodeContract;
  graphState?: GraphState;
}

export interface NodeExecutionResult {
  nodeId: string;
  status: AgentResult["status"];
  summary: string;
  evidenceIds: string[];
  concerns: string[];
  graphUpdate: Partial<GraphState>;
}

export interface NodeExecutor {
  execute(input: NodeExecutionInput): Promise<NodeExecutionResult>;
}
