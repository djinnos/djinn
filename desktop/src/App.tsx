import { useServerHealth } from "@/hooks/useServerHealth";
import { useEventSource } from "@/hooks/useEventSource";
import { LoadingScreen } from "@/components/LoadingScreen";
import { Sidebar } from "@/components/Sidebar";
import { Titlebar } from "@/components/Titlebar";
import { KanbanPage } from "@/pages/KanbanPage";
import { AgentsPage } from "@/pages/AgentsPage";
import { MetricsPage } from "@/pages/MetricsPage";
import { SettingsPage } from "@/pages/SettingsPage";
import { TaskSessionPage } from "@/pages/TaskSessionPage";
import { ChatPage } from "@/pages/ChatPage";
import { SyncHealthBanner } from "@/components/SyncHealthBanner";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useEffect } from "react";
import { useProjectsBootstrap } from "@/hooks/useProjectsBootstrap";
import { useSelectedProjectId } from "@/stores/useProjectStore";
import { Navigate, Route, Routes } from "react-router-dom";

function MainLayout() {
  return (
    <main className="flex h-screen overflow-hidden bg-background">
      <Sidebar />
      <div className="flex min-h-0 min-w-0 flex-1 flex-col">
        <Titlebar />
        <div className="flex min-h-0 flex-1 flex-col">
          <SyncHealthBanner />
          <Routes>
            {/* Global views (All Projects) */}
            <Route path="/kanban" element={<KanbanPage />} />
            <Route path="/chat" element={<ChatPage />} />
            <Route path="/chat/:sessionId" element={<ChatPage />} />
            <Route path="/agents" element={<AgentsPage />} />
            <Route path="/metrics" element={<MetricsPage />} />

            {/* Project-scoped views */}
            <Route path="/projects/:projectId/kanban" element={<KanbanPage />} />
            <Route path="/projects/:projectId/chat" element={<ChatPage />} />
            <Route path="/projects/:projectId/chat/:sessionId" element={<ChatPage />} />
            <Route path="/projects/:projectId/agents" element={<AgentsPage />} />
            <Route path="/projects/:projectId/metrics" element={<MetricsPage />} />

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

export default function App() {
  const { status, error, retry, isRetrying } = useServerHealth();
  const selectedProjectId = useSelectedProjectId();

  useProjectsBootstrap(status);
  useEventSource(selectedProjectId);

  useEffect(() => {
    if (status === "connected") {
      getCurrentWindow().show();
    }
  }, [status]);

  if (status === "loading") {
    return (
      <LoadingScreen
        status="loading"
        message="Connecting to server..."
      />
    );
  }

  if (status === "error") {
    return (
      <LoadingScreen
        status="error"
        message={error || "Failed to connect to server"}
        onRetry={retry}
        isRetrying={isRetrying}
      />
    );
  }

  return <MainLayout />;
}
