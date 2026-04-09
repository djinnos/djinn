import type { ReactNode } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import { Pulse01Icon } from "@hugeicons/core-free-icons";
import { Card } from "@/components/ui/card";
import { relativeTime } from "@/components/memory/memoryUtils";
import { cn } from "@/lib/utils";

interface FreshnessStripProps {
  lastWarmAt: string | null;
  pinnedCommit: string | null;
  commitsSincePin: number | null;
  architectActive: boolean;
  actions?: ReactNode;
}

function shortSha(sha: string | null): string {
  if (!sha) return "—";
  return sha.length > 7 ? sha.slice(0, 7) : sha;
}

export function FreshnessStrip({
  lastWarmAt,
  pinnedCommit,
  commitsSincePin,
  architectActive,
  actions,
}: FreshnessStripProps) {
  const lastPatrol = lastWarmAt ? relativeTime(lastWarmAt) : "not yet";
  const sha = shortSha(pinnedCommit);
  const drift = (commitsSincePin ?? 0) > 0;

  return (
    <Card size="sm" className="px-4 py-3">
      <div className="flex items-center gap-3">
        <span
          className={cn(
            "flex h-8 w-8 items-center justify-center rounded-full bg-emerald-500/10 text-emerald-400 animate-pulse",
            architectActive ? "[animation-duration:1.2s]" : "[animation-duration:3.5s]"
          )}
        >
          <HugeiconsIcon icon={Pulse01Icon} className="h-4 w-4" />
        </span>
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2 text-sm text-foreground">
            <span className="truncate">
              Last patrol {lastPatrol}
              {pinnedCommit && (
                <>
                  <span className="text-muted-foreground"> · pinned to origin/main @ </span>
                  <span className="font-mono">{sha}</span>
                </>
              )}
            </span>
            {architectActive && (
              <span className="shrink-0 rounded-full bg-emerald-500/10 px-2 py-0.5 text-[11px] font-medium text-emerald-400">
                patrolling…
              </span>
            )}
          </div>
          {drift && (
            <p className="mt-0.5 text-xs text-muted-foreground">
              {commitsSincePin} commits since patrol · architect will refresh on next dispatch
            </p>
          )}
        </div>
        {actions && <div className="shrink-0">{actions}</div>}
      </div>
    </Card>
  );
}
