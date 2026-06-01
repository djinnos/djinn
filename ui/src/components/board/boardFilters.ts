/**
 * Shared board filter state + predicates, used by BOTH the Tasks board and the
 * Dependencies graph so the two views stay in sync as you toggle between them.
 *
 * Filter state lives entirely in the URL search params (`project`, `epic`,
 * `owner`, `type`, `q`), so switching views (or reloading) preserves it.
 *
 * The filter *controls* (the dropdowns/avatars) live in `boardFilterControls`;
 * this file is logic-only so it can be imported anywhere without pulling JSX.
 */
import { useCallback, useMemo } from "react";
import { useSearchParams } from "react-router-dom";
import type { Epic, Task } from "@/api/types";

/** Owner-filter sentinel for tasks/epics with no human creator (agents). */
export const UNASSIGNED_OWNER = "__none__";
export const UNASSIGNED_OWNER_LABEL = "Unassigned";

export const ISSUE_TYPES = [
  { value: "task", label: "Task" },
  { value: "feature", label: "Feature" },
  { value: "bug", label: "Bug" },
  { value: "spike", label: "Spike" },
  { value: "research", label: "Research" },
  { value: "decomposition", label: "Breakdown" },
  { value: "review", label: "Review" },
] as const;

export const EPIC_STATUS_GROUPS: Array<{ key: string; label: string }> = [
  { key: "open", label: "Open" },
  { key: "drafting", label: "Drafting" },
  { key: "closed", label: "Closed" },
];

export function getEpicEmoji(epic: Epic | undefined): string {
  return epic?.emoji ?? "📌";
}

/** The current filter values, read from the URL. */
export interface BoardFilters {
  projectFilters: string[];
  epicFilters: string[];
  ownerFilters: string[];
  issueTypeFilters: string[];
  /** Free-text search; the board filters on it, the graph highlights on it. */
  search: string;
}

export interface BoardFilterControls extends BoardFilters {
  setProjectFilters: (ids: string[]) => void;
  setEpicFilters: (ids: string[]) => void;
  setOwnerFilters: (ids: string[]) => void;
  setIssueTypeFilters: (vals: string[]) => void;
  setSearch: (q: string) => void;
}

function readList(params: URLSearchParams, key: string): string[] {
  return (params.get(key) ?? "").split(",").filter(Boolean);
}

/**
 * Reads/writes the board filter values from the URL search params. Both the
 * Tasks board and the Dependencies graph call this, so they share one source
 * of truth that survives view toggles and reloads.
 */
export function useBoardFilters(): BoardFilterControls {
  const [searchParams, setSearchParams] = useSearchParams();

  const projectFilters = useMemo(
    () => readList(searchParams, "project"),
    [searchParams],
  );
  const epicFilters = useMemo(
    () => readList(searchParams, "epic"),
    [searchParams],
  );
  const ownerFilters = useMemo(
    () => readList(searchParams, "owner"),
    [searchParams],
  );
  const issueTypeFilters = useMemo(
    () => readList(searchParams, "type"),
    [searchParams],
  );
  const search = searchParams.get("q") ?? "";

  const setParam = useCallback(
    (key: string, value: string) => {
      setSearchParams(
        (prev) => {
          const next = new URLSearchParams(prev);
          if (value) next.set(key, value);
          else next.delete(key);
          return next;
        },
        { replace: true },
      );
    },
    [setSearchParams],
  );

  const setList = useCallback(
    (key: string) => (ids: string[]) => setParam(key, ids.join(",")),
    [setParam],
  );

  return {
    projectFilters,
    epicFilters,
    ownerFilters,
    issueTypeFilters,
    search,
    setProjectFilters: useMemo(() => setList("project"), [setList]),
    setEpicFilters: useMemo(() => setList("epic"), [setList]),
    setOwnerFilters: useMemo(() => setList("owner"), [setList]),
    setIssueTypeFilters: useMemo(() => setList("type"), [setList]),
    setSearch: useCallback((q: string) => setParam("q", q.trim()), [setParam]),
  };
}

/** Resolve an owner id to a display label given the org roster. */
export function makeOwnerLabel(
  userLabelById: Map<string, string>,
): (ownerId: string) => string {
  return (ownerId: string) =>
    ownerId === UNASSIGNED_OWNER
      ? UNASSIGNED_OWNER_LABEL
      : (userLabelById.get(ownerId) ?? ownerId);
}

/**
 * The distinct owner ids present across tasks + epics, sorted by label.
 * Epic creators are folded in so an owner with only un-broken-down epic
 * shells still appears in the filter.
 */
export function buildOwnerOptions(
  tasks: Task[],
  epics: Iterable<Epic>,
  ownerLabel: (id: string) => string,
): string[] {
  const owners = new Set<string>();
  for (const task of tasks)
    owners.add(task.created_by_user_id ?? UNASSIGNED_OWNER);
  for (const epic of epics) owners.add(epic.created_by_user_id ?? UNASSIGNED_OWNER);
  return Array.from(owners).sort((a, b) =>
    ownerLabel(a).localeCompare(ownerLabel(b)),
  );
}

/** Whether a task passes the structured (non-text) filters. */
export function matchesStructuredFilters(
  task: Task,
  filters: Pick<
    BoardFilters,
    "projectFilters" | "epicFilters" | "ownerFilters" | "issueTypeFilters"
  >,
): boolean {
  const { projectFilters, epicFilters, ownerFilters, issueTypeFilters } =
    filters;
  if (
    projectFilters.length > 0 &&
    !projectFilters.includes(task.project_id ?? "")
  )
    return false;
  if (epicFilters.length > 0 && !epicFilters.includes(task.epic_id ?? ""))
    return false;
  if (
    ownerFilters.length > 0 &&
    !ownerFilters.includes(task.created_by_user_id ?? UNASSIGNED_OWNER)
  )
    return false;
  if (
    issueTypeFilters.length > 0 &&
    !issueTypeFilters.includes(task.issue_type ?? "task")
  )
    return false;
  return true;
}

/** Whether an epic passes the owner filter. */
export function epicMatchesOwnerFilter(
  epic: Epic,
  ownerFilters: string[],
): boolean {
  if (ownerFilters.length === 0) return true;
  return ownerFilters.includes(epic.created_by_user_id ?? UNASSIGNED_OWNER);
}
