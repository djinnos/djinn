import type { Epic, Task, AcceptanceCriterion } from "@/api/types";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

type TaskDetailPanelProps = {
  task: Task | null;
  epic?: Epic;
  open: boolean;
  onClose: () => void;
};

const STATUS_LABELS: Record<string, string> = {
  open: "Open",
  needs_pm_intervention: "Agent Intervention",
  in_pm_intervention: "Agent Intervention",
  in_progress: "Agent Coding",
  verifying: "Automated Verification",
  needs_task_review: "Agent Review",
  in_task_review: "Agent Review",
  closed: "Done",
};

const PRIORITY_LABELS: Record<number, string> = {
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

export function TaskDetailPanel({ task, epic, open, onClose }: TaskDetailPanelProps) {
  if (!open || !task) return null;

  const criteria = (task.acceptance_criteria ?? []).map(parseCriterion);

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
              <div><span className="font-medium">Epic:</span> {epic?.title ?? "No Epic"}</div>
              <div><span className="font-medium">Owner:</span> {task.owner ?? "Unassigned"}</div>
              <div><span className="font-medium">Created:</span> {formatRelative(task.created_at)}</div>
              <div><span className="font-medium">Updated:</span> {formatRelative(task.updated_at)}</div>
            </div>
          </SectionCard>

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
        </div>
      </aside>
    </div>
  );
}
