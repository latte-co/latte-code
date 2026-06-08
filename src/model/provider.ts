import { providerTypeStatus } from "../config/types.js";
import type { FluxcodeConfig } from "../config/types.js";
import { FakeModelClient } from "./fake.js";
import { OpenAICompatibleModelClient } from "./openai-compatible.js";
import type { ModelClient, ModelTurn } from "./types.js";

export interface CreateModelClientOptions {
  config: FluxcodeConfig;
  env?: NodeJS.ProcessEnv;
  fakeScript?: readonly (ModelTurn | Error)[];
  fetch?: typeof fetch;
}

export function createModelClient(options: CreateModelClientOptions): ModelClient {
  const providerId = options.config.models.default;
  const provider = options.config.models.providers[providerId];
  if (provider === undefined) throw new Error(`Default model provider '${providerId}' is not defined`);
  if (Object.prototype.hasOwnProperty.call(provider, "apiMode")) {
    throw new Error(`Provider '${providerId}' uses unsupported apiMode; set models.providers.${providerId}.type explicitly instead`);
  }
  if (provider.type === "fake") {
    if (options.fakeScript === undefined) {
      throw new Error(`Provider '${providerId}' is the scripted fake provider but no fakeScript was supplied. Configure a real model provider via --config, ~/.fluxcode/fluxcode.jsonc, or .fluxcode/fluxcode.jsonc, or pass an explicit fakeScript in tests.`);
    }
    return new FakeModelClient(options.fakeScript);
  }
  if (provider.type === "openai-compatible") {
    return new OpenAICompatibleModelClient({
      providerId,
      config: provider,
      ...(options.env === undefined ? {} : { env: options.env }),
      ...(options.fetch === undefined ? {} : { fetch: options.fetch })
    });
  }
  const status = providerTypeStatus(provider.type);
  if (status === "future") throw new Error(`Provider '${providerId}' type '${provider.type}' is recognized but not implemented in this runtime`);
  throw new Error(`Provider '${providerId}' has unsupported provider type '${String(provider.type)}'`);
}
