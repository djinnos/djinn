import { useEffect, useMemo, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useTaskStore } from "@/stores/useTaskStore";
import { useEpicStore } from "@/stores/useEpicStore";
import { useProjects } from "@/stores/useProjectStore";
import { taskStore } from "@/stores/taskStore";
import { useMergedColumnStore } from "@/stores/useMergedColumnStore";
import { loadMoreClosedTasks } from "@/api/server";
import type { Epic, Task } from "@/api/types";
import { TaskCard, DoneTaskRow } from "@/components/TaskCard";
import { TaskDetailPanel } from "@/components/TaskDetailPanel";
import { BoardHealthBanner } from "@/components/BoardHealthBanner";
import { GitHubAppBanner } from "@/components/GitHubAppBanner";
import {
  ArrowDown01Icon,
  ArrowRight01Icon,
  CheckmarkCircle03Icon,
  CircleIcon,
  GitPullRequestIcon,
  Loading02Icon,
  Progress02Icon,
  type UnavailableIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { cn } from "@/lib/utils";
import { Card, CardContent } from "@/components/ui/card";
import {
  epicMatchesOwnerFilter,
  getEpicEmoji,
  matchesStructuredFilters,
  useBoardFilters,
} from "@/components/board/boardFilters";
import { useBoardServerSearch } from "@/components/board/useBoardServerSearch";

type ColumnKey = "open" | "in_progress" | "pr_ready" | "done";

const STATUS_COLUMNS: Array<{
  key: ColumnKey;
  label: string;
  colorClass: string;
  glowClass: string;
  icon: typeof UnavailableIcon;
}> = [
  {
    key: "open",
    label: "Open",
    colorClass: "bg-[#4B5563]",
    glowClass: "",
    icon: CircleIcon,
  },
  {
    key: "in_progress",
    label: "In Progress",
    colorClass: "bg-[#3B82F6]",
    glowClass: "shadow-[0_1px_6px_-1px] shadow-[#3B82F6]/40",
    icon: Progress02Icon,
  },
  {
    key: "pr_ready",
    label: "PR Ready",
    colorClass: "bg-[#8B5CF6]",
    glowClass: "shadow-[0_1px_6px_-1px] shadow-[#8B5CF6]/40",
    icon: GitPullRequestIcon,
  },
  {
    key: "done",
    label: "Merged",
    colorClass: "bg-[#10B981]",
    glowClass: "shadow-[0_1px_6px_-1px] shadow-[#10B981]/40",
    icon: CheckmarkCircle03Icon,
  },
];

function taskToColumnKey(task: Task): ColumnKey | null {
  if (task.status === "closed") {
    // A task lands in "Merged" when we have the landed merge-commit SHA. Tasks
    // closed before the SHA was persisted (legacy rows) won't have one, so fall
    // back to the PR-flow completion signal: a task that opened a PR (`pr_url`)
    // and closed as `completed` genuinely merged. This deliberately excludes
    // force-closed-without-merge (`close_reason` = "force_closed") and the
    // review/decomposition/planning/spike tasks that never open a PR.
    const merged =
      task.merge_commit_sha != null ||
      (task.pr_url != null && task.close_reason === "completed");
    return merged ? "done" : null;
  }
  if (
    task.status === "approved" ||
    task.status === "pr_draft" ||
    task.status === "pr_review"
  )
    return "pr_ready";
  if (
    task.status === "in_progress" ||
    task.status === "needs_task_review" ||
    task.status === "in_task_review" ||
    task.status === "needs_lead_intervention" ||
    task.status === "in_lead_intervention"
  )
    return "in_progress";
  return "open";
}

function getEpicTitle(
  epic: Epic | undefined,
  epicId: string | undefined,
): string {
  if (!epicId) return "No Epic";
  return epic?.title ?? "Unknown Epic";
}

type KanbanBoardProps = {
  tasks?: Task[];
  epics?: Map<string, Epic>;
  initialCollapsedEpics?: string[];
};

export function KanbanBoard({
  tasks: tasksProp,
  epics: epicsProp,
  initialCollapsedEpics,
}: KanbanBoardProps = {}) {
  const navigate = useNavigate();
  const storeTasks = useTaskStore((state) => Array.from(state.tasks.values()));
  const storeEpics = useEpicStore((state) => state.epics);
  const projects = useProjects();

  // Merged-column pagination: the board loads active tasks first, then the
  // closed/merged tasks page-by-page. `hasMoreClosed` drives the "Load more"
  // affordance and the "+" suffix on the Merged header count. When the board is
  // rendered in controlled mode (`tasksProp`) the store is empty, so both stay
  // falsy and the affordance is hidden.
  const hasMoreClosed = useMergedColumnStore((state) => state.hasMore());
  const loadingMoreClosed = useMergedColumnStore((state) => state.loadingMore);
  const handleLoadMoreClosed = () => {
    void loadMoreClosedTasks();
  };

  // Filter values come from the shared header via the URL search params.
  const { projectFilters, epicFilters, ownerFilters, issueTypeFilters, search } =
    useBoardFilters();

  // Backend-backed search: when the query is non-empty, fetch matching tasks
  // (including old merged ones the board never loaded) into the store. Disabled
  // in controlled mode (`tasksProp`, used by tests) where the store is unused.
  useBoardServerSearch({ enabled: !tasksProp });

  const tasks = tasksProp ?? storeTasks;
  const epics = epicsProp ?? storeEpics;
  const [collapsedEpics, setCollapsedEpics] = useState<Record<string, boolean>>(
    () => {
      const next: Record<string, boolean> = {};
      for (const key of initialCollapsedEpics ?? []) next[key] = true;
      return next;
    },
  );
  const [movingTaskIds, setMovingTaskIds] = useState<Record<string, boolean>>(
    {},
  );
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
      },
    );

    return unsubscribe;
  }, [tasksProp]);

  const [selectedTask, setSelectedTask] = useState<Task | null>(null);

  const handleTaskClick = (task: Task) => {
    navigate(`/task/${task.id}`);
  };

  const bannerSlugs = useMemo(() => {
    const source =
      projectFilters.length > 0
        ? projects.filter((p) => projectFilters.includes(p.id))
        : projects;
    return source.map((p) => `${p.github_owner}/${p.github_repo}`);
  }, [projects, projectFilters]);

  const filteredTasks = useMemo(() => {
    const q = search.trim().toLowerCase();
    const filters = {
      projectFilters,
      epicFilters,
      ownerFilters,
      issueTypeFilters,
    };
    return tasks.filter((task) => {
      if (!matchesStructuredFilters(task, filters)) return false;
      // Match title OR description, case-insensitive — mirrors the server's
      // `text` filter (ILIKE over title/description) so tasks surfaced by the
      // backend search aren't hidden by a narrower client-side predicate.
      if (q) {
        const title = (task.title ?? "").toLowerCase();
        const description = (task.description ?? "").toLowerCase();
        if (!title.includes(q) && !description.includes(q)) return false;
      }
      return true;
    });
  }, [
    tasks,
    projectFilters,
    epicFilters,
    ownerFilters,
    issueTypeFilters,
    search,
  ]);

  const groupedByStatusThenEpic = useMemo(() => {
    const byColumn = new Map<ColumnKey, Map<string, Task[]>>();

    for (const column of STATUS_COLUMNS) {
      byColumn.set(column.key, new Map());
    }

    for (const task of filteredTasks) {
      const epicKey = task.epic_id ?? "no-epic";
      const columnKey = taskToColumnKey(task);
      if (columnKey === null) continue;
      const columnMap = byColumn.get(columnKey);
      if (!columnMap) continue;

      const existing = columnMap.get(epicKey) ?? [];
      existing.push(task);
      columnMap.set(epicKey, existing);
    }

    // Seed empty open epics into the Open column so they are visible on the board
    const epicIdsWithTasks = new Set<string>();
    for (const columnMap of byColumn.values()) {
      for (const epicKey of columnMap.keys()) {
        epicIdsWithTasks.add(epicKey);
      }
    }
    const openColumn = byColumn.get("open");
    if (openColumn) {
      const visibleEpicIds =
        epicFilters.length > 0 ? new Set(epicFilters) : null;
      for (const [epicId, epic] of epics) {
        if (epic.status !== "open") continue;
        if (epicIdsWithTasks.has(epicId)) continue;
        if (visibleEpicIds && !visibleEpicIds.has(epicId)) continue;
        // Scope empty epic shells to the owner filter the same way tasks are:
        // an epic is "owned" by its `created_by_user_id` (which its tasks
        // inherit), so a shell only shows when its owner is selected.
        if (!epicMatchesOwnerFilter(epic, ownerFilters)) continue;
        openColumn.set(epicId, []);
      }
    }

    return byColumn;
  }, [filteredTasks, epics, epicFilters, ownerFilters]);

  const toggleEpic = (columnKey: ColumnKey, epicKey: string) => {
    const collapseKey = `${columnKey}:${epicKey}`;
    setCollapsedEpics((prev) => ({
      ...prev,
      [collapseKey]: !prev[collapseKey],
    }));
  };

  return (
    <div className="flex h-full min-h-0 flex-col gap-5 overflow-hidden px-4 pt-4 pb-2">
      <BoardHealthBanner projectSlugs={bannerSlugs} />

      <GitHubAppBanner projectSlugs={bannerSlugs} />

      <div className="flex min-h-0 flex-1 overflow-x-auto pb-1">
        {STATUS_COLUMNS.map((column, colIdx) => {
          const statusMap =
            groupedByStatusThenEpic.get(column.key) ??
            new Map<string, Task[]>();
          const epicGroups = Array.from(statusMap.entries());
          const taskCount = epicGroups.reduce(
            (total, [, epicTasks]) => total + epicTasks.length,
            0,
          );
          const hasContent = epicGroups.length > 0;

          return (
            <div key={column.key} className="flex min-h-0 min-w-[360px] flex-1">
              {colIdx > 0 && (
                <div className="w-px shrink-0 self-stretch bg-white/[0.03]" />
              )}
              <Card className="relative min-h-0 flex-1 gap-0 border-transparent bg-transparent py-0 ring-0 transition-all duration-300 ease-in-out">
                <div className="flex flex-col">
                  <div className="relative px-4 pb-2.5 pt-3.5 text-sm font-semibold">
                    <div className="flex items-center gap-2.5">
                      {column.key === "in_progress" ? (
                        <HugeiconsIcon
                          icon={Loading02Icon}
                          className="size-4 shrink-0 animate-spin text-blue-400"
                        />
                      ) : (
                        <HugeiconsIcon
                          icon={column.icon}
                          className={cn(
                            "size-4 shrink-0",
                            column.key === "done"
                              ? "text-[#10B981]"
                              : column.key === "pr_ready"
                                ? "text-[#8B5CF6]"
                                : "text-muted-foreground",
                          )}
                        />
                      )}
                      <span className="leading-none">{column.label}</span>
                      <span className="text-xs leading-none text-muted-foreground">
                        {taskCount}
                        {column.key === "done" && hasMoreClosed ? "+" : ""}
                      </span>
                    </div>
                  </div>
                  <div className="px-4">
                    <div
                      className={cn(
                        "h-0.5 w-10 rounded-full",
                        column.colorClass,
                        column.glowClass,
                      )}
                    />
                  </div>
                </div>

                <CardContent className="relative z-10 flex-1 overflow-y-auto px-3 pt-4">
                  {!hasContent &&
                  !(column.key === "done" && hasMoreClosed) ? (
                    <p className="px-1 text-xs text-muted-foreground/50">
                      No tasks
                    </p>
                  ) : (
                    <div className="flex flex-col gap-3.5">
                      {epicGroups.map(([epicKey, epicTasks]) => {
                        const firstTaskEpicId =
                          epicTasks[0]?.epic_id ??
                          (epicKey !== "no-epic" ? epicKey : undefined);
                        const epic = firstTaskEpicId
                          ? epics.get(firstTaskEpicId)
                          : undefined;
                        const collapseKey = `${column.key}:${epicKey}`;
                        const isCollapsed = !!collapsedEpics[collapseKey];

                        return (
                          <Card
                            key={epicKey}
                            size="sm"
                            className={cn(
                              "gap-0 cursor-pointer py-3 bg-zinc-900 ring-white/[0.04]",
                            )}
                            onClick={() => toggleEpic(column.key, epicKey)}
                          >
                            <CardContent>
                              <div className="flex w-full items-center justify-between gap-2 px-1 py-1.5 text-sm font-medium">
                                <span className="flex items-center gap-2 truncate">
                                  <span className="shrink-0 text-xs leading-none">
                                    {getEpicEmoji(epic)}
                                  </span>
                                  <span className="truncate">
                                    {getEpicTitle(epic, firstTaskEpicId)}
                                  </span>
                                </span>
                                <HugeiconsIcon
                                  icon={
                                    isCollapsed
                                      ? ArrowRight01Icon
                                      : ArrowDown01Icon
                                  }
                                  className="size-4 shrink-0 text-muted-foreground"
                                />
                              </div>

                              {!isCollapsed && epicTasks.length === 0 && (
                                <p className="px-1 pt-1.5 text-xs text-muted-foreground/50">
                                  No tasks yet
                                </p>
                              )}

                              {!isCollapsed &&
                                epicTasks.length > 0 &&
                                (column.key === "done" ? (
                                  <ul
                                    className="flex flex-col pt-1.5"
                                    onClick={(e) => e.stopPropagation()}
                                  >
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
                                  <ul
                                    className="flex flex-col gap-3 pt-2.5"
                                    onClick={(e) => e.stopPropagation()}
                                  >
                                    {epicTasks.map((task) => (
                                      <li key={task.id}>
                                        <TaskCard
                                          task={task}
                                          epic={
                                            task.epic_id
                                              ? epics.get(task.epic_id)
                                              : undefined
                                          }
                                          moving={!!movingTaskIds[task.id]}
                                          onClick={() => handleTaskClick(task)}
                                        />
                                      </li>
                                    ))}
                                  </ul>
                                ))}
                            </CardContent>
                          </Card>
                        );
                      })}
                    </div>
                  )}

                  {column.key === "done" && hasMoreClosed && (
                    <div className="px-1 pt-3">
                      <button
                        type="button"
                        onClick={handleLoadMoreClosed}
                        disabled={loadingMoreClosed}
                        className="w-full rounded-md border border-white/[0.06] bg-zinc-900/60 px-3 py-2 text-xs font-medium text-muted-foreground transition-colors hover:bg-zinc-800 disabled:cursor-not-allowed disabled:opacity-60"
                      >
                        {loadingMoreClosed ? "Loading…" : "Load more"}
                      </button>
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
        epic={
          selectedTask?.epic_id ? epics.get(selectedTask.epic_id) : undefined
        }
        onClose={() => setSelectedTask(null)}
      />
    </div>
  );
}
