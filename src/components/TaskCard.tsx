import type { Epic, Task } from "@/types";

type TaskCardProps = {
  task: Task;
  epic?: Epic;
  moving?: boolean;
};

const PRIORITY_STYLES: Record<Task["priority"], string> = {
  P0: "bg-red-100 text-red-700 border-red-200",
  P1: "bg-orange-100 text-orange-700 border-orange-200",
  P2: "bg-yellow-100 text-yellow-700 border-yellow-200",
  P3: "bg-gray-100 text-gray-700 border-gray-200",
};

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

function ownerInitials(owner: string | null): string {
  if (!owner) return "??";
  const parts = owner
    .split(/[\s._-]+/)
    .filter(Boolean)
    .slice(0, 2);
  if (parts.length === 0) return owner.slice(0, 2).toUpperCase();
  return parts.map((p) => p[0]?.toUpperCase() ?? "").join("");
}

export function TaskCard({ task, epic, moving = false }: TaskCardProps) {
  return (
    <article
      className={`rounded border bg-card p-2 text-sm transition-all duration-200 ease-in-out hover:-translate-y-px hover:shadow-sm ${moving ? "scale-[1.02] opacity-70" : "scale-100 opacity-100"}`}
    >
      <div className="mb-2 flex items-start justify-between gap-2">
        <h4 className="truncate font-medium" title={task.title}>
          {task.title}
        </h4>
        <span className={`shrink-0 rounded-full border px-2 py-0.5 text-[10px] font-semibold ${PRIORITY_STYLES[task.priority]}`}>
          {task.priority}
        </span>
      </div>

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
