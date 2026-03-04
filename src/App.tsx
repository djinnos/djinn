import { useServerHealth } from "@/hooks/useServerHealth";
import { LoadingScreen } from "@/components/LoadingScreen";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useEffect } from "react";
import { Button } from "@/components/ui/button";

export default function App() {
  const { status, port, error, retry, isRetrying } = useServerHealth();

  useEffect(() => {
    // Show the window when connected
    if (status === "connected") {
      getCurrentWindow().show();
    }
  }, [status]);

  if (status === "loading") {
    return <LoadingScreen status="loading" message="Connecting to server..." />;
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
