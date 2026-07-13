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
import { useSessionMessages } from "@/hooks/useSessionMessages";
import { SessionThread } from "@/components/SessionThread";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { cn } from "@/lib/utils";
import type { AcceptanceCriterion, Task } from "@/api/types";
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

function SectionHeader({ children }: { children: React.ReactNode }) {
  return (
    <h3 className="text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
      {children}
    </h3>
  );
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

// ── Left panel ───────────────────────────────────────────────────────────────

function TaskSidebar({ task }: { task: Task }) {
  const criteria = (task.acceptance_criteria ?? []).map(parseCriterion);
  return (
    <div className="flex w-80 shrink-0 flex-col gap-4 overflow-y-auto border-r border-border p-4">
      {/* Acceptance Criteria */}
      {criteria.length > 0 && (
        <div className="rounded-lg bg-card p-4 ring-1 ring-foreground/10">
          <SectionHeader>Acceptance Criteria</SectionHeader>
          <ul className="mt-3 space-y-2">
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
        <div className="rounded-lg bg-card p-4 ring-1 ring-foreground/10">
          <SectionHeader>Description</SectionHeader>
          <div className="prose prose-sm max-w-none pt-3 text-xs dark:prose-invert">
            <ReactMarkdown remarkPlugins={[remarkGfm]}>{task.description}</ReactMarkdown>
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
  const projectSlug = selectedProject
    ? `${selectedProject.github_owner}/${selectedProject.github_repo}`
    : null;
  const tasks = useTaskStore((s) => s.tasks);

  // Derive the task straight from the store. `taskStore` replaces the tasks Map
  // on every mutation, so the selector above re-renders on any change — no
  // effect-based syncing or manual subscription is needed.
  const task = taskId ? tasks.get(taskId) ?? null : null;

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

      {/* Content: sidebar + thread */}
      <div className="flex min-h-0 flex-1">
        <TaskSidebar task={task} />
        <div className="flex min-w-0 flex-1 flex-col">
          <SessionThread
            timeline={timeline}
            streamingText={streamingText}
            streamingThinking={streamingThinking}
            loading={loading}
            error={error}
            activeAgentType={activeSession?.agentType}
          />
        </div>
      </div>
    </div>
  );
}
