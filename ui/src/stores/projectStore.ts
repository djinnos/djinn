import { createStore } from "zustand/vanilla";
import { subscribeWithSelector } from "zustand/middleware";
import type { Project } from "@/api/types";

const STORAGE_KEY = "djinnos-selected-project-id";
const LAST_VIEW_KEY = "djinnos-last-view-per-project";

export type ProjectView = "tasks" | "dependencies" | "chat" | "agents" | "metrics" | "memory" | "code-graph" | "proposals";

/** Migrate legacy persisted view keys (renamed in the Tasks/Dependencies cut-over). */
const LEGACY_VIEW_ALIASES: Record<string, ProjectView> = {
  kanban: "tasks",
  roadmap: "dependencies",
};

/** Sentinel value meaning "all projects" — no project filter applied. */
export const ALL_PROJECTS = "__all__" as const;

export interface ProjectState {
  projects: Project[];
  selectedProjectId: string | null;
  /** Tracks the last-used view per project ID (including ALL_PROJECTS). */
  lastViewPerProject: Record<string, ProjectView>;
  setProjects: (projects: Project[]) => void;
  setSelectedProjectId: (projectId: string | null) => void;
  getSelectedProject: () => Project | undefined;
  /** True when the user has chosen the "All Projects" scope. */
  isAllProjects: () => boolean;
  /** Get the last-used view for a project (defaults to "tasks"). */
  getLastView: (projectId: string) => ProjectView;
  /** Record the current view for a project. */
  setLastView: (projectId: string, view: ProjectView) => void;
}

function getInitialSelectedProjectId(): string | null {
  try {
    return localStorage.getItem(STORAGE_KEY);
  } catch {
    return null;
  }
}

function getInitialLastViews(): Record<string, ProjectView> {
  try {
    const raw = localStorage.getItem(LAST_VIEW_KEY);
    if (!raw) return {};
    const parsed = JSON.parse(raw) as Record<string, string>;
    const migrated: Record<string, ProjectView> = {};
    for (const [projectId, view] of Object.entries(parsed)) {
      migrated[projectId] = LEGACY_VIEW_ALIASES[view] ?? (view as ProjectView);
    }
    return migrated;
  } catch {
    return {};
  }
}

export const projectStore = createStore<ProjectState>()(
  subscribeWithSelector((set, get) => ({
    projects: [],
    selectedProjectId: getInitialSelectedProjectId(),
    lastViewPerProject: getInitialLastViews(),

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

    getLastView: (projectId: string) => {
      return get().lastViewPerProject[projectId] ?? "tasks";
    },

    setLastView: (projectId: string, view: ProjectView) => {
      const next = { ...get().lastViewPerProject, [projectId]: view };
      localStorage.setItem(LAST_VIEW_KEY, JSON.stringify(next));
      set({ lastViewPerProject: next });
    },
  }))
);
