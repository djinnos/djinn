/**
 * Board search store.
 *
 * The Kanban board's search box is backed by the server (so it matches tasks
 * that were never loaded client-side — chiefly old merged tasks). Because the
 * search input lives in the shared `BoardFilterHeader` but the fetch is driven
 * from the board, this tiny store is the channel that lets the header render a
 * subtle in-flight spinner while a server search runs.
 */

import { createStore } from "zustand/vanilla";
import { subscribeWithSelector } from "zustand/middleware";

export interface BoardSearchState {
  /** A backend search fetch is currently in flight. */
  searching: boolean;
  setSearching: (searching: boolean) => void;
}

export const boardSearchStore = createStore<BoardSearchState>()(
  subscribeWithSelector((set) => ({
    searching: false,
    setSearching: (searching) => set({ searching }),
  })),
);

export { useBoardSearchStore } from "./useBoardSearchStore";
