import { useEffect, useState, useMemo } from "react";
import { Card, CardContent } from "@/components/ui/card";
import { Alert02Icon, Cancel01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { callMcpTool } from "@/api/mcpClient";

interface PrError {
  projectId: string;
  message: string;
}

interface BoardHealthData {
  projectIssues: Record<string, string>;
  prErrors: PrError[];
}

function useBoardHealth(projectSlugs: string[]): BoardHealthData | null {
  const [projectIssues, setProjectIssues] = useState<Record<string, string>>(
    {}
  );
  const [prErrors, setPrErrors] = useState<PrError[]>([]);

  // Stabilize the slugs array so deps don't fire on every render
  const slugsKey = projectSlugs.slice().sort().join("\0");
  const stableSlugs = useMemo(() => projectSlugs, [slugsKey]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    if (stableSlugs.length === 0) return;

    let active = true;
    const fetch = () => {
      Promise.all(
        stableSlugs.map((slug) =>
          callMcpTool("board_health", { project: slug }).catch(
            () => null as Record<string, unknown> | null
          )
        )
      ).then((results) => {
        if (!active) return;
        const allIssues: Record<string, string> = {};
        const allPrErrors: PrError[] = [];
        for (const result of results) {
          if (!result) continue;
          const i = result.project_issues as Record<string, string> | undefined;
          if (i) Object.assign(allIssues, i);
          const pe = result.pr_errors as Record<string, string> | undefined;
          if (pe) {
            for (const [projectId, message] of Object.entries(pe)) {
              allPrErrors.push({ projectId, message });
            }
          }
        }
        setProjectIssues(allIssues);
        setPrErrors(allPrErrors);
      });
    };

    fetch();
    // 60 s: board_health aggregates several board-wide queries server-side and
    // is fetched once per project per tick — a health banner does not need
    // 15 s freshness, and the tighter interval multiplied across open tabs
    // kept the server's heaviest read path continuously hot.
    const interval = setInterval(fetch, 60_000);
    return () => {
      active = false;
      clearInterval(interval);
    };
  }, [stableSlugs]);

  const hasIssues =
    Object.keys(projectIssues).length > 0 ||
    prErrors.length > 0;

  if (!hasIssues) return null;

  return { projectIssues, prErrors };
}

interface BoardHealthBannerProps {
  projectSlugs: string[];
}

export function BoardHealthBanner({ projectSlugs }: BoardHealthBannerProps) {
  const health = useBoardHealth(projectSlugs);
  const [dismissed, setDismissed] = useState(false);

  // Reset dismissed when project selection changes. Uses React's sanctioned
  // adjust-state-during-render pattern (track the previous key) instead of a
  // reset effect, so the banner re-appears in the same render the slugs change.
  const slugsKey = projectSlugs.slice().sort().join("\0");
  const [prevSlugsKey, setPrevSlugsKey] = useState(slugsKey);
  if (slugsKey !== prevSlugsKey) {
    setPrevSlugsKey(slugsKey);
    setDismissed(false);
  }

  if (!health || dismissed) return null;

  const { projectIssues, prErrors } = health;
  const issueEntries = Object.entries(projectIssues);
  const totalIssues =
    issueEntries.length + prErrors.length;

  return (
    <Card className="mx-4 border-amber-500/20 bg-amber-500/[0.04]">
      <CardContent className="py-3">
        <div className="flex items-start justify-between gap-3">
          <div className="flex items-start gap-2.5">
            <div className="flex h-6 w-6 shrink-0 items-center justify-center rounded-full bg-amber-500/15">
              <HugeiconsIcon
                icon={Alert02Icon}
                className="size-3.5 text-amber-400"
              />
            </div>
            <span className="text-sm font-medium text-amber-200">
              {totalIssues} health{" "}
              {totalIssues === 1 ? "issue" : "issues"}
            </span>
          </div>
          <button
            type="button"
            aria-label="Dismiss board health banner"
            onClick={() => setDismissed(true)}
            className="shrink-0 rounded-md p-0.5 text-muted-foreground transition-colors hover:bg-muted/40 hover:text-foreground"
          >
            <HugeiconsIcon icon={Cancel01Icon} className="size-3.5" />
          </button>
        </div>

        <div className="mt-2 flex flex-col gap-1.5 pl-8.5">
          {/* Project health issues */}
          {issueEntries.map(([projectId, message]) => (
            <div
              key={projectId}
              className="flex items-start gap-2 text-xs text-red-400"
            >
              <span className="mt-px shrink-0 font-medium">project:</span>
              <span className="text-red-300/80">{message}</span>
            </div>
          ))}

          {/* PR creation errors */}
          {prErrors.map((pe) => {
            const isOrgRestricted = pe.message.includes("OAuth App access restrictions");
            return (
              <div
                key={pe.projectId}
                className="flex items-start gap-2 text-xs text-red-400"
              >
                <span className="mt-px shrink-0 font-medium">PR blocked:</span>
                <span className="text-red-300/80">
                  {isOrgRestricted
                    ? "Your GitHub organization restricts OAuth App access. An org admin must approve the Djinn app."
                    : pe.message}
                  {isOrgRestricted && (
                    <a
                      href="https://docs.github.com/articles/restricting-access-to-your-organization-s-data/"
                      target="_blank"
                      rel="noopener noreferrer"
                      className="ml-1.5 text-amber-400 underline underline-offset-2 hover:text-amber-300"
                    >
                      Learn how
                    </a>
                  )}
                </span>
              </div>
            );
          })}

        </div>
      </CardContent>
    </Card>
  );
}
