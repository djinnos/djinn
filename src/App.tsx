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
import { useSidebarStore } from "@/stores/sidebarStore";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { Navigate, Route, Routes, useLocation } from "react-router-dom";

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

function CompletionStep() {
  return (
    <div className="flex flex-col gap-4 text-center">
      <h2 className="text-2xl font-semibold">You're All Set!</h2>
      <p className="text-muted-foreground">
        Your workspace is ready. Start creating amazing things.
      </p>
      <Button onClick={() => window.location.reload()}>
        Go to Dashboard
      </Button>
    </div>
  );
}

function MainLayout() {
  const { setActiveSection } = useSidebarStore();
  const location = useLocation();

  useEffect(() => {
    if (location.pathname.startsWith('/roadmap')) {
      setActiveSection('roadmap');
    } else if (location.pathname.startsWith('/settings')) {
      setActiveSection('settings');
    } else {
      setActiveSection('kanban');
    }
  }, [location.pathname, setActiveSection]);

  return (
    <main className="flex min-h-screen flex-col bg-background">
      <header className="flex h-14 items-center justify-between border-b px-4">
        <div className="flex items-center gap-2">
          <h1 className="text-sm font-semibold text-foreground">DjinnOS Desktop</h1>
        </div>
        <ConnectionStatus />
      </header>

      <div className="flex flex-1 overflow-hidden">
        <Sidebar />
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
  const { isCompleted } = useWizardStore();
  const [showWizard, setShowWizard] = useState(false);

  useEventSource();

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
        onComplete={() => setShowWizard(false)}
        onSkip={() => setShowWizard(false)}
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
        <WizardStep stepNumber={5}>
          <CompletionStep />
        </WizardStep>
      </Wizard>
    );
  }

  return <MainLayout />;
}
