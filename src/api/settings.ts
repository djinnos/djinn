import { callMcpTool } from "@/api/mcpClient";

export type AgentRole = "worker" | "task_reviewer" | "epic_reviewer";

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

interface SettingsGetToolResponse {
  settings?: {
    model_priority?: Record<string, string[]>;
    max_sessions?: Record<string, number>;
  };
  error?: string;
}

interface SettingsSetToolResponse {
  ok: boolean;
  error?: string;
}

interface ProviderModelsConnectedResponse {
  models: Array<{
    id: string;
    name: string;
    provider_id: string;
  }>;
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
  const response = await callMcpTool<SettingsGetToolResponse>("settings_get", {});
  if (response.error) {
    throw new Error(response.error);
  }

  const modelPriority = response.settings?.model_priority ?? {};
  const maxSessions = response.settings?.max_sessions ?? {};

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
        epic_reviewer: toPriorityItems(modelPriority.conflict_resolver),
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

  const response = await callMcpTool<SettingsSetToolResponse>("settings_set", {
    model_priority_worker: settings.agents.model_priorities.worker.map((item) =>
      combineModelId(item.provider, item.model)
    ),
    model_priority_task_reviewer: settings.agents.model_priorities.task_reviewer.map((item) =>
      combineModelId(item.provider, item.model)
    ),
    model_priority_conflict_resolver: settings.agents.model_priorities.epic_reviewer.map((item) =>
      combineModelId(item.provider, item.model)
    ),
    max_sessions: maxSessions,
  });

  if (!response.ok) {
    throw new Error(response.error ?? "Failed to save settings");
  }
}

export interface ProviderModel {
  id: string;
  name: string;
  provider: string;
}

export async function fetchProviderModels(): Promise<ProviderModel[]> {
  const response = await callMcpTool<ProviderModelsConnectedResponse>("provider_models_connected");
  return response.models.map((model) => ({
    id: model.id,
    name: model.name,
    provider: model.provider_id,
  }));
}
