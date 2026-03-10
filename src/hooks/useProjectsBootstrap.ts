import { useEffect } from "react";
import { fetchProjects } from "@/api/server";
import { projectStore } from "@/stores/projectStore";
import type { ConnectionStatus } from "@/hooks/useServerHealth";

export function useProjectsBootstrap(serverStatus: ConnectionStatus = "connected") {
  useEffect(() => {
    if (serverStatus !== "connected") return;

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
  }, [serverStatus]);
}
