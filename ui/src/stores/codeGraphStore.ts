/**
 * codeGraphStore — Zustand slice owning the cross-page selection / citation
 * state for `/code-graph` and the chat citation round-trip (PR D5).
 *
 * Source-of-truth contract per the plan:
 *
 * - `/chat` writes `citationIds` (and bumps `selectionId`) before it
 *   navigates, so when `<CodeGraphCanvas>` mounts on `/code-graph` the
 *   highlight reducer (PR D3) sees the selection synchronously.
 * - `/code-graph` writes `selectionId` on hover/click; chat reads
 *   nothing back — the store is one-way for now.
 *
 * This file ships the *fields and setters* the chat path (D5) needs.
 * D3 builds the reducer that turns these fields into Sigma node
 * decorations; nothing in this file talks to Sigma directly.
 */

import { create } from "zustand";

export interface CodeGraphState {
  /**
   * The "currently focused" node — last clicked / pinned via citation.
   * Null when nothing is selected. The canvas pans to this node and
   * adds a persistent ring around it.
   */
  selectionId: string | null;
  /**
   * Set of node ids that should pulse (i.e. recently arrived via a
   * citation click).  Multiple citations from the same chat reply may
   * land here at once.
   */
  citationIds: Set<string>;

  /** Set the persistent selection. Pass `null` to clear. */
  setSelection: (id: string | null) => void;
  /**
   * Replace the active citation set wholesale. Intended for the
   * common case where a chat click lights up exactly one (or a small
   * batch of) refs.
   */
  setCitations: (ids: string[]) => void;
  /** Add a single id to the existing citation set. */
  addCitation: (id: string) => void;
  /** Drop everything from the citation set. */
  clearCitations: () => void;
}

export const useCodeGraphStore = create<CodeGraphState>((set) => ({
  selectionId: null,
  citationIds: new Set<string>(),

  setSelection: (id) => set({ selectionId: id }),
  setCitations: (ids) =>
    set({
      citationIds: new Set(ids),
      // Convenience: also pin the first id as the selection so the
      // canvas pans to it. Clears selection when ids is empty so a
      // stale pin doesn't outlive the citation set.
      selectionId: ids[0] ?? null,
    }),
  addCitation: (id) =>
    set((state) => {
      if (state.citationIds.has(id)) return state;
      const next = new Set(state.citationIds);
      next.add(id);
      return { citationIds: next };
    }),
  clearCitations: () =>
    set({ citationIds: new Set<string>(), selectionId: null }),
}));
