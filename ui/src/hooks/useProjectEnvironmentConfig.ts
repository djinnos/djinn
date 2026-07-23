/**
 * Shared per-project environment-config query.
 *
 * The Repositories table needs each row's `selected_image_id`, which lives in
 * `project_environment_config_get`. Routing every row through the same
 * `queryOptions` (identical query key + staleTime) lets react-query dedupe the
 * per-row fan-out instead of storming the backend with duplicate calls.
 */
import { queryOptions, useQuery } from "@tanstack/react-query";

import { fetchEnvironmentConfig } from "@/api/environmentConfig";

export function projectEnvironmentConfigQueryOptions(projectId: string) {
  return queryOptions({
    queryKey: ["project", projectId, "environment-config"] as const,
    queryFn: () => fetchEnvironmentConfig(projectId),
    staleTime: 30_000,
  });
}

/** Read one project's environment config (assigned image id + name). */
export function useProjectEnvironmentConfig(projectId: string) {
  return useQuery(projectEnvironmentConfigQueryOptions(projectId));
}
