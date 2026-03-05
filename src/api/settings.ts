import { getServerPort } from "@/tauri/commands";

async function getBaseUrl(): Promise<string> {
  const port = await getServerPort();
  return `http://127.0.0.1:${port}`;
}

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

export async function fetchSettings(): Promise<SettingsResponse> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/settings/get`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
    },
    body: JSON.stringify({}),
  });
  
  if (!response.ok) {
    throw new Error(`Failed to fetch settings: ${response.status}`);
  }
  
  return response.json();
}

export async function saveSettings(settings: SettingsResponse): Promise<void> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/settings/set`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
    },
    body: JSON.stringify(settings),
  });
  
  if (!response.ok) {
    throw new Error(`Failed to save settings: ${response.status}`);
  }
}

export interface ProviderModel {
  id: string;
  name: string;
  provider: string;
}

export async function fetchProviderModels(): Promise<ProviderModel[]> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/providers/models`, {
    method: "GET",
    headers: {
      "Content-Type": "application/json",
    },
  });
  
  if (!response.ok) {
    throw new Error(`Failed to fetch provider models: ${response.status}`);
  }
  
  return response.json();
}
