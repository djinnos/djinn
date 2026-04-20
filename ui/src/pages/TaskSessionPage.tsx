/**
 * TaskSessionPage — full-page session viewer at /task/:taskId
 *
 * Left panel: task metadata, acceptance criteria, session list
 * Right panel: unified chat thread (ADR-007)
 */

import { useEffect, useRef, useState } from "react";
import { useStore } from "zustand";
import { useNavigate, useParams } from "react-router-dom";
import { useSelectedProject } from "@/stores/useProjectStore";
import { useTaskStore } from "@/stores/useTaskStore";
import { taskStore } from "@/stores/taskStore";
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
import { verificationStore } from "@/stores/verificationStore";
import {
  areSetupVerificationViewsEqual,
  buildSetupVerificationView,
  EMPTY_SETUP_VERIFICATION,
} from "@/lib/setupVerificationView";

// ── Status labels ────────────────────────────────────────────────────────────

const STATUS_LABELS: Record<string, string> = {
  open: "Open",
  in_progress: "Coding",
  verifying: "Verifying",
  needs_task_review: "Needs Review",
  in_task_review: "In Review",
  needs_lead_intervention: "Lead Intervention",
  in_lead_intervention: "Lead Intervening",
  closed: "Done",
};

const STATUS_COLORS: Record<string, string> = {
  open: "bg-blue-500/15 text-blue-400",
  in_progress: "bg-emerald-500/15 text-emerald-400",
  verifying: "bg-yellow-500/15 text-yellow-400",
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

// ── Left panel ───────────────────────────────────────────────────────────────

function TaskSidebar({ task }: { task: Task }) {
  const criteria = (task.acceptance_criteria ?? []).map(parseCriterion);
  return (
    <div className="flex w-80 shrink-0 flex-col gap-4 overflow-y-auto border-r border-border p-4">
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

  const { timeline, sessions, loading, error, streamingText, streamingThinking } = useSessionMessages(
    taskId ?? null,
    projectPath
  );

  // Build setup verification view for the session thread
  const setupVerificationRaw = useStore(verificationStore);
  const setupVerificationRef = useRef(EMPTY_SETUP_VERIFICATION);
  if (taskId) {
    const next = buildSetupVerificationView(taskId, setupVerificationRaw);
    if (!areSetupVerificationViewsEqual(setupVerificationRef.current, next)) {
      setupVerificationRef.current = next;
    }
  }
  const setupVerification = setupVerificationRef.current;

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
        <PriorityBadge priority={task.priority} />
        <span className="text-sm font-medium">{task.title}</span>
        <TaskIdLabel taskId={task.id} shortId={task.short_id} />
        <span className="flex-1" />
        {sessions.length > 0 && (() => {
          const totalIn = sessions.reduce((sum, s) => sum + s.tokensIn, 0);
          const totalOut = sessions.reduce((sum, s) => sum + s.tokensOut, 0);
          return (
            <span className="flex items-center gap-1.5 text-[10px] text-muted-foreground">
              <span>{formatTokens(totalIn)} in</span>
              <span>/</span>
              <span>{formatTokens(totalOut)} out</span>
            </span>
          );
        })()}
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
        <TaskSidebar task={task} />
        <div className="flex min-w-0 flex-1 flex-col">
          <SessionThread
            timeline={timeline}
            streamingText={streamingText}
            streamingThinking={streamingThinking}
            loading={loading}
            error={error}
            activeAgentType={activeSession?.agentType}
            setupVerification={setupVerification}
          />
        </div>
      </div>
    </div>
  );
}
