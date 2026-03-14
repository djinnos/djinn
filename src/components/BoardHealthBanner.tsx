import { useEffect, useState, useCallback } from "react";
import { useStore } from "zustand";
import { Card, CardContent } from "@/components/ui/card";
import { Alert02Icon, Cancel01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { callMcpTool } from "@/api/mcpClient";
import {
  verificationStore,
  type StepEntry,
  type VerificationRun,
} from "@/stores/verificationStore";

interface LspWarning {
  server: string;
  message: string;
}

interface BoardHealthData {
  lspWarnings: LspWarning[];
  projectIssues: Record<string, string>;
  failedSteps: StepEntry[];
  failedRun: VerificationRun | null;
}

function useBoardHealth(projectPath: string | null): BoardHealthData | null {
  const [lspWarnings, setLspWarnings] = useState<LspWarning[]>([]);
  const [projectIssues, setProjectIssues] = useState<Record<string, string>>(
    {}
  );

  const failedRun = useStore(
    verificationStore,
    useCallback(
      (state) => {
        if (!projectPath) return null;
        let latest: VerificationRun | null = null;
        for (const run of state.runs.values()) {
          if (run.projectId !== projectPath) continue;
          if (
            !latest ||
            new Date(run.startedAt).getTime() >
              new Date(latest.startedAt).getTime()
          ) {
            latest = run;
          }
        }
        return latest?.status === "failed" ? latest : null;
      },
      [projectPath]
    )
  );

  const failedSteps = failedRun?.steps.filter((s) => s.status === "failed") ?? [];

  useEffect(() => {
    if (!projectPath) return;

    let active = true;
    const fetch = () => {
      callMcpTool("board_health", { project: projectPath })
        .then((result: Record<string, unknown>) => {
          if (!active) return;
          setLspWarnings(
            (result.lsp_warnings as LspWarning[] | undefined) ?? []
          );
          setProjectIssues(
            (result.project_issues as Record<string, string> | undefined) ?? {}
          );
        })
        .catch(() => {
          // Silently ignore — board_health is optional enrichment
        });
    };

    fetch();
    const interval = setInterval(fetch, 15_000);
    return () => {
      active = false;
      clearInterval(interval);
    };
  }, [projectPath]);

  const hasIssues =
    lspWarnings.length > 0 ||
    Object.keys(projectIssues).length > 0 ||
    failedSteps.length > 0;

  if (!hasIssues) return null;

  return { lspWarnings, projectIssues, failedSteps, failedRun };
}

interface BoardHealthBannerProps {
  projectPath: string;
}

export function BoardHealthBanner({ projectPath }: BoardHealthBannerProps) {
  const health = useBoardHealth(projectPath);
  const [dismissed, setDismissed] = useState(false);

  // Reset dismissed when project changes
  useEffect(() => setDismissed(false), [projectPath]);

  if (!health || dismissed) return null;

  const { lspWarnings, projectIssues, failedSteps, failedRun } = health;
  const issueEntries = Object.entries(projectIssues);
  const totalIssues =
    lspWarnings.length + issueEntries.length + failedSteps.length;

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

          {/* LSP warnings */}
          {lspWarnings.map((w) => (
            <div
              key={w.server}
              className="flex items-start gap-2 text-xs text-amber-300/80"
            >
              <span className="mt-px shrink-0 font-medium text-amber-400">
                {w.server}:
              </span>
              <span>{w.message}</span>
            </div>
          ))}

          {/* Failed verification/setup steps */}
          {failedSteps.map((step) => (
            <div
              key={`${step.phase}-${step.index}`}
              className="flex items-start gap-2 text-xs text-red-400"
            >
              <span className="mt-px shrink-0 font-medium">
                {step.phase === "setup" ? "setup" : "verify"} failed:
              </span>
              <span className="text-red-300/80">
                {step.name}
                {step.command ? (
                  <code className="ml-1.5 rounded bg-white/5 px-1 py-0.5 font-mono text-[10px]">
                    {step.command}
                  </code>
                ) : null}
                {step.exitCode != null ? (
                  <span className="ml-1 text-muted-foreground">
                    (exit {step.exitCode})
                  </span>
                ) : null}
              </span>
            </div>
          ))}

          {/* Show stderr for the first failed step if available */}
          {failedSteps.length > 0 && failedSteps[0].stderr && (
            <pre className="mt-1 max-h-24 overflow-auto rounded bg-black/30 p-2 font-mono text-[10px] leading-relaxed text-red-300/70">
              {failedSteps[0].stderr.trim().slice(0, 500)}
            </pre>
          )}

          {/* Show which task failed if it was task-scoped */}
          {failedRun?.taskId && (
            <span className="text-[10px] text-muted-foreground">
              task: {failedRun.taskId}
            </span>
          )}
        </div>
      </CardContent>
    </Card>
  );
}
