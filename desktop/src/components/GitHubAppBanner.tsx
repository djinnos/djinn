import { useEffect, useState, useCallback, useMemo } from "react";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Alert02Icon, Cancel01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { Loading02Icon } from "@hugeicons/core-free-icons";
import { callMcpTool } from "@/api/mcpClient";
import { showToast } from "@/lib/toast";

interface GitHubAppBannerProps {
  projectPaths: string[];
}

type WarningKind = "not_connected" | "org_pending";

interface GitHubWarning {
  kind: WarningKind;
  orgs: string[];
}

export function GitHubAppBanner({ projectPaths }: GitHubAppBannerProps) {
  const [warning, setWarning] = useState<GitHubWarning | null>(null);
  const [dismissed, setDismissed] = useState(false);
  const [checking, setChecking] = useState(false);

  // Stabilize the paths array so deps don't fire on every render
  const pathsKey = projectPaths.slice().sort().join("\0");
  const stablePaths = useMemo(() => projectPaths, [pathsKey]); // eslint-disable-line react-hooks/exhaustive-deps

  const checkHealth = useCallback(async (): Promise<GitHubWarning | null> => {
    if (stablePaths.length === 0) return null;

    const results = await Promise.all(
      stablePaths.map((path) =>
        callMcpTool("board_health", { project: path }).catch(
          () => null as Record<string, unknown> | null
        )
      )
    );

    const pendingOrgs = new Set<string>();
    for (const result of results) {
      if (!result) continue;
      const warnings = result.warnings as string[] | undefined;
      if (!warnings) continue;
      if (warnings.includes("github_not_connected") || warnings.includes("github_app_not_installed")) {
        return { kind: "not_connected", orgs: [] };
      }
      for (const w of warnings) {
        if (w.startsWith("github_org_access_pending:")) {
          pendingOrgs.add(w.split(":")[1]);
        }
      }
    }
    if (pendingOrgs.size > 0) {
      return { kind: "org_pending", orgs: Array.from(pendingOrgs) };
    }
    return null;
  }, [stablePaths]);

  const showWarning = warning !== null;

  // Initial check
  useEffect(() => {
    let active = true;
    checkHealth().then((result) => {
      if (active) setWarning(result);
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
      const result = await checkHealth();
      if (result) {
        showToast.warning(
          result.kind === "org_pending"
            ? `Organization access still pending for: ${result.orgs.join(", ")}`
            : "App not yet installed. Make sure to install it on your organization."
        );
        setWarning(result);
      } else {
        setWarning(null);
        showToast.success("GitHub connected successfully!");
      }
    } catch {
      showToast.error("Failed to check GitHub status.");
    } finally {
      setChecking(false);
    }
  };

  if (!showWarning || dismissed) return null;

  const isOrgPending = warning?.kind === "org_pending";
  const title = isOrgPending
    ? "Organization Access Pending"
    : "GitHub App Not Installed";
  const description = isOrgPending
    ? `An org admin must approve the Djinn OAuth App for: ${warning.orgs.join(", ")}. PR creation is blocked until approved.`
    : "Install the Djinn app on your GitHub organization to enable PR creation and review feedback.";
  const actionLabel = isOrgPending ? "Request Access" : "Install on GitHub";
  const actionUrl = isOrgPending
    ? "https://docs.github.com/articles/restricting-access-to-your-organization-s-data/"
    : "https://github.com/apps/djinn-ai-bot/installations/new";

  return (
    <Card className="mx-4 border-none ring-orange-500/50 bg-orange-500/10">
      <CardContent className="py-4">
        <div className="flex items-start justify-between gap-3">
          <div className="flex items-start gap-3">
            <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-orange-500/20">
              <HugeiconsIcon
                icon={Alert02Icon}
                className="size-4 text-orange-400"
              />
            </div>
            <div className="flex flex-col gap-1">
              <h3 className="text-sm font-semibold text-orange-200">
                {title}
              </h3>
              <p className="text-sm text-muted-foreground">
                {description}
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
          <a
            href={actionUrl}
            target="_blank"
            rel="noopener noreferrer"
          >
            <Button
              variant="outline"
              size="sm"
              className="h-7 gap-1.5 px-3 text-xs"
            >
              {actionLabel}
            </Button>
          </a>
          <Button
            variant="ghost"
            size="sm"
            className="h-7 gap-1.5 px-3 text-xs"
            onClick={handleCheckAgain}
            disabled={checking}
          >
            {checking ? (
              <HugeiconsIcon icon={Loading02Icon} size={14} className="animate-spin" />
            ) : (
              "Check again"
            )}
          </Button>
        </div>
      </CardContent>
    </Card>
  );
}
