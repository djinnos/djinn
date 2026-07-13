import type { Project } from "@/api/server";
import type { ConnectionStatus } from "@/hooks/useServerHealth";

export type OnboardingDestination =
  | "checking"
  | "connection-error"
  | "project-error"
  | "repository"
  | "models"
  | "image"
  | "app";

/**
 * Resolve the first incomplete required setup step from server-backed facts.
 * Repository selection comes before model setup so the asynchronous clone and
 * stack detector can run while the user connects and assigns their models.
 */
export function resolveOnboardingDestination({
  hasProject,
  hasProvider,
  hasModels,
  projectNeedingImage,
  projectError,
  serverStatus,
}: {
  hasProject: boolean | null;
  hasProvider: boolean | null;
  hasModels: boolean | null;
  projectNeedingImage: Project | null;
  projectError: string | null;
  serverStatus: ConnectionStatus;
}): OnboardingDestination {
  if (serverStatus === "loading") return "checking";

  // An outage may keep the already-resolved application shell alive so its
  // connection banner can recover. It must never bypass an initial or
  // incomplete onboarding gate using unknown/stale readiness facts.
  if (serverStatus === "error") {
    const setupWasComplete =
      hasProject === true &&
      hasProvider === true &&
      hasModels === true &&
      projectNeedingImage === null &&
      projectError === null;
    return setupWasComplete ? "app" : "connection-error";
  }

  if (hasProject === null) {
    return projectError ? "project-error" : "checking";
  }
  if (!hasProject) return "repository";

  if (hasProvider === null || hasModels === null) return "checking";
  if (!hasProvider || !hasModels) return "models";

  if (projectNeedingImage) return "image";
  return "app";
}
