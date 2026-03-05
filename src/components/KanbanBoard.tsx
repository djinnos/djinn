import { useEffect, useMemo, useRef, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { useTaskStore } from "@/stores/useTaskStore";
import { useEpicStore } from "@/stores/useEpicStore";
import { taskStore } from "@/stores/taskStore";
import type { Epic, Task, TaskPriority, TaskStatus } from "@/types";
import { TaskCard } from "@/components/TaskCard";
import { TaskDetailPanel } from "@/components/TaskDetailPanel";

const STATUS_COLUMNS: Array<{ key: TaskStatus; label: string; accentClass: string }> = [
  { key: "pending", label: "Open", accentClass: "border-violet-500" },
  { key: "in_progress", label: "In Progress", accentClass: "border-purple-500" },
  { key: "blocked", label: "In Review", accentClass: "border-amber-500" },
  { key: "completed", label: "Closed", accentClass: "border-emerald-500" },
];

const PRIORITIES: TaskPriority[] = ["P0", "P1", "P2", "P3"];

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
  const [searchParams, setSearchParams] = useSearchParams();
  const [collapsedEpics, setCollapsedEpics] = useState<Record<string, boolean>>({});
  const [movingTaskIds, setMovingTaskIds] = useState<Record<string, boolean>>({});
  const previousTaskStatusesRef = useRef<Map<string, TaskStatus>>(new Map());

  useEffect(() => {
    const unsubscribe = taskStore.subscribe(
      (state) => state.tasks,
      (nextTasks) => {
        const previousStatuses = previousTaskStatusesRef.current;
        const nextStatuses = new Map<string, TaskStatus>();
        const changedTaskIds: string[] = [];

        nextTasks.forEach((task, id) => {
          nextStatuses.set(id, task.status);
          const previousStatus = previousStatuses.get(id);

          if (previousStatus !== undefined && previousStatus !== task.status) {
            changedTaskIds.push(id);
          }
        });

        previousTaskStatusesRef.current = nextStatuses;

        if (changedTaskIds.length === 0) return;

        setMovingTaskIds((prev) => {
          const next = { ...prev };
          for (const taskId of changedTaskIds) next[taskId] = true;
          return next;
        });

        window.setTimeout(() => {
          setMovingTaskIds((prev) => {
            const next = { ...prev };
            for (const taskId of changedTaskIds) delete next[taskId];
            return next;
          });
        }, 350);
      }
    );

    return unsubscribe;
  }, []);

  const [epicFilter, setEpicFilter] = useState<string>(searchParams.get("epic") ?? "");
  const [ownerFilter, setOwnerFilter] = useState<string>(searchParams.get("owner") ?? "");
  const [priorityFilters, setPriorityFilters] = useState<TaskPriority[]>(
    (searchParams.get("priority") ?? "")
      .split(",")
      .filter(Boolean)
      .filter((p): p is TaskPriority => PRIORITIES.includes(p as TaskPriority))
  );
  const [searchInput, setSearchInput] = useState<string>(searchParams.get("q") ?? "");
  const [textFilter, setTextFilter] = useState<string>(searchParams.get("q") ?? "");
  const [selectedTask, setSelectedTask] = useState<Task | null>(null);

  useEffect(() => {
    const timeout = setTimeout(() => setTextFilter(searchInput), 250);
    return () => clearTimeout(timeout);
  }, [searchInput]);

  useEffect(() => {
    const next = new URLSearchParams(searchParams);

    if (epicFilter) next.set("epic", epicFilter);
    else next.delete("epic");

    if (ownerFilter) next.set("owner", ownerFilter);
    else next.delete("owner");

    if (priorityFilters.length > 0) next.set("priority", priorityFilters.join(","));
    else next.delete("priority");

    if (textFilter.trim()) next.set("q", textFilter.trim());
    else next.delete("q");

    setSearchParams(next, { replace: true });
  }, [epicFilter, ownerFilter, priorityFilters, textFilter, searchParams, setSearchParams]);

  const epicOptions = useMemo(
    () => Array.from(epics.values()).sort((a, b) => a.title.localeCompare(b.title)),
    [epics]
  );

  const ownerOptions = useMemo(() => {
    const owners = new Set<string>();
    for (const task of tasks) {
      if (task.owner) owners.add(task.owner);
    }
    return Array.from(owners).sort((a, b) => a.localeCompare(b));
  }, [tasks]);

  const filteredTasks = useMemo(() => {
    const q = textFilter.trim().toLowerCase();

    return tasks.filter((task) => {
      if (epicFilter && (task.epicId ?? "") !== epicFilter) return false;
      if (ownerFilter && (task.owner ?? "") !== ownerFilter) return false;
      if (priorityFilters.length > 0 && !priorityFilters.includes(task.priority)) return false;
      if (q && !task.title.toLowerCase().includes(q)) return false;
      return true;
    });
  }, [tasks, epicFilter, ownerFilter, priorityFilters, textFilter]);

  const groupedByStatusThenEpic = useMemo(() => {
    const byStatus = new Map<TaskStatus, Map<string, Task[]>>();

    for (const column of STATUS_COLUMNS) {
      byStatus.set(column.key, new Map());
    }

    for (const task of filteredTasks) {
      const epicKey = task.epicId ?? "no-epic";
      const statusMap = byStatus.get(task.status);
      if (!statusMap) continue;

      const existing = statusMap.get(epicKey) ?? [];
      existing.push(task);
      statusMap.set(epicKey, existing);
    }

    return byStatus;
  }, [filteredTasks]);

  const toggleEpic = (columnKey: TaskStatus, epicKey: string) => {
    const collapseKey = `${columnKey}:${epicKey}`;
    setCollapsedEpics((prev) => ({ ...prev, [collapseKey]: !prev[collapseKey] }));
  };

  return (
    <div className="flex h-full min-h-0 flex-col gap-4">
      <div className="flex flex-wrap items-center gap-3 rounded-lg border bg-card p-3">
        <select
          value={epicFilter}
          onChange={(e) => setEpicFilter(e.target.value)}
          className="rounded border bg-background px-2 py-1 text-sm"
        >
          <option value="">All epics</option>
          {epicOptions.map((epic) => (
            <option key={epic.id} value={epic.id}>
              {getEpicEmoji(epic)} {epic.title}
            </option>
          ))}
        </select>

        <div className="flex items-center gap-1">
          {PRIORITIES.map((priority) => {
            const active = priorityFilters.includes(priority);
            return (
              <button
                key={priority}
                type="button"
                onClick={() =>
                  setPriorityFilters((prev) =>
                    prev.includes(priority)
                      ? prev.filter((p) => p !== priority)
                      : [...prev, priority]
                  )
                }
                className={`rounded-full border px-2 py-1 text-xs ${active ? "bg-primary text-primary-foreground" : "bg-background"}`}
              >
                {priority}
              </button>
            );
          })}
        </div>

        <select
          value={ownerFilter}
          onChange={(e) => setOwnerFilter(e.target.value)}
          className="rounded border bg-background px-2 py-1 text-sm"
        >
          <option value="">All owners</option>
          {ownerOptions.map((owner) => (
            <option key={owner} value={owner}>
              {owner}
            </option>
          ))}
        </select>

        <input
          value={searchInput}
          onChange={(e) => setSearchInput(e.target.value)}
          placeholder="Search tasks..."
          className="min-w-[220px] rounded border bg-background px-2 py-1 text-sm"
        />
      </div>

      <div className="flex min-h-0 flex-1 gap-4 overflow-x-auto pb-1">
        {STATUS_COLUMNS.map((column) => {
          const statusMap = groupedByStatusThenEpic.get(column.key) ?? new Map<string, Task[]>();
          const epicGroups = Array.from(statusMap.entries());
          const taskCount = epicGroups.reduce((total, [, epicTasks]) => total + epicTasks.length, 0);

          return (
            <section
              key={column.key}
              className="flex min-w-[260px] flex-1 flex-col rounded-lg border bg-card transition-all duration-300 ease-in-out"
            >
              <header className={`border-b-2 px-3 py-2 text-sm font-semibold ${column.accentClass}`}>
                {column.label} {taskCount}
              </header>

              <div className="flex-1 overflow-y-auto p-3">
                <div className="flex flex-col gap-3">
                  {epicGroups.map(([epicKey, epicTasks]) => {
                    const firstTaskEpicId = epicTasks[0]?.epicId ?? null;
                    const epic = firstTaskEpicId ? epics.get(firstTaskEpicId) : undefined;
                    const collapseKey = `${column.key}:${epicKey}`;
                    const isCollapsed = !!collapsedEpics[collapseKey];

                    return (
                      <div key={epicKey} className="rounded-md border bg-background transition-all duration-300 ease-in-out">
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
                          <ul className="flex flex-col gap-2 p-2 transition-all duration-300 ease-in-out">
                            {epicTasks.map((task) => (
                              <li key={task.id}>
                                <TaskCard
                                  task={task}
                                  epic={task.epicId ? epics.get(task.epicId) : undefined}
                                  moving={!!movingTaskIds[task.id]}
                                  onClick={() => setSelectedTask(task)}
                                />
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

      <TaskDetailPanel
        open={!!selectedTask}
        task={selectedTask}
        epic={selectedTask?.epicId ? epics.get(selectedTask.epicId) : undefined}
        onClose={() => setSelectedTask(null)}
      />
    </div>
  );
}
