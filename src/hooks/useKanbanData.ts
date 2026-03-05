import { useEffect, useMemo, useRef, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { useTaskStore } from "@/stores/useTaskStore";
import { useEpicStore } from "@/stores/useEpicStore";
import { taskStore } from "@/stores/taskStore";
import type { Epic, Task, TaskPriority, TaskStatus } from "@/types";

export const STATUS_COLUMNS: Array<{ key: TaskStatus; label: string; accentClass: string }> = [
  { key: "pending", label: "Open", accentClass: "border-violet-500" },
  { key: "in_progress", label: "In Progress", accentClass: "border-purple-500" },
  { key: "blocked", label: "In Review", accentClass: "border-amber-500" },
  { key: "completed", label: "Closed", accentClass: "border-emerald-500" },
];

export const PRIORITIES: TaskPriority[] = ["P0", "P1", "P2", "P3"];

export function getEpicEmoji(epic: Epic | undefined): string {
  if (!epic) return "📌";
  if (epic.status === "active") return "🚀";
  if (epic.status === "completed") return "✅";
  return "📦";
}

export function getEpicTitle(epic: Epic | undefined, epicId: string | null): string {
  if (!epicId) return "No Epic";
  return epic?.title ?? "Unknown Epic";
}

export function useKanbanData() {
  const tasks = useTaskStore((state) => Array.from(state.tasks.values()));
  const epics = useEpicStore((state) => state.epics);
  const [searchParams, setSearchParams] = useSearchParams();

  const [collapsedEpics, setCollapsedEpics] = useState<Record<string, boolean>>({});
  const [movingTaskIds, setMovingTaskIds] = useState<Record<string, boolean>>({});
  const previousTaskStatusesRef = useRef<Map<string, TaskStatus>>(new Map());

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
    const unsubscribe = taskStore.subscribe(
      (state) => state.tasks,
      (nextTasks) => {
        const previousStatuses = previousTaskStatusesRef.current;
        const nextStatuses = new Map<string, TaskStatus>();
        const changedTaskIds: string[] = [];

        nextTasks.forEach((task, id) => {
          nextStatuses.set(id, task.status);
          const previousStatus = previousStatuses.get(id);
          if (previousStatus !== undefined && previousStatus !== task.status) changedTaskIds.push(id);
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
    for (const task of tasks) if (task.owner) owners.add(task.owner);
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
    for (const column of STATUS_COLUMNS) byStatus.set(column.key, new Map());

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

  return {
    epics,
    collapsedEpics,
    movingTaskIds,
    epicFilter,
    ownerFilter,
    priorityFilters,
    searchInput,
    selectedTask,
    epicOptions,
    ownerOptions,
    groupedByStatusThenEpic,
    setEpicFilter,
    setOwnerFilter,
    setPriorityFilters,
    setSearchInput,
    setSelectedTask,
    toggleEpic,
  };
}
