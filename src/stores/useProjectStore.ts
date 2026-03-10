import { useCallback } from "react";
import { useStoreWithSelector } from "./useStoreWithSelector";
import { projectStore, type ProjectState, ALL_PROJECTS } from "./projectStore";
import type { Project } from "@/api/types";

export { projectStore, ALL_PROJECTS } from "./projectStore";

export function useProjectStore(): ProjectState;
export function useProjectStore<T>(selector: (state: ProjectState) => T): T;
export function useProjectStore<T>(selector?: (state: ProjectState) => T): ProjectState | T {
  return useStoreWithSelector(projectStore, selector);
}

export function useProjects(): Project[] {
  return useProjectStore((state) => state.projects);
}

export function useSelectedProjectId(): string | null {
  return useProjectStore((state) => state.selectedProjectId);
}

export function useSelectedProject(): Project | undefined {
  return useProjectStore(
    useCallback((state) => state.getSelectedProject(), [])
  );
}

export function useIsAllProjects(): boolean {
  return useProjectStore((state) => state.selectedProjectId === ALL_PROJECTS);
}
