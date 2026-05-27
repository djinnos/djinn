import { callMcpTool } from "@/api/mcpClient";
import type { McpToolOutput, ProviderModelsConnectedOutputSchema } from "@/api/generated/mcp-tools.gen";

export interface ModelEntry {
  model: string;
  provider: string;
  max_concurrent: number;
}

type SettingsGetToolResponse = McpToolOutput<"settings_get">;

interface ParsedSettingsGet {
  settings?: {
    max_sessions?: Record<string, number> | null;
  };
  error?: string;
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

/**
 * Global, operator-managed per-model concurrency caps (`settings_get.max_sessions`).
 * The model LIST itself is now per-user (see `@/api/userSettings`); this map only
 * carries the operational caps, which the Models tab renders/edits for admins.
 */
export async function fetchGlobalMaxSessions(): Promise<Record<string, number>> {
  const response = (await callMcpTool("settings_get", {})) as SettingsGetToolResponse;
  const parsed = response as ParsedSettingsGet;
  if (parsed.error) {
    throw new Error(parsed.error);
  }
  return parsed.settings?.max_sessions ?? {};
}

/**
 * Persist the global per-model concurrency caps. Admin-only server-side —
 * non-admin callers get `{ ok: false, error }`, surfaced as a thrown error.
 */
export async function saveGlobalMaxSessions(
  maxSessions: Record<string, number>,
): Promise<void> {
  const response = await callMcpTool("settings_set", {
    max_sessions: maxSessions,
  });

  if (!response.ok) {
    throw new Error(response.error ?? "Failed to save settings");
  }
}

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
