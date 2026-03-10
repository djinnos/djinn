import { callMcpTool } from "@/api/mcpClient";
import type { McpToolOutput } from "@/api/generated/mcp-tools.gen";
import { getServerPort } from "@/tauri/commands";
import type { Epic, Task } from "@/api/types";

async function getBaseUrl(): Promise<string> {
  const port = await getServerPort();
  return `http://127.0.0.1:${port}`;
}

function providerDescription(provider: ProviderCatalogItem): string {
  const tags: string[] = [];
  if (provider.oauth_supported) tags.push("OAuth supported");
  return tags.join(", ");
}

function fallbackKeyName(providerId: string): string {
  const normalized = providerId.replace(/[^a-zA-Z0-9]/g, "_").toUpperCase();
  return `${normalized}_API_KEY`;
}

let catalogCache: ProviderCatalogItem[] | null = null;

async function listProviderCatalogRaw(): Promise<ProviderCatalogItem[]> {
  if (catalogCache) return catalogCache;
  const response = await callMcpTool("provider_catalog");
  catalogCache = response.providers;
  return catalogCache;
}

export function invalidateProviderCatalogCache(): void {
  catalogCache = null;
}

async function resolveKeyName(providerId: string): Promise<string> {
  const providers = await listProviderCatalogRaw();
  const match = providers.find((provider) => provider.id === providerId);
  return match?.env_vars[0] ?? fallbackKeyName(providerId);
}

