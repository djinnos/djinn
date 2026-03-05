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
import { useWizardStore } from "@/stores/wizardStore";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";

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

export default function App() {
  const { status, port, error, retry, isRetrying } = useServerHealth();
  const { isFirstRun, isLoading: isFirstRunLoading } = useFirstRun();
  const { isCompleted } = useWizardStore();
  const [showWizard, setShowWizard] = useState(false);

  // Initialize EventSource connection for SSE events
  useEventSource();

  // Determine whether to show wizard based on first-run detection
  useEffect(() => {
    if (isFirstRunLoading) return;
    
    // Show wizard if it's the first run and wizard hasn't been completed
    if (isFirstRun === true && !isCompleted) {
      setShowWizard(true);
    } else {
      setShowWizard(false);
    }
  }, [isFirstRun, isFirstRunLoading, isCompleted]);

  // Show window when connected
  useEffect(() => {
    if (status === "connected") {
      getCurrentWindow().show();
    }
  }, [status]);

  // Loading states
  if (status === "loading" || isFirstRunLoading) {
    return (
      <LoadingScreen 
        status="loading" 
        message={status === "loading" ? "Connecting to server..." : "Checking first-run status..."}
      />
    );
  }

  // Server error state
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

  // Show wizard for first run
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

  // Main app view (kanban/dashboard)
  return (
    <main className="flex min-h-screen flex-col bg-background">
      {/* App Bar with Connection Status */}
      <header className="flex items-center justify-between border-b px-4 py-2">
        <div className="flex items-center gap-2">
          <h1 className="text-lg font-semibold">DjinnOS Desktop</h1>
        </div>
        <ConnectionStatus />
      </header>
      
      <div className="flex flex-1 items-center justify-center">
        <div className="flex flex-col items-center gap-4">
          <h1 className="text-4xl font-bold text-foreground">DjinnOS Desktop</h1>
          <p className="text-muted-foreground">
            Connected to server on port {port}
          </p>
          <div className="flex gap-4">
            <Button>Default Button</Button>
            <Button variant="secondary">Secondary</Button>
            <Button variant="outline">Outline</Button>
            <Button variant="ghost">Ghost</Button>
          </div>
          <div className="flex gap-4">
            <Button size="sm">Small</Button>
            <Button size="default">Default</Button>
            <Button size="lg">Large</Button>
          </div>
        </div>
      </div>
    </main>
  );
}
