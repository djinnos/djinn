/**
 * Merged-column pagination store.
 *
 * The Kanban board paints its active columns (Open / In Progress / PR Ready)
 * first, then lazily pages the closed/merged tasks into the Merged column. This
 * store tracks per-project pagination cursors (how many closed rows are loaded,
 * whether the server has more, and the server-reported closed total) so the
 * "Load more" affordance can advance every project that still has more.
 *
 * All-projects mode aggregates every project, so the cursor is kept per project
 * slug. On reconnect / lag re-hydration the whole map is reset and the first
 * closed page is re-fetched (see `fetchClosedFirstPage`).
 */

import { createStore } from "zustand/vanilla";
import { subscribeWithSelector } from "zustand/middleware";

export interface MergedProjectPage {
  slug: string;
  projectId: string | null;
  /** Number of closed rows loaded so far for this project (= next offset). */
  loaded: number;
  /** Server has additional closed pages beyond what we've loaded. */
  hasMore: boolean;
  /**
   * Server-reported total for `status=closed`. This OVERCOUNTS the Merged
   * column: it includes force-closed / review / spike tasks that never merged
   * and are hidden by `taskToColumnKey`. Used only for a rough "~N" hint.
   */
  totalClosed: number;
}

export interface MergedColumnState {
  /** Per-project pagination state, keyed by project slug. */
  projects: Record<string, MergedProjectPage>;
  /** A "Load more" fetch is in flight. */
  loadingMore: boolean;

  reset: () => void;
  setProjectPage: (page: MergedProjectPage) => void;
  setLoadingMore: (loading: boolean) => void;

  /** Projects that still have unloaded closed pages. */
  projectsWithMore: () => MergedProjectPage[];
  /** True when any project still has unloaded closed pages. */
  hasMore: () => boolean;
  /** Sum of server-reported closed totals across all projects (rough hint). */
  totalClosed: () => number;
}

export const mergedColumnStore = createStore<MergedColumnState>()(
  subscribeWithSelector((set, get) => ({
    projects: {},
    loadingMore: false,

    reset: () => set({ projects: {}, loadingMore: false }),

    setProjectPage: (page) =>
      set((state) => ({
        projects: { ...state.projects, [page.slug]: page },
      })),

    setLoadingMore: (loading) => set({ loadingMore: loading }),

    projectsWithMore: () =>
      Object.values(get().projects).filter((p) => p.hasMore),

    hasMore: () => Object.values(get().projects).some((p) => p.hasMore),

    totalClosed: () =>
      Object.values(get().projects).reduce((sum, p) => sum + p.totalClosed, 0),
  })),
);

export { useMergedColumnStore } from "./useMergedColumnStore";
