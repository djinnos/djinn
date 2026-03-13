import { useServerHealth } from "@/hooks/useServerHealth";
import { useEventSource } from "@/hooks/useEventSource";
import { useFirstRun } from "@/hooks/useFirstRun";
import { LoadingScreen } from "@/components/LoadingScreen";
import { Wizard } from "@/components/Wizard";
import { WizardStep } from "@/components/WizardStep";
import { ServerCheckStep } from "@/components/ServerCheckStep";
import { ProviderSetupStep } from "@/components/ProviderSetupStep";
import { ProjectSetupStep } from "@/components/ProjectSetupStep";
import { Sidebar } from "@/components/Sidebar";
import { Titlebar } from "@/components/Titlebar";
import { KanbanPage } from "@/pages/KanbanPage";
import { SettingsPage } from "@/pages/SettingsPage";
import { TaskSessionPage } from "@/pages/TaskSessionPage";
import { ChatPage } from "@/pages/ChatPage";
import { SyncHealthBanner } from "@/components/SyncHealthBanner";
import { useWizardStore } from "@/stores/wizardStore";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useEffect, useState } from "react";
import { useProjectsBootstrap } from "@/hooks/useProjectsBootstrap";
import { useSelectedProjectId } from "@/stores/useProjectStore";
import { Button } from "@/components/ui/button";
import { Navigate, Route, Routes, useNavigate } from "react-router-dom";


function WelcomeStep() {
  return (
    <div className="flex flex-col gap-4 text-center">
      <h2 className="text-2xl font-semibold">Welcome to DjinnOS</h2>
      <p className="text-muted-foreground">
        Let's set up your workspace in a few simple steps.
      </p>
    </div>
  );
}

function DoneState({ onDismiss }: { onDismiss: (path: "/" | "/settings") => void }) {

  return (
    <div className="flex min-h-screen flex-col bg-background">
      <main className="flex flex-1 flex-col items-center justify-center p-6">
        <div className="w-full max-w-md rounded-lg border border-border bg-card p-8 text-center">
          <h2 className="text-2xl font-semibold">You're All Set!</h2>
          <p className="mt-2 text-muted-foreground">
            Setup is complete. What's next?
          </p>
          <div className="mt-6 flex flex-col gap-3">
            <Button onClick={() => onDismiss("/")}>View Kanban Board</Button>
            <Button variant="outline" onClick={() => onDismiss("/settings")}>
              Explore Settings
            </Button>
          </div>
        </div>
      </main>
    </div>
  );
}

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

            {/* Project-scoped views */}
            <Route path="/projects/:projectId/kanban" element={<KanbanPage />} />
            <Route path="/projects/:projectId/chat" element={<ChatPage />} />
            <Route path="/projects/:projectId/chat/:sessionId" element={<ChatPage />} />

            {/* Task session (global) */}
            <Route path="/task/:taskId" element={<TaskSessionPage />} />

            {/* Settings */}
            <Route path="/settings" element={<Navigate to="/settings/providers" replace />} />
            <Route path="/settings/:category" element={<SettingsPage />} />

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
  const { isFirstRun, isLoading: isFirstRunLoading } = useFirstRun();
  const { isCompleted, resetWizard } = useWizardStore();
  const selectedProjectId = useSelectedProjectId();
  const navigate = useNavigate();

  useProjectsBootstrap(status);
  const [showWizard, setShowWizard] = useState(false);
  const [showDoneState, setShowDoneState] = useState(false);

  useEventSource(selectedProjectId);

  useEffect(() => {
    if (isFirstRunLoading) return;

    if (isFirstRun === true && !isCompleted) {
      setShowWizard(true);
    } else {
      setShowWizard(false);
    }
  }, [isFirstRun, isFirstRunLoading, isCompleted]);

  useEffect(() => {
    if (status === "connected") {
      getCurrentWindow().show();
    }
  }, [status]);

  if (status === "loading" || isFirstRunLoading) {
    return (
      <LoadingScreen
        status="loading"
        message={status === "loading" ? "Connecting to server..." : "Checking first-run status..."}
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

  if (showWizard) {
    return (
      <Wizard
        onComplete={() => {
          setShowWizard(false);
          setShowDoneState(true);
        }}
        onSkip={() => {
          localStorage.removeItem("djinnos-wizard-storage");
          resetWizard();
          setShowWizard(false);
          setShowDoneState(false);
        }}
      >
        <WizardStep stepNumber={1}>
          <WelcomeStep />
        </WizardStep>
        <WizardStep stepNumber={2}>
          <ServerCheckStep />
        </WizardStep>
        <WizardStep stepNumber={3}>
          <ProviderSetupStep />
        </WizardStep>
        <WizardStep stepNumber={4}>
          <ProjectSetupStep />
        </WizardStep>
      </Wizard>
    );
  }

  if (showDoneState) {
    return <DoneState onDismiss={(path) => {
      setShowDoneState(false);
      navigate(path === "/" ? "/kanban" : path);
    }} />;
  }

  return <MainLayout />;
}
