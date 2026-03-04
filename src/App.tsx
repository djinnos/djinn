import { useServerHealth } from "@/hooks/useServerHealth";
import { LoadingScreen } from "@/components/LoadingScreen";
import { Wizard } from "@/components/Wizard";
import { WizardStep } from "@/components/WizardStep";
import { ServerCheckStep } from "@/components/ServerCheckStep";
import { ProviderSetupStep } from "@/components/ProviderSetupStep";
import { useWizardStore, shouldShowWizard } from "@/stores/wizardStore";
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

function ProjectSetupStep() {
  return (
    <div className="flex flex-col gap-4 text-center">
      <h2 className="text-2xl font-semibold">Create Project</h2>
      <p className="text-muted-foreground">
        Create your first project to organize your work.
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
    </div>
  );
}

export default function App() {
  const { status, port, error, retry, isRetrying } = useServerHealth();
  const [showWizard, setShowWizard] = useState(() => shouldShowWizard());
  const { isCompleted } = useWizardStore();

  useEffect(() => {
    if (status === "connected") {
      getCurrentWindow().show();
    }
  }, [status]);

  if (status === "loading") {
    return <LoadingScreen status="loading" message="Connecting to server..."></LoadingScreen>;
  }

  if (status === "error") {
    return (
      <LoadingScreen
        status="error"
        message={error || "Failed to connect to server"}
        onRetry={retry}
        isRetrying={isRetrying}
      ></LoadingScreen>
    );
  }

  if (showWizard && !isCompleted) {
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

  return (
    <main className="flex min-h-screen flex-col bg-background">
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
