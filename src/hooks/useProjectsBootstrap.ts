import { useEffect } from "react";
import { fetchProjects } from "@/api/server";
import { projectStore } from "@/stores/projectStore";

export function useProjectsBootstrap() {
  useEffect(() => {
    let cancelled = false;

    const loadProjects = async () => {
      try {
        const projects = await fetchProjects();
        if (!cancelled) {
          projectStore.getState().setProjects(projects);
        }
      } catch (error) {
        console.error("Failed to load projects:", error);
      }
    };

    void loadProjects();

    return () => {
      cancelled = true;
    };
  }, []);
}
