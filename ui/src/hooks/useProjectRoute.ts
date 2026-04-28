/**
 * useProjectRoute — view navigation + last-view tracking.
 *
 * Project selection lives entirely in projectStore — it is NOT reflected in
 * the URL. Only the view (kanban/chat/agents/...) lives in the path. This
 * hook exposes helpers for switching view and for switching the selected
 * project (which stays on the current view).
 */

import { useEffect, useCallback } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import { projectStore, ALL_PROJECTS, type ProjectView } from "@/stores/projectStore";
import { useSelectedProjectId } from "@/stores/useProjectStore";

function parseViewFromPath(pathname: string): ProjectView | null {
  if (pathname.includes("/chat")) return "chat";
  if (pathname.includes("/agents")) return "agents";
  if (pathname.includes("/metrics")) return "metrics";
  if (pathname.includes("/roadmap")) return "roadmap";
  if (pathname.includes("/memory")) return "memory";
  if (pathname.includes("/code-graph")) return "code-graph";
  if (pathname.includes("/proposals")) return "proposals";
  if (pathname.includes("/kanban") || pathname === "/") return "kanban";
  return null;
}

export function useProjectRoute() {
  const location = useLocation();
  const navigate = useNavigate();
  const selectedProjectId = useSelectedProjectId();

  const currentView = parseViewFromPath(location.pathname);

  useEffect(() => {
    if (!currentView) return;
    const pid = selectedProjectId ?? ALL_PROJECTS;
    projectStore.getState().setLastView(pid, currentView);
  }, [selectedProjectId, currentView]);

  const navigateToProject = useCallback((projectId: string) => {
    projectStore.getState().setSelectedProjectId(projectId);
  }, []);

  const navigateToView = useCallback(
    (view: ProjectView) => {
      navigate(`/${view}`);
    },
    [navigate]
  );

  return {
    currentView,
    navigateToProject,
    navigateToView,
  };
}
