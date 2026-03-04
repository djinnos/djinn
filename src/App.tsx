import { useServerHealth } from "@/hooks/useServerHealth";
import { LoadingScreen } from "@/components/LoadingScreen";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useEffect } from "react";

export default function App() {
  const { status } = useServerHealth();

  useEffect(() => {
    // Show the window when connected
    if (status === "connected") {
      getCurrentWindow().show();
    }
  }, [status]);

  if (status === "loading") {
    return <LoadingScreen />;
  }

  if (status === "error") {
    return <LoadingScreen message="Failed to connect to server" />;
  }

  return (
    <main className="flex min-h-screen flex-col bg-background">
      <div className="flex flex-1 items-center justify-center">
        <p className="text-up text-muted-foreground">Application ready - empty shell</p>
      </div>
    </main>
  );
}
