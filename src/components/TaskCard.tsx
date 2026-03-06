import type { Epic, Task } from "@/types";
import { TaskIdLabel } from "@/components/TaskIdLabel";
import { Card, CardContent } from "@/components/ui/card";
import {
  CheckmarkCircle03Icon,
  FullSignalIcon,
  Loading03Icon,
  LowSignalIcon,
  MediumSignalIcon,
  NoSignalIcon,
  Progress01Icon,
  Task01Icon,
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

const PRIORITY_CONFIG: Record<Task["priority"], { icon: typeof NoSignalIcon; color: string }> = {
  P0: { icon: FullSignalIcon, color: "text-red-500" },
  P1: { icon: MediumSignalIcon, color: "text-yellow-500" },
  P2: { icon: LowSignalIcon, color: "text-green-500" },
  P3: { icon: NoSignalIcon, color: "text-muted-foreground" },
};

function PriorityBadge({ priority }: { priority: Task["priority"] }) {
  const config = PRIORITY_CONFIG[priority];
  return (
    <HugeiconsIcon
      icon={config.icon}
      size={16}
      className={`shrink-0 ${config.color}`}
      aria-label={`Priority ${priority}`}
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

function ownerInitials(owner: string | null): string {
  if (!owner) return "??";
  const parts = owner
    .split(/[\s._-]+/)
    .filter(Boolean)
    .slice(0, 2);
  if (parts.length === 0) return owner.slice(0, 2).toUpperCase();
  return parts.map((p) => p[0]?.toUpperCase() ?? "").join("");
}

export function TaskCard({ task, moving = false, onClick }: TaskCardProps) {
  const isRunning = task.status === "in_progress";
  const [now, setNow] = useState(() => Date.now());

  const runningSessionStartMs = useMemo(() => {
    if (!task.activeSessionStartedAt || task.status !== "in_progress") {
      return null;
    }

    const parsed = Date.parse(task.activeSessionStartedAt);
    return Number.isNaN(parsed) ? null : parsed;
  }, [task.activeSessionStartedAt, task.status]);

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
    const persisted = task.trackedSeconds ?? 0;

    if (!runningSessionStartMs) {
      return persisted;
    }

    const extraSeconds = Math.max(0, Math.floor((now - runningSessionStartMs) / 1000));
    return persisted + extraSeconds;
  }, [now, runningSessionStartMs, task.trackedSeconds]);

  const shouldShowDuration = totalTrackedSeconds > 0 || (task.sessionCount ?? 0) > 0;

  return (
    <Card
      size="sm"
      className={cn(
        "cursor-pointer bg-zinc-800 py-2 ring-white/[0.06] transition-all duration-200 ease-in-out hover:bg-zinc-700/80 hover:ring-white/[0.1]",
        moving ? "scale-[1.02] opacity-70" : "scale-100 opacity-100"
      )}
      onClick={onClick}
    >
      <CardContent className="flex flex-col gap-1.5">
        <div className="flex items-center justify-between gap-2">
          <TaskIdLabel taskId={task.id} shortId={task.shortId} />
          <div
            className="flex h-6 w-6 shrink-0 items-center justify-center rounded-full bg-muted text-[10px] font-semibold uppercase text-muted-foreground"
            title={task.owner ?? "Unassigned"}
            aria-label={`Owner: ${task.owner ?? "Unassigned"}`}
          >
            {ownerInitials(task.owner)}
          </div>
        </div>

        <div className="flex items-start gap-2">
          <div className="mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center">
            {(task.unresolvedBlockerCount ?? 0) > 0 ? (
              <HugeiconsIcon
                icon={UnavailableIcon}
                size={16}
                className="shrink-0 text-red-500"
                aria-label="Blocked"
              />
            ) : isRunning || task.reviewPhase === "in_task_review" ? (
              <HugeiconsIcon
                icon={Loading03Icon}
                size={16}
                className={`shrink-0 animate-spin ${task.reviewPhase === "in_task_review" ? "text-yellow-400" : "text-blue-500"}`}
                aria-label={task.reviewPhase === "in_task_review" ? "In review" : "Task running"}
              />
            ) : task.reviewPhase === "needs_task_review" ? (
              <HugeiconsIcon
                icon={Progress01Icon}
                size={16}
                className="shrink-0 text-yellow-400"
                aria-label="Needs review"
              />
            ) : task.status === "completed" ? (
              <HugeiconsIcon
                icon={CheckmarkCircle03Icon}
                size={16}
                className="shrink-0 text-emerald-500"
                aria-label="Completed"
              />
            ) : (
              <HugeiconsIcon
                icon={Progress01Icon}
                size={16}
                className="shrink-0 text-muted-foreground/40"
                aria-label="Not started"
              />
            )}
          </div>
          <h4 className="font-medium leading-snug" title={task.title}>
            {task.title}
          </h4>
        </div>

        <div className="flex items-center justify-between gap-2">
          <div className="flex items-center gap-2 text-xs text-muted-foreground">
            <PriorityBadge priority={task.priority} />
            <HugeiconsIcon icon={Task01Icon} size={16} className="shrink-0" aria-label="Task" />
          </div>
          {task.sessionModelId || shouldShowDuration ? (
            <div className="flex items-center gap-1.5 text-[10px] text-muted-foreground">
              {task.sessionModelId ? (
                <span className="max-w-[100px] truncate" title={task.sessionModelId}>{task.sessionModelId}</span>
              ) : null}
              {shouldShowDuration ? (
                <span>{formatCompactDuration(totalTrackedSeconds)}</span>
              ) : null}
            </div>
          ) : null}
        </div>
      </CardContent>
    </Card>
  );
}
