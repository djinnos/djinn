import { useEffect, useMemo, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useTaskStore } from "@/stores/useTaskStore";
import { useEpicStore } from "@/stores/useEpicStore";
import { useProjects } from "@/stores/useProjectStore";
import { taskStore } from "@/stores/taskStore";
import type { Epic, Task } from "@/api/types";
import { TaskCard, DoneTaskRow } from "@/components/TaskCard";
import { TaskIdLabel } from "@/components/TaskIdLabel";
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
import { useQuery } from "@tanstack/react-query";
import { usersQueryOptions } from "@/api/queryOptions";
import { UserAvatar } from "@/components/UserAvatar";
import { HugeiconsIcon } from "@hugeicons/react";
import { cn } from "@/lib/utils";
import {
  epicMatchesOwnerFilter,
  matchesStructuredFilters,
  useBoardFilters,
} from "@/components/board/boardFilters";
import { useBoardServerSearch } from "@/components/board/useBoardServerSearch";

type ColumnKey = "open" | "in_progress" | "pr_ready" | "done";

const NO_PROPOSAL_KEY = "no-proposal";

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

// Shared column grid: the sticky status header and every swimlane use the same
// template so cards stay aligned under their column while scrolling lanes.
const COLUMN_GRID_CLASS =
  "grid grid-cols-[repeat(4,minmax(280px,1fr))] gap-x-8";

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

/**
 * One full-width swimlane. Lanes are building proposals (Linear's "project"
 * rows), labelled with the build owner's avatar; everything without a
 * building-proposal linkage (standalone epics, un-epiced tasks) folds into a
 * single catch-all lane at the bottom. The epic renders on each card, not on
 * the lane.
 */
type Lane = {
  key: string;
  kind: "proposal" | "other";
  title: string;
  proposalId?: string;
  proposalShortId?: string | null;
  buildOwnerUserId?: string | null;
  columns: Map<ColumnKey, Task[]>;
  total: number;
  mergedCount: number;
};

/** Whether an epic belongs to a proposal that is actively building. */
function isBuildingProposalEpic(epic: Epic | undefined): boolean {
  return !!epic?.proposal_id && epic.proposal_status === "building";
}

type KanbanBoardProps = {
  tasks?: Task[];
  epics?: Map<string, Epic>;
  /** Lane keys (`proposal:<id>`, `epic:<id>`, or `no-epic`) that start collapsed. */
  initialCollapsedLanes?: string[];
};

