import { useMemo } from "react";
import { Alert02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { useAuthUser } from "@/components/AuthGate";
import { Card, CardContent } from "@/components/ui/card";
import type { Project } from "@/api/types";
import {
  useDispatchPauseStore,
  type DispatchPauseEntry,
  type DispatchPauseState,
} from "@/stores/dispatchPauseStore";
import { ALL_PROJECTS } from "@/stores/projectStore";
import { useProjectStore } from "@/stores/useProjectStore";

interface ProjectContext {
  isAllProjects: boolean;
  selectedProjectId: string | null;
  projects: Project[];
}

interface DecoratedDispatchPauseEntry {
  entry: DispatchPauseEntry;
  key: string;
  label: string;
  scopeLabel: string;
}

export interface DispatchPauseBannerProps {
  /** Test/story override. When omitted, applicable pauses are selected from stores. */
  entries?: DispatchPauseEntry[];
  /** Test/story override for project filtering. Defaults to projectStore selection. */
  selectedProjectId?: string | null;
  /** Test/story override for current-user filtering. Defaults to AuthGate user id. */
  currentUserId?: string | null;
  /** Test/story override for all-project expansion. Defaults to projectStore projects. */
  allProjectIds?: string[];
}

function normalizeId(value: string | null | undefined): string | null {
  if (typeof value !== "string") return null;
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
}

function projectSlug(project: Project): string {
  return `${project.github_owner}/${project.github_repo}`;
}

function projectIdentifiers(project: Project | undefined): string[] {
  if (!project) return [];
  return [project.id, projectSlug(project)].filter((id) => normalizeId(id) != null);
}

function projectMatchesTarget(project: Project | undefined, targetId: string | null): boolean {
  const normalizedTarget = normalizeId(targetId);
  if (!normalizedTarget) return false;
  return projectIdentifiers(project).some((id) => id === normalizedTarget);
}

function projectLabel(projects: Project[], targetId: string | null): string {
  const project = projects.find((candidate) => projectMatchesTarget(candidate, targetId));
  if (!project) return targetId ? `Project ${targetId}` : "Project";
  return project.name || projectSlug(project);
}

function currentUserPauseApplies(entry: DispatchPauseEntry, currentUserId: string | null): boolean {
  const targetId = normalizeId(entry.target_id);
  const affectsCurrentUser = (entry as DispatchPauseEntry & { affects_current_user?: boolean }).affects_current_user;

  if (currentUserId && targetId === currentUserId) return true;
  if (affectsCurrentUser === true) return true;

  // If the authenticated user is unknown, do not guess based on a user-scope
  // target id. Only the server's explicit current-viewer marker may surface it.
  return false;
}

function stateFromEntryList(entries: DispatchPauseEntry[]): Pick<DispatchPauseState, "global" | "projects" | "users"> {
  const state: Pick<DispatchPauseState, "global" | "projects" | "users"> = {
    global: null,
    projects: {},
    users: {},
  };

  for (const entry of entries) {
    const targetId = normalizeId(entry.target_id);
    if (entry.scope === "global") {
      state.global = { ...entry, target_id: null };
    } else if (entry.scope === "project" && targetId) {
      state.projects[targetId] = { ...entry, target_id: targetId };
    } else if (entry.scope === "user" && targetId) {
      state.users[targetId] = { ...entry, target_id: targetId };
    }
  }

  return state;
}

function projectFromId(projectId: string): Project {
  return {
    id: projectId,
    name: projectId,
    github_owner: "story",
    github_repo: projectId,
  } satisfies Project;
}

// eslint-disable-next-line react-refresh/only-export-components -- pure selector colocated with the banner for its unit tests.
export function selectVisibleDispatchPauseEntries(
  entries: Pick<DispatchPauseState, "global" | "projects" | "users">,
  projectContext: ProjectContext,
  currentUserId: string | null,
): DispatchPauseEntry[] {
  const visible: DispatchPauseEntry[] = [];

  if (entries.global) {
    visible.push(entries.global);
  }

  const projectEntries = Object.values(entries.projects);
  if (projectContext.isAllProjects) {
    visible.push(...projectEntries);
  } else {
    const selectedProject = projectContext.projects.find(
      (project) => project.id === projectContext.selectedProjectId,
    );
    visible.push(
      ...projectEntries.filter((entry) => {
        if (projectMatchesTarget(selectedProject, entry.target_id)) return true;
        return normalizeId(entry.target_id) === normalizeId(projectContext.selectedProjectId);
      }),
    );
  }

  visible.push(
    ...Object.values(entries.users).filter((entry) => currentUserPauseApplies(entry, currentUserId)),
  );

  const seen = new Set<string>();
  return visible.filter((entry) => {
    const key = `${entry.scope}:${entry.target_id ?? ""}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

function formatTimestamp(value: string | null | undefined): string {
  if (!value) return "time unavailable";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(date);
}

function formatReason(value: string | null | undefined): string {
  const reason = normalizeId(value ?? null);
  return reason ?? "No reason provided";
}

function formatPausedBy(value: string | null | undefined): string {
  return normalizeId(value ?? null) ?? "unknown operator";
}

function decorateEntry(entry: DispatchPauseEntry, projects: Project[]): DecoratedDispatchPauseEntry {
  if (entry.scope === "global") {
    return {
      entry,
      key: "global",
      label: "Global dispatch pause",
      scopeLabel: "Global",
    };
  }

  if (entry.scope === "project") {
    const label = projectLabel(projects, entry.target_id);
    return {
      entry,
      key: `project:${entry.target_id ?? ""}`,
      label: `Project dispatch pause: ${label}`,
      scopeLabel: "Project",
    };
  }

  return {
    entry,
    key: `user:${entry.target_id ?? ""}`,
    label: "User dispatch pause affecting you",
    scopeLabel: "User",
  };
}

function summaryFor(entries: DecoratedDispatchPauseEntry[]): string {
  const count = entries.length;
  if (count === 1) {
    return `${entries[0].scopeLabel} dispatch is paused`;
  }

  const scopes = entries.reduce(
    (acc, { entry }) => {
      acc[entry.scope] += 1;
      return acc;
    },
    { global: 0, project: 0, user: 0 } satisfies Record<DispatchPauseEntry["scope"], number>,
  );
  const parts = [
    scopes.global ? `${scopes.global} global` : null,
    scopes.project ? `${scopes.project} project` : null,
    scopes.user ? `${scopes.user} user` : null,
  ].filter(Boolean);

  return `${count} dispatch pauses affect this view (${parts.join(", ")})`;
}

export function DispatchPauseBanner({
  entries: entryOverride,
  selectedProjectId: selectedProjectIdOverride,
  currentUserId: currentUserIdOverride,
  allProjectIds,
}: DispatchPauseBannerProps = {}) {
  const authUser = useAuthUser();
  const projectStoreContext = useProjectStore((state) => ({
    projects: state.projects,
    selectedProjectId: state.selectedProjectId,
  }));
  const storePauseEntries = useDispatchPauseStore((state) => ({
    global: state.global,
    projects: state.projects,
    users: state.users,
  }));

  const selectedProjectId = selectedProjectIdOverride ?? projectStoreContext.selectedProjectId;
  const projects = useMemo(() => {
    if (!allProjectIds) return projectStoreContext.projects;
    const knownProjects = new Map(projectStoreContext.projects.map((project) => [project.id, project]));
    return allProjectIds.map((projectId) => knownProjects.get(projectId) ?? projectFromId(projectId));
  }, [allProjectIds, projectStoreContext.projects]);
  const pauseEntries = useMemo(
    () => (entryOverride ? stateFromEntryList(entryOverride) : storePauseEntries),
    [entryOverride, storePauseEntries],
  );
  const currentUserId = normalizeId(currentUserIdOverride ?? authUser?.id ?? null);

  const visibleEntries = useMemo(
    () =>
      selectVisibleDispatchPauseEntries(
        pauseEntries,
        {
          projects,
          selectedProjectId,
          isAllProjects: selectedProjectId === ALL_PROJECTS,
        },
        currentUserId,
      ),
    [currentUserId, pauseEntries, projects, selectedProjectId],
  );

  const decoratedEntries = useMemo(
    () => visibleEntries.map((entry) => decorateEntry(entry, projects)),
    [projects, visibleEntries],
  );

  if (decoratedEntries.length === 0) return null;

  return (
    <Card
      className="mx-4 border-red-500/25 bg-red-500/[0.06]"
      role="status"
      aria-label="Dispatch paused"
      aria-live="polite"
    >
      <CardContent className="py-3">
        <div className="flex items-start gap-2.5">
          <div className="flex h-6 w-6 shrink-0 items-center justify-center rounded-full bg-red-500/15">
            <HugeiconsIcon icon={Alert02Icon} className="size-3.5 text-red-300" />
          </div>
          <div className="min-w-0 flex-1 space-y-2">
            <div className="space-y-1">
              <p className="text-sm font-semibold text-red-100">{summaryFor(decoratedEntries)}</p>
              <p className="text-xs leading-relaxed text-red-100/80">
                Running sessions and chat are unaffected. New dispatch is deferred until the pause is resumed.
              </p>
            </div>

            <div className="flex flex-col gap-1.5" aria-label="Active dispatch pauses">
              {decoratedEntries.map(({ entry, key, label, scopeLabel }) => (
                <div key={key} className="rounded-md border border-red-500/15 bg-black/10 px-2.5 py-2 text-xs text-red-100/85">
                  <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
                    <span className="font-medium text-red-100">{label}</span>
                    <span className="rounded-full bg-red-500/15 px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wide text-red-200/80">
                      {scopeLabel}
                    </span>
                  </div>
                  <div className="mt-1 flex flex-wrap gap-x-3 gap-y-1 text-[11px] text-red-100/70">
                    <span>
                      <span className="font-medium text-red-100/80">Paused by:</span> {formatPausedBy(entry.paused_by)}
                    </span>
                    <span>
                      <span className="font-medium text-red-100/80">Paused at:</span> {formatTimestamp(entry.paused_at)}
                    </span>
                    <span>
                      <span className="font-medium text-red-100/80">Reason:</span> {formatReason(entry.reason)}
                    </span>
                  </div>
                </div>
              ))}
            </div>
          </div>
        </div>
      </CardContent>
    </Card>
  );
}
