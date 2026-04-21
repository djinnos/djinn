import { useQueries } from '@tanstack/react-query';
import { fetchDevcontainerStatus, type DevcontainerStatus } from '@/api/devcontainer';
import { useProjects } from '@/stores/useProjectStore';

/**
 * Returns the count of projects whose devcontainer is in a state that
 * requires user action (missing devcontainer.json or a failed image
 * build). "Building" is excluded — it's informational. The lock file is
 * NOT surfaced as a warning: the image pipeline doesn't require it, and
 * generating one mechanically requires the devcontainers CLI which we
 * don't ship on the server.
 *
 * Mirrors the triage logic in {@link DevcontainerStatusBadge.deriveState}.
 */
export function useDevcontainerWarnings(): { count: number } {
  const projects = useProjects();

  const results = useQueries({
    queries: projects.map((project) => ({
      queryKey: ['devcontainer', 'status', project.id] as const,
      queryFn: () => fetchDevcontainerStatus(project.id),
      staleTime: 30_000,
      refetchInterval: 30_000,
    })),
  });

  let count = 0;
  for (const result of results) {
    const status = result.data as DevcontainerStatus | undefined;
    if (!status) continue;
    if (!status.has_devcontainer || status.image_status === 'failed') {
      count += 1;
    }
  }

  return { count };
}
