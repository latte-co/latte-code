import type { AgentLoop } from "../core/agent-loop.js";
import type { AgentResult } from "../core/agent-loop.js";
import { createHeadlessRunEnvelopeFromAgentResult } from "../core/contracts.js";
import type { NodeExecutionInput, NodeExecutionResult, NodeExecutor } from "./types.js";

export class AgentNodeExecutor implements NodeExecutor {
  constructor(private readonly loop: AgentLoop) {}

  async execute(input: NodeExecutionInput): Promise<NodeExecutionResult> {
    const result = await this.loop.run({ input: input.input, allowedTools: input.contract.allowedTools, ...(input.sessionId === undefined ? {} : { sessionId: input.sessionId }) });
    const envelope = createHeadlessRunEnvelopeFromAgentResult(result);
    return {
      nodeId: input.contract.nodeId,
      status: envelope.status,
      summary: summarizeResult(result),
      evidenceIds: envelope.handoff?.evidenceRefs ?? result.session.evidenceIds,
      concerns: envelope.status === "completed" ? [] : [concernFor(result)],
      graphUpdate: {
        evidenceIds: result.session.evidenceIds,
        eventCursor: result.session.lastEventSeq
      }
    };
  }
}

export function summarizeResult(result: AgentResult): string {
  if (result.handoff !== undefined) return result.handoff.summary;
  if (result.runState?.handoff !== undefined) return result.runState.handoff.summary;
  if (result.finalResponse !== undefined) return result.finalResponse;
  if (result.error !== undefined) return result.error;
  if (result.pendingInput !== undefined) return result.pendingInput.kind === "permission" ? result.pendingInput.reason : result.pendingInput.prompt;
  if (result.pendingPermission !== undefined) return result.pendingPermission.reason;
  /* v8 ignore next -- canonical agent results always expose a summary source above. */
  return "Agent loop ended without final response.";
}

export function concernFor(result: AgentResult): string {
  if (result.handoff?.risks[0] !== undefined) return result.handoff.risks[0];
  if (result.runState?.handoff?.risks[0] !== undefined) return result.runState.handoff.risks[0];
  if (result.handoff?.blockers[0] !== undefined) return result.handoff.blockers[0];
  if (result.runState?.handoff?.blockers[0] !== undefined) return result.runState.handoff.blockers[0];
  if (result.error !== undefined) return result.error;
  if (result.pendingInput !== undefined) return result.pendingInput.kind === "permission" ? result.pendingInput.reason : result.pendingInput.prompt;
  if (result.pendingPermission !== undefined) return result.pendingPermission.reason;
  /* v8 ignore next -- canonical non-completed agent results always expose a concern source above. */
  return "Non-completed agent result";
}
