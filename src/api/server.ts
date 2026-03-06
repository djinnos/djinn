import { callMcpTool } from "@/api/mcpClient";
import type { McpToolOutput } from "@/api/generated/mcp-tools.gen";
import { getServerPort } from "@/tauri/commands";
import type { Epic, EpicStatus, Task, TaskPriority, TaskStatus } from "@/types";

async function getBaseUrl(): Promise<string> {
  const port = await getServerPort();
  return `http://127.0.0.1:${port}`;
}

function providerDescription(provider: ProviderCatalogItem): string {
  const tags: string[] = [];
  if (provider.is_openai_compatible) tags.push("openai-compatible");
  if (provider.oauth_supported) tags.push("oauth");
  const suffix = tags.length > 0 ? ` (${tags.join(", ")})` : "";
  return `${provider.npm}${suffix}`;
}

function fallbackKeyName(providerId: string): string {
  const normalized = providerId.replace(/[^a-zA-Z0-9]/g, "_").toUpperCase();
  return `${normalized}_API_KEY`;
}

async function listProviderCatalogRaw(): Promise<ProviderCatalogItem[]> {
  const response = await callMcpTool("provider_catalog");
  return response.providers;
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
export type TaskListMcpResponse = McpToolOutput<"task_list">;
export type EpicListMcpResponse = McpToolOutput<"epic_list">;

export async function fetchProviderCatalog(): Promise<Provider[]> {
  const providers = await listProviderCatalogRaw();
  return providers.map((provider) => ({
    id: provider.id,
    name: provider.name,
    description: providerDescription(provider),
    requires_api_key: provider.env_vars.length > 0,
  }));
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


export async function deleteProviderCredentials(providerId: string): Promise<void> {
  const credentials = await callMcpTool("credential_list");
  const keys = credentials.credentials
    .filter((credential) => credential.provider_id === providerId)
    .map((credential) => credential.key_name);

  if (keys.length === 0) {
    return;
  }

  const results = await Promise.all(
    keys.map((keyName) =>
      callMcpTool("credential_delete", {
        key_name: keyName,
      })
    )
  );

  const failed = results.find((result) => !result.success || !result.deleted);
  if (failed) {
    throw new Error(failed.error ?? "Failed to delete credentials");
  }
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

  return response.json();
}

export function mapPriority(priority: number): TaskPriority {
  if (priority <= 0) return "P0";
  if (priority === 1) return "P1";
  if (priority === 2) return "P2";
  return "P3";
}

export function mapTaskStatus(status: string): TaskStatus {
  if (status === "in_progress") return "in_progress";
  if (status === "closed") return "completed";
  return "pending";
}

export function mapEpicStatus(status: string): EpicStatus {
  if (status === "closed") return "completed";
  if (status === "archived") return "archived";
  return "active";
}

export function mapTaskFromMcp(task: TaskListMcpResponse["tasks"][number]): Task {
  return {
    id: task.id,
    shortId: task.short_id,
    title: task.title,
    description: task.description,
    design: task.design ?? "",
    acceptanceCriteria: (task.acceptance_criteria ?? []).map((raw) => {
      let item: any = raw;
      if (typeof item === "string") {
        try { item = JSON.parse(item); } catch { /* keep as plain string */ }
      }
      if (typeof item === "string") {
        return { criterion: item, met: false };
      }
      return {
        criterion: item.criterion ?? item.description ?? item.text ?? "",
        met: Boolean(item.met),
      };
    }),
    activity: [],
    status: mapTaskStatus(task.status),
    reviewPhase: task.status === "needs_task_review" || task.status === "in_task_review" ? task.status : undefined,
    priority: mapPriority(task.priority),
    epicId: task.epic_id ?? null,
    labels: task.labels,
    owner: task.owner || null,
    createdAt: task.created_at,
    updatedAt: task.updated_at,
    sessionModelId: task.active_session?.model_id ?? undefined,
    sessionCount: typeof task.session_count === "number" ? task.session_count : undefined,
    trackedSeconds: typeof task.duration_seconds === "number" ? task.duration_seconds : undefined,
    activeSessionStartedAt: task.active_session?.started_at ?? null,
    reopenCount: typeof task.reopen_count === "number" ? task.reopen_count : undefined,
    unresolvedBlockerCount: typeof task.unresolved_blocker_count === "number" ? task.unresolved_blocker_count : undefined,
  };
}

export function mapEpicFromMcp(epic: NonNullable<EpicListMcpResponse["epics"]>[number]): Epic {
  return {
    id: epic.id,
    title: epic.title,
    description: epic.description,
    status: mapEpicStatus(epic.status),
    priority: "P2",
    labels: [],
    owner: epic.owner || null,
    createdAt: epic.created_at,
    updatedAt: epic.updated_at,
  };
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
    tasks: taskList.tasks.map(mapTaskFromMcp),
    epics: (epicList.epics ?? []).map(mapEpicFromMcp),
  };
}

