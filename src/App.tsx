import { useServerHealth } from "@/hooks/useServerHealth";
import { useEventSource } from "@/hooks/useEventSource";
import { useFirstRun } from "@/hooks/useFirstRun";
import { LoadingScreen } from "@/components/LoadingScreen";
import { Wizard } from "@/components/Wizard";
import { WizardStep } from "@/components/WizardStep";
import { ServerCheckStep } from "@/components/ServerCheckStep";
import { ProviderSetupStep } from "@/components/ProviderSetupStep";
import { ProjectSetupStep } from "@/components/ProjectSetupStep";
import { ConnectionStatus } from "@/components/ConnectionStatus";
import { Sidebar } from "@/components/Sidebar";
import { KanbanPage } from "@/pages/KanbanPage";
import { RoadmapPage } from "@/pages/RoadmapPage";
import { SettingsPage } from "@/pages/SettingsPage";
import { useWizardStore } from "@/stores/wizardStore";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useEffect, useState } from "react";
import { ProjectSelector } from "@/components/ProjectSelector";
import { useProjectsBootstrap } from "@/hooks/useProjectsBootstrap";
import { useProjectStore, useProjects, useSelectedProjectId } from "@/stores/useProjectStore";
import { Button } from "@/components/ui/button";
import { useSidebar } from "@/hooks/useSidebar";
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

function DoneState() {
  const navigate = useNavigate();
  const { resetWizard } = useWizardStore();

  const handleNavigate = (path: "/" | "/settings") => {
    localStorage.removeItem("djinnos-wizard-storage");
    resetWizard();
    navigate(path);
  };

  return (
    <div className="flex min-h-screen flex-col bg-background">
      <main className="flex flex-1 flex-col items-center justify-center p-6">
        <div className="w-full max-w-md rounded-lg border border-border bg-card p-8 text-center">
          <h2 className="text-2xl font-semibold">You're All Set!</h2>
          <p className="mt-2 text-muted-foreground">
            Setup is complete. What's next?
          </p>
          <div className="mt-6 flex flex-col gap-3">
            <Button onClick={() => handleNavigate("/")}>View Kanban Board</Button>
            <Button variant="outline" onClick={() => handleNavigate("/settings")}>
              Explore Settings
            </Button>
          </div>
        </div>
      </main>
    </div>
  );
}

function MainLayout() {
  const sidebar = useSidebar();
  const projects = useProjects();
  const selectedProjectId = useSelectedProjectId();
  const setSelectedProjectId = useProjectStore((state) => state.setSelectedProjectId);

  return (
    <main className="flex min-h-screen flex-col bg-background">
      <header className="flex h-14 items-center justify-between border-b px-4">
        <div className="flex items-center gap-2">
          <h1 className="text-sm font-semibold text-foreground">DjinnOS Desktop</h1>
        </div>
        <div className="flex items-center gap-4">
          <ProjectSelector
            projects={projects}
            selectedId={selectedProjectId}
            onSelect={setSelectedProjectId}
          />
          <ConnectionStatus />
        </div>
      </header>

      <div className="flex flex-1 overflow-hidden">
        <Sidebar {...sidebar} />
        <div className="flex-1 overflow-auto">
          <Routes>
            <Route path="/" element={<KanbanPage />} />
            <Route path="/roadmap" element={<RoadmapPage />} />
            <Route path="/settings" element={<Navigate to="/settings/providers" replace />} />
            <Route path="/settings/:category" element={<SettingsPage />} />
            <Route path="*" element={<Navigate to="/" replace />} />
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

  useProjectsBootstrap();
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
    return <DoneState />;
  }

  return <MainLayout />;
}
