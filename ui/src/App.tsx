import { useServerHealth } from "@/hooks/useServerHealth";
import { useEventSource } from "@/hooks/useEventSource";
import { Sidebar } from "@/components/Sidebar";
import { KanbanPage } from "@/pages/KanbanPage";
import { DependenciesPage } from "@/pages/DependenciesPage";
import { BoardLayout } from "@/components/board/BoardLayout";
import { AgentsPage } from "@/pages/AgentsPage";
import { SettingsPage } from "@/pages/SettingsPage";
import { TaskSessionPage } from "@/pages/TaskSessionPage";
import { ChatPage } from "@/pages/ChatPage";
import { MemoryPage } from "@/pages/MemoryPage";
import { CodeGraphPage } from "@/pages/CodeGraphPage";
import { ProposalsPage } from "@/pages/ProposalsPage";
import { RepositoriesPage } from "@/pages/RepositoriesPage";
import { ImagesPage } from "@/pages/ImagesPage";
import { ImageEditorPage } from "@/pages/ImageEditorPage";
import { ProjectEnvironmentPage } from "@/pages/ProjectEnvironmentPage";
import { UsersPage } from "@/pages/UsersPage";
import { UsageDashboardPage } from "@/pages/UsageDashboardPage";
import { ConnectionBanner } from "@/components/ConnectionBanner";
import { DispatchPauseBanner } from "@/components/DispatchPauseBanner";
import { AuthGate, useAuthUser } from "@/components/AuthGate";
import { useEffect, useRef } from "react";
import { useProjectsBootstrap } from "@/hooks/useProjectsBootstrap";
import { Navigate, Route, Routes } from "react-router-dom";
import { useProviderGateStore } from "@/stores/providerGateStore";
import { ProviderOnboarding } from "@/components/ProviderOnboarding";
import { useModelGateStore } from "@/stores/modelGateStore";
import { ModelOnboarding } from "@/components/ModelOnboarding";
import { useProjectGateStore } from "@/stores/projectGateStore";
import { RepositoryOnboarding } from "@/components/RepositoryOnboarding";
import { useDispatchPauseHydration } from "@/hooks/useDispatchPauseHydration";

function MainLayout() {
  const isAdmin = useAuthUser()?.isAdmin ?? false;
  return (
    <main className="flex h-screen overflow-hidden bg-background">
      <Sidebar />
      <div className="flex min-h-0 min-w-0 flex-1 flex-col">
        <div className="flex min-h-0 flex-1 flex-col">
          <ConnectionBanner />
          <DispatchPauseBanner />
          <Routes>
            {/* Views — project selection lives in the projectStore, not the URL.
                Tasks + Dependencies share the BoardLayout (filter header + view
                toggle); the old /kanban and /roadmap paths redirect in. */}
            <Route element={<BoardLayout />}>
              <Route path="/tasks" element={<KanbanPage />} />
              <Route path="/dependencies" element={<DependenciesPage />} />
            </Route>
            <Route path="/kanban" element={<Navigate to="/tasks" replace />} />
            <Route
              path="/roadmap"
              element={<Navigate to="/dependencies" replace />}
            />
            <Route path="/chat" element={<ChatPage />} />
            <Route path="/chat/:sessionId" element={<ChatPage />} />
            <Route path="/agents" element={<AgentsPage />} />
            <Route path="/memory" element={<MemoryPage />} />
            <Route path="/code-graph" element={<CodeGraphPage />} />
            <Route path="/pulse" element={<Navigate to="/code-graph" replace />} />
            <Route path="/proposals" element={<ProposalsPage />} />
            <Route path="/proposals/:id" element={<ProposalsPage />} />
            <Route path="/metrics" element={<Navigate to="/agents" replace />} />

            {/* Task session (global) */}
            <Route path="/task/:taskId" element={<TaskSessionPage />} />

            {/* Repositories */}
            <Route path="/repositories" element={<RepositoriesPage />} />

            {/* Image catalog */}
            <Route path="/images" element={<ImagesPage />} />
            <Route path="/images/new" element={<ImageEditorPage />} />
            <Route path="/images/:id/edit" element={<ImageEditorPage />} />

            {/* Per-project pages */}
            <Route path="/projects/:id/environment" element={<ProjectEnvironmentPage />} />

            {/* Users — admin-only roster; non-admins are bounced to the board */}
            <Route
              path="/users"
              element={isAdmin ? <UsersPage /> : <Navigate to="/tasks" replace />}
            />

            {/* Usage & Analytics — admin-only dashboard */}
            <Route
              path="/admin/usage"
              element={isAdmin ? <UsageDashboardPage /> : <Navigate to="/tasks" replace />}
            />

            {/* Settings */}
            <Route path="/settings" element={<SettingsPage />} />
            <Route path="/settings/*" element={<SettingsPage />} />

            {/* Default redirect */}
            <Route path="*" element={<Navigate to="/tasks" replace />} />
          </Routes>
        </div>
      </div>
    </main>
  );
}

function AuthenticatedApp() {
  const { status } = useServerHealth();
  const { hasProvider, refresh: refreshGate } = useProviderGateStore();
  const { hasModels, refresh: refreshModelGate } = useModelGateStore();
  const { hasProject, refresh: refreshProjectGate } = useProjectGateStore();
  const hasConnectedOnce = useRef(false);

  useProjectsBootstrap(status);
  useDispatchPauseHydration(status);
  useEventSource();

  useEffect(() => {
    if (status === 'connected') {
      hasConnectedOnce.current = true;
      void refreshGate();
      void refreshModelGate();
      void refreshProjectGate();
    }
  }, [status, refreshGate, refreshModelGate, refreshProjectGate]);

  // If the server has been reached once and then drops, keep rendering the
  // shell so the ConnectionBanner can surface the outage without forcing the
  // user back to a blocking overlay.
  if (status !== 'connected' && hasConnectedOnce.current) {
    return <MainLayout />;
  }

  if (hasProvider === false) {
    return <ProviderOnboarding />;
  }

  if (hasModels === false) {
    return <ModelOnboarding />;
  }

  if (hasProject === false) {
    return <RepositoryOnboarding />;
  }

  return <MainLayout />;
}

export default function App() {
  // Gate 1: Authentication (currently passthrough until server exposes /auth)
  return (
    <AuthGate>
      {/* Gate 2 & 3: Provider + Model onboarding, then main app */}
      <AuthenticatedApp />
    </AuthGate>
  );
}
