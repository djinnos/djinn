/**
 * The filter header shared by the Tasks board and the Dependencies graph:
 * projects / epics / types / owners filters, a search box, and a Tasks ⇄
 * Dependencies view toggle (in the slot the old refresh button used).
 */
import { useEffect, useMemo, useState } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { usersQueryOptions } from "@/api/queryOptions";
import { type OrgUser, userDisplayName } from "@/api/users";
import { useTaskStore } from "@/stores/useTaskStore";
import { useEpicStore } from "@/stores/useEpicStore";
import { useProjects } from "@/stores/useProjectStore";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  KanbanIcon,
  Loading02Icon,
  Search01Icon,
  WorkflowSquare06Icon,
} from "@hugeicons/core-free-icons";
import { useBoardSearchStore } from "@/stores/useBoardSearchStore";
import { cn } from "@/lib/utils";
import {
  Combobox,
  ComboboxContent,
  ComboboxEmpty,
  ComboboxInput,
  ComboboxItem,
  ComboboxList,
} from "@/components/ui/combobox";
import {
  InputGroup,
  InputGroupAddon,
  InputGroupInput,
} from "@/components/ui/input-group";
import {
  buildOwnerOptions,
  ISSUE_TYPES,
  makeOwnerLabel,
  UNASSIGNED_OWNER,
  useBoardFilters,
} from "./boardFilters";
import {
  EpicFilter,
  OwnerAvatar,
  ProjectFilter,
} from "./boardFilterControls";

export const TASKS_PATH = "/tasks";
export const DEPENDENCIES_PATH = "/dependencies";

function ViewToggle() {
  const location = useLocation();
  const navigate = useNavigate();
  const isDependencies = location.pathname === DEPENDENCIES_PATH;

  // Preserve the current filters (query string) when switching views.
  const go = (path: string) =>
    navigate({ pathname: path, search: location.search });

  const baseBtn =
    "flex h-8 w-9 items-center justify-center transition-colors first:rounded-l-lg last:rounded-r-lg";
  const active = "bg-input/60 text-foreground";
  const inactive = "text-muted-foreground hover:text-foreground hover:bg-accent";

  return (
    <div
      role="tablist"
      aria-label="Board view"
      className="flex items-center rounded-lg border border-input dark:bg-input/30"
    >
      <button
        type="button"
        role="tab"
        aria-selected={!isDependencies}
        aria-label="Tasks view"
        title="Tasks"
        onClick={() => go(TASKS_PATH)}
        className={cn(baseBtn, !isDependencies ? active : inactive)}
      >
        <HugeiconsIcon icon={KanbanIcon} className="size-4" />
      </button>
      <button
        type="button"
        role="tab"
        aria-selected={isDependencies}
        aria-label="Dependencies view"
        title="View dependencies"
        onClick={() => go(DEPENDENCIES_PATH)}
        className={cn(
          baseBtn,
          "border-l border-input",
          isDependencies ? active : inactive,
        )}
      >
        <HugeiconsIcon icon={WorkflowSquare06Icon} className="size-4" />
      </button>
    </div>
  );
}

export function BoardFilterHeader() {
  const tasks = useTaskStore((state) => Array.from(state.tasks.values()));
  const epics = useEpicStore((state) => state.epics);
  const projects = useProjects();

  const { data: users = [] } = useQuery(usersQueryOptions());
  const userById = useMemo(() => {
    const map = new Map<string, OrgUser>();
    for (const user of users) map.set(user.id, user);
    return map;
  }, [users]);
  const ownerLabel = useMemo(() => {
    const labels = new Map<string, string>();
    for (const user of users) labels.set(user.id, userDisplayName(user));
    return makeOwnerLabel(labels);
  }, [users]);

  const {
    projectFilters,
    epicFilters,
    ownerFilters,
    issueTypeFilters,
    search,
    setProjectFilters,
    setEpicFilters,
    setOwnerFilters,
    setIssueTypeFilters,
    setSearch,
  } = useBoardFilters();

  // A subtle spinner replaces the search glyph while a backend search runs.
  const searching = useBoardSearchStore((state) => state.searching);

  const epicOptions = useMemo(
    () =>
      Array.from(epics.values()).sort((a, b) =>
        (a.title ?? "").localeCompare(b.title ?? ""),
      ),
    [epics],
  );

  const ownerOptions = useMemo(
    () => buildOwnerOptions(tasks, epics.values(), ownerLabel),
    [tasks, epics, ownerLabel],
  );

  // Local input mirrors the URL `q` but debounces writes so typing stays snappy.
  // When `q` changes from elsewhere (toggle, back/forward), re-sync during
  // render — the React-recommended way to adjust state to a changing source.
  const [searchInput, setSearchInput] = useState(search);
  const [syncedSearch, setSyncedSearch] = useState(search);
  if (search !== syncedSearch) {
    setSyncedSearch(search);
    setSearchInput(search);
  }
  useEffect(() => {
    if (searchInput === search) return;
    const t = setTimeout(() => setSearch(searchInput), 250);
    return () => clearTimeout(t);
  }, [searchInput, search, setSearch]);

  return (
    <div className="flex flex-wrap items-center gap-3 border-b border-white/[0.04] px-4 pb-5">
      <ProjectFilter
        projects={projects}
        selected={projectFilters}
        onChange={setProjectFilters}
      />

      <EpicFilter
        epics={epicOptions}
        selected={epicFilters}
        onChange={setEpicFilters}
      />

      <Combobox
        multiple
        value={issueTypeFilters}
        onValueChange={(v) => setIssueTypeFilters(v ?? [])}
        itemToStringLabel={(val) =>
          ISSUE_TYPES.find((t) => t.value === val)?.label ?? val
        }
      >
        <ComboboxInput
          placeholder={
            issueTypeFilters.length > 0
              ? `${issueTypeFilters.length} type${issueTypeFilters.length > 1 ? "s" : ""}`
              : "All types"
          }
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
        itemToStringLabel={ownerLabel}
      >
        <ComboboxInput
          placeholder={
            ownerFilters.length > 0
              ? `${ownerFilters.length} owner${ownerFilters.length > 1 ? "s" : ""}`
              : "All owners"
          }
          className="w-44"
        />
        <ComboboxContent className="min-w-60">
          <ComboboxList>
            <ComboboxEmpty>No owners found</ComboboxEmpty>
            {ownerOptions.map((owner) => (
              <ComboboxItem key={owner} value={owner}>
                <OwnerAvatar
                  user={
                    owner === UNASSIGNED_OWNER ? undefined : userById.get(owner)
                  }
                />
                <span className="truncate">{ownerLabel(owner)}</span>
              </ComboboxItem>
            ))}
          </ComboboxList>
        </ComboboxContent>
      </Combobox>

      <div className="ml-auto flex items-center gap-2">
        <InputGroup className="w-56">
          <InputGroupAddon>
            {searching ? (
              <HugeiconsIcon
                icon={Loading02Icon}
                className="size-3.5 animate-spin text-muted-foreground"
                aria-label="Searching"
              />
            ) : (
              <HugeiconsIcon icon={Search01Icon} className="size-3.5" />
            )}
          </InputGroupAddon>
          <InputGroupInput
            value={searchInput}
            onChange={(e) => setSearchInput(e.target.value)}
            placeholder="Search tasks..."
          />
        </InputGroup>
        <ViewToggle />
      </div>
    </div>
  );
}
