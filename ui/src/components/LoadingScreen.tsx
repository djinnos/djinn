
import { AlertCircleIcon, Loading02Icon, Refresh01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { Button } from "@/components/ui/button";

interface LoadingScreenProps {
  status?: "loading" | "error";
  message?: string;
  onRetry?: () => void;
  isRetrying?: boolean;
}

export function LoadingScreen({
  status = "loading",
  message = "Connecting to server...",
  onRetry,
  isRetrying = false,
}: LoadingScreenProps) {
  if (status === "error") {
    return (
      <div className="flex min-h-screen flex-col items-center justify-center gap-6 bg-background p-4">
        <div className="flex flex-col items-center gap-4 max-w-md text-center">
          <HugeiconsIcon icon={AlertCircleIcon} size={64} className="text-destructive" />
          <h1 className="text-2xl font-bold text-foreground">Server Connection Failed</h1>
          <p className="text-muted-foreground">{message}</p>
          <p className="text-sm text-muted-foreground">
            Please make sure the server is running or try restarting it.
          </p>
          {onRetry && (
            <Button
              onClick={onRetry}
              disabled={isRetrying}
              size="lg"
              className="gap-2"
            >
              {isRetrying ? (
                <>
                  <HugeiconsIcon icon={Loading02Icon} size={16} className="animate-spin" />
                  Retrying...
                </>
              ) : (
                <>
                  <HugeiconsIcon icon={Refresh01Icon} size={20} />
                  Retry Connection
                </>
              )}
            </Button>
          )}
        </div>
      </div>
    );
  }

  return (
    <div className="flex min-h-screen flex-col items-center justify-center gap-6 bg-background">
      <div className="flex flex-col items-center gap-4">
        <HugeiconsIcon icon={Loading02Icon} size={48} className="animate-spin text-primary" />
        <h1 className="text-2xl font-semibold text-foreground">Djinn</h1>
        <p className="text-muted-foreground">{message}</p>
      </div>
    </div>
  );
}
