import { useEffect, useMemo, useRef, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { useTaskStore } from "@/stores/useTaskStore";
import { useEpicStore } from "@/stores/useEpicStore";
import { taskStore } from "@/stores/taskStore";
import type { Epic, Task, TaskPriority, TaskStatus } from "@/types";
import { TaskCard } from "@/components/TaskCard";
import { TaskDetailPanel } from "@/components/TaskDetailPanel";
import {
  ArrowDown01Icon,
  ArrowRight01Icon,
  CheckmarkCircle03Icon,
  CircleIcon,
  Progress02Icon,
  Progress04Icon,
  type UnavailableIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { cn } from "@/lib/utils";
import {
  Combobox,
  ComboboxContent,
  ComboboxInput,
  ComboboxItem,
  ComboboxList,
  ComboboxEmpty,
} from "@/components/ui/combobox";
import { Card, CardContent } from "@/components/ui/card";
import {
  InputGroup,
  InputGroupAddon,
  InputGroupInput,
} from "@/components/ui/input-group";
import { Search01Icon } from "@hugeicons/core-free-icons";

type ColumnKey = TaskStatus | "in_review";

const STATUS_COLUMNS: Array<{
  key: ColumnKey;
  label: string;
  colorClass: string;
  glowClass: string;
  icon: typeof UnavailableIcon;
}> = [
  { key: "pending", label: "Open", colorClass: "bg-violet-500", glowClass: "shadow-[0_1px_6px_-1px] shadow-violet-500/40", icon: CircleIcon },
  { key: "in_progress", label: "In Progress", colorClass: "bg-blue-500", glowClass: "shadow-[0_1px_6px_-1px] shadow-blue-500/40", icon: Progress02Icon },
  { key: "in_review", label: "In Review", colorClass: "bg-amber-500", glowClass: "shadow-[0_1px_6px_-1px] shadow-amber-500/40", icon: Progress04Icon },
  { key: "completed", label: "Closed", colorClass: "bg-emerald-500", glowClass: "shadow-[0_1px_6px_-1px] shadow-emerald-500/40", icon: CheckmarkCircle03Icon },
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

type KanbanBoardProps = {
  tasks?: Task[];
  epics?: Map<string, Epic>;
  initialCollapsedEpics?: string[];
  disableSearchParamSync?: boolean;
};

export function KanbanBoard({
  tasks: tasksProp,
  epics: epicsProp,
  initialCollapsedEpics,
  disableSearchParamSync,
}: KanbanBoardProps = {}) {
  const storeTasks = useTaskStore((state) => Array.from(state.tasks.values()));
  const storeEpics = useEpicStore((state) => state.epics);
  const tasks = tasksProp ?? storeTasks;
  const epics = epicsProp ?? storeEpics;
  const [searchParams, setSearchParams] = useSearchParams();
  const [collapsedEpics, setCollapsedEpics] = useState<Record<string, boolean>>(() => {
    const next: Record<string, boolean> = {};
    for (const key of initialCollapsedEpics ?? []) next[key] = true;
    return next;
  });
  const [movingTaskIds, setMovingTaskIds] = useState<Record<string, boolean>>({});
  const previousTaskStatusesRef = useRef<Map<string, TaskStatus>>(new Map());

  useEffect(() => {
    if (tasksProp) return;

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
  }, [tasksProp]);

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
    if (disableSearchParamSync) return;

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
  }, [epicFilter, ownerFilter, priorityFilters, textFilter, searchParams, setSearchParams, disableSearchParamSync]);

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
    const byColumn = new Map<ColumnKey, Map<string, Task[]>>();

    for (const column of STATUS_COLUMNS) {
      byColumn.set(column.key, new Map());
    }

    for (const task of filteredTasks) {
      const epicKey = task.epicId ?? "no-epic";
      const columnKey: ColumnKey = task.reviewPhase ? "in_review" : task.status;
      const columnMap = byColumn.get(columnKey);
      if (!columnMap) continue;

      const existing = columnMap.get(epicKey) ?? [];
      existing.push(task);
      columnMap.set(epicKey, existing);
    }

    return byColumn;
  }, [filteredTasks]);

  const toggleEpic = (columnKey: ColumnKey, epicKey: string) => {
    const collapseKey = `${columnKey}:${epicKey}`;
    setCollapsedEpics((prev) => ({ ...prev, [collapseKey]: !prev[collapseKey] }));
  };

  return (
    <div className="flex h-full min-h-0 flex-col gap-4">
      <div className="flex flex-wrap items-center gap-2">
        <Combobox
          value={epicFilter}
          onValueChange={(v) => setEpicFilter(v ?? "")}
        >
          <ComboboxInput placeholder="All epics" className="w-40" />
          <ComboboxContent>
            <ComboboxList>
              <ComboboxEmpty>No epics found</ComboboxEmpty>
              <ComboboxItem value="">All epics</ComboboxItem>
              {epicOptions.map((epic) => (
                <ComboboxItem key={epic.id} value={epic.id}>
                  {getEpicEmoji(epic)} {epic.title}
                </ComboboxItem>
              ))}
            </ComboboxList>
          </ComboboxContent>
        </Combobox>

        <Combobox
          value={priorityFilters.join(",")}
          onValueChange={(v) => {
            const val = v ?? "";
            setPriorityFilters(
              val ? val.split(",").filter((p): p is TaskPriority => PRIORITIES.includes(p as TaskPriority)) : []
            );
          }}
        >
          <ComboboxInput
            placeholder={priorityFilters.length > 0 ? `Priority (${priorityFilters.length})` : "Priority"}
            className="w-32"
          />
          <ComboboxContent>
            <ComboboxList>
              {PRIORITIES.map((priority) => (
                <ComboboxItem key={priority} value={priority}>
                  {priority}
                </ComboboxItem>
              ))}
            </ComboboxList>
          </ComboboxContent>
        </Combobox>

        <Combobox
          value={ownerFilter}
          onValueChange={(v) => setOwnerFilter(v ?? "")}
        >
          <ComboboxInput placeholder="All owners" className="w-36" />
          <ComboboxContent>
            <ComboboxList>
              <ComboboxEmpty>No owners found</ComboboxEmpty>
              <ComboboxItem value="">All owners</ComboboxItem>
              {ownerOptions.map((owner) => (
                <ComboboxItem key={owner} value={owner}>
                  {owner}
                </ComboboxItem>
              ))}
            </ComboboxList>
          </ComboboxContent>
        </Combobox>

        <InputGroup className="ml-auto w-56">
          <InputGroupAddon>
            <HugeiconsIcon icon={Search01Icon} className="size-3.5" />
          </InputGroupAddon>
          <InputGroupInput
            value={searchInput}
            onChange={(e) => setSearchInput(e.target.value)}
            placeholder="Search tasks..."
          />
        </InputGroup>
      </div>

      <div className="flex min-h-0 flex-1 gap-4 overflow-x-auto pb-1">
        {STATUS_COLUMNS.map((column) => {
          const statusMap = groupedByStatusThenEpic.get(column.key) ?? new Map<string, Task[]>();
          const epicGroups = Array.from(statusMap.entries());
          const taskCount = epicGroups.reduce((total, [, epicTasks]) => total + epicTasks.length, 0);

          return (
            <Card
              key={column.key}
              className="min-w-[260px] flex-1 gap-0 py-0 transition-all duration-300 ease-in-out"
            >
              <div className="flex flex-col">
                <div className="px-3 py-2 text-sm font-semibold">
                  <div className="flex items-center gap-2">
                    <HugeiconsIcon
                      icon={column.icon}
                      className="size-4 shrink-0 text-muted-foreground"
                    />
                    <span className="leading-none">{column.label}</span>
                    <span className="text-xs leading-none text-muted-foreground">{taskCount}</span>
                  </div>
                </div>
                <div className="px-2">
                  <div className={cn("h-0.5 w-full rounded-full", column.colorClass, column.glowClass)} />
                </div>
              </div>

              <CardContent className="flex-1 overflow-y-auto pt-3">
                <div className="flex flex-col gap-3">
                  {epicGroups.map(([epicKey, epicTasks]) => {
                    const firstTaskEpicId = epicTasks[0]?.epicId ?? null;
                    const epic = firstTaskEpicId ? epics.get(firstTaskEpicId) : undefined;
                    const collapseKey = `${column.key}:${epicKey}`;
                    const isCollapsed = !!collapsedEpics[collapseKey];

                    return (
                      <Card key={epicKey} size="sm" className="gap-0 bg-zinc-800/50 py-2">
                        <CardContent>
                          <button
                            type="button"
                            onClick={() => toggleEpic(column.key, epicKey)}
                            className="flex w-full items-center justify-between gap-2 rounded-md px-1 py-1 text-left text-sm font-medium transition-colors hover:bg-muted/40"
                          >
                            <span className="truncate">
                              {getEpicEmoji(epic)} {getEpicTitle(epic, firstTaskEpicId)}
                            </span>
                            <HugeiconsIcon
                              icon={isCollapsed ? ArrowRight01Icon : ArrowDown01Icon}
                              className="size-4 shrink-0 text-muted-foreground"
                            />
                          </button>

                          {!isCollapsed && (
                            <ul className="flex flex-col gap-2 pt-2">
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
                        </CardContent>
                      </Card>
                    );
                  })}
                </div>
              </CardContent>
            </Card>
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
