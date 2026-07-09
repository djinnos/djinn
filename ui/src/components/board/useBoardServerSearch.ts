/**
 * Debounced backend search for the Kanban board.
 *
 * The board only loads active tasks plus the first page of merged tasks, so a
 * purely client-side search box can never match old merged tasks that were
 * never loaded. This hook closes that gap: while the search query is non-empty
 * it debounces, calls `task_list` with the case-insensitive `text` filter for
 * every project, and additively merges the matches into the task store. The
 * existing client-side predicate then displays them.
 *
 * The fetch is idempotent (id-keyed store) and never touches the Merged-column
 * pagination cursors, so "Load more" keeps working independently.
 */

import { useEffect } from "react";
import { useProjects } from "@/stores/useProjectStore";
import { searchTasksAcrossProjects } from "@/api/server";
import { boardSearchStore } from "@/stores/boardSearchStore";
import { useBoardFilters } from "./boardFilters";

/** Debounce window before a keystroke triggers a backend search. */
export const SEARCH_DEBOUNCE_MS = 300;

export function useBoardServerSearch(options?: { enabled?: boolean }): void {
  const enabled = options?.enabled ?? true;
  const { search } = useBoardFilters();
  const projects = useProjects();
  // Re-run only when the *set* of projects changes, not on every metadata edit.
  const slugKey = projects
    .map((p) => `${p.github_owner}/${p.github_repo}`)
    .sort()
    .join(",");

  useEffect(() => {
    if (!enabled) return;
    const query = search.trim();
    if (query.length === 0) {
      boardSearchStore.getState().setSearching(false);
      return;
    }
    const slugs = slugKey ? slugKey.split(",") : [];
    if (slugs.length === 0) return;

    let cancelled = false;
    const timer = setTimeout(async () => {
      boardSearchStore.getState().setSearching(true);
      try {
        await searchTasksAcrossProjects(slugs, query);
      } catch (error) {
        console.error("Board search failed:", error);
      } finally {
        if (!cancelled) boardSearchStore.getState().setSearching(false);
      }
    }, SEARCH_DEBOUNCE_MS);

    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [enabled, search, slugKey]);
}
