import type { Epic, Task, CiGateSnapshot } from "@/api/types";
import { getAgentAvatar } from "@/lib/agentIdentity";

import { TaskIdLabel } from "@/components/TaskIdLabel";
import { AcceptanceProgressBadge } from "@/components/AcceptanceProgressBadge";
import { Card, CardContent } from "@/components/ui/card";
import {
  AlertDiamondIcon,
  ArrowReloadHorizontalIcon,
  FullSignalIcon,
  LowSignalIcon,
  MediumSignalIcon,
  NoSignalIcon,
  UnavailableIcon,
  LinkSquare02Icon,
  GitMergeIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { openUrl } from "@/lib/openUrl";
import { cn } from "@/lib/utils";
import { useEffect, useMemo, useState } from "react";
import { useIsAllProjects } from "@/stores/useProjectStore";
import { projectStore } from "@/stores/projectStore";

type TaskCardProps = {
  task: Task;
  /** When provided, the card names its epic (emoji + title) at the bottom —
   * lanes group by proposal, so the epic context lives on the card. */
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

// Label set by the coordinator on the human-review remediation task when
// repeated automated remediation fails. Closing that task revives the held
// source task — so it's the one item a human must act on.
const HUMAN_REVIEW_HOLD_LABEL = "human-review-hold";

function hasHumanReviewHold(task: Task): boolean {
  return task.labels?.includes(HUMAN_REVIEW_HOLD_LABEL) ?? false;
}

// The pre-first-turn tracking state a dispatched run reports (via
// `active_session.status`) until its first reply-loop session upgrades it to
// `running`. Matches the backend `TaskRunStatus::Starting` wire string.
const STARTING_SESSION_STATUS = "starting";

function getStatusBadge(
  status: string,
  sessionStatus: string | undefined,
  allAcMet: boolean,
): { label: string; className: string } | null {
  if ((status === "needs_task_review" || status === "in_task_review") && !sessionStatus && allAcMet) {
    return { label: "merging", className: "text-green-400 animate-pulse" };
  }
  // Real dispatched-but-pre-session state, surfaced from the tracked run —
  // NOT a "setting up" pseudo-status inferred from a missing session.
  if (status === "in_progress" && sessionStatus === STARTING_SESSION_STATUS) {
    return { label: "starting", className: "text-blue-400 animate-pulse" };
  }
  return null;
}

function StatusBadge({ status, sessionStatus, allAcMet }: { status: string; sessionStatus: string | undefined; allAcMet: boolean }) {
  const badge = getStatusBadge(status, sessionStatus, allAcMet);
  if (!badge) return null;
  return (
    <span className={cn("text-[10px] font-medium", badge.className)}>
      {badge.label}
    </span>
  );
}

// --- CI status badge ---

const CI_STATUS_CONFIG: Record<string, { label: string; className: string }> = {
  passing: { label: "CI: passing", className: "text-emerald-400" },
  pending: { label: "CI: pending", className: "text-blue-400 animate-pulse" },
  failing: { label: "CI: failing", className: "text-red-400" },
  unknown: { label: "CI: unknown", className: "text-zinc-400" },
  awaiting_ci: { label: "CI: awaiting_ci", className: "text-blue-400 animate-pulse" },
};

function formatCiFailingTitle(ci: CiGateSnapshot): string {
  const checks = ci.blocking_required_check_names;
  const reason = ci.merge_blocked_reason ?? ci.summary_reason;
  if (checks.length === 0) return reason ? `CI: failing — ${reason}` : "CI: failing";
  const primary = ci.primary_blocking_check ?? checks[0];
  const remaining = checks.length > 1 ? ` (+${checks.length - 1} more)` : "";
  return `CI: failing — ${reason ?? primary}${remaining}`;
}

export function CiBadge({ ci }: { ci?: CiGateSnapshot | null }) {
  if (!ci) return null;

  const displayState = ci.gate_state ?? ci.status;
  const config = CI_STATUS_CONFIG[displayState] ?? CI_STATUS_CONFIG.unknown;
  const title = ci.status === "failing" ? formatCiFailingTitle(ci) : undefined;

  return (
    <span
      className={cn("text-[10px] font-medium", config.className)}
      title={title}
      data-testid="taskcard-ci-badge"
    >
      {config.label}
      {ci.status === "failing" && ci.blocking_required_check_names.length > 0 && (
        <span className="ml-0.5 opacity-70">
          ({ci.blocking_required_check_names[0]}
          {ci.blocking_required_check_names.length > 1 ? ` +${ci.blocking_required_check_names.length - 1}` : ""})
        </span>
      )}
    </span>
  );
}

// --- Card tint based on status ---

function getCardTint(task: Task): { ring: string; bg: string; hover: string; actionsBg: string } | null {
  // Human-review remediation hold: the one item a human must act on. Subtle
  // amber tint so it stands out without the alarm of the old red "stuck" tint.
  if (hasHumanReviewHold(task)) {
    return { ring: "ring-amber-500/40", bg: "bg-amber-500/5", hover: "hover:bg-amber-500/10 hover:ring-amber-500/60", actionsBg: "bg-amber-500/10 text-white" };
  }
  return null;
}

function agentAvatar(agentType?: string): string {
  return getAgentAvatar(agentType);
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

export function TaskCard({ task, epic, moving = false, onClick }: TaskCardProps) {
  const [now, setNow] = useState(() => Date.now());

  const startedAt = task.active_session?.started_at;
  const runningSessionStartMs = useMemo(() => {
    if (!startedAt) {
      return null;
    }

    const parsed = Date.parse(startedAt);
    return Number.isNaN(parsed) ? null : parsed;
  }, [startedAt]);

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
  const needsReview = hasHumanReviewHold(task);
  // Dispatched but not yet at its first reply-loop turn: the tracked run
  // reports `starting` via `active_session.status`. Suppress duration/model/
  // avatar until the session is really running (model/agent are unknown while
  // starting).
  const isStarting = task.active_session?.status === STARTING_SESSION_STATUS;

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
      <CardContent className="flex min-h-[3rem] flex-col gap-1">
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
          <AcceptanceProgressBadge criteria={ac} />

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

          {/* CI status badge */}
          <CiBadge ci={task.ci} />

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
              <StatusBadge status={task.status} sessionStatus={task.active_session?.status} allAcMet={acTotal > 0 && acMet === acTotal} />
            </span>
          )}

          {/* Human-review remediation hold — not in-flight (status `open`), so
              render its own status text outside the isInFlight gate. */}
          {needsReview && (
            <span
              className="inline-flex items-center gap-1"
              data-testid="taskcard-needs-review-badge"
            >
              <span className="text-[10px] font-medium text-amber-400">needs your review</span>
            </span>
          )}

          {/* Duration & model for in-flight / done (hidden during setup — shown in tooltip) */}
          {shouldShowDuration && !isStarting && (
            <span className="text-[10px]">{formatCompactDuration(totalTrackedSeconds)}</span>
          )}
          {task.active_session?.model_id && !isStarting && (
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

        {/* Epic breadcrumb — lanes group by proposal, so each card names its
            epic at the bottom (Linear's sub-issue parent pattern). */}
        {epic && (
          <div
            className={cn(
              "flex items-center gap-1 overflow-hidden text-[10px] text-muted-foreground",
              task.active_session && "pr-12",
            )}
            title={epic.title}
            data-testid="taskcard-epic-chip"
          >
            {epic.emoji && (
              <span className="shrink-0 leading-none">{epic.emoji}</span>
            )}
            <span className="truncate">{epic.title}</span>
          </div>
        )}

        {/* Agent avatar – shown when task has an active session (hidden during setup) */}
        {task.active_session && !isStarting && (
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
