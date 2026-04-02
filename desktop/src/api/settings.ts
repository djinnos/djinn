import { callMcpTool } from "@/api/mcpClient";
import type { McpToolOutput, ProviderModelsConnectedOutputSchema } from "@/api/generated/mcp-tools.gen";

export interface ModelEntry {
  model: string;
  provider: string;
  max_concurrent: number;
}

export interface LangfuseSettings {
  publicKey: string;
  secretKey: string;
  endpoint: string;
}

export interface SettingsResponse {
  models: ModelEntry[];
  langfuse: LangfuseSettings;
}

type SettingsGetToolResponse = McpToolOutput<"settings_get">;

interface ParsedSettingsGet {
  settings?: {
    model_priority?: Record<string, string[]> | null;
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

export async function fetchSettings(): Promise<SettingsResponse> {
  const response = (await callMcpTool("settings_get", {})) as SettingsGetToolResponse;
  const parsed = response as ParsedSettingsGet;
  if (parsed.error) {
    throw new Error(parsed.error);
  }

  const modelPriority = parsed.settings?.model_priority ?? {};
  const maxSessions = parsed.settings?.max_sessions ?? {};

  // Collect unique model IDs from all roles; fall back to max_sessions keys
  const seen = new Set<string>();
  const modelIds: string[] = [];
  for (const ids of Object.values(modelPriority ?? {})) {
    for (const id of ids) {
      if (!seen.has(id)) {
        seen.add(id);
        modelIds.push(id);
      }
    }
  }

  if (modelIds.length === 0) {
    for (const id of Object.keys(maxSessions ?? {})) {
      if (!seen.has(id)) {
        seen.add(id);
        modelIds.push(id);
      }
    }
  }

  const models: ModelEntry[] = modelIds.map((modelId) => {
    const { provider, model } = splitModelId(modelId);
    return {
      provider,
      model,
      max_concurrent: (maxSessions ?? {})[modelId] ?? 1,
    };
  });

  const settings = parsed.settings ?? {};
  const langfuse: LangfuseSettings = {
    publicKey: (settings as Record<string, unknown>).langfuse_public_key as string ?? "",
    secretKey: (settings as Record<string, unknown>).langfuse_secret_key as string ?? "",
    endpoint: (settings as Record<string, unknown>).langfuse_endpoint as string ?? "",
  };

  return { models, langfuse };
}

export async function saveSettings(settings: SettingsResponse): Promise<void> {
  const modelIds = settings.models.map((m) => combineModelId(m.provider, m.model));
  const maxSessions = settings.models.reduce<Record<string, number>>((acc, m) => {
    acc[combineModelId(m.provider, m.model)] = m.max_concurrent;
    return acc;
  }, {});

  const response = await callMcpTool("settings_set", {
    model_priority_worker: modelIds,
    model_priority_lead: modelIds,
    model_priority_reviewer: modelIds,
    model_priority_planner: modelIds,
    max_sessions: maxSessions,
    langfuse_public_key: settings.langfuse.publicKey,
    langfuse_secret_key: settings.langfuse.secretKey,
    langfuse_endpoint: settings.langfuse.endpoint,
  });

  if (!response.ok) {
    throw new Error(response.error ?? "Failed to save settings");
  }
}

export async function saveLangfuseSettings(langfuse: LangfuseSettings): Promise<void> {
  const response = await callMcpTool("settings_set", {
    langfuse_public_key: langfuse.publicKey,
    langfuse_secret_key: langfuse.secretKey,
    langfuse_endpoint: langfuse.endpoint,
  });

  if (!response.ok) {
    throw new Error(response.error ?? "Failed to save Langfuse settings");
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
