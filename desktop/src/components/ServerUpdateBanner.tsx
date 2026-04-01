import { useServerHealth } from "@/hooks/useServerHealth";
import { useNavigate } from "react-router-dom";
import { AlertCircleIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { Button } from "@/components/ui/button";

/**
 * Banner shown when the connected server is running an outdated version.
 * Directs the user to Settings where they can redeploy.
 */
export function ServerUpdateBanner() {
  const { updateAvailable, serverVersion } = useServerHealth();
  const navigate = useNavigate();

  if (!updateAvailable) {
    return null;
  }

  return (
    <div className="border-l-4 border-amber-500 bg-amber-50 dark:bg-amber-900/20 px-4 py-3">
      <div className="flex items-center gap-3">
        <HugeiconsIcon
          icon={AlertCircleIcon}
          size={18}
          className="text-amber-600 dark:text-amber-400 shrink-0"
        />
        <div className="flex-1 min-w-0">
          <p className="text-sm font-medium text-amber-900 dark:text-amber-100">
            Server update available
            {serverVersion ? ` \u2014 your server is running ${serverVersion}` : ""}
          </p>
        </div>
        <div className="flex items-center gap-2 shrink-0">
          <Button
            variant="outline"
            size="sm"
            onClick={() => navigate("/settings")}
          >
            Update
          </Button>
        </div>
      </div>
    </div>
  );
}
