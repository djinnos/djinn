import { callMcpTool } from "@/api/mcpClient";
import type { McpToolOutput, ProjectListOutputSchema } from "@/api/generated/mcp-tools.gen";
import { getServerBaseUrl } from "@/api/serverUrl";
import type { Epic, Task } from "@/api/types";
import { projectStore } from "@/stores/projectStore";

async function getBaseUrl(): Promise<string> {
  return getServerBaseUrl();
}

function providerDescription(provider: ProviderCatalogItem): string {
  const tags: string[] = [];
  if (provider.oauth_supported) tags.push("OAuth supported");
  return tags.join(", ");
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
  /** True when the provider was already connected (cached token still valid). */
  success: boolean;
  /** Populated on failure. */
  error?: string;
  /**
   * True when the device-code flow is in progress: the server has handed us
   * a short `user_code` and spawned a background task that polls OpenAI for
   * tokens. The UI renders `verification_uri_complete` as a clickable link
   * (with the code pre-filled) and waits for a `credential.updated` SSE
   * event to confirm completion.
   */
  pending?: boolean;
  /** Short human-typable code (e.g. `ABCD-1234`). */
  user_code?: string;
  /** Bare URL where the user enters the code manually. */
  verification_uri?: string;
  /** Convenience URL with `user_code` pre-filled as a query string. */
  verification_uri_complete?: string;
  /** Recommended polling interval (seconds). Informational. */
  interval?: number;
  /** Max time the user has to complete sign-in (seconds). */
  expires_in?: number;
};

export async function startProviderOAuth(
  providerId: string,
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
        verification_uri_complete: result.verification_uri_complete ?? undefined,
        interval: typeof result.interval === "number" ? result.interval : undefined,
        expires_in: typeof result.expires_in === "number" ? result.expires_in : undefined,
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

// ── GitHub-origin projects (Migration 2) ─────────────────────────────────────

export interface GithubRepoEntry {
  owner: string;
  repo: string;
  default_branch: string;
  private: boolean;
  description?: string | null;
  /**
   * GitHub App installation id that surfaced this repo. Pass back to
   * `project_add_from_github` so the server can scope the clone to the
   * same installation without re-scanning.
   */
  installation_id: number;
  /** Login of the account (user or org) the installation is scoped to. */
  account_login: string;
}

/**
 * A single GitHub App installation the authenticated user can act as.
 * Returned by {@link listGithubInstallations}.
 */
export interface Installation {
  id: number;
  accountLogin: string;
  accountType: string;
  installUrl: string;
}

/**
 * List GitHub repositories the Djinn App can access. Backs the Add-Project
 * picker after Migration 2: the server owns cloning, so the desktop no
 * longer passes a local path.
 */
export async function listGithubRepos(perPage = 50): Promise<GithubRepoEntry[]> {
  const response = await callMcpTool("github_list_repos", { per_page: perPage });
  if (response.status.startsWith("error")) {
    throw new Error(response.status.replace(/^error:\s*/, ""));
  }
  return (response.repos ?? []) as GithubRepoEntry[];
}

/**
 * List GitHub App installations reachable for the signed-in user.
 *
 * Returns the installations plus an `installUrl` the UI can deep-link to
 * when the user has no installations (or wants another one). `installUrl`
 * is only populated server-side when `GITHUB_APP_SLUG` is configured.
 */
export async function listGithubInstallations(): Promise<{
  installations: Installation[];
  installUrl: string | null;
}> {
  const response = await callMcpTool("github_app_installations", {});
  if (response.status.startsWith("error")) {
    throw new Error(response.status.replace(/^error:\s*/, ""));
  }
  const installations: Installation[] = (response.installations ?? []).map((entry) => ({
    id: entry.id,
    accountLogin: entry.account_login,
    accountType: entry.account_type,
    installUrl: entry.install_url,
  }));
  return {
    installations,
    installUrl: response.install_url ?? null,
  };
}

/**
 * Fetch a standalone install URL for the Djinn GitHub App. Use this to
 * power a bare "Install Djinn" button that doesn't care whether the user
 * currently has installations. Throws when the server has no
 * `GITHUB_APP_SLUG` configured (i.e. no public install URL).
 */
export async function getGithubInstallUrl(): Promise<string> {
  const response = await callMcpTool("github_app_install_url", {});
  if (response.status.startsWith("error")) {
    throw new Error(response.status.replace(/^error:\s*/, ""));
  }
  if (!response.url) {
    throw new Error("GitHub App install URL is not configured on the server");
  }
  return response.url;
}

/**
 * Ask the server to clone a GitHub repo and register it as a project.
 * The server clones into `/root/.djinn/projects/{owner}/{repo}` (persisted
 * on the host via the `~/.djinn` bind mount) and returns the project record.
 */
export async function addProjectFromGithub(args: {
  owner: string;
  repo: string;
  name?: string;
  ref?: string;
  installation_id?: number;
}): Promise<Project> {
  const response = await callMcpTool("project_add_from_github", {
    owner: args.owner,
    repo: args.repo,
    ...(args.name ? { name: args.name } : {}),
    ...(args.ref ? { ref: args.ref } : {}),
    ...(args.installation_id !== undefined ? { installation_id: args.installation_id } : {}),
  });

  if (response.status.startsWith("error")) {
    throw new Error(response.status.replace(/^error:\s*/, ""));
  }

  return response.project;
}

/**
 * List local git branches for a project's server-owned clone. Used by the
 * Titlebar branch picker to avoid the blind "type anything" UX.
 */
export async function fetchProjectBranches(
  projectId: string,
): Promise<{ branches: string[]; current: string | null }> {
  const response = await callMcpTool("project_branches", { project_id: projectId });
  if (response.status.startsWith("error")) {
    throw new Error(response.status.replace(/^error:\s*/, ""));
  }
  return {
    branches: response.branches ?? [],
    current: response.current ?? null,
  };
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
