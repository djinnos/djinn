import { useServerHealth } from "@/hooks/useServerHealth";
import { useEventSource } from "@/hooks/useEventSource";
import { LoadingScreen } from "@/components/LoadingScreen";
import { Wizard } from "@/components/Wizard";
import { WizardStep } from "@/components/WizardStep";
import { ServerCheckStep } from "@/components/ServerCheckStep";
import { ProviderSetupStep } from "@/components/ProviderSetupStep";
import { ConnectionStatus } from "@/components/ConnectionStatus";
import {
  CommandDialog,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
  CommandShortcut,
} from "@/components/ui/command";
import { useWizardStore, shouldShowWizard } from "@/stores/wizardStore";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { Kanban, LayoutDashboard, Settings } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { Button } from "@/components/ui/button";

type Route = "kanban" | "roadmap" | "settings";

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
  const [commandOpen, setCommandOpen] = useState(false);
  const [sidebarOpen, setSidebarOpen] = useState(true);
  const [currentRoute, setCurrentRoute] = useState<Route>("kanban");

  // Initialize EventSource connection for SSE events
  useEventSource();

  useEffect(() => {
    if (status === "connected") {
      getCurrentWindow().show();
    }
  }, [status]);

  const navigateTo = useCallback((route: Route) => {
    setCurrentRoute(route);
    setCommandOpen(false);
  }, []);

  const shortcuts = useMemo(
    () => ({
      openPalette: (event: KeyboardEvent) => (event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k",
      openSettings: (event: KeyboardEvent) => event.metaKey && event.key === ",",
      toggleSidebar: (event: KeyboardEvent) => event.metaKey && event.key === "/",
      dismiss: (event: KeyboardEvent) => event.key === "Escape",
    }),
    [],
  );

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const isEditable =
        target?.tagName === "INPUT" ||
        target?.tagName === "TEXTAREA" ||
        target?.isContentEditable;

      if (shortcuts.dismiss(event)) {
        if (commandOpen || showWizard) {
          event.preventDefault();
          setCommandOpen(false);
          setShowWizard(false);
        }
        return;
      }

      if (shortcuts.openPalette(event)) {
        event.preventDefault();
        setCommandOpen((open) => !open);
        return;
      }

      if (isEditable && !event.metaKey && !event.ctrlKey) {
        return;
      }

      if (shortcuts.openSettings(event)) {
        event.preventDefault();
        navigateTo("settings");
        return;
      }

      if (shortcuts.toggleSidebar(event)) {
        event.preventDefault();
        setSidebarOpen((open) => !open);
      }
    };

    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [commandOpen, navigateTo, shortcuts, showWizard]);

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
    <main className="flex min-h-screen bg-background text-foreground">
      {sidebarOpen && (
        <aside className="w-64 border-r p-4">
          <h2 className="mb-4 text-sm font-semibold uppercase text-muted-foreground">Sidebar</h2>
          <div className="space-y-2 text-sm">
            <p>Use Cmd+/ to toggle this panel.</p>
          </div>
        </aside>
      )}

      <div className="flex min-h-screen flex-1 flex-col">
        <header className="flex items-center justify-between border-b px-4 py-2">
          <div className="flex items-center gap-2">
            <h1 className="text-lg font-semibold">DjinnOS Desktop</h1>
          </div>
          <ConnectionStatus />
        </header>

        <div className="flex flex-1 items-center justify-center">
          <div className="flex flex-col items-center gap-4">
            <h1 className="text-4xl font-bold text-foreground">{currentRoute.toUpperCase()}</h1>
            <p className="text-muted-foreground">Connected to server on port {port}</p>
            <div className="flex gap-4">
              <Button onClick={() => setCommandOpen(true)}>Open Command Palette</Button>
              <Button variant="secondary" onClick={() => navigateTo("settings")}>Go to Settings</Button>
            </div>
          </div>
        </div>
      </div>

      <CommandDialog open={commandOpen} onOpenChange={setCommandOpen}>
        <CommandInput placeholder="Type a command or search..." />
        <CommandList>
          <CommandEmpty>No results found.</CommandEmpty>
          <CommandGroup heading="Navigation">
            <CommandItem onSelect={() => navigateTo("kanban")}>
              <Kanban />
              <span>Go to Kanban</span>
              <CommandShortcut>⌘1</CommandShortcut>
            </CommandItem>
            <CommandItem onSelect={() => navigateTo("roadmap")}>
              <LayoutDashboard />
              <span>Go to Roadmap</span>
              <CommandShortcut>⌘2</CommandShortcut>
            </CommandItem>
            <CommandItem onSelect={() => navigateTo("settings")}>
              <Settings />
              <span>Go to Settings</span>
              <CommandShortcut>⌘,</CommandShortcut>
            </CommandItem>
          </CommandGroup>
        </CommandList>
      </CommandDialog>
    </main>
  );
}
