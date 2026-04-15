import { useServerHealth } from "@/hooks/useServerHealth";
import { AlertCircleIcon, Loading02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { Button } from "@/components/ui/button";

/**
 * Banner shown when the Djinn server is not reachable.
 *
 * Since the server runs via docker-compose, the remediation is to run
 * `docker compose up` in the repo root — there is no in-app setup flow.
 */
export function ServerUnreachableBanner() {
  const { status, baseUrl, error, retry, isRetrying } = useServerHealth();

  if (status === "connected" || status === "loading") {
    return null;
  }

  const displayUrl = baseUrl ?? "http://localhost:8372";

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
            Server not reachable at {displayUrl}
          </p>
          <p className="text-xs text-red-800 dark:text-red-200 mt-0.5">
            Run{" "}
            <code className="rounded bg-red-100 dark:bg-red-900/40 px-1 py-0.5 font-mono">
              docker compose up
            </code>{" "}
            in the repo root to start the Djinn server.
          </p>
          {error && (
            <p className="text-xs text-red-700/80 dark:text-red-300/80 mt-0.5 truncate">
              {error}
            </p>
          )}
        </div>
        <div className="flex items-center gap-2 shrink-0">
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
