import type { AgentLoop } from "../core/agent-loop.js";
import type { AgentResult } from "../core/agent-loop.js";
import type { NodeExecutionInput, NodeExecutionResult, NodeExecutor } from "./types.js";

export class AgentNodeExecutor implements NodeExecutor {
  constructor(private readonly loop: AgentLoop) {}

  async execute(input: NodeExecutionInput): Promise<NodeExecutionResult> {
    const result = await this.loop.run({ input: input.input, allowedTools: input.contract.allowedTools, ...(input.sessionId === undefined ? {} : { sessionId: input.sessionId }) });
    return {
      nodeId: input.contract.nodeId,
      status: result.status,
      summary: summarizeResult(result),
      evidenceIds: result.session.evidenceIds,
      concerns: result.status === "completed" ? [] : [concernFor(result)],
      graphUpdate: {
        evidenceIds: result.session.evidenceIds,
        eventCursor: result.session.lastEventSeq
      }
    };
  }
}

export function summarizeResult(result: AgentResult): string {
  if (result.finalResponse !== undefined) return result.finalResponse;
  if (result.error !== undefined) return result.error;
  if (result.pendingPermission !== undefined) return result.pendingPermission.reason;
  /* c8 ignore next */
  return "Agent loop ended without final response.";
}

export function concernFor(result: AgentResult): string {
  if (result.error !== undefined) return result.error;
  if (result.pendingPermission !== undefined) return result.pendingPermission.reason;
  /* c8 ignore next */
  return "Non-completed agent result";
}
