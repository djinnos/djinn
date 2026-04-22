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
   * - `ready`: warm completed at least once (`graph_warmed_at` is set).
   * - `failed`: image build failed, warm cannot run.
   */
  graph_warm_status: "pending" | "running" | "ready" | "failed" | string;
  error?: string | null;
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
