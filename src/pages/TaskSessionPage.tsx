/**
 * TaskSessionPage — full-page session viewer at /task/:taskId
 *
 * Left panel: task metadata, acceptance criteria, session list
 * Right panel: unified chat thread (ADR-007)
 */

import { useEffect, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { useSelectedProject } from "@/stores/useProjectStore";
import { useTaskStore } from "@/stores/useTaskStore";
import { taskStore } from "@/stores/taskStore";
import { useEpicStore } from "@/stores/useEpicStore";
import { useSessionMessages, type SessionInfo } from "@/hooks/useSessionMessages";
import { SessionThread } from "@/components/SessionThread";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { cn } from "@/lib/utils";
import type { AcceptanceCriterion, Task } from "@/api/types";
import { ArrowLeft02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

// ── Status labels ────────────────────────────────────────────────────────────

const STATUS_LABELS: Record<string, string> = {
  backlog: "Backlog",
  grooming: "Grooming",
  ready: "Ready",
  open: "Open",
  in_progress: "Coding",
  verifying: "Verifying",
  needs_task_review: "Needs Review",
  in_task_review: "In Review",
  needs_pm_intervention: "PM Intervention",
  in_pm_intervention: "PM Intervening",
  closed: "Done",
};

const STATUS_COLORS: Record<string, string> = {
  open: "bg-blue-500/15 text-blue-400",
  in_progress: "bg-emerald-500/15 text-emerald-400",
  verifying: "bg-yellow-500/15 text-yellow-400",
  needs_task_review: "bg-amber-500/15 text-amber-400",
  in_task_review: "bg-amber-500/15 text-amber-400",
  needs_pm_intervention: "bg-red-500/15 text-red-400",
  in_pm_intervention: "bg-red-500/15 text-red-400",
  closed: "bg-muted text-muted-foreground",
};

// ── Helper components ────────────────────────────────────────────────────────

function parseCriterion(raw: string | AcceptanceCriterion): { criterion: string; met: boolean } {
  if (typeof raw === "string") return { criterion: raw, met: false };
  return { criterion: raw.criterion, met: Boolean(raw.met) };
}

function formatDuration(seconds: number): string {
  if (seconds < 60) return `${seconds}s`;
  const m = Math.floor(seconds / 60);
  const h = Math.floor(m / 60);
  if (h > 0) return `${h}h ${m % 60}m`;
  return `${m}m`;
}

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(0)}k`;
  return String(n);
}

function SectionHeader({ children }: { children: React.ReactNode }) {
  return (
    <h3 className="text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
      {children}
    </h3>
  );
}

// ── Session list sidebar item ────────────────────────────────────────────────

const AGENT_COLORS: Record<string, string> = {
  worker: "text-blue-400",
  task_reviewer: "text-amber-400",
  conflict_resolver: "text-rose-400",
  pm: "text-purple-400",
  epic_reviewer: "text-teal-400",
};

function SessionListItem({ session }: { session: SessionInfo }) {
  const isActive = session.status === "running" || session.status === "active";
  const color = AGENT_COLORS[session.agentType] ?? "text-muted-foreground";

  return (
    <div className="flex items-center gap-2 rounded px-2 py-1.5 text-xs hover:bg-muted/30">
      <span className={cn("font-medium", color)}>{session.agentType}</span>
      <span className="flex-1" />
      {isActive && <span className="h-1.5 w-1.5 animate-pulse rounded-full bg-emerald-400" />}
      <span className="text-muted-foreground">
        {formatTokens(session.tokensIn + session.tokensOut)} tok
      </span>
    </div>
  );
}

// ── Left panel ───────────────────────────────────────────────────────────────

function TaskSidebar({ task, sessions }: { task: Task; sessions: SessionInfo[] }) {
  const epics = useEpicStore((s) => s.epics);
  const epic = task.epic_id ? epics.get(task.epic_id) : undefined;
  const criteria = (task.acceptance_criteria ?? []).map(parseCriterion);
  const acMet = criteria.filter((c: { met: boolean }) => c.met).length;
  const totalDuration = task.duration_seconds ?? 0;

  return (
    <div className="flex w-80 shrink-0 flex-col gap-4 overflow-y-auto border-r border-border p-4">
      {/* Title & status */}
      <div>
        <div className="flex items-center gap-2">
          <span
            className={cn(
              "rounded px-1.5 py-0.5 text-[10px] font-semibold",
              STATUS_COLORS[task.status] ?? "bg-muted text-muted-foreground"
            )}
          >
            {STATUS_LABELS[task.status] ?? task.status}
          </span>
          {task.short_id && (
            <span className="text-[10px] font-medium text-muted-foreground">{task.short_id}</span>
          )}
          {task.reopen_count > 0 && (
            <span className="rounded bg-amber-500/15 px-1.5 py-0.5 text-[10px] font-medium text-amber-400">
              {task.reopen_count}x reopened
            </span>
          )}
        </div>
        <h1 className="mt-2 text-lg font-semibold leading-tight">{task.title}</h1>
        {epic && (
          <p className="mt-1 text-xs text-muted-foreground">{epic.title}</p>
        )}
      </div>

      {/* Stats */}
      <div className="grid grid-cols-2 gap-2 text-xs">
        <div className="rounded bg-muted/30 px-2 py-1.5">
          <div className="text-[10px] text-muted-foreground">Duration</div>
          <div className="font-medium">{totalDuration > 0 ? formatDuration(totalDuration) : "—"}</div>
        </div>
        <div className="rounded bg-muted/30 px-2 py-1.5">
          <div className="text-[10px] text-muted-foreground">Sessions</div>
          <div className="font-medium">{sessions.length}</div>
        </div>
        <div className="rounded bg-muted/30 px-2 py-1.5">
          <div className="text-[10px] text-muted-foreground">AC Progress</div>
          <div className="font-medium">
            {criteria.length > 0 ? `${acMet}/${criteria.length}` : "—"}
          </div>
        </div>
        <div className="rounded bg-muted/30 px-2 py-1.5">
          <div className="text-[10px] text-muted-foreground">Priority</div>
          <div className="font-medium">P{task.priority}</div>
        </div>
      </div>

      {/* Acceptance Criteria */}
      {criteria.length > 0 && (
        <div className="space-y-2">
          <SectionHeader>Acceptance Criteria</SectionHeader>
          <ul className="space-y-1">
            {criteria.map((item: { criterion: string; met: boolean }, idx: number) => (
              <li key={idx} className="flex items-start gap-2 text-xs">
                <span
                  className={cn(
                    "mt-0.5 flex h-3.5 w-3.5 shrink-0 items-center justify-center rounded-sm border text-[9px]",
                    item.met
                      ? "border-emerald-500/50 bg-emerald-500/20 text-emerald-400"
                      : "border-border"
                  )}
                >
                  {item.met ? "✓" : ""}
                </span>
                <span className={item.met ? "text-muted-foreground line-through" : ""}>
                  {item.criterion}
                </span>
              </li>
            ))}
          </ul>
        </div>
      )}

      {/* Description */}
      {task.description && (
        <div className="space-y-2">
          <SectionHeader>Description</SectionHeader>
          <div className="prose prose-sm max-w-none text-xs dark:prose-invert">
            <ReactMarkdown remarkPlugins={[remarkGfm]}>{task.description}</ReactMarkdown>
          </div>
        </div>
      )}

      {/* Sessions */}
      {sessions.length > 0 && (
        <div className="space-y-2">
          <SectionHeader>Sessions</SectionHeader>
          <div className="space-y-0.5">
            {sessions.map((s) => (
              <SessionListItem key={s.id} session={s} />
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

// ── Page ─────────────────────────────────────────────────────────────────────

export function TaskSessionPage() {
  const { taskId } = useParams<{ taskId: string }>();
  const navigate = useNavigate();
  const selectedProject = useSelectedProject();
  const projectPath = selectedProject?.path ?? null;
  const tasks = useTaskStore((s) => s.tasks);
  const [task, setTask] = useState<Task | null>(null);

  // Find task from store
  useEffect(() => {
    if (!taskId) return;
    const found = tasks.get(taskId);
    if (found) setTask(found);
  }, [taskId, tasks]);

  // Keep task updated from store
  useEffect(() => {
    if (!taskId) return;
    const unsub = taskStore.subscribe((state) => {
      const updated = state.tasks.get(taskId);
      if (updated) setTask(updated);
    });
    return unsub;
  }, [taskId]);

  const { timeline, sessions, loading, error, streamingText } = useSessionMessages(
    taskId ?? null,
    projectPath
  );

  // Determine active agent type for streaming display
  const activeSession = sessions.find(
    (s) => s.status === "running" || s.status === "active"
  );

  if (!task) {
    return (
      <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
        Task not found
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col">
      {/* Top bar */}
      <div className="flex shrink-0 items-center gap-3 border-b border-border px-4 py-2">
        <button
          type="button"
          className="rounded p-1 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
          onClick={() => navigate(-1)}
          title="Back to board"
        >
          <HugeiconsIcon icon={ArrowLeft02Icon} size={16} />
        </button>
        <span className="text-sm font-medium">{task.title}</span>
        <span className="text-xs text-muted-foreground">{task.short_id}</span>
        <span className="flex-1" />
        <span
          className={cn(
            "rounded px-1.5 py-0.5 text-[10px] font-semibold",
            STATUS_COLORS[task.status] ?? "bg-muted text-muted-foreground"
          )}
        >
          {STATUS_LABELS[task.status] ?? task.status}
        </span>
      </div>

      {/* Content: sidebar + thread */}
      <div className="flex min-h-0 flex-1">
        <TaskSidebar task={task} sessions={sessions} />
        <div className="flex min-w-0 flex-1 flex-col">
          <SessionThread
            timeline={timeline}
            streamingText={streamingText}
            loading={loading}
            error={error}
            activeAgentType={activeSession?.agentType}
          />
        </div>
      </div>
    </div>
  );
}