export function KanbanBoard({
  tasks: tasksProp,
  epics: epicsProp,
  initialCollapsedLanes,
}: KanbanBoardProps = {}) {
  const navigate = useNavigate();
  const storeTasks = useTaskStore((state) => Array.from(state.tasks.values()));
  const storeEpics = useEpicStore((state) => state.epics);
  const projects = useProjects();

  // Org roster, for the build-owner avatar on proposal swimlanes.
  const { data: orgUsers = [] } = useQuery(usersQueryOptions());
  const usersById = useMemo(
    () => new Map(orgUsers.map((u) => [u.id, u])),
    [orgUsers],
  );

  // Filter values come from the shared header via the URL search params.
  const { projectFilters, epicFilters, ownerFilters, issueTypeFilters, search } =
    useBoardFilters();

  // Backend-backed search: when the query is non-empty, fetch matching tasks
  // (including old merged ones the board never loaded) into the store. Disabled
  // in controlled mode (`tasksProp`, used by tests) where the store is unused.
  useBoardServerSearch({ enabled: !tasksProp });

  const tasks = tasksProp ?? storeTasks;
  const epics = epicsProp ?? storeEpics;
  const [collapsedLanes, setCollapsedLanes] = useState<Record<string, boolean>>(
    () => {
      const next: Record<string, boolean> = {};
      for (const key of initialCollapsedLanes ?? []) next[key] = true;
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

  // Group tasks into swimlanes: one lane per building proposal (Linear
  // "project" rows), plus a single catch-all lane for everything without a
  // building-proposal linkage. Merged tasks whose proposal is no longer
  // building are dropped — retired proposals age off the board — while their
  // *active* stragglers stay visible in the catch-all so live work never
  // disappears.
  const lanes = useMemo<Lane[]>(() => {
    const byLane = new Map<string, Lane>();

    const proposalLane = (epic: Epic): Lane => {
      const key = `proposal:${epic.proposal_id}`;
      let lane = byLane.get(key);
      if (!lane) {
        lane = {
          key,
          kind: "proposal",
          title:
            epic.proposal_title ??
            (epic.proposal_short_id
              ? `Proposal ${epic.proposal_short_id}`
              : "Proposal"),
          proposalId: epic.proposal_id!,
          proposalShortId: epic.proposal_short_id,
          buildOwnerUserId: epic.proposal_build_owner_user_id,
          columns: new Map(),
          total: 0,
          mergedCount: 0,
        };
        byLane.set(key, lane);
      }
      return lane;
    };

    const otherLane = (): Lane => {
      let lane = byLane.get(NO_PROPOSAL_KEY);
      if (!lane) {
        lane = {
          key: NO_PROPOSAL_KEY,
          kind: "other",
          title: "No proposal",
          columns: new Map(),
          total: 0,
          mergedCount: 0,
        };
        byLane.set(NO_PROPOSAL_KEY, lane);
      }
      return lane;
    };

    for (const task of filteredTasks) {
      const columnKey = taskToColumnKey(task);
      if (columnKey === null) continue;
      const epic = task.epic_id ? epics.get(task.epic_id) : undefined;

      let lane: Lane;
      if (epic && isBuildingProposalEpic(epic)) {
        lane = proposalLane(epic);
      } else {
        // Merged work of a retired (non-building) proposal ages off the board.
        if (epic?.proposal_id && columnKey === "done") continue;
        lane = otherLane();
      }

      const existing = lane.columns.get(columnKey) ?? [];
      existing.push(task);
      lane.columns.set(columnKey, existing);
      lane.total += 1;
      if (columnKey === "done") lane.mergedCount += 1;
    }

    // Seed lanes for building proposals whose epics have no tasks yet, so a
    // freshly kicked-off build is visible before its breakdown lands.
    const visibleEpicIds = epicFilters.length > 0 ? new Set(epicFilters) : null;
    for (const [epicId, epic] of epics) {
      if (epic.status !== "open") continue;
      if (!isBuildingProposalEpic(epic)) continue;
      if (visibleEpicIds && !visibleEpicIds.has(epicId)) continue;
      // Scope empty epic shells to the owner filter the same way tasks are:
      // an epic is "owned" by its `created_by_user_id` (which its tasks
      // inherit), so a shell only shows when its owner is selected.
      if (!epicMatchesOwnerFilter(epic, ownerFilters)) continue;
      proposalLane(epic);
    }

    return Array.from(byLane.values()).sort((a, b) => {
      if (a.kind !== b.kind) return a.kind === "proposal" ? -1 : 1;
      return a.title.localeCompare(b.title);
    });
  }, [filteredTasks, epics, epicFilters, ownerFilters]);

  const columnCounts = useMemo(() => {
    const counts = new Map<ColumnKey, number>();
    for (const column of STATUS_COLUMNS) counts.set(column.key, 0);
    for (const lane of lanes) {
      for (const [columnKey, columnTasks] of lane.columns) {
        counts.set(columnKey, (counts.get(columnKey) ?? 0) + columnTasks.length);
      }
    }
    return counts;
  }, [lanes]);

  const toggleLane = (laneKey: string) => {
    setCollapsedLanes((prev) => ({
      ...prev,
      [laneKey]: !prev[laneKey],
    }));
  };

  const handleLaneNameClick = (lane: Lane) => {
    if (lane.kind === "proposal" && lane.proposalId) {
      navigate(`/proposals/${lane.proposalId}`);
    }
  };

  return (
    <div className="flex h-full min-h-0 flex-col gap-5 overflow-hidden px-4 pt-4 pb-2">
      <BoardHealthBanner projectSlugs={bannerSlugs} />

      <GitHubAppBanner projectSlugs={bannerSlugs} />

      <div className="min-h-0 flex-1 overflow-auto pb-1">
        <div className="min-w-fit">
          {/* Status column header — sticky above the swimlanes. */}
          <div
            className={cn(
              COLUMN_GRID_CLASS,
              "sticky top-0 z-20 bg-background/95 px-1 backdrop-blur-sm",
            )}
          >
            {STATUS_COLUMNS.map((column) => {
              const taskCount = columnCounts.get(column.key) ?? 0;
              return (
                <div key={column.key} className="flex flex-col">
                  <div className="relative pb-2.5 pt-1.5 text-sm font-semibold">
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
                      </span>
                    </div>
                  </div>
                  <div
                    className={cn(
                      "h-0.5 w-10 rounded-full",
                      column.colorClass,
                      column.glowClass,
                    )}
                  />
                </div>
              );
            })}
          </div>

          {lanes.length === 0 ? (
            <p className="px-1 pt-4 text-xs text-muted-foreground/50">
              No tasks
            </p>
          ) : (
            <div className="flex flex-col pt-3">
              {lanes.map((lane) => {
                const isCollapsed = !!collapsedLanes[lane.key];
                const isClickableName = lane.kind === "proposal";

                return (
                  <section key={lane.key} className="pb-4">
                    {/* Swimlane header: full-width row spanning all columns. */}
                    <div
                      className="flex cursor-pointer items-center gap-2 rounded-md bg-zinc-900 px-2.5 py-2 ring-1 ring-white/[0.04] transition-colors hover:bg-zinc-800/80"
                      onClick={() => toggleLane(lane.key)}
                      data-testid={`lane-header-${lane.key}`}
                    >
                      <HugeiconsIcon
                        icon={isCollapsed ? ArrowRight01Icon : ArrowDown01Icon}
                        className="size-4 shrink-0 text-muted-foreground"
                      />
                      {lane.kind === "proposal" && (
                        <UserAvatar
                          user={usersById.get(lane.buildOwnerUserId ?? "")}
                          className="size-4"
                        />
                      )}
                      {lane.proposalShortId && (
                        <TaskIdLabel
                          taskId={lane.proposalId ?? lane.proposalShortId}
                          shortId={lane.proposalShortId}
                          className="shrink-0"
                        />
                      )}
                      {isClickableName ? (
                        <button
                          type="button"
                          className="cursor-pointer truncate text-sm font-medium hover:underline"
                          onClick={(e) => {
                            e.stopPropagation();
                            handleLaneNameClick(lane);
                          }}
                          title={lane.title}
                        >
                          {lane.title}
                        </button>
                      ) : (
                        <span className="truncate text-sm font-medium">
                          {lane.title}
                        </span>
                      )}
                      <span className="text-xs text-muted-foreground">
                        {lane.total}
                      </span>
                      <div className="flex-1" />
                      {lane.mergedCount > 0 && (
                        <span className="shrink-0 text-[11px] text-muted-foreground">
                          {lane.mergedCount}/{lane.total} merged
                        </span>
                      )}
                    </div>

                    {/* Lane body: cards aligned under their status column. */}
                    {!isCollapsed &&
                      (lane.total === 0 ? (
                        <p className="px-2 pt-2.5 pb-1 text-xs text-muted-foreground/50">
                          No tasks yet
                        </p>
                      ) : (
                        <div className="relative">
                          {/* Column separators, scoped to this lane's card
                              area so they never cross the lane header rows.
                              Same grid template as the cards, one hairline
                              centered in each column gap. */}
                          <div
                            aria-hidden
                            className={cn(
                              COLUMN_GRID_CLASS,
                              "pointer-events-none absolute inset-x-0 top-3 bottom-1 px-1",
                            )}
                          >
                            {STATUS_COLUMNS.map((column, index) => (
                              <div
                                key={column.key}
                                className={
                                  index === 0
                                    ? undefined
                                    : "-ml-4 w-px bg-border/70"
                                }
                              />
                            ))}
                          </div>
                          <div className={cn(COLUMN_GRID_CLASS, "px-1 pt-4 pb-3")}>
                          {STATUS_COLUMNS.map((column) => {
                            const columnTasks =
                              lane.columns.get(column.key) ?? [];
                            if (columnTasks.length === 0) {
                              return <div key={column.key} />;
                            }
                            return column.key === "done" ? (
                              <ul
                                key={column.key}
                                data-testid={`lane-${lane.key}-${column.key}`}
                                className="flex flex-col self-start"
                              >
                                {columnTasks.map((task) => (
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
                                key={column.key}
                                data-testid={`lane-${lane.key}-${column.key}`}
                                className="flex flex-col gap-3 self-start"
                              >
                                {columnTasks.map((task) => (
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
                            );
                          })}
                          </div>
                        </div>
                      ))}
                  </section>
                );
              })}
            </div>
          )}
        </div>
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
