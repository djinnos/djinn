/**
 * memoryGraphStore — UI-side selection/hover/co-access state for the Memory
 * tab graph canvas.
 *
 * Mirrors the shape of `codeGraphStore` but kept minimal: the memory tab's
 * reducer view only needs selection + hover layers (no citation, blast-radius,
 * or kind filters). The co-access toggle is the one piece of canvas-local
 * state that doesn't belong to the highlight pipeline — it controls whether
 * the canvas fetches per-node `memory_associations` and layers co-access
 * edges on top of the wikilink graph.
 */

import { create } from "zustand";

export interface MemoryGraphState {
  /** Node clicked by the user — drives the 1-hop highlight layer. */
  selectedNodeId: string | null;
  /** Node under the cursor — drives the transient hover highlight. */
  hoveredNodeId: string | null;
  /**
   * Co-access edge layer toggle. OFF by default (required for AC): turning it
   * on triggers N `memory_associations` calls (one per node).
   */
  coAccessEnabled: boolean;
}

export interface MemoryGraphActions {
  setSelectedNodeId: (id: string | null) => void;
  setHoveredNodeId: (id: string | null) => void;
  setCoAccessEnabled: (enabled: boolean) => void;
  reset: () => void;
}

const INITIAL_STATE: MemoryGraphState = {
  selectedNodeId: null,
  hoveredNodeId: null,
  coAccessEnabled: false,
};

export const useMemoryGraphStore = create<MemoryGraphState & MemoryGraphActions>(
  (set) => ({
    ...INITIAL_STATE,

    setSelectedNodeId: (id) => set({ selectedNodeId: id }),
    setHoveredNodeId: (id) => set({ hoveredNodeId: id }),
    setCoAccessEnabled: (enabled) => set({ coAccessEnabled: enabled }),

    reset: () => set({ ...INITIAL_STATE }),
  }),
);
