import { callMcpTool } from "@/api/mcpClient";
import type { ProviderModelsConnectedOutputSchema } from "@/api/generated/mcp-tools.gen";

export interface ModelEntry {
  model: string;
  provider: string;
  max_concurrent: number;
}

function splitModelId(modelId: string): { provider: string; model: string } {
  const slashIndex = modelId.indexOf("/");
  if (slashIndex < 0) {
    return { provider: "unknown", model: modelId };
  }
  return {
    provider: modelId.slice(0, slashIndex),
    model: modelId.slice(slashIndex + 1),
  };
}

function combineModelId(provider: string, model: string): string {
  if (model.startsWith(`${provider}/`)) {
    return model;
  }
  return `${provider}/${model}`;
}

export { splitModelId, combineModelId };

export type ProviderModel = ProviderModelsConnectedOutputSchema.ProviderModelOutput;

export async function fetchProviderModels(): Promise<ProviderModel[]> {
  const response = await callMcpTool("provider_models_connected");
  const seen = new Set<string>();
  const models: ProviderModel[] = [];

  for (const model of response.models) {
    // Chat selector should only include models that support tool calling.
    if (model.tool_call === false) continue;

    const key = model.id;
    if (seen.has(key)) continue;

    seen.add(key);
    models.push(model);
  }

  return models;
}
