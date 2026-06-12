import { callMcpTool } from "@/api/mcpClient";
import type { McpToolInput, McpToolOutput } from "@/api/generated/mcp-tools.gen";

export type DispatchPauseStatusInput = McpToolInput<"dispatch_pause_status">;
export type DispatchPauseStatusOutput = McpToolOutput<"dispatch_pause_status">;
export type DispatchPauseScope = NonNullable<DispatchPauseStatusInput["scope"]>;
export type DispatchPauseMetadata = NonNullable<DispatchPauseStatusOutput["current"]>;
export type DispatchPauseState = NonNullable<DispatchPauseStatusOutput["state"]>;

// Back-compat aliases for callers added by the API-wrapper task.
export type DispatchPauseStatusArgs = DispatchPauseStatusInput;
export type DispatchPauseStatusResponse = DispatchPauseStatusOutput;
export type DispatchPauseStatusEntry = DispatchPauseMetadata;

function sanitizeStatusArgs(args?: Partial<DispatchPauseStatusInput>): DispatchPauseStatusInput {
  const sanitized: DispatchPauseStatusInput = {};

  if (args?.scope != null) {
    sanitized.scope = args.scope;
  }
  if (typeof args?.target_id === "string" && args.target_id.trim() !== "") {
    sanitized.target_id = args.target_id;
  }

  return sanitized;
}

/**
 * Fetch dispatch pause status. This wrapper is intentionally read-only: it only
 * invokes `dispatch_pause_status` and strips unknown/mutation-like fields from
 * caller-provided objects before sending MCP arguments.
 */
export async function fetchDispatchPauseStatus(
  args?: Partial<DispatchPauseStatusInput>,
): Promise<DispatchPauseStatusOutput> {
  return callMcpTool("dispatch_pause_status", sanitizeStatusArgs(args));
}

export function fetchGlobalDispatchPauseStatus(): Promise<DispatchPauseStatusOutput> {
  return fetchDispatchPauseStatus();
}

export function fetchProjectDispatchPauseStatus(targetId: string): Promise<DispatchPauseStatusOutput> {
  return fetchDispatchPauseStatus({ scope: "project", target_id: targetId });
}

export function fetchUserDispatchPauseStatus(targetId: string): Promise<DispatchPauseStatusOutput> {
  return fetchDispatchPauseStatus({ scope: "user", target_id: targetId });
}
