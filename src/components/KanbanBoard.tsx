import { useEffect, useMemo, useRef, useState } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { useTaskStore } from "@/stores/useTaskStore";
import { useEpicStore } from "@/stores/useEpicStore";
import { useProjects, useSelectedProjectId } from "@/stores/useProjectStore";
import { taskStore } from "@/stores/taskStore";
import type { Epic, Task } from "@/api/types";
import { TaskCard, DoneTaskRow } from "@/components/TaskCard";
import { TaskDetailPanel } from "@/components/TaskDetailPanel";
import { GitRemoteSetupBanner, useGitRemoteCheck } from "@/components/GitRemoteSetupBanner";
import { BoardHealthBanner } from "@/components/BoardHealthBanner";
import {
  ArrowDown01Icon,
  ArrowRight01Icon,
  CheckmarkCircle03Icon,
  CircleIcon,
  FullSignalIcon,
  Loading03Icon,
  LowSignalIcon,
  MediumSignalIcon,
  NoSignalIcon,
  Progress02Icon,
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

type ColumnKey = "open" | "in_flight" | "done";

const ISSUE_TYPES = [
  { value: "task", label: "Task" },
  { value: "feature", label: "Feature" },
  { value: "bug", label: "Bug" },
  { value: "spike", label: "Spike" },
  { value: "research", label: "Research" },
  { value: "decomposition", label: "Breakdown" },
  { value: "review", label: "Review" },
] as const;

const STATUS_COLUMNS: Array<{
  key: ColumnKey;
  label: string;
  colorClass: string;
  glowClass: string;
  icon: typeof UnavailableIcon;
}> = [
  { key: "open", label: "Open", colorClass: "bg-[#4B5563]", glowClass: "", icon: CircleIcon },
  { key: "in_flight", label: "In Flight", colorClass: "bg-[#3B82F6]", glowClass: "shadow-[0_1px_6px_-1px] shadow-[#3B82F6]/40", icon: Progress02Icon },
  { key: "done", label: "Done", colorClass: "bg-[#10B981]", glowClass: "shadow-[0_1px_6px_-1px] shadow-[#10B981]/40", icon: CheckmarkCircle03Icon },
];

const PRIORITIES = [0, 1, 2, 3] as const;

const PRIORITY_ICONS: Record<number, { icon: typeof FullSignalIcon; color: string; activeColor: string }> = {
  0: { icon: FullSignalIcon, color: "text-muted-foreground/50", activeColor: "text-[#D1D5DB]" },
  1: { icon: MediumSignalIcon, color: "text-muted-foreground/50", activeColor: "text-[#9CA3AF]" },
  2: { icon: LowSignalIcon, color: "text-muted-foreground/50", activeColor: "text-[#6B7280]" },
  3: { icon: NoSignalIcon, color: "text-muted-foreground/50", activeColor: "text-[#4B5563]" },
};

function taskToColumnKey(task: Task): ColumnKey {
  if (task.status === "closed") return "done";
  if (
    task.status === "in_progress" ||
    task.status === "verifying" ||
    task.status === "needs_task_review" ||
    task.status === "in_task_review" ||
    task.status === "needs_pm_intervention" ||
    task.status === "in_pm_intervention"
  )
    return "in_flight";
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
  const navigate = useNavigate();
  const storeTasks = useTaskStore((state) => Array.from(state.tasks.values()));
  const storeEpics = useEpicStore((state) => state.epics);
  const projects = useProjects();
  const selectedProjectId = useSelectedProjectId();

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
  const [issueTypeFilters, setIssueTypeFilters] = useState<string[]>(
    (searchParams.get("type") ?? "").split(",").filter(Boolean)
  );
  const [searchInput, setSearchInput] = useState<string>(searchParams.get("q") ?? "");
  const [textFilter, setTextFilter] = useState<string>(searchParams.get("q") ?? "");
  const [selectedTask, setSelectedTask] = useState<Task | null>(null);

  const IN_FLIGHT = new Set(["in_progress", "verifying", "needs_task_review", "in_task_review", "needs_pm_intervention", "in_pm_intervention"]);

  const handleTaskClick = (task: Task) => {
    if (IN_FLIGHT.has(task.status) || (task.session_count ?? 0) > 0 || task.active_session) {
      navigate(`/task/${task.id}`);
    } else {
      setSelectedTask(task);
    }
  };

  const selectedProject = projects.find((p) => p.id === selectedProjectId);
  const { hasRemote, check: checkRemote, setHasRemote } = useGitRemoteCheck(selectedProject?.path);

  // Reset epic filters when project changes (epic IDs are project-specific)
  useEffect(() => {
    setEpicFilters([]);
    setSelectedTask(null);
    checkRemote();
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

    if (issueTypeFilters.length > 0) next.set("type", issueTypeFilters.join(","));
    else next.delete("type");

    if (textFilter.trim()) next.set("q", textFilter.trim());
    else next.delete("q");

    setSearchParams(next, { replace: true });
  }, [epicFilters, ownerFilters, priorityFilters, issueTypeFilters, textFilter, searchParams, setSearchParams, disableSearchParamSync]);

  const epicOptions = useMemo(
    () => Array.from(epics.values()).sort((a, b) => (a.title ?? "").localeCompare(b.title ?? "")),
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
      if (issueTypeFilters.length > 0 && !issueTypeFilters.includes(task.issue_type ?? "task")) return false;
      if (q && !task.title.toLowerCase().includes(q)) return false;
      return true;
    });
  }, [tasks, epicFilters, ownerFilters, priorityFilters, issueTypeFilters, textFilter]);

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
      <div className="flex flex-wrap items-center gap-3 border-b border-white/[0.04] px-4 pb-5">
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
                    ? "bg-primary/20 ring-1 ring-primary/40"
                    : "hover:bg-muted/50"
                )}
              >
                <HugeiconsIcon
                  icon={config.icon}
                  size={16}
                  className={cn(
                    "shrink-0 transition-colors",
                    isActive ? "text-primary" : noFilters ? config.activeColor : config.color
                  )}
                />
              </button>
            );
          })}
          {priorityFilters.length > 0 && (
            <button
              type="button"
              title="Clear priority filter"
              onClick={() => setPriorityFilters([])}
              className="flex h-5 w-5 items-center justify-center rounded text-muted-foreground/70 hover:text-foreground transition-colors"
            >
              <span className="text-xs leading-none">✕</span>
            </button>
          )}
        </div>

        <Combobox
          multiple
          value={issueTypeFilters}
          onValueChange={(v) => setIssueTypeFilters(v ?? [])}
          itemToStringLabel={(val) => ISSUE_TYPES.find((t) => t.value === val)?.label ?? val}
        >
          <ComboboxInput
            placeholder={issueTypeFilters.length > 0 ? `${issueTypeFilters.length} type${issueTypeFilters.length > 1 ? "s" : ""}` : "All types"}
            className="w-28"
          />
          <ComboboxContent>
            <ComboboxList>
              <ComboboxEmpty>No types found</ComboboxEmpty>
              {ISSUE_TYPES.map((type) => (
                <ComboboxItem key={type.value} value={type.value}>
                  {type.label}
                </ComboboxItem>
              ))}
            </ComboboxList>
          </ComboboxContent>
        </Combobox>

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

      <BoardHealthBanner
        projectPaths={
          selectedProject?.path
            ? [selectedProject.path]
            : projects.map((p) => p.path)
        }
      />

      {hasRemote === false && selectedProject?.path && (
        <GitRemoteSetupBanner
          projectPath={selectedProject.path}
          onResolved={() => setHasRemote(true)}
        />
      )}

      <div className="flex min-h-0 flex-1 overflow-x-auto pb-1">
        {STATUS_COLUMNS.map((column, colIdx) => {
          const statusMap = groupedByStatusThenEpic.get(column.key) ?? new Map<string, Task[]>();
          const epicGroups = Array.from(statusMap.entries());
          const taskCount = epicGroups.reduce((total, [, epicTasks]) => total + epicTasks.length, 0);

          return (
            <div key={column.key} className="flex min-h-0 min-w-[320px] flex-1">
              {colIdx > 0 && <div className="w-px shrink-0 self-stretch bg-white/[0.03]" />}
              <Card
                className="relative min-h-0 flex-1 gap-0 border-transparent bg-transparent py-0 ring-0 transition-all duration-300 ease-in-out"
            >
              <div className="flex flex-col">
                <div className="relative px-4 pb-2.5 pt-3.5 text-sm font-semibold">
                  <div className="flex items-center gap-2.5">
                    {column.key === "in_flight" && taskCount > 0 ? (
                      <HugeiconsIcon
                        icon={Loading03Icon}
                        className="size-4 shrink-0 animate-spin text-blue-400"
                      />
                    ) : (
                      <HugeiconsIcon
                        icon={column.icon}
                        className={cn("size-4 shrink-0", column.key === "done" ? "text-[#10B981]" : "text-muted-foreground")}
                      />
                    )}
                    <span className="leading-none">{column.label}</span>
                    <span className="text-xs leading-none text-muted-foreground">{taskCount}</span>
                  </div>
                </div>
                <div className="px-4">
                  <div className={cn("h-0.5 w-10 rounded-full", column.colorClass, column.glowClass)} />
                </div>
              </div>

              <CardContent className="relative z-10 flex-1 overflow-y-auto px-3 pt-4">
                {taskCount === 0 ? (
                  <p className="px-1 text-xs text-muted-foreground/50">No tasks</p>
                ) : (
                <div className="flex flex-col gap-3.5">
                  {epicGroups.map(([epicKey, epicTasks]) => {
                    const firstTaskEpicId = epicTasks[0]?.epic_id;
                    const epic = firstTaskEpicId ? epics.get(firstTaskEpicId) : undefined;
                    const collapseKey = `${column.key}:${epicKey}`;
                    const isCollapsed = !!collapsedEpics[collapseKey];

                    return (
                      <Card key={epicKey} size="sm" className="gap-0 cursor-pointer bg-zinc-900 py-3 ring-white/[0.04]" onClick={() => toggleEpic(column.key, epicKey)}>
                        <CardContent>
                          <div className="flex w-full items-center justify-between gap-2 px-1 py-1.5 text-sm font-medium">
                            <span className="flex items-center gap-2 truncate">
                              <span className="shrink-0 text-xs leading-none">{getEpicEmoji(epic)}</span>
                              <span className="truncate">{getEpicTitle(epic, firstTaskEpicId)}</span>
                            </span>
                            <HugeiconsIcon
                              icon={isCollapsed ? ArrowRight01Icon : ArrowDown01Icon}
                              className="size-4 shrink-0 text-muted-foreground"
                            />
                          </div>

                          {!isCollapsed && (
                            column.key === "done" ? (
                              <ul className="flex flex-col pt-1.5" onClick={(e) => e.stopPropagation()}>
                                {epicTasks.map((task) => (
                                  <li key={task.id}>
                                    <DoneTaskRow
                                      task={task}
                                      onClick={() => handleTaskClick(task)}
                                    />
                                  </li>
                                ))}
                              </ul>
                            ) : (
                              <ul className="flex flex-col gap-3 pt-2.5" onClick={(e) => e.stopPropagation()}>
                                {epicTasks.map((task) => (
                                  <li key={task.id}>
                                    <TaskCard
                                      task={task}
                                      epic={task.epic_id ? epics.get(task.epic_id) : undefined}
                                      moving={!!movingTaskIds[task.id]}
                                      onClick={() => handleTaskClick(task)}
                                    />
                                  </li>
                                ))}
                              </ul>
                            )
                          )}
                        </CardContent>
                      </Card>
                    );
                  })}
                </div>
                )}
              </CardContent>
            </Card>
            </div>
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
