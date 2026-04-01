import { useServerHealth } from "@/hooks/useServerHealth";
import { useNavigate } from "react-router-dom";
import { AlertCircleIcon, Loading02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { Button } from "@/components/ui/button";

/**
 * Banner shown when the server is not connected.
 * Lets the user retry or navigate to settings to reconfigure.
 */
export function ConnectionBanner() {
  const { status, error, retry, isRetrying } = useServerHealth();
  const navigate = useNavigate();

  if (status === "connected" || status === "loading") {
    return null;
  }

  return (
    <div className="border-l-4 border-red-500 bg-red-50 dark:bg-red-900/20 px-4 py-3">
      <div className="flex items-center gap-3">
        <HugeiconsIcon
          icon={AlertCircleIcon}
          size={18}
          className="text-red-600 dark:text-red-400 shrink-0"
        />
        <div className="flex-1 min-w-0">
          <p className="text-sm font-medium text-red-900 dark:text-red-100">
            Server not connected
          </p>
          {error && (
            <p className="text-xs text-red-800 dark:text-red-200 mt-0.5 truncate">
              {error}
            </p>
          )}
        </div>
        <div className="flex items-center gap-2 shrink-0">
          <Button
            variant="outline"
            size="sm"
            onClick={() => navigate("/settings")}
          >
            Settings
          </Button>
          <Button size="sm" onClick={() => void retry()} disabled={isRetrying}>
            {isRetrying ? (
              <>
                <HugeiconsIcon
                  icon={Loading02Icon}
                  size={14}
                  className="animate-spin"
                />
                Retrying...
              </>
            ) : (
              "Retry"
            )}
          </Button>
        </div>
      </div>
    </div>
  );
}
