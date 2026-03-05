import { useMemo, useState } from "react";
import { useTaskStore } from "@/stores/useTaskStore";
import { useEpicStore } from "@/stores/useEpicStore";
import type { Epic, Task, TaskStatus } from "@/types";

const STATUS_COLUMNS: Array<{ key: TaskStatus; label: string }> = [
  { key: "pending", label: "Open" },
  { key: "in_progress", label: "In Progress" },
  { key: "blocked", label: "Needs Review" },
  { key: "completed", label: "Approved" },
  { key: "canceled", label: "Closed" },
];

function getEpicEmoji(epic: Epic | undefined): string {
  if (!epic) return "📌";
  if (epic.status === "active") return "🚀";
  if (epic.status === "completed") return "✅";
  return "📦";
}

function getEpicTitle(epic: Epic | undefined, epicId: string | null): string {
  if (!epicId) return "No Epic";
  return epic?.title ?? "Unknown Epic";
}

export function KanbanBoard() {
  const tasks = useTaskStore((state) => Array.from(state.tasks.values()));
  const epics = useEpicStore((state) => state.epics);
  const [collapsedEpics, setCollapsedEpics] = useState<Record<string, boolean>>({});

  const groupedByStatusThenEpic = useMemo(() => {
    const byStatus = new Map<TaskStatus, Map<string, Task[]>>();

    for (const column of STATUS_COLUMNS) {
      byStatus.set(column.key, new Map());
    }

    for (const task of tasks) {
      const epicKey = task.epicId ?? "no-epic";
      const statusMap = byStatus.get(task.status);
      if (!statusMap) continue;

      const existing = statusMap.get(epicKey) ?? [];
      existing.push(task);
      statusMap.set(epicKey, existing);
    }

    return byStatus;
  }, [tasks]);

  const toggleEpic = (columnKey: TaskStatus, epicKey: string) => {
    const collapseKey = `${columnKey}:${epicKey}`;
    setCollapsedEpics((prev) => ({ ...prev, [collapseKey]: !prev[collapseKey] }));
  };

  return (
    <div className="flex h-full gap-4 overflow-x-auto p-4">
      {STATUS_COLUMNS.map((column) => {
        const statusMap = groupedByStatusThenEpic.get(column.key) ?? new Map<string, Task[]>();
        const epicGroups = Array.from(statusMap.entries());
        const taskCount = epicGroups.reduce((total, [, epicTasks]) => total + epicTasks.length, 0);

        return (
          <section
            key={column.key}
            className="flex min-w-[260px] flex-1 flex-col rounded-lg border bg-card"
          >
            <header className="border-b px-3 py-2 text-sm font-semibold">
              {column.label} ({taskCount})
            </header>

            <div className="flex-1 overflow-y-auto p-3">
              <div className="flex flex-col gap-3">
                {epicGroups.map(([epicKey, epicTasks]) => {
                  const firstTaskEpicId = epicTasks[0]?.epicId ?? null;
                  const epic = firstTaskEpicId ? epics.get(firstTaskEpicId) : undefined;
                  const collapseKey = `${column.key}:${epicKey}`;
                  const isCollapsed = !!collapsedEpics[collapseKey];

                  return (
                    <div key={epicKey} className="rounded-md border bg-background">
                      <button
                        type="button"
                        onClick={() => toggleEpic(column.key, epicKey)}
                        className="flex w-full items-center justify-between gap-2 border-b px-2 py-1 text-left text-sm font-medium"
                      >
                        <span className="truncate">
                          {getEpicEmoji(epic)} {getEpicTitle(epic, firstTaskEpicId)}
                        </span>
                        <span>{isCollapsed ? "▸" : "▾"}</span>
                      </button>

                      {!isCollapsed && (
                        <ul className="flex flex-col gap-2 p-2">
                          {epicTasks.map((task) => (
                            <li key={task.id} className="rounded border bg-card p-2 text-sm">
                              {task.title}
                            </li>
                          ))}
                        </ul>
                      )}
                    </div>
                  );
                })}
              </div>
            </div>
          </section>
        );
      })}
    </div>
  );
}
