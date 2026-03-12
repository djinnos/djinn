import { callMcpTool } from "@/api/mcpClient";
import type { McpToolOutput, ProviderModelsConnectedOutputSchema } from "@/api/generated/mcp-tools.gen";

export type AgentRole = "worker" | "task_reviewer" | "conflict_resolver" | "pm" | "groomer";

export interface ModelPriorityItem {
  model: string;
  provider: string;
}

export interface ModelSessionLimit {
  model: string;
  provider: string;
  max_concurrent: number;
  current_active: number;
}

export interface AgentSettings {
  model_priorities: Record<AgentRole, ModelPriorityItem[]>;
  session_limits: ModelSessionLimit[];
}

export interface SettingsResponse {
  agents: AgentSettings;
}

type SettingsGetToolResponse = McpToolOutput<"settings_get">;

interface ParsedSettingsGet {
  settings?: {
    model_priority?: Record<string, string[]>;
    max_sessions?: Record<string, number>;
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

  const toPriorityItems = (values: string[] | undefined): ModelPriorityItem[] =>
    (values ?? []).map((value) => {
      const split = splitModelId(value);
      return {
        provider: split.provider,
        model: split.model,
      };
    });

  const sessionLimits: ModelSessionLimit[] = Object.entries(maxSessions).map(
    ([modelId, maxConcurrent]) => {
      const split = splitModelId(modelId);
      return {
        provider: split.provider,
        model: split.model,
        max_concurrent: maxConcurrent,
        current_active: 0,
      };
    }
  );

  return {
    agents: {
      model_priorities: {
        worker: toPriorityItems(modelPriority.worker),
        task_reviewer: toPriorityItems(modelPriority.task_reviewer),
        conflict_resolver: toPriorityItems(modelPriority.conflict_resolver),
        pm: toPriorityItems(modelPriority.pm),
        groomer: toPriorityItems(modelPriority.groomer),
      },
      session_limits: sessionLimits,
    },
  };
}

export async function saveSettings(settings: SettingsResponse): Promise<void> {
  const maxSessions = settings.agents.session_limits.reduce<Record<string, number>>(
    (acc, item) => {
      acc[combineModelId(item.provider, item.model)] = item.max_concurrent;
      return acc;
    },
    {}
  );

  const response = await callMcpTool("settings_set", {
    model_priority_worker: settings.agents.model_priorities.worker.map((item) =>
      combineModelId(item.provider, item.model)
    ),
    model_priority_task_reviewer: settings.agents.model_priorities.task_reviewer.map((item) =>
      combineModelId(item.provider, item.model)
    ),
    model_priority_conflict_resolver: settings.agents.model_priorities.conflict_resolver.map((item) =>
      combineModelId(item.provider, item.model)
    ),
    model_priority_pm: settings.agents.model_priorities.pm.map((item) =>
      combineModelId(item.provider, item.model)
    ),
    model_priority_groomer: settings.agents.model_priorities.groomer.map((item) =>
      combineModelId(item.provider, item.model)
    ),
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
