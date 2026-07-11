/**
 * Devcontainer / stack MCP tool wrappers (Phase 3 PR 6).
 *
 * The three underlying MCP tools (`get_project_stack`,
 * `get_project_devcontainer_status`, `retrigger_image_build`) are
 * type-checked against the generated `mcp-tools.gen.ts`.
 */
import { callMcpTool } from "@/api/mcpClient";

/**
 * Minimal shape of the persisted stack JSON. Intentionally narrow — the
 * banner only needs `manifest_signals`; the full schema is produced by
 * `djinn_stack::Stack` on the server side.
 */
export interface StackManifestSignals {
  has_package_json?: boolean;
  has_cargo_toml?: boolean;
  has_pyproject_toml?: boolean;
  has_go_mod?: boolean;
  has_pnpm_workspace?: boolean;
  has_turbo_json?: boolean;
}

export interface Stack {
  detected_at?: string;
  primary_language?: string | null;
  languages?: Array<{ name: string; bytes: number; pct: number }>;
  package_managers?: string[];
  monorepo_tools?: string[];
  is_monorepo?: boolean;
  test_runners?: string[];
  frameworks?: string[];
  manifest_signals?: StackManifestSignals;
  [key: string]: unknown;
}

// Narrow response shape mirroring the Rust `GetProjectStackResponse`.
export interface GetProjectStackResponse {
  stack?: Stack | null;
  error?: string | null;
}

export interface DevcontainerStatus {
  image_tag?: string | null;
  image_status: "none" | "building" | "ready" | "failed" | string;
  /**
   * `true` when the project has no catalog image assigned yet — it needs
   * setup before it can build/dispatch. Surfaced as a "Needs image" state.
   */
  needs_image?: boolean;
  image_last_error?: string | null;
  /**
   * ISO-8601 UTC timestamp of the last successful canonical-graph warm
   * for this project. Absent/null means the warmer has not completed a
   * run yet — the coordinator will not dispatch tasks until this is set.
   */
  graph_warmed_at?: string | null;
  /**
   * Derived warm-pipeline status for the banner row:
   * - `pending`: no warm has run (image not yet ready).
   * - `running`: image is ready; warm Job should be in flight or imminent.
   * - `ready`: warm completed cleanly for every workspace.
   * - `warning`: a non-fatal graph-cache shrink backstop fired; cache is
   *   still fresh but operators should inspect `workspace_warm_statuses`.
   * - `degraded`: the overall graph warmed but at least one workspace is
   *   `failed`/`timed_out` (partial success — usable but incomplete). The
   *   failing workspaces are listed in `workspace_warm_statuses`.
   * - `failed`: image build failed (no warm possible), or a workspace
   *   failed/timed out with no successful warm at all.
   */
  graph_warm_status:
    | "pending"
    | "running"
    | "ready"
    | "warning"
    | "degraded"
    | "failed"
    | string;
  /**
   * Per-workspace warm results, sourced from the durable
   * `project_workspace_graph` rows. Entries with `failed`/`timed_out`
   * explain which workspace did not produce a SCIP artifact.
   */
  workspace_warm_statuses?: WorkspaceWarmStatusEntry[];
  error?: string | null;
}

export interface WorkspaceWarmStatusEntry {
  workspace_slug: string;
  status: "ready" | "warming" | "failed" | "timed_out" | string;
  commit_sha?: string | null;
  warmed_at?: string | null;
}

export interface RetriggerImageBuildResponse {
  status: string;
  error?: string | null;
}

/**
 * Fetch the raw detected stack for a project by UUID.
 */
export async function fetchProjectStack(
  projectId: string,
): Promise<GetProjectStackResponse> {
  return callMcpTool("get_project_stack", { project: projectId });
}

/**
 * Fetch the devcontainer + image-build status snapshot for the onboarding banner.
 */
export async function fetchDevcontainerStatus(
  projectId: string,
): Promise<DevcontainerStatus> {
  return callMcpTool("get_project_devcontainer_status", { project: projectId });
}

/**
 * Force a rebuild of the project's per-project image on the next
 * mirror-fetch tick.
 */
export async function retriggerImageBuild(
  projectId: string,
): Promise<RetriggerImageBuildResponse> {
  return callMcpTool("retrigger_image_build", { project: projectId });
}
