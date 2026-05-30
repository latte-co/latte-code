import type { ModelClient, ModelRequest, ModelTurn } from "./types.js";

export class FakeModelClient implements ModelClient {
  private cursor = 0;
  readonly requests: ModelRequest[] = [];

  constructor(private readonly script: readonly (ModelTurn | Error)[]) {}

  async generate(request: ModelRequest): Promise<ModelTurn> {
    this.requests.push(request);
    const next = this.script[this.cursor];
    this.cursor += 1;
    if (next === undefined) return { type: "message", content: "No scripted response." };
    if (next instanceof Error) throw next;
    return next;
  }

  get calls(): number {
    return this.cursor;
  }
}
