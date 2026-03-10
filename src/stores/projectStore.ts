import { createStore } from "zustand/vanilla";
import { subscribeWithSelector } from "zustand/middleware";
import type { Project } from "@/api/types";

const STORAGE_KEY = "djinnos-selected-project-id";

/** Sentinel value meaning "all projects" — no project filter applied. */
export const ALL_PROJECTS = "__all__" as const;

export interface ProjectState {
  projects: Project[];
  selectedProjectId: string | null;
  setProjects: (projects: Project[]) => void;
  setSelectedProjectId: (projectId: string | null) => void;
  getSelectedProject: () => Project | undefined;
  /** True when the user has chosen the "All Projects" scope. */
  isAllProjects: () => boolean;
}

function getInitialSelectedProjectId(): string | null {
  return localStorage.getItem(STORAGE_KEY);
}

export const projectStore = createStore<ProjectState>()(
  subscribeWithSelector((set, get) => ({
    projects: [],
    selectedProjectId: getInitialSelectedProjectId(),

    isAllProjects: () => get().selectedProjectId === ALL_PROJECTS,

    setProjects: (projects) => {
      const { selectedProjectId } = get();
      const isAll = selectedProjectId === ALL_PROJECTS;
      const hasSelected = !isAll && selectedProjectId ? projects.some((p) => p.id === selectedProjectId) : false;
      const nextSelected = isAll ? ALL_PROJECTS : hasSelected ? selectedProjectId : (projects[0]?.id ?? null);

      if (nextSelected) {
        localStorage.setItem(STORAGE_KEY, nextSelected);
      } else {
        localStorage.removeItem(STORAGE_KEY);
      }

      set({ projects, selectedProjectId: nextSelected });
    },

    setSelectedProjectId: (projectId) => {
      if (projectId) {
        localStorage.setItem(STORAGE_KEY, projectId);
      } else {
        localStorage.removeItem(STORAGE_KEY);
      }
      set({ selectedProjectId: projectId });
    },

    getSelectedProject: () => {
      const { projects, selectedProjectId } = get();
      if (!selectedProjectId || selectedProjectId === ALL_PROJECTS) return undefined;
      return projects.find((p) => p.id === selectedProjectId);
    },
  }))
);
