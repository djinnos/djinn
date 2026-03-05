import { showToast } from "@/lib/toast";
import type { Epic, Task } from "@/types";
import { Clock3 } from "lucide-react";
import { useEffect, useMemo, useState } from "react";

type TaskCardProps = {
  task: Task;
  epic?: Epic;
  moving?: boolean;
  onClick?: () => void;
};

const PRIORITY_BAR_COLORS: Record<Task["priority"], string> = {
  P0: "bg-red-500",
  P1: "bg-orange-500",
  P2: "bg-amber-500",
  P3: "bg-gray-400",
};

const PRIORITY_BAR_COUNT: Record<Task["priority"], number> = {
  P0: 4,
  P1: 3,
  P2: 2,
  P3: 1,
};

function PriorityBars({ priority }: { priority: Task["priority"] }) {
  const activeBars = PRIORITY_BAR_COUNT[priority];
  const activeColor = PRIORITY_BAR_COLORS[priority];

  return (
    <span
      className="inline-flex h-4 items-end gap-0.5"
      title={`Priority ${priority}`}
      aria-label={`Priority ${priority}`}
    >
      {[0, 1, 2, 3].map((bar) => {
        const height = ["h-1.5", "h-2.5", "h-3.5", "h-4"][bar];
        const isActive = bar < activeBars;
        return (
          <span
            key={bar}
            className={`w-1 rounded-sm ${height} ${isActive ? activeColor : "bg-muted"}`}
            aria-hidden="true"
          />
        );
      })}
    </span>
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

function getEpicEmoji(epic: Epic | undefined): string {
  if (!epic) return "📌";
  if (epic.status === "active") return "🚀";
  if (epic.status === "completed") return "✅";
  return "📦";
}

function getEpicDotColor(epic: Epic | undefined): string {
  if (!epic) return "bg-gray-400";
  if (epic.status === "active") return "bg-emerald-500";
  if (epic.status === "completed") return "bg-blue-500";
  return "bg-violet-500";
}

function getReviewIndicator(reviewPhase: Task["reviewPhase"]): { dotClass: string; animateClass?: string; title: string } | null {
  if (reviewPhase === "needs_task_review") {
    return { dotClass: "bg-amber-500", title: "Waiting for review" };
  }
  if (reviewPhase === "in_task_review") {
    return { dotClass: "bg-blue-500", animateClass: "animate-spin", title: "Agent reviewing" };
  }
  return null;
}


function RunningSpinner() {
  return (
    <span
      className="inline-block h-3 w-3 shrink-0 animate-spin rounded-full border border-blue-500 border-t-transparent opacity-80"
      title="Task running"
      aria-label="Task running"
    />
  );
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

async function copyTaskId(taskId: string): Promise<void> {
  await navigator.clipboard.writeText(taskId);
  showToast.success("Task ID copied");
}

export function TaskCard({ task, epic, moving = false, onClick }: TaskCardProps) {
  const reviewIndicator = getReviewIndicator(task.reviewPhase);
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
    <article
      className={`rounded border bg-card p-2 text-sm transition-all duration-200 ease-in-out hover:-translate-y-px hover:shadow-sm ${moving ? "scale-[1.02] opacity-70" : "scale-100 opacity-100"} ${onClick ? "cursor-pointer" : ""}`}
      onClick={onClick}
    >
      <div className="mb-1 flex items-center gap-1 text-[10px] text-muted-foreground">
        <span className="font-semibold uppercase">{task.shortId ?? task.id.slice(0, 4)}</span>
        <button
          type="button"
          className="inline-flex h-4 w-4 items-center justify-center rounded hover:bg-muted"
          aria-label="Copy task ID"
          title="Copy full task ID"
          onClick={(event) => {
            event.stopPropagation();
            void copyTaskId(task.id);
          }}
        >
          ⧉
        </button>
      </div>

      <div className="mb-2 flex items-start justify-between gap-2">
        <h4 className="truncate font-medium" title={task.title}>
          {task.title}
        </h4>
        {task.status === "in_progress" ? <RunningSpinner /> : null}
        {reviewIndicator ? (
          <span
            className={`h-2 w-2 shrink-0 rounded-full ${reviewIndicator.dotClass} ${reviewIndicator.animateClass ?? ""}`}
            title={reviewIndicator.title}
            aria-label={reviewIndicator.title}
          />
        ) : null}
        <PriorityBars priority={task.priority} />
      </div>

      {shouldShowDuration ? (
        <div className="mb-2 flex items-center gap-1 text-xs text-muted-foreground" title="Time spent">
          <Clock3 className="h-3 w-3 shrink-0" aria-hidden="true" />
          <span>{formatCompactDuration(totalTrackedSeconds)}</span>
        </div>
      ) : null}

      <div className="flex items-center justify-between gap-2 text-xs text-muted-foreground">
        <div className="flex min-w-0 items-center gap-1" title={epic?.title ?? "No Epic"}>
          <span className={`h-2 w-2 shrink-0 rounded-full ${getEpicDotColor(epic)}`} aria-hidden="true" />
          <span role="img" aria-label="epic emoji" className="shrink-0">
            {getEpicEmoji(epic)}
          </span>
          <span className="truncate">{epic?.title ?? "No Epic"}</span>
        </div>

        <div
          className="flex h-6 w-6 shrink-0 items-center justify-center rounded-full border bg-background text-[10px] font-semibold uppercase"
          title={task.owner ?? "Unassigned"}
          aria-label={`Owner: ${task.owner ?? "Unassigned"}`}
        >
          {ownerInitials(task.owner)}
        </div>
      </div>
    </article>
  );
}
