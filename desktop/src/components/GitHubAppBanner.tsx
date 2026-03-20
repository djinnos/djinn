import { useEffect, useState, useCallback, useMemo } from "react";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Alert02Icon, Cancel01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { Loader2Icon } from "lucide-react";
import { callMcpTool } from "@/api/mcpClient";
import { showToast } from "@/lib/toast";

interface GitHubAppBannerProps {
  projectPaths: string[];
}

export function GitHubAppBanner({ projectPaths }: GitHubAppBannerProps) {
  const [showWarning, setShowWarning] = useState(false);
  const [dismissed, setDismissed] = useState(false);
  const [checking, setChecking] = useState(false);

  // Stabilize the paths array so deps don't fire on every render
  const pathsKey = projectPaths.slice().sort().join("\0");
  const stablePaths = useMemo(() => projectPaths, [pathsKey]); // eslint-disable-line react-hooks/exhaustive-deps

  const checkHealth = useCallback(async () => {
    if (stablePaths.length === 0) return false;

    const results = await Promise.all(
      stablePaths.map((path) =>
        callMcpTool("board_health", { project: path }).catch(
          () => null as Record<string, unknown> | null
        )
      )
    );

    for (const result of results) {
      if (!result) continue;
      const warnings = result.warnings as string[] | undefined;
      if (warnings?.includes("github_not_connected") || warnings?.includes("github_app_not_installed")) {
        return true;
      }
    }
    return false;
  }, [stablePaths]);

  // Initial check
  useEffect(() => {
    let active = true;
    checkHealth().then((hasWarning) => {
      if (active) setShowWarning(hasWarning);
    });
    return () => {
      active = false;
    };
  }, [checkHealth]);

  // Reset dismissed when project selection changes
  useEffect(() => setDismissed(false), [pathsKey]);

  const handleCheckAgain = async () => {
    setChecking(true);
    try {
      const stillDisconnected = await checkHealth();
      if (stillDisconnected) {
        showToast.warning(
          "App not yet installed. Make sure to install it on your organization."
        );
      } else {
        setShowWarning(false);
        showToast.success("GitHub App connected successfully!");
      }
    } catch {
      showToast.error("Failed to check GitHub App status.");
    } finally {
      setChecking(false);
    }
  };

  if (!showWarning || dismissed) return null;

  return (
    <Card className="mx-4 border-amber-500/30 bg-amber-500/10">
      <CardContent className="py-4">
        <div className="flex items-start justify-between gap-3">
          <div className="flex items-start gap-3">
            <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-amber-500/20">
              <HugeiconsIcon
                icon={Alert02Icon}
                className="size-4 text-amber-400"
              />
            </div>
            <div className="flex flex-col gap-1">
              <h3 className="text-sm font-semibold text-amber-200">
                GitHub App Not Installed
              </h3>
              <p className="text-sm text-muted-foreground">
                Install the Djinn app on your GitHub organization to enable PR
                creation and review feedback.
              </p>
            </div>
          </div>
          <button
            type="button"
            aria-label="Dismiss GitHub App banner"
            onClick={() => setDismissed(true)}
            className="shrink-0 rounded-md p-1 text-muted-foreground transition-colors hover:bg-muted/40 hover:text-foreground"
          >
            <HugeiconsIcon icon={Cancel01Icon} className="size-4" />
          </button>
        </div>

        <div className="mt-3 flex items-center gap-2 pl-11">
          <Button
            variant="outline"
            size="sm"
            className="h-7 gap-1.5 px-3 text-xs"
            onClick={() =>
              window.open(
                "https://github.com/apps/djinn-ai-bot/installations/new",
                "_blank"
              )
            }
          >
            Install on GitHub
          </Button>
          <Button
            variant="ghost"
            size="sm"
            className="h-7 gap-1.5 px-3 text-xs"
            onClick={handleCheckAgain}
            disabled={checking}
          >
            {checking ? (
              <Loader2Icon className="h-3.5 w-3.5 animate-spin" />
            ) : (
              "Check again"
            )}
          </Button>
        </div>
      </CardContent>
    </Card>
  );
}