export async function checkServerHealth(): Promise<{ status: "ok" }> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/health`);
  if (!response.ok) {
    throw new Error(`Health check failed: ${response.status}`);
  }
  return response.json();
}

export interface Provider {
  id: string;
  name: string;
  description: string;
  requires_api_key: boolean;
  oauth_supported: boolean;
  connection_methods: string[];
}

export interface CustomProviderRequest {
  name: string;
  base_url?: string;
}

export interface ProviderCredential {
  provider_id: string;
  configured: boolean;
  valid: boolean;
  api_key_masked?: string;
}

type ProviderCatalogResponse = McpToolOutput<"provider_catalog">;
type ProviderCatalogItem = ProviderCatalogResponse["providers"][number];

export async function fetchProviderCatalog(): Promise<Provider[]> {
  const providers = await listProviderCatalogRaw();
  return providers.map((provider) => ({
    id: provider.id,
    name: provider.name,
    description: providerDescription(provider),
    requires_api_key: provider.env_vars.length > 0,
    oauth_supported: provider.oauth_supported,
    connection_methods: provider.connection_methods,
  }));
}

export async function startProviderOAuth(
  providerId: string
): Promise<{ success: boolean; error?: string }> {
  try {
    const result = await callMcpTool("provider_oauth_start", {
      provider_id: providerId,
    });
    if (!result.ok || !result.success) {
      return { success: false, error: result.error ?? "OAuth flow failed" };
    }
    return { success: true };
  } catch (error) {
    return {
      success: false,
      error: error instanceof Error ? error.message : "OAuth flow failed",
    };
  }
}

export async function validateProviderApiKey(
  providerId: string,
  apiKey: string
): Promise<{ valid: boolean; error?: string }> {
  try {
    const providers = await listProviderCatalogRaw();
    const provider = providers.find((entry) => entry.id === providerId);
    if (!provider) {
      return { valid: false, error: `Unknown provider: ${providerId}` };
    }

    const result = await callMcpTool("provider_validate", {
      provider_id: providerId,
      base_url: provider.base_url,
      api_key: apiKey,
    });

    if (!result.ok) {
      return { valid: false, error: result.error ?? "Validation failed" };
    }

    return { valid: true };
  } catch (error) {
    return {
      valid: false,
      error: error instanceof Error ? error.message : "Validation failed",
    };
  }
}

export async function saveProviderCredentials(
  providerId: string,
  apiKey: string
): Promise<void> {
  const keyName = await resolveKeyName(providerId);
  const response = await callMcpTool("credential_set", {
    provider_id: providerId,
    key_name: keyName,
    api_key: apiKey,
  });

  if (!response.success) {
    throw new Error(response.error ?? "Failed to save credentials");
  }
}

// Project-related types and API functions

export interface Project {
  id: string;
  name: string;
  path: string;
  branch?: string;
  auto_merge?: boolean;
  created_at?: string;
  updated_at?: string;
}

export async function fetchProjects(): Promise<Project[]> {
  const data = await callMcpTool("project_list");
  return data.projects.map((project) => ({
    id: project.id,
    name: project.name,
    path: project.path,
  }));
}

export async function addProject(path: string): Promise<Project> {
  const segments = path.split(/[\\/]/).filter(Boolean);
  const inferredName = segments[segments.length - 1] ?? "project";
  const response = await callMcpTool("project_add", {
    name: inferredName,
    path,
  });

  return response.project;
}

// Provider configuration check

export interface ProviderConfigStatus {
  configured: boolean;
  providers: string[];
}

export async function fetchProviderConfigStatus(): Promise<ProviderConfigStatus> {
  const response = await callMcpTool("provider_connected");
  const providers = response.providers.map((provider) => provider.id);
  return {
    configured: providers.length > 0,
    providers,
  };
}


export async function fetchCredentialList(): Promise<ProviderCredential[]> {
  const [credentials, connected] = await Promise.all([
    callMcpTool("credential_list"),
    callMcpTool("provider_connected"),
  ]);

  const connectedProviders = new Set(connected.providers.map((provider) => provider.id));

  const byProvider = new Map<string, ProviderCredential>();

  for (const credential of credentials.credentials) {
    byProvider.set(credential.provider_id, {
      provider_id: credential.provider_id,
      configured: true,
      valid: connectedProviders.has(credential.provider_id),
    });
  }

  for (const providerId of connectedProviders) {
    if (!byProvider.has(providerId)) {
      byProvider.set(providerId, {
        provider_id: providerId,
        configured: true,
        valid: true,
      });
    }
  }

  return Array.from(byProvider.values());
}


export async function removeProviderFull(providerId: string): Promise<void> {
  const result = await callMcpTool("provider_remove", { provider_id: providerId });
  if (!result.ok || !result.success) {
    throw new Error(result.error ?? "Failed to remove provider");
  }
}


export async function updateProject(projectId: string, updates: { branch?: string; auto_merge?: boolean }): Promise<void> {
  const projects = await callMcpTool("project_list");
  const project = projects.projects.find((entry) => entry.id === projectId);
  if (!project) {
    throw new Error("Project not found");
  }

  await callMcpTool("project_add", {
    name: project.name,
    path: project.path,
    branch: updates.branch ?? project.branch,
    auto_merge: updates.auto_merge ?? project.auto_merge,
  });
}

export async function removeProject(projectId: string): Promise<void> {
  const projects = await callMcpTool("project_list");
  const project = projects.projects.find((entry) => entry.id === projectId);
  if (!project) {
    throw new Error("Project not found");
  }

  await callMcpTool("project_remove", {
    name: project.name,
  });
}

export async function addCustomProvider(payload: CustomProviderRequest): Promise<Provider> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/providers/add_custom`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
    },
    body: JSON.stringify(payload),
  });

  if (!response.ok) {
    const error = await response.text();
    throw new Error(`Failed to add custom provider: ${error || response.status}`);
  }

  invalidateProviderCatalogCache();
  return response.json();
}

export interface KanbanSnapshot {
  projectPath: string | null;
  tasks: Task[];
  epics: Epic[];
}

export async function fetchKanbanSnapshot(projectPath?: string | null): Promise<KanbanSnapshot> {
  const resolvedProjectPath = projectPath ?? null;

  if (!resolvedProjectPath) {
    return { projectPath: null, tasks: [], epics: [] };
  }

  const [taskList, epicList] = await Promise.all([
    callMcpTool("task_list", {
      project: resolvedProjectPath,
      issue_type: "!epic",
      limit: 500,
      offset: 0,
    }),
    callMcpTool("epic_list", {
      project: resolvedProjectPath,
      limit: 500,
      offset: 0,
    }),
  ]);

  return {
    projectPath: resolvedProjectPath,
    tasks: taskList.tasks as unknown as Task[],
    epics: (epicList.epics ?? []) as unknown as Epic[],
  };
}
