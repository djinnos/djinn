import type { Epic, Task, AcceptanceCriterion, CiGateSnapshot } from "@/api/types";
import { useQuery } from "@tanstack/react-query";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { usersQueryOptions } from "@/api/queryOptions";
import { userDisplayName } from "@/api/users";
import { useTaskActions } from "@/hooks/useTaskActions";
import { useExecutionControl } from "@/hooks/useExecutionControl";
import { useSelectedProject } from "@/stores/useProjectStore";
import { Button } from "@/components/ui/button";
import { Cancel01Icon, PlayIcon, Refresh01Icon, StopIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

type TaskDetailPanelProps = {
  task: Task | null;
  epic?: Epic;
  open: boolean;
  onClose: () => void;
};

const STATUS_LABELS: Record<string, string> = {
  open: "Open",
  in_progress: "In Flight — Coding",
  needs_task_review: "In Flight — Review",
  in_task_review: "In Flight — Review",
  needs_lead_intervention: "In Flight — Lead Intervention",
  in_lead_intervention: "In Flight — Lead Intervention",
  closed: "Done",
};

const PRIORITY_LABELS: Record<number, string> = {
  [-1]: "Critical",
  0: "P0",
  1: "P1",
  2: "P2",
  3: "P3",
};

function formatRelative(dateString: string): string {
  const date = new Date(dateString);
  const now = new Date();
  const diffMs = date.getTime() - now.getTime();
  const rtf = new Intl.RelativeTimeFormat("en", { numeric: "auto" });
  const minutes = Math.round(diffMs / 60000);
  const hours = Math.round(minutes / 60);
  const days = Math.round(hours / 24);

  if (Math.abs(minutes) < 60) return rtf.format(minutes, "minute");
  if (Math.abs(hours) < 24) return rtf.format(hours, "hour");
  return rtf.format(days, "day");
}

function parseCriterion(raw: string | AcceptanceCriterion): { criterion: string; met: boolean } {
  if (typeof raw === "string") {
    return { criterion: raw, met: false };
  }
  return { criterion: raw.criterion, met: Boolean(raw.met) };
}

function SectionCard({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section className="space-y-2">
      <h3 className="text-sm font-semibold uppercase tracking-wide text-muted-foreground">{title}</h3>
      <div className="rounded-md border bg-card p-4 text-sm">{children}</div>
    </section>
  );
}

function TaskActions({ task }: { task: Task }) {
  const project = useSelectedProject();
  const { busy: transitioning, transition } = useTaskActions();
  const { busy: killing, killTask } = useExecutionControl();
  const busy = transitioning || killing;

  if (!project?.github_owner || !project?.github_repo) return null;
  const projectSlug = `${project.github_owner}/${project.github_repo}`;

  const isOpen = task.status === "open";
  const isInProgress = task.status === "in_progress";
  const isClosed = task.status === "closed";
  const isBlocked = (task.unresolved_blocker_count ?? 0) > 0;

  return (
    <div className="flex items-center gap-2">
      {isOpen && !isBlocked && (
        <Button
          size="sm"
          variant="default"
          disabled={busy}
          onClick={() => transition(task.id, projectSlug, "start")}
          className="gap-1.5 bg-emerald-600 hover:bg-emerald-700"
        >
          <HugeiconsIcon icon={PlayIcon} size={14} />
          Start
        </Button>
      )}
      {isInProgress && (
        <Button
          size="sm"
          variant="destructive"
          disabled={busy}
          onClick={() => killTask(task.id)}
          className="gap-1.5"
        >
          <HugeiconsIcon icon={StopIcon} size={14} />
          Stop
        </Button>
      )}
      {isClosed && (
        <Button
          size="sm"
          variant="outline"
          disabled={busy}
          onClick={() => transition(task.id, projectSlug, "reopen", "Reopened from desktop")}
          className="gap-1.5"
        >
          <HugeiconsIcon icon={Refresh01Icon} size={14} />
          Reopen
        </Button>
      )}
      {!isClosed && !isInProgress && (
        <Button
          size="sm"
          variant="ghost"
          disabled={busy}
          onClick={() => transition(task.id, projectSlug, "force_close", "Closed from desktop")}
          className="gap-1.5 text-muted-foreground hover:text-destructive"
        >
          <HugeiconsIcon icon={Cancel01Icon} size={14} />
          Close
        </Button>
      )}
    </div>
  );
}

const CI_STATUS_LABELS: Record<string, { label: string; className: string }> = {
  passing: { label: "Passing", className: "text-emerald-500" },
  pending: { label: "Pending", className: "text-blue-500" },
  failing: { label: "Failing", className: "text-red-500" },
  unknown: { label: "Unknown", className: "text-zinc-400" },
  awaiting_ci: { label: "Awaiting CI", className: "text-blue-500" },
};

function CiStatusSection({ ci }: { ci?: CiGateSnapshot | null }) {
  if (!ci) return null;

  const displayState = ci.gate_state ?? ci.status;
  const config = CI_STATUS_LABELS[displayState] ?? CI_STATUS_LABELS.unknown;

  return (
    <SectionCard title="CI Status">
      <div className="space-y-2 text-sm">
        <div className="flex items-center gap-2">
          <span className="font-medium">Required CI:</span>
          <span className={config.className}>{config.label}</span>
        </div>
        {ci.summary_reason && (
          <div>
            <span className="font-medium">Summary:</span> {ci.summary_reason}
          </div>
        )}
        {ci.merge_blocked_reason && (
          <div>
            <span className="font-medium">Merge blocked reason:</span> {ci.merge_blocked_reason}
          </div>
        )}
        {ci.head_sha && (
          <div>
            <span className="font-medium">Head SHA:</span>{" "}
            <span className="font-mono text-xs">{ci.head_sha.slice(0, 8)}</span>
          </div>
        )}
        {ci.pr_number != null && (
          <div>
            <span className="font-medium">PR:</span> #{ci.pr_number}
          </div>
        )}
        {ci.status === "failing" && ci.blocking_required_check_names.length > 0 && (
          <div>
            <span className="font-medium">Blocking checks:</span>{" "}
            <ul className="mt-1 space-y-0.5">
              {ci.blocking_required_check_names.map((name) => (
                <li key={name} className="font-mono text-xs text-red-400">
                  {name}
                </li>
              ))}
            </ul>
          </div>
        )}
        {ci.status === "failing" && ci.failure_fingerprint && (
          <div>
            <span className="font-medium">Failure fingerprint:</span>{" "}
            <span className="font-mono text-xs">{ci.failure_fingerprint}</span>
          </div>
        )}
        {ci.same_signature_count > 1 && (
          <div>
            <span className="font-medium">Repeat count:</span> {ci.same_signature_count}
          </div>
        )}
      </div>
    </SectionCard>
  );
}

export function TaskDetailPanel({ task, epic, open, onClose }: TaskDetailPanelProps) {
  // Hooks must run unconditionally, so this shared (and aggressively cached)
  // roster query is read before the early return below. Every other consumer
  // fetches it unconditionally too, so react-query dedupes it to the cache.
  const { data: users = [] } = useQuery(usersQueryOptions());

  if (!open || !task) return null;

  const criteria = (task.acceptance_criteria ?? []).map(parseCriterion);
  const creator = task.created_by_user_id
    ? users.find((u) => u.id === task.created_by_user_id)
    : undefined;
  const createdByLabel = task.created_by_user_id
    ? (creator ? userDisplayName(creator) : task.created_by_user_id)
    : "Agent / unassigned";
  return (
    <div className="fixed inset-0 z-50 flex justify-end bg-black/40" role="dialog" aria-modal="true">
      <button type="button" className="h-full flex-1 cursor-default" onClick={onClose} aria-label="Close task details" />
      <aside className="h-full w-full max-w-2xl overflow-y-auto border-l bg-background p-6 shadow-2xl">
        <div className="mb-4 flex items-start justify-between gap-2">
          <div className="space-y-2">
            <div className="flex items-center gap-2">
              <h2 className="text-xl font-semibold">{task.title}</h2>
              {task.short_id ? <span className="rounded bg-muted px-2 py-0.5 text-xs font-semibold uppercase">{task.short_id}</span> : null}
              {task.reopen_count > 0 ? (
                <span className="rounded bg-amber-100 px-2 py-0.5 text-xs font-medium text-amber-800">Reopened {task.reopen_count}x</span>
              ) : null}
            </div>
            {!!task.labels?.length && (
              <div className="flex flex-wrap gap-1">
                {task.labels.map((label: string) => (
                  <span key={label} className="rounded-full border px-2 py-0.5 text-xs text-muted-foreground">
                    {label}
                  </span>
                ))}
              </div>
            )}
            <TaskActions task={task} />
          </div>
          <button type="button" className="rounded border px-2 py-1 text-sm" onClick={onClose}>
            Close
          </button>
        </div>

        <div className="space-y-5">
          <SectionCard title="Metadata">
            <div className="grid grid-cols-2 gap-2 text-sm">
              <div><span className="font-medium">Status:</span> {STATUS_LABELS[task.status] ?? task.status}</div>
              <div><span className="font-medium">Priority:</span> {PRIORITY_LABELS[task.priority] ?? `P${task.priority}`}</div>
              {task.issue_type && <div><span className="font-medium">Type:</span> {task.issue_type}</div>}
              <div><span className="font-medium">Epic:</span> {epic?.title ?? "No Epic"}</div>
              <div><span className="font-medium">Created by:</span> {createdByLabel}</div>
              <div><span className="font-medium">Created:</span> {formatRelative(task.created_at)}</div>
              <div><span className="font-medium">Updated:</span> {formatRelative(task.updated_at)}</div>
            </div>
          </SectionCard>

          <CiStatusSection ci={task.ci} />

          <SectionCard title="Description">
            <div className="prose prose-sm max-w-none dark:prose-invert">
              <ReactMarkdown remarkPlugins={[remarkGfm]}>{task.description || "No description"}</ReactMarkdown>
            </div>
          </SectionCard>

          <SectionCard title="Acceptance Criteria">
            <ul className="space-y-2">
              {(criteria.length ? criteria : [{ criterion: "No acceptance criteria", met: false }]).map((item: { criterion: string; met: boolean }, idx: number) => (
                <li key={`${item.criterion}-${idx}`} className="flex items-start gap-2">
                  <input type="checkbox" checked={item.met} readOnly className="mt-0.5" />
                  <span>{item.criterion}</span>
                </li>
              ))}
            </ul>
          </SectionCard>

          <SectionCard title="Design Notes">
            <div className="prose prose-sm max-w-none dark:prose-invert">
              <ReactMarkdown remarkPlugins={[remarkGfm]}>{task.design || "No design notes"}</ReactMarkdown>
            </div>
          </SectionCard>

          {(task.review_feedback?.length > 0 || task.pr_url) && (
            <SectionCard title="PR Review Feedback">
              <div className="space-y-3">
                {task.pr_url && (
                  <div>
                    <a
                      href={task.pr_url as string}
                      target="_blank"
                      rel="noopener noreferrer"
                      className="text-blue-400 hover:underline"
                    >
                      View PR on GitHub
                    </a>
                  </div>
                )}
                {(task.review_cycle_count != null || task.review_feedback?.length > 0) && (
                  <div className="text-muted-foreground">
                    Review cycles:{" "}
                    <span className="font-medium text-foreground">
                      {task.review_cycle_count ?? task.review_feedback?.length ?? 0}
                    </span>
                  </div>
                )}
                {task.review_feedback?.length > 0 ? (
                  <div className="space-y-4">
                    {(task.review_feedback as Array<{ cycle?: number; comments?: Array<{ file?: string; line_start?: number; line_end?: number; body?: string; suggestion?: string; reviewer?: string }> }>).map(
                      (reviewCycle, cycleIdx) => (
                        <div key={cycleIdx} className="space-y-3">
                          <div className="flex items-center gap-2">
                            <div className="h-px flex-1 bg-border" />
                            <span className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
                              Cycle {reviewCycle.cycle ?? cycleIdx + 1}
                            </span>
                            <div className="h-px flex-1 bg-border" />
                          </div>
                          {reviewCycle.comments?.length ? (
                            <div className="space-y-3">
                              {reviewCycle.comments.map((comment, commentIdx) => (
                                <div key={commentIdx} className="rounded border bg-muted/40 p-3 space-y-1.5">
                                  {(comment.file || comment.line_start != null) && (
                                    <div className="flex items-center gap-2 text-xs text-muted-foreground font-mono">
                                      {comment.file && <span>{comment.file}</span>}
                                      {comment.line_start != null && (
                                        <span className="text-muted-foreground/70">
                                          L{comment.line_start}
                                          {comment.line_end != null && comment.line_end !== comment.line_start
                                            ? `–L${comment.line_end}`
                                            : ""}
                                        </span>
                                      )}
                                      {comment.reviewer && (
                                        <span className="ml-auto italic">{comment.reviewer}</span>
                                      )}
                                    </div>
                                  )}
                                  {comment.body && (
                                    <p className="text-sm text-foreground">{comment.body}</p>
                                  )}
                                  {comment.suggestion && (
                                    <pre className="mt-1 overflow-x-auto rounded bg-muted px-3 py-2 text-xs font-mono text-muted-foreground whitespace-pre-wrap">
                                      {comment.suggestion}
                                    </pre>
                                  )}
                                </div>
                              ))}
                            </div>
                          ) : (
                            <p className="text-sm text-muted-foreground">No comments in this cycle.</p>
                          )}
                        </div>
                      )
                    )}
                  </div>
                ) : (
                  !task.pr_url && (
                    <p className="text-sm text-muted-foreground">No review feedback yet.</p>
                  )
                )}
              </div>
            </SectionCard>
          )}
        </div>
      </aside>
    </div>
  );
}
