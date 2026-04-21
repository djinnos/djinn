import { useQueries } from '@tanstack/react-query';
import { fetchDevcontainerStatus, type DevcontainerStatus } from '@/api/devcontainer';
import { useProjects } from '@/stores/useProjectStore';

/**
 * Returns the count of projects whose devcontainer is in a state that
 * requires user action (missing devcontainer.json, missing lockfile, or
 * a failed image build). "Building" is excluded — it's informational.
 *
 * Mirrors the triage logic in {@link DevcontainerBanner.deriveState}.
 */
export function useDevcontainerWarnings(): { count: number } {
  const projects = useProjects();

  const results = useQueries({
    queries: projects.map((project) => ({
      queryKey: ['devcontainer', 'status', project.id] as const,
      queryFn: () => fetchDevcontainerStatus(project.id),
      staleTime: 30_000,
    })),
  });

  let count = 0;
  for (const result of results) {
    const status = result.data as DevcontainerStatus | undefined;
    if (!status) continue;
    if (
      !status.has_devcontainer ||
      !status.has_devcontainer_lock ||
      status.image_status === 'failed'
    ) {
      count += 1;
    }
  }

  return { count };
}
