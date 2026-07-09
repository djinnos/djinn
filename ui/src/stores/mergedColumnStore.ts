/**
 * Merged-column pagination store.
 *
 * The Kanban board paints its active columns (Open / In Progress / PR Ready)
 * first, then lazily pages the merged tasks into the Merged column. This store
 * tracks per-project pagination cursors (how many merged rows are loaded,
 * whether the server has more, and the server-reported merged total) so the
 * "Load more" affordance can advance every project that still has more, and the
 * Merged header can show the exact grand total.
 *
 * All-projects mode aggregates every project, so the cursor is kept per project
 * slug. On reconnect / lag re-hydration the whole map is reset and the first
 * merged page is re-fetched (see `fetchClosedFirstPage`).
 *
 * "Merged" here is the backend `status=merged` pseudo-filter (closed AND
 * actually merged), which mirrors the UI's `taskToColumnKey`. Because the
 * server counts only actually-merged rows, `totalMerged` is exact — unlike the
 * old `status=closed` fetch, which overcounted with force-closed/review/spike
 * tasks that `taskToColumnKey` hid.
 */

import { createStore } from "zustand/vanilla";
import { subscribeWithSelector } from "zustand/middleware";

export interface MergedProjectPage {
  slug: string;
  projectId: string | null;
  /** Number of merged rows loaded so far for this project (= next offset). */
  loaded: number;
  /** Server has additional merged pages beyond what we've loaded. */
  hasMore: boolean;
  /**
   * Server-reported total for `status=merged` (closed AND actually merged).
   * This is the EXACT Merged-column total: the backend predicate matches
   * `taskToColumnKey`, so no hidden rows are counted. Drives the Merged header.
   */
  totalMerged: number;
}

export interface MergedColumnState {
  /** Per-project pagination state, keyed by project slug. */
  projects: Record<string, MergedProjectPage>;
  /** A "Load more" fetch is in flight. */
  loadingMore: boolean;

  reset: () => void;
  setProjectPage: (page: MergedProjectPage) => void;
  setLoadingMore: (loading: boolean) => void;

  /** Projects that still have unloaded merged pages. */
  projectsWithMore: () => MergedProjectPage[];
  /** True when any project still has unloaded merged pages. */
  hasMore: () => boolean;
  /** True once at least one project has reported a merged total. */
  hasTotals: () => boolean;
  /** Exact sum of server-reported merged totals across all projects. */
  totalMerged: () => number;
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

    hasTotals: () => Object.keys(get().projects).length > 0,

    totalMerged: () =>
      Object.values(get().projects).reduce((sum, p) => sum + p.totalMerged, 0),
  })),
);

export { useMergedColumnStore } from "./useMergedColumnStore";
