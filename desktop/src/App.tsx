import { useServerHealth } from "@/hooks/useServerHealth";
import { useEventSource } from "@/hooks/useEventSource";
import { Sidebar } from "@/components/Sidebar";
import { Titlebar } from "@/components/Titlebar";
import { KanbanPage } from "@/pages/KanbanPage";
import { RoadmapPage } from "@/pages/RoadmapPage";
import { AgentsPage } from "@/pages/AgentsPage";
import { SettingsPage } from "@/pages/SettingsPage";
import { TaskSessionPage } from "@/pages/TaskSessionPage";
import { ChatPage } from "@/pages/ChatPage";
import { MemoryPage } from "@/pages/MemoryPage";
import { PulsePage } from "@/pages/PulsePage";
import { SyncHealthBanner } from "@/components/SyncHealthBanner";
import { ConnectionBanner } from "@/components/ConnectionBanner";
import { ServerUpdateBanner } from "@/components/ServerUpdateBanner";
import { ServerOnboarding } from "@/components/ServerOnboarding";
import { AuthGate } from "@/components/AuthGate";
import { useEffect, useRef } from "react";
import { useProjectsBootstrap } from "@/hooks/useProjectsBootstrap";
import { useSelectedProjectId } from "@/stores/useProjectStore";
import { Navigate, Route, Routes } from "react-router-dom";
import { useProviderGateStore } from "@/stores/providerGateStore";
import { ProviderOnboarding } from "@/components/ProviderOnboarding";
import { useModelGateStore } from "@/stores/modelGateStore";
import { ModelOnboarding } from "@/components/ModelOnboarding";

function MainLayout() {
  return (
    <main className="flex h-screen overflow-hidden bg-background">
      <Sidebar />
      <div className="flex min-h-0 min-w-0 flex-1 flex-col">
        <Titlebar />
        <div className="flex min-h-0 flex-1 flex-col">
          <ConnectionBanner />
          <ServerUpdateBanner />
          <SyncHealthBanner />
          <Routes>
            {/* Global views (All Projects) */}
            <Route path="/kanban" element={<KanbanPage />} />
            <Route path="/roadmap" element={<RoadmapPage />} />
            <Route path="/chat" element={<ChatPage />} />
            <Route path="/chat/:sessionId" element={<ChatPage />} />
            <Route path="/agents" element={<AgentsPage />} />
            <Route path="/memory" element={<MemoryPage />} />
            <Route path="/pulse" element={<PulsePage />} />
            <Route path="/metrics" element={<Navigate to="/agents" replace />} />

            {/* Project-scoped views */}
            <Route path="/projects/:projectId/kanban" element={<KanbanPage />} />
            <Route path="/projects/:projectId/roadmap" element={<RoadmapPage />} />
            <Route path="/projects/:projectId/chat" element={<ChatPage />} />
            <Route path="/projects/:projectId/chat/:sessionId" element={<ChatPage />} />
            <Route path="/projects/:projectId/agents" element={<AgentsPage />} />
            <Route path="/projects/:projectId/memory" element={<MemoryPage />} />
            <Route path="/projects/:projectId/pulse" element={<PulsePage />} />
            <Route path="/projects/:projectId/metrics" element={<Navigate to="../agents" replace />} />

            {/* Task session (global) */}
            <Route path="/task/:taskId" element={<TaskSessionPage />} />

            {/* Settings */}
            <Route path="/settings" element={<SettingsPage />} />
            <Route path="/settings/*" element={<SettingsPage />} />

            {/* Default redirect */}
            <Route path="*" element={<Navigate to="/kanban" replace />} />
          </Routes>
        </div>
      </div>
    </main>
  );
}

function AuthenticatedApp() {
  const { status } = useServerHealth();
  const selectedProjectId = useSelectedProjectId();
  const { hasProvider, refresh: refreshGate } = useProviderGateStore();
  const { hasModels, refresh: refreshModelGate } = useModelGateStore();
  const hasConnectedOnce = useRef(false);

  useProjectsBootstrap(status);
  useEventSource(selectedProjectId);

  useEffect(() => {
    if (status === 'connected') {
      hasConnectedOnce.current = true;
      void refreshGate();
      void refreshModelGate();
    }
  }, [status, refreshGate, refreshModelGate]);

  // After initial onboarding, if server disconnects show MainLayout
  // with ConnectionBanner so user can access Settings.
  if (status !== 'connected' && hasConnectedOnce.current) {
    return <MainLayout />;
  }

  if (hasProvider === false) {
    return <ProviderOnboarding />;
  }

  if (hasModels === false) {
    return <ModelOnboarding />;
  }

  return <MainLayout />;
}

export default function App() {
  // Gate 1: Server connection
  return (
    <ServerOnboarding>
      {/* Gate 2: GitHub authentication (requires server) */}
      <AuthGate>
        {/* Gate 3 & 4: Provider + Model onboarding, then main app */}
        <AuthenticatedApp />
      </AuthGate>
    </ServerOnboarding>
  );
}
