import { callMcpTool } from "@/api/mcpClient";
import type { McpToolOutput, ProjectListOutputSchema } from "@/api/generated/mcp-tools.gen";
import { getServerBaseUrl } from "@/api/serverUrl";
import type { Epic, Task } from "@/api/types";
import { projectStore } from "@/stores/projectStore";
import { taskStore } from "@/stores/taskStore";
import { mergedColumnStore } from "@/stores/mergedColumnStore";

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
  /**
   * Set when the provider's stored credential was rejected (a 401 during a run)
   * and marked revoked server-side. The provider is reported disconnected and
   * this reason is shown so the user reconnects. Persisted, so it survives a
   * page reload (not just a live SSE event).
   */
  revoked_reason?: string;
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
    revoked_reason:
      (provider as unknown as { revoked_reason?: string | null }).revoked_reason ?? undefined,
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
        const config = await callMcpTool("project_config_get", {
          project: `${p.github_owner}/${p.github_repo}`,
        });
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
 * The server clones into `$DJINN_HOME/projects/{owner}/{repo}` (mounted at
 * `/var/lib/djinn/projects` under the Helm chart, bind-mounted to
 * `~/.djinn/projects` under docker-compose) and returns the project record.
 */
export interface AddProjectFromGithubArgs {
  owner: string;
  repo: string;
  name?: string;
  ref?: string;
  installation_id?: number;
}

export function githubRepoToProjectArgs(entry: GithubRepoEntry): AddProjectFromGithubArgs {
  return {
    owner: entry.owner,
    repo: entry.repo,
    ref: entry.default_branch || undefined,
    installation_id: entry.installation_id,
  };
}

export async function addProjectFromGithub(args: AddProjectFromGithubArgs): Promise<Project> {
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
  const slug = `${project.github_owner}/${project.github_repo}`;

  const configCalls: Promise<unknown>[] = [];
  if (updates.branch !== undefined) {
    configCalls.push(callMcpTool("project_config_set", {
      project: slug,
      key: "target_branch",
      value: updates.branch,
    }));
  }
  if (updates.auto_merge !== undefined) {
    configCalls.push(callMcpTool("project_config_set", {
      project: slug,
      key: "auto_merge",
      value: String(updates.auto_merge),
    }));
  }
  await Promise.all(configCalls);
}

export async function removeProject(projectId: string): Promise<void> {
  await callMcpTool("project_remove", {
    project: projectId,
  });
}

const KANBAN_PAGE_SIZE = 200;

/** Resolve a project slug (`owner/repo`) to its stable project id. */
function projectIdForSlug(slug: string): string | null {
  return (
    projectStore
      .getState()
      .projects.find((p) => `${p.github_owner}/${p.github_repo}` === slug)?.id ??
    null
  );
}

/**
 * Fetch every page of `task_list` for a project honoring the given filters.
 *
 * The first page is fetched serially; if the server reports more, the remaining
 * pages are fetched in parallel using `total_count` (rather than the old serial
 * `has_more` walk) so a large backlog paints in one round-trip's worth of
 * latency instead of N.
 */
async function fetchAllTaskPages(
  slug: string,
  params: Record<string, unknown>,
): Promise<Task[]> {
  const first = await callMcpTool("task_list", {
    project: slug,
    limit: KANBAN_PAGE_SIZE,
    offset: 0,
    ...params,
  });
  const tasks: Task[] = [...(first.tasks as unknown as Task[])];
  if (first.has_more) {
    const total = first.total_count;
    const offsets: number[] = [];
    for (let o = tasks.length; o < total; o += KANBAN_PAGE_SIZE) offsets.push(o);
    const rest = await Promise.all(
      offsets.map((offset) =>
        callMcpTool("task_list", {
          project: slug,
          limit: KANBAN_PAGE_SIZE,
          offset,
          ...params,
        }),
      ),
    );
    for (const page of rest) tasks.push(...(page.tasks as unknown as Task[]));
  }
  return tasks;
}

/**
 * Fetch every page of `epic_list` for a project. The server caps epic_list at
 * ~200 per page (a higher requested limit is clamped), so without this walk the
 * *newest* epics fall off the first page — and since the default sort is
 * created-asc, those are the currently-active ones, whose tasks then render
 * under "Unknown Epic" once the closed-epic count grows past one page.
 *
 * Kept as a serial `has_more` walk (unlike the parallel task pagination): the
 * epic_list schema types `total_count` as optional, so a `total`-driven
 * parallel fan-out has no reliable page bound.
 */
async function fetchAllEpicPages(slug: string): Promise<Epic[]> {
  const first = await callMcpTool("epic_list", {
    project: slug,
    limit: KANBAN_PAGE_SIZE,
    offset: 0,
  });
  const epics: Epic[] = [...((first.epics ?? []) as unknown as Epic[])];
  if (first.has_more) {
    let offset = epics.length;
    while (true) {
      const page = await callMcpTool("epic_list", {
        project: slug,
        limit: KANBAN_PAGE_SIZE,
        offset,
      });
      epics.push(...(page.epics as unknown as Epic[]));
      if (!page.has_more) break;
      offset += (page.epics as unknown[]).length;
    }
  }
  return epics;
}

export interface ActiveSnapshot {
  /** Non-closed tasks (Open / In Progress / PR Ready columns). */
  tasks: Task[];
  epics: Epic[];
}

