import { callMcpTool } from "@/api/mcpClient";
import type { McpToolOutput, ProjectListOutputSchema } from "@/api/generated/mcp-tools.gen";
import { getServerPort } from "@/electron/commands";
import type { Epic, Task } from "@/api/types";
import { projectStore } from "@/stores/projectStore";

/**
 * Notify the server of the authenticated user's identity.
 * Called after every successful Clerk auth (login, silent refresh, token rotation).
 */
export async function setUserIdentity(email: string, userId: string): Promise<void> {
  try {
    // Tool removed from server — kept as no-op for call-site compatibility
    void email; void userId;
  } catch (e) {
    console.warn("Failed to set user identity on server:", e);
  }
}

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

export type OAuthResult = {
  success: boolean;
  error?: string;
  /** For device-code flows: code the user must enter. */
  user_code?: string;
  /** For device-code flows: URL where the user enters the code. */
  verification_uri?: string;
  /** True when the flow is pending (device-code polling in background). */
  pending?: boolean;
};

export async function startProviderOAuth(
  providerId: string
): Promise<OAuthResult> {
  try {
    const result = await callMcpTool("provider_oauth_start", {
      provider_id: providerId,
    });
    if (!result.ok) {
      return { success: false, error: result.error ?? "OAuth flow failed" };
    }
    if (result.pending) {
      return {
        success: false,
        pending: true,
        user_code: result.user_code ?? undefined,
        verification_uri: result.verification_uri ?? undefined,
      };
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

export type Project = ProjectListOutputSchema.ProjectInfo & {
  branch?: string;
  auto_merge?: boolean;
  created_at?: string;
  updated_at?: string;
};

export async function fetchProjects(): Promise<Project[]> {
  const data = await callMcpTool("project_list");
  const projects: Project[] = await Promise.all(
    data.projects.map(async (p) => {
      try {
        const config = await callMcpTool("project_config_get", { project: p.path });
        return { ...p, branch: config.target_branch, auto_merge: config.auto_merge };
      } catch {
        return p;
      }
    }),
  );
  return projects;
}

export async function addProject(path: string): Promise<Project> {
  const segments = path.split(/[\\/]/).filter(Boolean);
  const inferredName = segments[segments.length - 1] ?? "project";
  const response = await callMcpTool("project_add", {
    name: inferredName,
    path,
  });

  if (response.status.startsWith("error")) {
    throw new Error(response.status.replace(/^error:\s*/, ""));
  }

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

  const configCalls: Promise<unknown>[] = [];
  if (updates.branch !== undefined) {
    configCalls.push(callMcpTool("project_config_set", {
      project: project.path,
      key: "target_branch",
      value: updates.branch,
    }));
  }
  if (updates.auto_merge !== undefined) {
    configCalls.push(callMcpTool("project_config_set", {
      project: project.path,
      key: "auto_merge",
      value: String(updates.auto_merge),
    }));
  }
  await Promise.all(configCalls);
}

// Project commands — tools removed from server (now in .djinn/settings.json)
// Kept as stubs for call-site compatibility.

export interface ProjectCommandSpec {
  name: string;
  command: string;
  timeout_secs?: number;
}

export interface ProjectCommands {
  setup_commands: ProjectCommandSpec[];
  verification_commands: ProjectCommandSpec[];
}

export interface CommandValidationError {
  command_name: string;
  exit_code: number;
  stderr: string;
  stdout: string;
}

export async function fetchProjectCommands(_projectPath: string): Promise<ProjectCommands> {
  // Tools removed — settings.json is now source of truth
  return { setup_commands: [], verification_commands: [] };
}

export async function saveProjectCommands(
  _projectPath: string,
  _commands: Partial<ProjectCommands>,
): Promise<CommandValidationError[]> {
  // Tool removed — settings.json is now source of truth
  return [];
}

export async function removeProject(projectId: string): Promise<void> {
  const projects = await callMcpTool("project_list");
  const project = projects.projects.find((entry) => entry.id === projectId);
  if (!project) {
    throw new Error("Project not found");
  }

  await callMcpTool("project_remove", {
    name: project.name,
    path: project.path,
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

/** Fetch tasks + epics for a single project, or aggregate across all projects. */
export async function fetchKanbanSnapshot(
  projectPath?: string | null,
  allProjectPaths?: string[],
): Promise<KanbanSnapshot> {
  // All-projects mode: fetch from every project and merge
  if (allProjectPaths && allProjectPaths.length > 0) {
    const snapshots = await Promise.all(
      allProjectPaths.map((path) => fetchKanbanSnapshot(path))
    );
    return {
      projectPath: null,
      tasks: snapshots.flatMap((s) => s.tasks),
      epics: snapshots.flatMap((s) => s.epics),
    };
  }

  const resolvedProjectPath = projectPath ?? null;

  if (!resolvedProjectPath) {
    return { projectPath: null, tasks: [], epics: [] };
  }

  // Fetch first page of tasks + all epics in parallel
  const PAGE_SIZE = 200;
  const [firstTaskPage, epicList] = await Promise.all([
    callMcpTool("task_list", {
      project: resolvedProjectPath,
      limit: PAGE_SIZE,
      offset: 0,
    }),
    callMcpTool("epic_list", {
      project: resolvedProjectPath,
      limit: 500,
      offset: 0,
    }),
  ]);

  // Paginate remaining tasks if the server has more (server caps at ~200 per page)
  const allTasks: Task[] = [...(firstTaskPage.tasks as unknown as Task[])];
  if (firstTaskPage.has_more) {
    let offset = allTasks.length;
     
    while (true) {
      const page = await callMcpTool("task_list", {
        project: resolvedProjectPath,
        limit: PAGE_SIZE,
        offset,
      });
      allTasks.push(...(page.tasks as unknown as Task[]));
      if (!page.has_more) break;
      offset += (page.tasks as unknown[]).length;
    }
  }

  // Stamp each task with the project it belongs to (needed for all-projects view)
  const projectId = projectStore.getState().projects.find((p) => p.path === resolvedProjectPath)?.id ?? null;
  const tasks = allTasks.map((t) => ({ ...t, project_id: projectId }));

  return {
    projectPath: resolvedProjectPath,
    tasks,
    epics: (epicList.epics ?? []) as unknown as Epic[],
  };
}
