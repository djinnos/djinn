import { useEffect, useState, useCallback } from "react";
import { checkServerHealth } from "@/api/server";
import { useWizardStore } from "@/stores/wizardStore";
import { CheckmarkCircle04Icon, AlertCircleIcon, Loading02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { Button } from "@/components/ui/button";

export function ServerCheckStep() {
  const [status, setStatus] = useState<"checking" | "success" | "error">("checking");
  const [errorMessage, setErrorMessage] = useState<string | null>(null);
  const { nextStep } = useWizardStore();

  const checkHealth = useCallback(async () => {
    setStatus("checking");
    setErrorMessage(null);
    
    try {
      await checkServerHealth();
      setStatus("success");
      // Auto-advance after a brief delay to show success state
      setTimeout(() => {
        nextStep();
      }, 800);
    } catch (error) {
      setStatus("error");
      setErrorMessage(error instanceof Error ? error.message : "Failed to connect to server");
    }
  }, [nextStep]);

  useEffect(() => {
    // Start health check on mount
    const timer = setTimeout(() => {
      void checkHealth();
    }, 100);
    return () => clearTimeout(timer);
  }, [checkHealth]);

  return (
    <div className="flex flex-col items-center gap-6 text-center">
      <div className="flex flex-col items-center gap-4">
        {status === "checking" && (
          <>
            <HugeiconsIcon icon={Loading02Icon} size={48} className="animate-spin text-primary" />
            <div>
              <h2 className="text-xl font-semibold">Connecting to Server</h2>
              <p className="text-sm text-muted-foreground">
                Checking server health...
              </p>
            </div>
          </>
        )}
        
        {status === "success" && (
          <>
            <HugeiconsIcon icon={CheckmarkCircle04Icon} size={48} className="text-green-500" />
            <div>
              <h2 className="text-xl font-semibold">Server Connected</h2>
              <p className="text-sm text-muted-foreground">
                Successfully connected to the Djinn server.
              </p>
            </div>
          </>
        )}
        
        {status === "error" && (
          <>
            <HugeiconsIcon icon={AlertCircleIcon} size={48} className="text-destructive" />
            <div>
              <h2 className="text-xl font-semibold">Connection Failed</h2>
              <p className="text-sm text-muted-foreground">
                {errorMessage || "Could not connect to the server."}
              </p>
            </div>
            <Button onClick={() => void checkHealth()} variant="outline">
              Retry Connection
            </Button>
          </>
        )}
      </div>
    </div>
  );
}
