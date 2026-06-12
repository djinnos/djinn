import { AlertCircleIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { Card, CardContent } from "@/components/ui/card";
import { useAuthUser } from "@/components/AuthGate";
import {
  useDispatchPauseStore,
  type DispatchPauseEntry,
} from "@/stores/dispatchPauseStore";
import {
  ALL_PROJECTS,
  useProjects,
  useSelectedProjectId,
} from "@/stores/useProjectStore";

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

function formatScope(entry: DispatchPauseEntry): string {
  if (entry.scope === "global") return "Global dispatch pause";
  if (entry.scope === "project") {
    return `Project dispatch pause${entry.target_id ? `: ${entry.target_id}` : ""}`;
  }
  return `User dispatch pause${entry.target_id ? `: ${entry.target_id}` : ""}`;
}

function formatDate(value?: string | null): string | null {
  if (!value) return null;
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return date.toLocaleString();
}

function unique(entries: DispatchPauseEntry[]): DispatchPauseEntry[] {
  const seen = new Set<string>();
  return entries.filter((entry) => {
    const key = `${entry.scope}:${entry.target_id ?? ""}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

export function getApplicableDispatchPauseEntries({
  entries,
  selectedProjectId,
  currentUserId,
  allProjectIds = [],
}: {
  entries: DispatchPauseEntry[];
  selectedProjectId?: string | null;
  currentUserId?: string | null;
  allProjectIds?: string[];
}): DispatchPauseEntry[] {
  return unique(
    entries.filter((entry) => {
      if (entry.scope === "global") return true;
      if (entry.scope === "user") {
        return Boolean(currentUserId && entry.target_id === currentUserId);
      }
      if (!entry.target_id) return false;
      if (selectedProjectId === ALL_PROJECTS) {
        return allProjectIds.length === 0 || allProjectIds.includes(entry.target_id);
      }
      return selectedProjectId === entry.target_id;
    }),
  );
}

export function DispatchPauseBanner({
  entries,
  selectedProjectId: selectedProjectIdOverride,
  currentUserId: currentUserIdOverride,
  allProjectIds: allProjectIdsOverride,
}: DispatchPauseBannerProps) {
  const selectedProjectId = useSelectedProjectId();
  const projects = useProjects();
  const authUser = useAuthUser();
  const storeEntries = useDispatchPauseStore((state) => {
    const allEntries: DispatchPauseEntry[] = [];
    if (state.global) allEntries.push(state.global);
    allEntries.push(...Object.values(state.projects));
    allEntries.push(...Object.values(state.users));
    return allEntries;
  });

  const applicableEntries = getApplicableDispatchPauseEntries({
    entries: entries ?? storeEntries,
    selectedProjectId: selectedProjectIdOverride ?? selectedProjectId,
    currentUserId: currentUserIdOverride ?? authUser?.id ?? null,
    allProjectIds: allProjectIdsOverride ?? projects.map((project) => project.id),
  });

  if (applicableEntries.length === 0) return null;

  return (
    <Card
      role="status"
      aria-label="Dispatch paused"
      className="mx-4 border-amber-500/30 bg-amber-500/10"
    >
      <CardContent className="py-4">
        <div className="flex items-start gap-3">
          <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-amber-500/20">
            <HugeiconsIcon icon={AlertCircleIcon} className="size-4 text-amber-400" />
          </div>
          <div className="min-w-0 flex-1 space-y-2">
            <div>
              <h3 className="text-sm font-semibold text-amber-100">
                Dispatch is paused
              </h3>
              <p className="text-sm text-amber-100/80">
                Running sessions and chat are unaffected; new dispatch is deferred until the pause is resumed.
              </p>
            </div>
            <ul className="space-y-2" aria-label="Active dispatch pauses">
              {applicableEntries.map((entry) => {
                const pausedAt = formatDate(entry.paused_at);
                return (
                  <li
                    key={`${entry.scope}:${entry.target_id ?? "global"}`}
                    className="rounded-md border border-amber-500/20 bg-background/30 p-2 text-xs text-muted-foreground"
                  >
                    <div className="font-medium text-amber-100">
                      {formatScope(entry)}
                    </div>
                    <div className="mt-1 flex flex-wrap gap-x-3 gap-y-1">
                      {entry.paused_by ? <span>Paused by {entry.paused_by}</span> : null}
                      {pausedAt ? <span>Paused at {pausedAt}</span> : null}
                      {entry.reason ? <span>Reason: {entry.reason}</span> : null}
                    </div>
                  </li>
                );
              })}
            </ul>
          </div>
        </div>
      </CardContent>
    </Card>
  );
}
