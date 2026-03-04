import { Loader2, AlertCircle, RefreshCw } from "lucide-react";
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
          <AlertCircle className="h-16 w-16 text-destructive" />
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
                  <Loader2 className="h-4 w-4 animate-spin" />
                  Retrying...
                </>
              ) : (
                <>
                  <RefreshCw className="h-5 w-5" />
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
        <Loader2 className="h-12 w-12 animate-spin text-primary" />
        <h1 className="text-2xl font-semibold text-foreground">DjinnOS Desktop</h1>
        <p className="text-muted-foreground">{message}</p>
      </div>
    </div>
  );
}
