/**
 * TaskSessionPage — full-page session viewer at /task/:taskId
 *
 * Left panel: task metadata, acceptance criteria, session list
 * Right panel: unified chat thread (ADR-007)
 */

import { useEffect, useMemo, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { useProjects, useSelectedProject } from "@/stores/useProjectStore";
import { useTaskStore } from "@/stores/useTaskStore";
import { useSessionMessages } from "@/hooks/useSessionMessages";
import { SessionLedger } from "@/components/session/SessionLedger";
import { buildLedger } from "@/components/session/buildLedger";
import { cn } from "@/lib/utils";
import type { AcceptanceCriterion } from "@/api/types";
import {
  AlertDiamondIcon,
  ArrowLeft02Icon,
  FullSignalIcon,
  LowSignalIcon,
  MediumSignalIcon,
  NoSignalIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { TaskIdLabel } from "@/components/TaskIdLabel";
import { PageHeader } from "@/components/PageHeader";
import { EmptyState } from "@/components/EmptyState";
import { Skeleton, PanelSkeleton, TextSkeleton } from "@/components/ui/skeleton";

// ── Status labels ────────────────────────────────────────────────────────────

const STATUS_LABELS: Record<string, string> = {
  open: "Open",
  in_progress: "Coding",
  needs_task_review: "Needs Review",
  in_task_review: "In Review",
  needs_lead_intervention: "Lead Intervention",
  in_lead_intervention: "Lead Intervening",
  closed: "Done",
};

const STATUS_COLORS: Record<string, string> = {
  open: "bg-blue-500/15 text-blue-400",
  in_progress: "bg-emerald-500/15 text-emerald-400",
  needs_task_review: "bg-amber-500/15 text-amber-400",
  in_task_review: "bg-amber-500/15 text-amber-400",
  needs_lead_intervention: "bg-red-500/15 text-red-400",
  in_lead_intervention: "bg-red-500/15 text-red-400",
  closed: "bg-muted text-muted-foreground",
};

// ── Priority badge ──────────────────────────────────────────────────────────

const PRIORITY_CONFIG: Record<number, { icon: typeof NoSignalIcon; color: string }> = {
  [-1]: { icon: AlertDiamondIcon, color: "text-orange-400" },
  0: { icon: FullSignalIcon, color: "text-[#D1D5DB]" },
  1: { icon: MediumSignalIcon, color: "text-[#9CA3AF]" },
  2: { icon: LowSignalIcon, color: "text-[#6B7280]" },
  3: { icon: NoSignalIcon, color: "text-[#4B5563]" },
};

function PriorityBadge({ priority }: { priority: number }) {
  const config = PRIORITY_CONFIG[priority] ?? PRIORITY_CONFIG[Math.min(Math.max(priority ?? 3, 0), 3)];
  return (
    <HugeiconsIcon
      icon={config.icon}
      size={14}
      className={`shrink-0 ${config.color}`}
      aria-label={priority === -1 ? "Priority Critical" : `Priority P${priority}`}
    />
  );
}

// ── Helper components ────────────────────────────────────────────────────────

function parseCriterion(raw: string | AcceptanceCriterion): { criterion: string; met: boolean } {
  if (typeof raw === "string") return { criterion: raw, met: false };
  return { criterion: raw.criterion, met: Boolean(raw.met) };
}

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(0)}k`;
  return String(n);
}


// ── Token totals derived from sessions ──────────────────────────────────────

function TokenTotals({ sessions }: { sessions: { tokensIn: number; tokensOut: number; cacheReadTokens: number; cacheWriteTokens: number }[] }) {
  const totalIn = sessions.reduce((sum, s) => sum + s.tokensIn, 0);
  const totalOut = sessions.reduce((sum, s) => sum + s.tokensOut, 0);
  const totalCacheRead = sessions.reduce((sum, s) => sum + s.cacheReadTokens, 0);
  const totalCacheWrite = sessions.reduce((sum, s) => sum + s.cacheWriteTokens, 0);
  return (
    <span className="flex items-center gap-1.5 text-[10px] text-muted-foreground">
      <span>{formatTokens(totalIn)} in</span>
      <span>/</span>
      <span>{formatTokens(totalOut)} out</span>
      {(totalCacheRead > 0 || totalCacheWrite > 0) && (
        <span title="Prompt cache: reads (hits) / writes (creation)">
          ({formatTokens(totalCacheRead)} cache hit / {formatTokens(totalCacheWrite)} write)
        </span>
      )}
    </span>
  );
}

// ── Sidebar skeleton ─────────────────────────────────────────────────────────

function TaskSidebarSkeleton() {
  return (
    <div className="flex w-80 shrink-0 flex-col gap-4 overflow-y-auto border-r border-border p-4">
      <PanelSkeleton rowCount={4} />
      <div className="rounded-lg bg-card p-4 ring-1 ring-foreground/10">
        <div className="flex items-center gap-2 pb-2 border-b border-border">
          <Skeleton className="h-4 w-24" />
        </div>
        <div className="pt-3">
          <TextSkeleton lines={4} />
        </div>
      </div>
    </div>
  );
}

// ── Page ─────────────────────────────────────────────────────────────────────

export function TaskSessionPage() {
  const { taskId } = useParams<{ taskId: string }>();
  const navigate = useNavigate();
  const projects = useProjects();
  const selectedProject = useSelectedProject();
  const tasks = useTaskStore((s) => s.tasks);

  // Derive the task straight from the store. `taskStore` replaces the tasks Map
  // on every mutation, so the selector above re-renders on any change — no
  // effect-based syncing or manual subscription is needed.
  const task = taskId ? tasks.get(taskId) ?? null : null;

  // A task belongs to exactly one project, so the task is the authority — not
  // the global selector. Deriving it here is what lets a deep link into a task
  // outside the currently selected project load its sessions at all.
  const project = useMemo(
    () => projects.find((p) => p.id === task?.project_id) ?? selectedProject,
    [projects, task?.project_id, selectedProject]
  );
  const projectSlug = project ? `${project.github_owner}/${project.github_repo}` : null;

  // Give the store a brief moment to populate before showing "not found". The
  // grace window resets whenever the task id changes (render-phase pattern) and
  // only closes from an async timer, keeping this out of the
  // set-state-in-effect anti-pattern.
  const [graceTaskId, setGraceTaskId] = useState(taskId);
  const [graceElapsed, setGraceElapsed] = useState(false);
  if (taskId !== graceTaskId) {
    setGraceTaskId(taskId);
    setGraceElapsed(false);
  }
  useEffect(() => {
    if (!taskId || task) return;
    const timer = setTimeout(() => setGraceElapsed(true), 500);
    return () => clearTimeout(timer);
  }, [taskId, task]);

  const loadingTask = !task && !graceElapsed;

  const { timeline, sessions, loading, error, streamingText, streamingThinking } = useSessionMessages(
    taskId ?? null,
    projectSlug
  );

  // Determine active agent type for streaming display
  const activeSession = sessions.find(
    (s) => s.status === "running" || s.status === "active"
  );

  const ledger = useMemo(
    () =>
      buildLedger({
        timeline,
        sessions,
        description: task?.description ?? undefined,
        criteria: (task?.acceptance_criteria ?? []).map(parseCriterion),
        filedBy: task?.created_by_user_id ?? undefined,
        filedAt: task?.created_at,
      }),
    [timeline, sessions, task]
  );

  // The strand's live tail comes from the stream, not from persisted messages.
  const live = useMemo(() => {
    if (!ledger.live) return null;
    const thinking = activeSession ? streamingThinking.get(activeSession.id) : undefined;
    const text = activeSession ? streamingText.get(activeSession.id) : undefined;
    const tail = (thinking ?? text ?? "").split("\n").find((l) => l.trim());
    return {
      ...ledger.live,
      nowLabel: tail ? tail.replace(/\*\*/g, "").slice(0, 120) : "working",
    };
  }, [ledger.live, activeSession, streamingThinking, streamingText]);

  const blockers = useMemo(
    () =>
      (task?.unresolved_blocker_count ?? 0) > 0
        ? [`${task?.unresolved_blocker_count} unresolved`]
        : [],
    [task?.unresolved_blocker_count]
  );

  // Loading / skeleton state
  if (loadingTask || loading) {
    return (
      <div className="flex h-full flex-col">
        <div className="shrink-0 border-b border-border px-4 py-3">
          <div className="flex items-center gap-3">
            <Skeleton className="h-8 w-8 rounded" />
            <Skeleton className="h-5 w-48" />
            <span className="flex-1" />
            <Skeleton className="h-4 w-24" />
          </div>
        </div>
        <div className="flex min-h-0 flex-1">
          <TaskSidebarSkeleton />
          <div className="flex min-w-0 flex-1 flex-col p-4">
            <Skeleton className="h-20 w-full" />
            <Skeleton className="mt-2 h-20 w-5/6" />
            <Skeleton className="mt-2 h-20 w-4/5" />
          </div>
        </div>
      </div>
    );
  }

  // Not-found state
  if (!task) {
    return (
      <div className="flex h-full flex-col">
        <div className="flex-1 p-6">
          <EmptyState
            title="Task not found"
            message="The task you're looking for doesn't exist or you don't have access to it."
            actionLabel="Go back"
            onAction={() => navigate(-1)}
          />
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col">
      {/* Top bar via PageHeader */}
      <PageHeader
        title={task.title}
        leading={
          <button
            type="button"
            className="rounded p-1 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
            onClick={() => navigate(-1)}
            title="Back to board"
          >
            <HugeiconsIcon icon={ArrowLeft02Icon} size={16} />
          </button>
        }
        actions={
          <>
            <PriorityBadge priority={task.priority} />
            <TaskIdLabel taskId={task.id} shortId={task.short_id} />
            {sessions.length > 0 && <TokenTotals sessions={sessions} />}
            <span
              className={cn(
                "rounded px-1.5 py-0.5 text-[10px] font-semibold",
                STATUS_COLORS[task.status] ?? "bg-muted text-muted-foreground"
              )}
            >
              {STATUS_LABELS[task.status] ?? task.status}
            </span>
          </>
        }
        className="shrink-0 border-b border-border px-4 py-2 mb-0"
      />

      {/* Content: the ledger owns the rail and the thread */}
      <div className="flex min-h-0 flex-1">
        <SessionLedger
          showHeader={false}
          taskShortId={task.short_id}
          taskTitle={task.title}
          statusLabel={STATUS_LABELS[task.status] ?? task.status}
          criteria={ledger.criteria}
          agents={ledger.agents}
          blockers={blockers}
          entries={ledger.entries}
          live={live}
          emptyMessage={error ?? "No session activity yet."}
        />
      </div>
    </div>
  );
}
