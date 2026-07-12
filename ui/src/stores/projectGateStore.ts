import { create } from 'zustand';
import { fetchProjects, type Project } from '@/api/server';
import { fetchDevcontainerStatus } from '@/api/devcontainer';

const PENDING_PROJECT_STORAGE_KEY = 'djinn:onboarding:pending-project-image:v1';

let refreshGeneration = 0;

function readPendingProjectId(): string | null {
  try {
    return window.localStorage.getItem(PENDING_PROJECT_STORAGE_KEY);
  } catch {
    return null;
  }
}

function writePendingProjectId(projectId: string | null): void {
  try {
    if (projectId) {
      window.localStorage.setItem(PENDING_PROJECT_STORAGE_KEY, projectId);
    } else {
      window.localStorage.removeItem(PENDING_PROJECT_STORAGE_KEY);
    }
  } catch {
    // Storage can be unavailable in hardened/private browser contexts. The
    // in-memory gate still completes the current session correctly.
  }
}

interface ProjectGateState {
  /** null = not yet checked */
  hasProject: boolean | null;
  /** Project added by this browser that still needs its first image. */
  projectNeedingImage: Project | null;
  /** Persisted id used to resume the just-added project's setup after reload. */
  pendingProjectId: string | null;
  isChecking: boolean;
  markPendingProject: (project: Project) => void;
  clearPendingProject: (projectId?: string) => void;
  refresh: () => Promise<void>;
}

export const useProjectGateStore = create<ProjectGateState>((set, get) => ({
  hasProject: null,
  projectNeedingImage: null,
  pendingProjectId: readPendingProjectId(),
  isChecking: false,

  markPendingProject: (project) => {
    // Invalidate any older refresh before exposing the newly-added project.
    refreshGeneration += 1;
    writePendingProjectId(project.id);
    set({
      hasProject: true,
      projectNeedingImage: project,
      pendingProjectId: project.id,
      isChecking: false,
    });
  },

  clearPendingProject: (projectId) => {
    const pendingProjectId = get().pendingProjectId;
    // A stale completion must not clear a newer repository's onboarding.
    if (projectId && pendingProjectId && projectId !== pendingProjectId) return;

    refreshGeneration += 1;
    writePendingProjectId(null);
    set({
      projectNeedingImage: null,
      pendingProjectId: null,
      isChecking: false,
    });
  },

  refresh: async () => {
    const generation = ++refreshGeneration;
    set({ isChecking: true });
    try {
      const projects = await fetchProjects();
      if (generation !== refreshGeneration) return;

      if (projects.length === 0) {
        writePendingProjectId(null);
        set({
          hasProject: false,
          projectNeedingImage: null,
          pendingProjectId: null,
          isChecking: false,
        });
        return;
      }

      const pendingProjectId = get().pendingProjectId;
      if (!pendingProjectId) {
        // Existing/legacy image-less projects keep using the in-app status badge.
        // Full-screen onboarding is reserved for the repository this browser
        // just added, so an upgrade never turns into a global forced migration.
        set({ hasProject: true, projectNeedingImage: null, isChecking: false });
        return;
      }

      const pendingProject = projects.find((project) => project.id === pendingProjectId);
      if (!pendingProject) {
        writePendingProjectId(null);
        set({
          hasProject: true,
          projectNeedingImage: null,
          pendingProjectId: null,
          isChecking: false,
        });
        return;
      }

      try {
        const status = await fetchDevcontainerStatus(pendingProject.id);
        if (generation !== refreshGeneration) return;

        if (status.error) {
          // The status tool can resolve an error payload with default false-y
          // fields. Keep reload recovery instead of mistaking that payload for
          // proof that image assignment completed.
          set({ hasProject: true, projectNeedingImage: null, isChecking: false });
          return;
        }

        if (status.needs_image) {
          set({
            hasProject: true,
            projectNeedingImage: pendingProject,
            isChecking: false,
          });
        } else {
          writePendingProjectId(null);
          set({
            hasProject: true,
            projectNeedingImage: null,
            pendingProjectId: null,
            isChecking: false,
          });
        }
      } catch {
        if (generation !== refreshGeneration) return;
        // Do not trap the user on a transient status failure, but retain the
        // persisted id so a reload/reconnect can resume the pending setup.
        set({ hasProject: true, projectNeedingImage: null, isChecking: false });
      }
    } catch {
      if (generation !== refreshGeneration) return;
      // Preserve a directly-marked pending project when the post-add list
      // refresh fails. Otherwise leave the gate open and let the app retry.
      set({ hasProject: true, isChecking: false });
    }
  },
}));
