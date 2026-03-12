/**
 * useProjectRoute — Syncs URL params to projectStore and derives navigation helpers.
 *
 * URL patterns:
 *   /kanban, /epics, /chat       → ALL_PROJECTS view
 *   /projects/:projectId/kanban  → specific project view
 *   /projects/:projectId/epics   → specific project view
 *   /projects/:projectId/chat    → specific project view
 *   /task/:taskId                → task session (no project change)
 *   /settings/*                  → settings (no project change)
 *
 * NOTE: This hook parses the URL directly (not useParams) so it works
 * when called from components outside <Routes> (e.g. Sidebar, Titlebar).
 */

import { useEffect, useCallback } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import { projectStore, ALL_PROJECTS, type ProjectView } from "@/stores/projectStore";
import { useSelectedProjectId } from "@/stores/useProjectStore";

/** Extract projectId from /projects/:projectId/... URLs */
function parseProjectIdFromPath(pathname: string): string | undefined {
  const match = pathname.match(/^\/projects\/([^/]+)/);
  return match?.[1];
}

/** Derive the current view from the pathname */
function parseViewFromPath(pathname: string): ProjectView | null {
  if (pathname.includes("/chat")) return "chat";
  if (pathname.includes("/epics")) return "epics";
  if (pathname.includes("/kanban") || pathname === "/") return "kanban";
  return null;
}

export function useProjectRoute() {
  const location = useLocation();
  const navigate = useNavigate();
  const selectedProjectId = useSelectedProjectId();

  const routeProjectId = parseProjectIdFromPath(location.pathname);
  const currentView = parseViewFromPath(location.pathname);

  // Sync route → projectStore when project route changes
  useEffect(() => {
    if (routeProjectId) {
      if (selectedProjectId !== routeProjectId) {
        projectStore.getState().setSelectedProjectId(routeProjectId);
      }
    } else if (currentView) {
      // Global view route (/kanban, /epics) → ALL_PROJECTS
      if (selectedProjectId !== ALL_PROJECTS) {
        projectStore.getState().setSelectedProjectId(ALL_PROJECTS);
      }
    }
  }, [routeProjectId, currentView]);

  // Track last view per project
  useEffect(() => {
    if (!currentView) return;
    const pid = routeProjectId ?? ALL_PROJECTS;
    projectStore.getState().setLastView(pid, currentView);
  }, [routeProjectId, currentView]);

  /** Navigate to a specific project, keeping the current view. */
  const navigateToProject = useCallback(
    (projectId: string) => {
      const view = currentView ?? "kanban";
      if (projectId === ALL_PROJECTS) {
        navigate(`/${view}`);
      } else {
        navigate(`/projects/${projectId}/${view}`);
      }
    },
    [navigate, currentView]
  );

  /** Navigate to a view within the currently selected project. */
  const navigateToView = useCallback(
    (view: ProjectView) => {
      const pid = selectedProjectId ?? ALL_PROJECTS;
      if (pid === ALL_PROJECTS) {
        navigate(`/${view}`);
      } else {
        navigate(`/projects/${pid}/${view}`);
      }
    },
    [navigate, selectedProjectId]
  );

  return {
    routeProjectId,
    currentView,
    navigateToProject,
    navigateToView,
  };
}