/**
 * Phase 1 of Kanban hydration: fetch the ACTIVE (non-closed) tasks plus all
 * epics across the given projects. This is the fast paint — active tasks are
 * few (~dozens) so the board renders without waiting on the (potentially
 * thousands of) closed/merged rows.
 */
export async function fetchActiveSnapshot(
  slugs: string[],
): Promise<ActiveSnapshot> {
  if (slugs.length === 0) return { tasks: [], epics: [] };
  const perProject = await Promise.all(
    slugs.map(async (slug) => {
      const projectId = projectIdForSlug(slug);
      const [tasks, epics] = await Promise.all([
        fetchAllTaskPages(slug, { status: "!closed" }),
        fetchAllEpicPages(slug),
      ]);
      return {
        tasks: tasks.map((t) => ({ ...t, project_id: projectId })),
        epics,
      };
    }),
  );
  return {
    tasks: perProject.flatMap((p) => p.tasks),
    epics: perProject.flatMap((p) => p.epics),
  };
}

/**
 * Fetch a single page of MERGED tasks for a project (sorted newest-closed
 * first) and record the pagination cursor in {@link mergedColumnStore}.
 *
 * Uses the backend `status=merged` pseudo-filter (closed AND actually merged),
 * so every returned row lands in the Merged column and `total_count` is the
 * exact merged total. Closed-but-not-merged tasks (force-closed / review /
 * spike) are never fetched — `taskToColumnKey` hid them anyway.
 */
async function fetchClosedPageForProject(
  slug: string,
  offset: number,
): Promise<Task[]> {
  const projectId = projectIdForSlug(slug);
  const page = await callMcpTool("task_list", {
    project: slug,
    status: "merged",
    sort: "closed",
    limit: KANBAN_PAGE_SIZE,
    offset,
  });
  const tasks = (page.tasks as unknown as Task[]).map((t) => ({
    ...t,
    project_id: projectId,
  }));
  mergedColumnStore.getState().setProjectPage({
    slug,
    projectId,
    loaded: offset + tasks.length,
    hasMore: page.has_more,
    totalMerged: page.total_count,
  });
  return tasks;
}

/**
 * Phase 2 of Kanban hydration: fetch the FIRST page of merged tasks for every
 * project. Resets the merged-column pagination cursors first, so a reconnect
 * re-hydration cleanly drops any extra pages that were previously loaded (the
 * task store is keyed by id, so re-adding page 1 is idempotent).
 */
export async function fetchClosedFirstPage(slugs: string[]): Promise<Task[]> {
  mergedColumnStore.getState().reset();
  if (slugs.length === 0) return [];
  const perProject = await Promise.all(
    slugs.map((slug) => fetchClosedPageForProject(slug, 0)),
  );
  return perProject.flatMap((tasks) => tasks);
}

/**
 * "Load more" for the Merged column: fetch the next closed page for every
 * project that still has more, append the rows into the task store, and return
 * the newly-loaded tasks. Idempotent and safe to call repeatedly; a no-op when
 * nothing is left to load.
 */
export async function loadMoreClosedTasks(): Promise<Task[]> {
  const pending = mergedColumnStore.getState().projectsWithMore();
  if (pending.length === 0) return [];
  mergedColumnStore.getState().setLoadingMore(true);
  try {
    const results = await Promise.all(
      pending.map((p) => fetchClosedPageForProject(p.slug, p.loaded)),
    );
    const tasks = results.flatMap((t) => t);
    taskStore.getState().upsertTasks(tasks);
    return tasks;
  } finally {
    mergedColumnStore.getState().setLoadingMore(false);
  }
}

/**
 * Cap on server-side search results per project. The board's search box only
 * needs to *surface* matches that aren't already loaded (chiefly old merged
 * tasks): one page of matches per project is plenty, and we deliberately do NOT
 * paginate exhaustively — an unbounded fan-out over a project with thousands of
 * closed tasks would defeat the whole active-first load. A query specific enough
 * to matter matches far fewer than this.
 */
export const SEARCH_RESULT_CAP = 200;

/**
 * Backend-backed board search: for every project, run `task_list` with the
 * case-insensitive `text` filter and additively merge the matches into the task
 * store. This is what lets the search box match tasks the board never loaded
 * (e.g. merged tasks beyond the first Merged-column page).
 *
 * Notes:
 * - All statuses are searched (not just closed). Active tasks are already
 *   loaded, but the store is keyed by id so re-upserting them is a cheap no-op;
 *   searching everything keeps the semantics simple ("find any matching task").
 * - The Merged-column pagination cursors ({@link mergedColumnStore}) are left
 *   untouched: search-surfaced closed tasks render in the Merged column out of
 *   cursor order (fine), while "Load more" keeps advancing from its own offsets.
 * - Results are capped at {@link SEARCH_RESULT_CAP} per project (single page).
 */
export async function searchTasksAcrossProjects(
  slugs: string[],
  query: string,
): Promise<Task[]> {
  const trimmed = query.trim();
  if (slugs.length === 0 || trimmed.length === 0) return [];
  const perProject = await Promise.all(
    slugs.map(async (slug) => {
      const projectId = projectIdForSlug(slug);
      const page = await callMcpTool("task_list", {
        project: slug,
        text: trimmed,
        limit: SEARCH_RESULT_CAP,
        offset: 0,
      });
      return (page.tasks as unknown as Task[]).map((t) => ({
        ...t,
        project_id: projectId,
      }));
    }),
  );
  const tasks = perProject.flatMap((t) => t);
  taskStore.getState().upsertTasks(tasks);
  return tasks;
}
