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
  has_devcontainer?: boolean;
  has_devcontainer_lock?: boolean;
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
  has_devcontainer: boolean;
  has_devcontainer_lock: boolean;
  image_tag?: string | null;
  image_status: "none" | "building" | "ready" | "failed" | string;
  image_last_error?: string | null;
  starter_json?: string | null;
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
 * Convenience wrapper — the starter JSON is already embedded in
 * {@link fetchDevcontainerStatus}, but callers sometimes want it on
 * demand (e.g. regenerating the template after the user edits the stack).
 */
export async function generateStarterJson(projectId: string): Promise<string | null> {
  const status = await fetchDevcontainerStatus(projectId);
  return status.starter_json ?? null;
}

/**
 * Force a rebuild of the project's per-project devcontainer image on
 * the next mirror-fetch tick.
 */
export async function retriggerImageBuild(
  projectId: string,
): Promise<RetriggerImageBuildResponse> {
  return callMcpTool("retrigger_image_build", { project: projectId });
}
