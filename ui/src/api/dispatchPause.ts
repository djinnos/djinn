import { callMcpTool } from "@/api/mcpClient";
import type { McpToolInput, McpToolOutput } from "@/api/generated/mcp-tools.gen";

export type DispatchPauseStatusArgs = McpToolInput<"dispatch_pause_status">;
export type DispatchPauseStatusResponse = McpToolOutput<"dispatch_pause_status">;
export type DispatchPauseScope = NonNullable<DispatchPauseStatusArgs["scope"]>;
export type DispatchPauseState = NonNullable<DispatchPauseStatusResponse["state"]>;
export type DispatchPauseStatusEntry = NonNullable<
  DispatchPauseStatusResponse["current"]
>;

function statusArgs(args?: DispatchPauseStatusArgs): DispatchPauseStatusArgs {
  const next: DispatchPauseStatusArgs = {};

  if (args?.scope != null) {
    next.scope = args.scope;
  }
  if (args?.target_id != null) {
    next.target_id = args.target_id;
  }

  return next;
}

/**
 * Fetch dispatch pause status. This wrapper is intentionally read-only: it only
 * invokes `dispatch_pause_status` and strips unknown/mutation-like fields from
 * caller-provided objects before sending MCP arguments.
 */
export async function fetchDispatchPauseStatus(
  args?: DispatchPauseStatusArgs,
): Promise<DispatchPauseStatusResponse> {
  return callMcpTool("dispatch_pause_status", statusArgs(args));
}

export async function fetchGlobalDispatchPauseStatus(): Promise<DispatchPauseStatusResponse> {
  return fetchDispatchPauseStatus();
}

export async function fetchProjectDispatchPauseStatus(
  projectId: string,
): Promise<DispatchPauseStatusResponse> {
  return fetchDispatchPauseStatus({ scope: "project", target_id: projectId });
}

export async function fetchUserDispatchPauseStatus(
  userId: string,
): Promise<DispatchPauseStatusResponse> {
  return fetchDispatchPauseStatus({ scope: "user", target_id: userId });
}
