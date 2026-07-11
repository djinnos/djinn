import type { DevcontainerStatus } from "@/api/devcontainer";

export type BadgeState =
  | { kind: "needs_image" }
  | { kind: "building"; status: DevcontainerStatus }
  | { kind: "failed"; status: DevcontainerStatus }
  | { kind: "warming"; status: DevcontainerStatus }
  | { kind: "degraded"; status: DevcontainerStatus }
  | { kind: "ready" }
  | { kind: "unknown" };

export function deriveState(status: DevcontainerStatus | null): BadgeState {
  if (!status) return { kind: "unknown" };
  // A project with no catalog image assigned can't build/dispatch — surface
  // the setup state ahead of any build/warm status (migration 46 cut-over).
  if (status.needs_image) {
    return { kind: "needs_image" };
  }
  if (status.image_status === "building") {
    return { kind: "building", status };
  }
  if (status.image_status === "failed") {
    return { kind: "failed", status };
  }
  // The image built but the graph warm degraded: at least one workspace
  // failed/timed out while others warmed. The graph is usable but
  // incomplete — surface it distinctly (amber) instead of masking it as an
  // in-progress "Warming" spinner that never resolves.
  if (
    status.image_status === "ready" &&
    status.graph_warm_status === "degraded"
  ) {
    return { kind: "degraded", status };
  }
  if (status.image_status === "ready" && status.graph_warm_status !== "ready") {
    return { kind: "warming", status };
  }
  return { kind: "ready" };
}

/** Workspaces whose warm did not produce a usable artifact. */
export function failingWorkspaces(status: DevcontainerStatus): string[] {
  return (status.workspace_warm_statuses ?? [])
    .filter((w) => w.status === "failed" || w.status === "timed_out")
    .map((w) => `${w.workspace_slug} (${w.status})`);
}
