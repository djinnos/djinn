import type { Epic, Task } from "@/api/types";
import { getAgentAvatar } from "@/lib/agentIdentity";

import { TaskIdLabel } from "@/components/TaskIdLabel";
import { Card, CardContent } from "@/components/ui/card";
import {
  AlertDiamondIcon,
  ArrowReloadHorizontalIcon,
  FullSignalIcon,
  LowSignalIcon,
  MediumSignalIcon,
  NoSignalIcon,
  Progress01Icon,
  Progress02Icon,
  Progress03Icon,
  Progress04Icon,
  Tick01Icon,
  UnavailableIcon,
  LinkSquare02Icon,
  GitMergeIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { openUrl } from "@/electron/shims/opener";
import { cn } from "@/lib/utils";
import { useEffect, useMemo, useState } from "react";
import { useIsAllProjects } from "@/stores/useProjectStore";
import { projectStore } from "@/stores/projectStore";
import { verificationStore } from "@/stores/verificationStore";
import { useStoreWithEqualityFn } from "zustand/traditional";

type TaskCardProps = {
  task: Task;
  epic?: Epic;
  moving?: boolean;
  onClick?: () => void;
};

const ISSUE_TYPE_CONFIG: Record<string, { label: string; className: string }> = {
  feature: { label: "feature", className: "bg-emerald-500/15 text-emerald-400" },
  bug: { label: "bug", className: "bg-red-500/15 text-red-400" },
  spike: { label: "spike", className: "bg-amber-500/15 text-amber-400" },
  research: { label: "research", className: "bg-violet-500/15 text-violet-400" },
  decomposition: { label: "breakdown", className: "bg-cyan-500/15 text-cyan-400" },
  review: { label: "review", className: "bg-lime-500/15 text-lime-400" },
};

function IssueTypeBadge({ issueType }: { issueType: string }) {
  const config = ISSUE_TYPE_CONFIG[issueType];
  if (!config) return null;
  return (
    <span className={cn("rounded px-1 py-px text-[10px] font-medium", config.className)}>
      {config.label}
    </span>
  );
}

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

function formatCompactDuration(totalSeconds: number): string {
  const safeSeconds = Math.max(0, totalSeconds);
  const totalMinutes = Math.floor(safeSeconds / 60);
  const hours = Math.floor(totalMinutes / 60);
  const minutes = totalMinutes % 60;

  if (hours > 0) {
    return `${hours}h ${minutes}m`;
  }

  if (totalMinutes > 0) {
    return `${minutes}m`;
  }

  return `${safeSeconds}s`;
}

// --- Status badge for in-flight cards ---

function getStatusBadge(status: string, hasSession: boolean, allAcMet: boolean, hasSetupStep: boolean): { label: string; className: string } | null {
  if (status === "needs_lead_intervention" && !hasSession) {
    return { label: "agent stuck", className: "text-red-400" };
  }
  if (status === "verifying") {
    return { label: "verifying", className: "text-yellow-400 animate-pulse" };
  }
  if ((status === "needs_task_review" || status === "in_task_review") && !hasSession && allAcMet) {
    return { label: "merging", className: "text-green-400 animate-pulse" };
  }
  if (status === "in_progress" && (!hasSession || hasSetupStep)) {
    return { label: "setting up", className: "text-blue-400 animate-pulse" };
  }
  return null;
}

function StatusBadge({ status, hasSession, allAcMet, hasSetupStep, tooltip }: { status: string; hasSession: boolean; allAcMet: boolean; hasSetupStep: boolean; tooltip?: string }) {
  const badge = getStatusBadge(status, hasSession, allAcMet, hasSetupStep);
  if (!badge) return null;
  return (
    <span className={cn("text-[10px] font-medium", badge.className)} title={tooltip || undefined}>
      {badge.label}
    </span>
  );
}

function getTaskRunningStep(taskId: string, state: ReturnType<typeof verificationStore.getState>) {
  const lifecycle = state.lifecycleSteps.get(taskId) ?? [];
  const run = Array.from(state.runs.values()).find((candidate) => candidate.taskId === taskId);
  const verificationSteps = run?.steps ?? [];

  const runningLifecycle = lifecycle[lifecycle.length - 1];
  if (runningLifecycle) {
    return {
      phase: "setup" as const,
      name: runningLifecycle.detail ? `${runningLifecycle.step}: ${runningLifecycle.detail}` : runningLifecycle.step,
    };
  }

  const runningVerification = verificationSteps.find((step) => step.status === "running");
  if (runningVerification) {
    return {
      phase: "verification" as const,
      name: runningVerification.name,
    };
  }

  return null;
}

// --- Card tint based on status ---

function getCardTint(task: Task): { ring: string; bg: string; hover: string; actionsBg: string } | null {
  if (task.status === "needs_lead_intervention" && !task.active_session) {
    return { ring: "ring-red-500/40", bg: "bg-red-500/5", hover: "hover:bg-red-500/10 hover:ring-red-500/60", actionsBg: "bg-red-500/10 text-white" };
  }
  if ((task.status === "needs_lead_intervention" && task.active_session) || task.status === "in_lead_intervention") {
    return { ring: "ring-red-500/40", bg: "bg-red-500/5", hover: "hover:bg-red-500/10 hover:ring-red-500/60", actionsBg: "bg-red-500/10 text-white" };
  }
  return null;
}

function agentAvatar(agentType?: string): string {
  return getAgentAvatar(agentType);
}

function acProgressIcon(met: number, total: number) {
  if (met === total) return Tick01Icon;
  if (met === 0) return Progress01Icon;
  const pct = met / total;
  if (pct <= 0.25) return Progress02Icon;
  if (pct <= 0.5) return Progress03Icon;
  return Progress04Icon;
}

function ProjectBadge({ projectId }: { projectId?: string }) {
  const isAll = useIsAllProjects();
  if (!isAll || !projectId) return null;
  const name = projectStore.getState().projects.find((p) => p.id === projectId)?.name;
  if (!name) return null;
  return (
    <span className="rounded bg-zinc-600/40 px-1 py-px text-[9px] font-medium text-zinc-400">
      {name}
    </span>
  );
}

export function TaskCard({ task, moving = false, onClick }: TaskCardProps) {
  const [now, setNow] = useState(() => Date.now());

  const runningSessionStartMs = useMemo(() => {
    if (!task.active_session?.started_at) {
      return null;
    }

    const parsed = Date.parse(task.active_session.started_at);
    return Number.isNaN(parsed) ? null : parsed;
  }, [task.active_session?.started_at]);

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

  const shouldShowDuration = totalTrackedSeconds > 0 || !!runningSessionStartMs;
  const isInFlight =
    task.status === "in_progress" ||
    task.status === "verifying" ||
    task.status === "needs_task_review" ||
    task.status === "in_task_review" ||
    task.status === "needs_lead_intervention" ||
    task.status === "in_lead_intervention";
  const isDone = task.status === "closed";
  const hasBlockers = (task.unresolved_blocker_count ?? 0) > 0;
  const ac = task.acceptance_criteria ?? [];
  const acTotal = ac.length;
  const acMet = ac.filter((c: { met?: boolean }) => c.met).length;
  const cardTint = getCardTint(task);
  const runningStep = useStoreWithEqualityFn(verificationStore, (state) => getTaskRunningStep(task.id, state));
  const isSettingUp = task.status === "in_progress" && runningStep?.phase === "setup";
  const statusLabel =
    isSettingUp
      ? `setting up: ${runningStep.name}`
      : task.status === "verifying" && runningStep?.phase === "verification"
        ? `verifying: ${runningStep.name}`
        : null;
  // Build tooltip for the "setting up" badge: shows step, duration, and model
  const setupTooltip = isSettingUp
    ? [
        runningStep.name,
        shouldShowDuration ? formatCompactDuration(totalTrackedSeconds) : null,
        task.active_session?.model_id,
      ].filter(Boolean).join(" · ")
    : undefined;

  return (
    <Card
      size="sm"
      className={cn(
        "group/taskcard relative cursor-pointer py-2 ring-1 transition-all duration-200 ease-in-out",
        cardTint ? `${cardTint.ring} ${cardTint.bg} ${cardTint.hover}` : "bg-zinc-800 ring-white/[0.06] hover:bg-zinc-700/80 hover:ring-white/[0.1]",
        moving ? "scale-[1.02] opacity-70" : "scale-100 opacity-100"
      )}
      onClick={onClick}
    >
      <CardContent className="flex min-h-[3.5rem] flex-col gap-1.5">
        {/* Row 1: ID, priority, badges, pipeline */}
        <div className="flex items-center gap-2 overflow-hidden text-[11px] text-muted-foreground">
          <TaskIdLabel taskId={task.id} shortId={task.short_id} />
          <ProjectBadge projectId={task.project_id ?? undefined} />
          <PriorityBadge priority={task.priority} />

          {/* Issue type badge – shown for non-default types */}
          {task.issue_type && task.issue_type !== "task" && (
            <IssueTypeBadge issueType={task.issue_type} />
          )}

          {/* Acceptance criteria progress */}
          {acTotal > 0 && (
            <span className={cn(
              "inline-flex items-center gap-0.5 rounded px-1 py-px text-[10px] font-medium",
              acMet === acTotal
                ? "bg-emerald-500/15 text-emerald-400"
                : acMet === 0
                  ? "bg-zinc-500/10 text-muted-foreground"
                  : "bg-amber-500/15 text-amber-400"
            )}>
              <HugeiconsIcon icon={acProgressIcon(acMet, acTotal)} size={10} className="shrink-0" />
              {acMet}/{acTotal}
            </span>
          )}

          {/* Blocker badge */}
          {hasBlockers && (
            <span className="inline-flex items-center gap-0.5 rounded bg-red-500/15 px-1 py-px text-[10px] font-medium text-red-400">
              <HugeiconsIcon icon={UnavailableIcon} size={10} className="shrink-0" />
              {task.unresolved_blocker_count}
            </span>
          )}

          {/* Merge conflict badge */}
          {task.merge_conflict_metadata && (
            <span
              className="inline-flex items-center gap-0.5 rounded bg-rose-500/15 px-1 py-px text-[10px] font-medium text-rose-400"
              title={
                typeof task.merge_conflict_metadata === "object" && task.merge_conflict_metadata !== null
                  ? (task.merge_conflict_metadata as { conflicting_files?: string[] }).conflicting_files?.join(", ") ?? "merge conflict"
                  : "merge conflict"
              }
            >
              <HugeiconsIcon icon={GitMergeIcon} size={10} className="shrink-0" />
              conflict
            </span>
          )}

          {/* PR URL link */}
          {task.pr_url && (
            <a
              href={task.pr_url}
              target="_blank"
              rel="noopener noreferrer"
              onClick={(e) => {
                e.preventDefault();
                e.stopPropagation();
                openUrl(task.pr_url!);
              }}
              className="inline-flex shrink-0 items-center gap-0.5 text-[10px] font-medium text-violet-400 hover:text-violet-300 hover:underline"
              title={task.pr_url}
            >
              <HugeiconsIcon icon={LinkSquare02Icon} size={10} className="shrink-0" />
              {task.pr_url.match(/\/pull\/(\d+)/)?.[0]?.replace("/pull/", "PR #") ?? "PR"}
            </a>
          )}

          {/* Reopen badge */}
          {task.reopen_count > 0 && (
            <span className="inline-flex items-center gap-0.5 rounded bg-amber-500/15 px-1 py-px text-[10px] font-medium text-amber-400">
              <HugeiconsIcon icon={ArrowReloadHorizontalIcon} size={10} className="shrink-0" />
              {task.reopen_count}
            </span>
          )}

          {/* Spacer */}
          <div className="flex-1" />

          {/* Status badge for in-flight */}
          {isInFlight && (
            <span className="inline-flex items-center gap-1" data-testid="taskcard-status-badge">
              <StatusBadge status={task.status} hasSession={!!task.active_session} allAcMet={acTotal > 0 && acMet === acTotal} hasSetupStep={!!isSettingUp} tooltip={setupTooltip} />
              {statusLabel && !isSettingUp && <span className="text-[10px] font-medium text-muted-foreground">{statusLabel}</span>}
            </span>
          )}

          {/* Duration & model for in-flight / done (hidden during setup — shown in tooltip) */}
          {shouldShowDuration && !isSettingUp && (
            <span className="text-[10px]">{formatCompactDuration(totalTrackedSeconds)}</span>
          )}
          {task.active_session?.model_id && !isSettingUp && (
            <span className="min-w-0 shrink truncate text-[10px]" title={task.active_session.model_id}>
              {task.active_session.model_id}
            </span>
          )}

        </div>

        {/* Row 2: Title */}
        <h4
          className={cn(
            "text-sm font-medium leading-snug",
            task.active_session && "pr-12",
            isDone && "text-muted-foreground line-through decoration-muted-foreground/30"
          )}
          title={task.title}
        >
          {task.title}
        </h4>

        {task.labels?.length > 0 && (
          <div className="flex flex-wrap gap-1">
            {task.labels.map((label: string) => (
              <span
                key={label}
                className="rounded bg-zinc-700/60 px-1.5 py-0.5 text-[10px] text-zinc-200"
              >
                {label}
              </span>
            ))}
          </div>
        )}

        {/* Agent avatar – shown when task has an active session (hidden during setup) */}
        {task.active_session && !isSettingUp && (
          <img
            src={agentAvatar(task.active_session.agent_type)}
            alt={task.active_session.agent_type ?? "agent"}
            className="pointer-events-none absolute bottom-0 right-1 h-12 w-12"
          />
        )}

      </CardContent>

    </Card>
  );
}

export function DoneTaskRow({ task, onClick }: { task: Task; onClick?: () => void }) {
  const duration = task.duration_seconds ?? 0;

  return (
    <button
      type="button"
      className="flex w-full cursor-pointer items-center gap-2 rounded-md px-1.5 py-0.5 text-left text-[11px] leading-tight text-muted-foreground transition-colors hover:bg-muted/40"
      onClick={onClick}
      title={task.title}
    >
      <TaskIdLabel taskId={task.id} shortId={task.short_id} />
      <PriorityBadge priority={task.priority} />
      {task.issue_type && task.issue_type !== "task" && (
        <IssueTypeBadge issueType={task.issue_type} />
      )}
      <span className="min-w-0 flex-1 truncate">{task.title}</span>
      {duration > 0 && (
        <span className="shrink-0 text-[10px]">{formatCompactDuration(duration)}</span>
      )}
    </button>
  );
}
