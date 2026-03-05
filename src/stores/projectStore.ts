import { createStore } from "zustand/vanilla";
import { subscribeWithSelector } from "zustand/middleware";
import type { Project } from "@/types";

const STORAGE_KEY = "djinnos-selected-project-id";

export interface ProjectState {
  projects: Project[];
  selectedProjectId: string | null;
  setProjects: (projects: Project[]) => void;
  setSelectedProjectId: (projectId: string | null) => void;
  getSelectedProject: () => Project | undefined;
}

function getInitialSelectedProjectId(): string | null {
  return localStorage.getItem(STORAGE_KEY);
}

export const projectStore = createStore<ProjectState>()(
  subscribeWithSelector((set, get) => ({
    projects: [],
    selectedProjectId: getInitialSelectedProjectId(),

    setProjects: (projects) => {
      const { selectedProjectId } = get();
      const hasSelected = selectedProjectId ? projects.some((p) => p.id === selectedProjectId) : false;
      const nextSelected = hasSelected ? selectedProjectId : (projects[0]?.id ?? null);

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
      if (!selectedProjectId) return undefined;
      return projects.find((p) => p.id === selectedProjectId);
    },
  }))
);
