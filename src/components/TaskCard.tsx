import type { Epic, Task } from "@/api/types";
import { TaskIdLabel } from "@/components/TaskIdLabel";
import { Card, CardContent } from "@/components/ui/card";
import {
  ArrowReloadHorizontalIcon,
  FullSignalIcon,
  LowSignalIcon,
  MediumSignalIcon,
  NoSignalIcon,
  UnavailableIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { cn } from "@/lib/utils";
import { useEffect, useMemo, useState } from "react";

type TaskCardProps = {
  task: Task;
  epic?: Epic;
  moving?: boolean;
  onClick?: () => void;
};

const PRIORITY_CONFIG: Record<number, { icon: typeof NoSignalIcon; color: string }> = {
  0: { icon: FullSignalIcon, color: "text-red-500" },
  1: { icon: MediumSignalIcon, color: "text-yellow-500" },
  2: { icon: LowSignalIcon, color: "text-green-500" },
  3: { icon: NoSignalIcon, color: "text-muted-foreground" },
};

function PriorityBadge({ priority }: { priority: number }) {
  const config = PRIORITY_CONFIG[Math.min(Math.max(priority, 0), 3)];
  return (
    <HugeiconsIcon
      icon={config.icon}
      size={14}
      className={`shrink-0 ${config.color}`}
      aria-label={`Priority P${priority}`}
    />
  );
}

function formatCompactDuration(totalSeconds: number): string {
  const safeSeconds = Math.max(0, totalSeconds);
  const totalMinutes = Math.floor(safeSeconds / 60);
  const hours = Math.floor(totalMinutes / 60);
  const minutes = totalMinutes % 60;

  if (hours > 0) {
    return `${hours}h ${minutes}m`;
  }

  return `${minutes}m`;
}

// --- Pipeline dots for in-flight cards ---

type PipelineStage = "coding" | "verifying" | "reviewing";

const PIPELINE_STAGES: PipelineStage[] = ["coding", "verifying", "reviewing"];

function statusToPipelineStage(status: string): PipelineStage | null {
  if (status === "in_progress") return "coding";
  if (status === "verifying") return "verifying";
  if (status === "needs_task_review" || status === "in_task_review") return "reviewing";
  return null;
}

function getStatusOverlay(status: string): { label: string; className: string } | null {
  if (status === "conflict_resolution") {
    return { label: "resolving…", className: "text-orange-400 animate-pulse" };
  }
  if (status === "needs_pm_intervention" || status === "in_pm_intervention") {
    return { label: "intervening…", className: "text-red-400 animate-pulse" };
  }
  return null;
}

function PipelineIndicator({ status }: { status: string }) {
  const overlay = getStatusOverlay(status);
  if (overlay) {
    return (
      <span className={cn("text-[10px] font-medium", overlay.className)}>
        {overlay.label}
      </span>
    );
  }

  const activeStage = statusToPipelineStage(status);
  if (!activeStage) return null;

  const activeIdx = PIPELINE_STAGES.indexOf(activeStage);

  return (
    <div className="flex items-center gap-0.5" aria-label={`Pipeline: ${activeStage}`}>
      {PIPELINE_STAGES.map((stage, idx) => (
        <div key={stage} className="flex items-center">
          {idx > 0 && (
            <div
              className={cn(
                "mx-px h-px w-1.5",
                idx <= activeIdx ? "bg-emerald-400/60" : "bg-blue-400/20"
              )}
            />
          )}
          <div
            className={cn(
              "size-1.5 rounded-full",
              idx < activeIdx
                ? "bg-emerald-400"
                : idx === activeIdx
                  ? "bg-blue-400 animate-pulse"
                  : "bg-blue-400/40"
            )}
            title={stage}
          />
        </div>
      ))}
    </div>
  );
}

// --- Card tint based on status ---

function getCardTint(task: Task): { ring: string; bg: string } | null {
  if (task.status === "conflict_resolution") {
    return { ring: "ring-orange-500/40", bg: "bg-orange-500/5" };
  }
  if (task.status === "needs_pm_intervention" || task.status === "in_pm_intervention") {
    return { ring: "ring-red-500/40", bg: "bg-red-500/5" };
  }
  return null;
}

// --- Backlog badge ---

function getBacklogBadge(status: string): { label: string; className: string } | null {
  if (status === "grooming" || status === "backlog") {
    return { label: "grooming", className: "text-zinc-400 bg-zinc-400/10" };
  }
  if (status === "ready") {
    return { label: "ready", className: "text-violet-400 bg-violet-400/10" };
  }
  return null;
}

export function TaskCard({ task, moving = false, onClick }: TaskCardProps) {
  const [now, setNow] = useState(() => Date.now());

  const runningSessionStartMs = useMemo(() => {
    if (!task.active_session?.started_at || task.status !== "in_progress") {
      return null;
    }

    const parsed = Date.parse(task.active_session.started_at);
    return Number.isNaN(parsed) ? null : parsed;
  }, [task.active_session?.started_at, task.status]);

  useEffect(() => {
    if (!runningSessionStartMs) {
      return;
    }

    const interval = window.setInterval(() => {
      setNow(Date.now());
    }, 1000);

    return () => {
      window.clearInterval(interval);
    };
  }, [runningSessionStartMs]);

  const totalTrackedSeconds = useMemo(() => {
    const persisted = task.duration_seconds ?? 0;

    if (!runningSessionStartMs) {
      return persisted;
    }

    const extraSeconds = Math.max(0, Math.floor((now - runningSessionStartMs) / 1000));
    return persisted + extraSeconds;
  }, [now, runningSessionStartMs, task.duration_seconds]);

  const shouldShowDuration = totalTrackedSeconds > 0 || (task.session_count ?? 0) > 0;
  const isInFlight =
    task.status === "in_progress" ||
    task.status === "verifying" ||
    task.status === "needs_task_review" ||
    task.status === "in_task_review" ||
    task.status === "needs_pm_intervention" ||
    task.status === "in_pm_intervention" ||
    task.status === "conflict_resolution";
  const isDone = task.status === "closed";
  const hasBlockers = (task.unresolved_blocker_count ?? 0) > 0;
  const cardTint = getCardTint(task);
  const backlogBadge = getBacklogBadge(task.status);

  return (
    <Card
      size="sm"
      className={cn(
        "cursor-pointer py-2 ring-1 transition-all duration-200 ease-in-out hover:bg-zinc-700/80 hover:ring-white/[0.1]",
        cardTint ? `${cardTint.ring} ${cardTint.bg}` : "bg-zinc-800 ring-white/[0.06]",
        moving ? "scale-[1.02] opacity-70" : "scale-100 opacity-100"
      )}
      onClick={onClick}
    >
      <CardContent className="flex flex-col gap-1.5">
        {/* Row 1: ID, priority, badges, pipeline */}
        <div className="flex items-center gap-2 text-[11px] text-muted-foreground">
          <TaskIdLabel taskId={task.id} shortId={task.short_id} />
          <PriorityBadge priority={task.priority} />

          {/* Blocker badge */}
          {hasBlockers && (
            <span className="inline-flex items-center gap-0.5 rounded bg-red-500/15 px-1 py-px text-[10px] font-medium text-red-400">
              <HugeiconsIcon icon={UnavailableIcon} size={10} className="shrink-0" />
              {task.unresolved_blocker_count}
            </span>
          )}

          {/* Reopen badge */}
          {task.reopen_count > 0 && (
            <span className="inline-flex items-center gap-0.5 rounded bg-amber-500/15 px-1 py-px text-[10px] font-medium text-amber-400">
              <HugeiconsIcon icon={ArrowReloadHorizontalIcon} size={10} className="shrink-0" />
              {task.reopen_count}
            </span>
          )}

          {/* Backlog badge (grooming / ready) */}
          {backlogBadge && (
            <span className={cn("rounded px-1 py-px text-[10px] font-medium", backlogBadge.className)}>
              {backlogBadge.label}
            </span>
          )}

          {/* Spacer */}
          <div className="flex-1" />

          {/* Pipeline indicator for in-flight */}
          {isInFlight && <PipelineIndicator status={task.status} />}

          {/* Duration & model for in-flight / done */}
          {shouldShowDuration && (
            <span className="text-[10px]">{formatCompactDuration(totalTrackedSeconds)}</span>
          )}
          {task.active_session?.model_id && (
            <span className="max-w-[80px] truncate text-[10px]" title={task.active_session.model_id}>
              {task.active_session.model_id}
            </span>
          )}
        </div>

        {/* Row 2: Title */}
        <h4
          className={cn(
            "text-sm font-medium leading-snug",
            isDone && "text-muted-foreground line-through decoration-muted-foreground/30"
          )}
          title={task.title}
        >
          {task.title}
        </h4>
      </CardContent>
    </Card>
  );
}
