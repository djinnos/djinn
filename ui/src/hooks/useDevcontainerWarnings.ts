import { useQueries } from '@tanstack/react-query';
import { fetchDevcontainerStatus, type DevcontainerStatus } from '@/api/devcontainer';
import { useProjects } from '@/stores/useProjectStore';

/**
 * Returns the count of projects whose per-project image build is in a
 * state that requires user action (failed). "Building" is excluded —
 * it's informational.
 *
 * Mirrors the triage logic in {@link ImageStatusBadge.deriveState}.
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
    if (status.image_status === 'failed') {
      count += 1;
    }
  }

  return { count };
}
