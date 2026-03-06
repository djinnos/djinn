import { useEffect, useMemo, useRef, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { useTaskStore } from "@/stores/useTaskStore";
import { useEpicStore } from "@/stores/useEpicStore";
import { useProjects, useSelectedProjectId, useProjectStore } from "@/stores/useProjectStore";
import { taskStore } from "@/stores/taskStore";
import type { Epic, Task } from "@/api/types";
import { TaskCard } from "@/components/TaskCard";
import { TaskDetailPanel } from "@/components/TaskDetailPanel";
import {
  ArrowDown01Icon,
  ArrowRight01Icon,
  CheckmarkCircle03Icon,
  CircleIcon,
  FullSignalIcon,
  LowSignalIcon,
  MediumSignalIcon,
  NoSignalIcon,
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

type ColumnKey = "open" | "in_progress" | "in_review" | "closed";

const STATUS_COLUMNS: Array<{
  key: ColumnKey;
  label: string;
  colorClass: string;
  glowClass: string;
  icon: typeof UnavailableIcon;
}> = [
  { key: "open", label: "Open", colorClass: "bg-violet-500", glowClass: "shadow-[0_1px_6px_-1px] shadow-violet-500/40", icon: CircleIcon },
  { key: "in_progress", label: "In Progress", colorClass: "bg-blue-500", glowClass: "shadow-[0_1px_6px_-1px] shadow-blue-500/40", icon: Progress02Icon },
  { key: "in_review", label: "In Review", colorClass: "bg-amber-500", glowClass: "shadow-[0_1px_6px_-1px] shadow-amber-500/40", icon: Progress04Icon },
  { key: "closed", label: "Closed", colorClass: "bg-emerald-500", glowClass: "shadow-[0_1px_6px_-1px] shadow-emerald-500/40", icon: CheckmarkCircle03Icon },
];

const PRIORITIES = [0, 1, 2, 3] as const;

const PRIORITY_ICONS: Record<number, { icon: typeof FullSignalIcon; color: string; activeColor: string }> = {
  0: { icon: FullSignalIcon, color: "text-muted-foreground/50", activeColor: "text-red-500" },
  1: { icon: MediumSignalIcon, color: "text-muted-foreground/50", activeColor: "text-yellow-500" },
  2: { icon: LowSignalIcon, color: "text-muted-foreground/50", activeColor: "text-green-500" },
  3: { icon: NoSignalIcon, color: "text-muted-foreground/50", activeColor: "text-muted-foreground" },
};

function taskToColumnKey(task: Task): ColumnKey {
  if (task.status === "needs_task_review" || task.status === "in_task_review") return "in_review";
  if (task.status === "closed") return "closed";
  if (task.status === "in_progress") return "in_progress";
  return "open";
}

function getEpicEmoji(epic: Epic | undefined): string {
  return epic?.emoji ?? "📌";
}

function getEpicTitle(epic: Epic | undefined, epicId: string | undefined): string {
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
  const projects = useProjects();
  const selectedProjectId = useSelectedProjectId();
  const setSelectedProjectId = useProjectStore((state) => state.setSelectedProjectId);
  const tasks = tasksProp ?? storeTasks;
  const epics = epicsProp ?? storeEpics;
  const [searchParams, setSearchParams] = useSearchParams();
  const [collapsedEpics, setCollapsedEpics] = useState<Record<string, boolean>>(() => {
    const next: Record<string, boolean> = {};
    for (const key of initialCollapsedEpics ?? []) next[key] = true;
    return next;
  });
  const [movingTaskIds, setMovingTaskIds] = useState<Record<string, boolean>>({});
  const previousTaskStatusesRef = useRef<Map<string, string>>(new Map());

  useEffect(() => {
    if (tasksProp) return;

    const unsubscribe = taskStore.subscribe(
      (state) => state.tasks,
      (nextTasks) => {
        const previousStatuses = previousTaskStatusesRef.current;
        const nextStatuses = new Map<string, string>();
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

  const [epicFilters, setEpicFilters] = useState<string[]>(
    (searchParams.get("epic") ?? "").split(",").filter(Boolean)
  );
  const [ownerFilters, setOwnerFilters] = useState<string[]>(
    (searchParams.get("owner") ?? "").split(",").filter(Boolean)
  );
  const [priorityFilters, setPriorityFilters] = useState<number[]>(
    (searchParams.get("priority") ?? "")
      .split(",")
      .filter(Boolean)
      .map((p) => {
        const match = p.match(/^P?(\d)$/i);
        return match ? Number(match[1]) : -1;
      })
      .filter((p) => p >= 0 && p <= 3)
  );
  const [searchInput, setSearchInput] = useState<string>(searchParams.get("q") ?? "");
  const [textFilter, setTextFilter] = useState<string>(searchParams.get("q") ?? "");
  const [selectedTask, setSelectedTask] = useState<Task | null>(null);

  // Reset epic filters when project changes (epic IDs are project-specific)
  useEffect(() => {
    setEpicFilters([]);
    setSelectedTask(null);
  }, [selectedProjectId]);

  useEffect(() => {
    const timeout = setTimeout(() => setTextFilter(searchInput), 250);
    return () => clearTimeout(timeout);
  }, [searchInput]);

  useEffect(() => {
    if (disableSearchParamSync) return;

    const next = new URLSearchParams(searchParams);

    if (epicFilters.length > 0) next.set("epic", epicFilters.join(","));
    else next.delete("epic");

    if (ownerFilters.length > 0) next.set("owner", ownerFilters.join(","));
    else next.delete("owner");

    if (priorityFilters.length > 0) next.set("priority", priorityFilters.map((p) => `P${p}`).join(","));
    else next.delete("priority");

    if (textFilter.trim()) next.set("q", textFilter.trim());
    else next.delete("q");

    setSearchParams(next, { replace: true });
  }, [epicFilters, ownerFilters, priorityFilters, textFilter, searchParams, setSearchParams, disableSearchParamSync]);

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
      if (epicFilters.length > 0 && !epicFilters.includes(task.epic_id ?? "")) return false;
      if (ownerFilters.length > 0 && !ownerFilters.includes(task.owner ?? "")) return false;
      if (priorityFilters.length > 0 && !priorityFilters.includes(task.priority)) return false;
      if (q && !task.title.toLowerCase().includes(q)) return false;
      return true;
    });
  }, [tasks, epicFilters, ownerFilters, priorityFilters, textFilter]);

  const groupedByStatusThenEpic = useMemo(() => {
    const byColumn = new Map<ColumnKey, Map<string, Task[]>>();

    for (const column of STATUS_COLUMNS) {
      byColumn.set(column.key, new Map());
    }

    for (const task of filteredTasks) {
      const epicKey = task.epic_id ?? "no-epic";
      const columnKey = taskToColumnKey(task);
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
    <div className="flex h-full min-h-0 flex-col gap-5 overflow-hidden px-4 pt-5 pb-2">
      <div className="flex flex-wrap items-center gap-3 px-4">
        <Combobox
          value={selectedProjectId ?? ""}
          onValueChange={(v) => setSelectedProjectId(v || null)}
          itemToStringLabel={(id) => projects.find((p) => p.id === id)?.name ?? id}
        >
          <ComboboxInput placeholder="Project" className="w-44" />
          <ComboboxContent>
            <ComboboxList>
              {projects.map((project) => (
                <ComboboxItem key={project.id} value={project.id}>
                  {project.name}
                </ComboboxItem>
              ))}
            </ComboboxList>
          </ComboboxContent>
        </Combobox>

        <Combobox
          multiple
          value={epicFilters}
          onValueChange={(v) => setEpicFilters(v ?? [])}
          itemToStringLabel={(id) => {
            const epic = epics.get(id);
            return epic ? `${getEpicEmoji(epic)} ${epic.title}` : id;
          }}
        >
          <ComboboxInput
            placeholder={epicFilters.length > 0 ? `${epicFilters.length} epic${epicFilters.length > 1 ? "s" : ""}` : "All epics"}
            className="w-40"
          />
          <ComboboxContent className="!min-w-80">
            <ComboboxList>
              <ComboboxEmpty>No epics found</ComboboxEmpty>
              {epicOptions.map((epic) => (
                <ComboboxItem key={epic.id} value={epic.id} className="truncate">
                  {getEpicEmoji(epic)} {epic.title}
                </ComboboxItem>
              ))}
            </ComboboxList>
          </ComboboxContent>
        </Combobox>

        <div className="flex h-8 items-center gap-1 rounded-lg border border-input px-1.5 dark:bg-input/30">
          {PRIORITIES.map((priority) => {
            const config = PRIORITY_ICONS[priority];
            const isActive = priorityFilters.includes(priority);
            const noFilters = priorityFilters.length === 0;
            return (
              <button
                key={priority}
                type="button"
                title={`P${priority}`}
                onClick={() =>
                  setPriorityFilters((prev) =>
                    prev.includes(priority)
                      ? prev.filter((p) => p !== priority)
                      : [...prev, priority]
                  )
                }
                className={cn(
                  "flex h-7 w-7 items-center justify-center rounded-md transition-colors",
                  isActive
                    ? "bg-muted"
                    : "hover:bg-muted/50"
                )}
              >
                <HugeiconsIcon
                  icon={config.icon}
                  size={16}
                  className={cn(
                    "shrink-0 transition-colors",
                    isActive ? config.activeColor : noFilters ? config.activeColor : config.color
                  )}
                />
              </button>
            );
          })}
        </div>

        <Combobox
          multiple
          value={ownerFilters}
          onValueChange={(v) => setOwnerFilters(v ?? [])}
        >
          <ComboboxInput
            placeholder={ownerFilters.length > 0 ? `${ownerFilters.length} owner${ownerFilters.length > 1 ? "s" : ""}` : "All owners"}
            className="w-36"
          />
          <ComboboxContent>
            <ComboboxList>
              <ComboboxEmpty>No owners found</ComboboxEmpty>
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

      <div className="flex min-h-0 flex-1 gap-5 overflow-x-auto pb-1">
        {STATUS_COLUMNS.map((column) => {
          const statusMap = groupedByStatusThenEpic.get(column.key) ?? new Map<string, Task[]>();
          const epicGroups = Array.from(statusMap.entries());
          const taskCount = epicGroups.reduce((total, [, epicTasks]) => total + epicTasks.length, 0);

          return (
            <Card
              key={column.key}
              className="min-h-0 min-w-[280px] flex-1 gap-0 border-transparent bg-transparent py-0 ring-0 transition-all duration-300 ease-in-out"
            >
              <div className="flex flex-col">
                <div className="px-4 pb-2.5 pt-3.5 text-sm font-semibold">
                  <div className="flex items-center gap-2.5">
                    <HugeiconsIcon
                      icon={column.icon}
                      className="size-4 shrink-0 text-muted-foreground"
                    />
                    <span className="leading-none">{column.label}</span>
                    <span className="text-xs leading-none text-muted-foreground">{taskCount}</span>
                  </div>
                </div>
                <div className="px-3">
                  <div className={cn("h-0.5 w-full rounded-full", column.colorClass, column.glowClass)} />
                </div>
              </div>

              <CardContent className="flex-1 overflow-y-auto px-3 pt-4">
                <div className="flex flex-col gap-3.5">
                  {epicGroups.map(([epicKey, epicTasks]) => {
                    const firstTaskEpicId = epicTasks[0]?.epic_id;
                    const epic = firstTaskEpicId ? epics.get(firstTaskEpicId) : undefined;
                    const collapseKey = `${column.key}:${epicKey}`;
                    const isCollapsed = !!collapsedEpics[collapseKey];

                    return (
                      <Card key={epicKey} size="sm" className="gap-0 bg-muted/30 py-3 ring-white/[0.04]">
                        <CardContent>
                          <button
                            type="button"
                            onClick={() => toggleEpic(column.key, epicKey)}
                            className="flex w-full items-center justify-between gap-2 rounded-md px-1 py-1.5 text-left text-sm font-medium transition-colors hover:bg-muted/40"
                          >
                            <span className="flex items-center gap-2 truncate">
                              <span className="shrink-0 text-xs leading-none">{getEpicEmoji(epic)}</span>
                              <span className="truncate">{getEpicTitle(epic, firstTaskEpicId)}</span>
                            </span>
                            <HugeiconsIcon
                              icon={isCollapsed ? ArrowRight01Icon : ArrowDown01Icon}
                              className="size-4 shrink-0 text-muted-foreground"
                            />
                          </button>

                          {!isCollapsed && (
                            <ul className="flex flex-col gap-3 pt-2.5">
                              {epicTasks.map((task) => (
                                <li key={task.id}>
                                  <TaskCard
                                    task={task}
                                    epic={task.epic_id ? epics.get(task.epic_id) : undefined}
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
        epic={selectedTask?.epic_id ? epics.get(selectedTask.epic_id) : undefined}
        onClose={() => setSelectedTask(null)}
      />
    </div>
  );
}
