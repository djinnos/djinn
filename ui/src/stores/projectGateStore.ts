import { create } from 'zustand';
import { fetchProjects, type Project } from '@/api/server';
import { fetchDevcontainerStatus } from '@/api/devcontainer';

let refreshGeneration = 0;

interface ProjectGateState {
  /** null = not yet checked */
  hasProject: boolean | null;
  /** First project whose durable server status still requires image setup. */
  projectNeedingImage: Project | null;
  /** Retryable project-list/status failure while resolving required setup. */
  error: string | null;
  isChecking: boolean;
  markPendingProject: (project: Project) => void;
  clearPendingProject: (projectId?: string) => void;
  refresh: () => Promise<void>;
}

export const useProjectGateStore = create<ProjectGateState>((set, get) => ({
  hasProject: null,
  projectNeedingImage: null,
  error: null,
  isChecking: false,

  markPendingProject: (project) => {
    // Expose the newly-added project immediately while its first durable
    // status check catches up. Invalidate any older refresh so it cannot hide
    // this required step with stale server data.
    refreshGeneration += 1;
    set({
      hasProject: true,
      projectNeedingImage: project,
      error: null,
      isChecking: false,
    });
  },

  clearPendingProject: (projectId) => {
    const pendingProjectId = get().projectNeedingImage?.id;
    // A stale completion must not clear a newer repository's onboarding.
    if (projectId && pendingProjectId && projectId !== pendingProjectId) return;

    refreshGeneration += 1;
    set({
      projectNeedingImage: null,
      error: null,
      isChecking: false,
    });
  },

  refresh: async () => {
    const generation = ++refreshGeneration;
    set({ isChecking: true, error: null });
    try {
      const projects = await fetchProjects();
      if (generation !== refreshGeneration) return;

      if (projects.length === 0) {
        set({
          hasProject: false,
          projectNeedingImage: null,
          error: null,
          isChecking: false,
        });
        return;
      }

      // Browser storage cannot be the source of truth for required setup: it
      // disappears in a new browser/profile and can drift from the database.
      // Resolve every project from its durable server status, then route the
      // first project that needs an image or whose status is unresolved.
      const checks = await Promise.all(
        projects.map(async (project) => {
          try {
            return { project, status: await fetchDevcontainerStatus(project.id), error: null };
          } catch (error) {
            return { project, status: null, error };
          }
        }),
      );
      if (generation !== refreshGeneration) return;

      const incomplete = checks.find(
        ({ status, error }) => error || status?.error || status?.needs_image !== false,
      );

      if (incomplete) {
        const error = incomplete.error
          ? incomplete.error instanceof Error
            ? incomplete.error.message
            : "Could not check image setup"
          : incomplete.status?.error ??
            (incomplete.status?.needs_image === true
              ? null
              : "Image setup status was incomplete");
        set({
          hasProject: true,
          projectNeedingImage: incomplete.project,
          error,
          isChecking: false,
        });
        return;
      }

      set({
        hasProject: true,
        projectNeedingImage: null,
        error: null,
        isChecking: false,
      });
    } catch (error) {
      if (generation !== refreshGeneration) return;
      // Do not guess that a repository exists when the required readiness call
      // failed. App renders a retry state for this unresolved gate.
      set({
        hasProject: get().projectNeedingImage ? true : null,
        error: error instanceof Error ? error.message : "Could not load repositories",
        isChecking: false,
      });
    }
  },
}));
