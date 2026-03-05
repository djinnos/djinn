import type { Epic, Task } from "@/types";

type TaskDetailPanelProps = {
  task: Task | null;
  epic?: Epic;
  open: boolean;
  onClose: () => void;
};

const STATUS_LABELS: Record<Task["status"], string> = {
  pending: "Open",
  in_progress: "In Progress",
  blocked: "Needs Review",
  completed: "Approved",
  canceled: "Closed",
};

function renderMarkdown(markdown: string): string {
  return markdown
    .replace(/^###\s+(.*)$/gm, "<h3>$1</h3>")
    .replace(/^##\s+(.*)$/gm, "<h2>$1</h2>")
    .replace(/^#\s+(.*)$/gm, "<h1>$1</h1>")
    .replace(/\*\*(.*?)\*\*/g, "<strong>$1</strong>")
    .replace(/\*(.*?)\*/g, "<em>$1</em>")
    .replace(/`([^`]+)`/g, "<code>$1</code>")
    .replace(/\n/g, "<br />");
}

export function TaskDetailPanel({ task, epic, open, onClose }: TaskDetailPanelProps) {
  if (!open || !task) return null;

  return (
    <div className="fixed inset-0 z-50 flex justify-end bg-black/40" role="dialog" aria-modal="true">
      <button type="button" className="h-full flex-1 cursor-default" onClick={onClose} aria-label="Close task details" />
      <aside className="h-full w-full max-w-2xl overflow-y-auto border-l bg-background p-6 shadow-2xl">
        <div className="mb-4 flex items-start justify-between gap-2">
          <h2 className="text-xl font-semibold">{task.title}</h2>
          <button type="button" className="rounded border px-2 py-1 text-sm" onClick={onClose}>
            Close
          </button>
        </div>

        <section className="mb-5 grid grid-cols-2 gap-2 text-sm">
          <div><span className="font-medium">Status:</span> {STATUS_LABELS[task.status]}</div>
          <div><span className="font-medium">Priority:</span> {task.priority}</div>
          <div><span className="font-medium">Epic:</span> {epic?.title ?? "No Epic"}</div>
          <div><span className="font-medium">Owner:</span> {task.owner ?? "Unassigned"}</div>
        </section>

        <section className="mb-5">
          <h3 className="mb-2 text-sm font-semibold uppercase tracking-wide text-muted-foreground">Description</h3>
          <div
            className="rounded border bg-card p-3 text-sm leading-relaxed"
            dangerouslySetInnerHTML={{ __html: renderMarkdown(task.description || "No description") }}
          />
        </section>

        <section className="mb-5">
          <h3 className="mb-2 text-sm font-semibold uppercase tracking-wide text-muted-foreground">Acceptance Criteria</h3>
          <ul className="space-y-2 rounded border bg-card p-3 text-sm">
            {(task.acceptanceCriteria?.length ? task.acceptanceCriteria : ["No acceptance criteria"]).map((criterion, idx) => (
              <li key={`${criterion}-${idx}`} className="flex items-start gap-2">
                <input type="checkbox" readOnly className="mt-0.5" />
                <span>{criterion}</span>
              </li>
            ))}
          </ul>
        </section>

        <section className="mb-5">
          <h3 className="mb-2 text-sm font-semibold uppercase tracking-wide text-muted-foreground">Design Notes</h3>
          <div
            className="rounded border bg-card p-3 text-sm leading-relaxed"
            dangerouslySetInnerHTML={{ __html: renderMarkdown(task.design || "No design notes") }}
          />
        </section>

        <section>
          <h3 className="mb-2 text-sm font-semibold uppercase tracking-wide text-muted-foreground">Activity Log</h3>
          <ul className="space-y-2 rounded border bg-card p-3 text-sm">
            {(task.activity?.length ? task.activity : ["No recent activity"]).map((event, idx) => (
              <li key={`${event}-${idx}`} className="text-muted-foreground">• {event}</li>
            ))}
          </ul>
        </section>
      </aside>
    </div>
  );
}
