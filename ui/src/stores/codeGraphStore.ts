/**
 * codeGraphStore — UI-side highlight + filter state for `/code-graph`.
 *
 * Owns the layered highlight slice that the Sigma `nodeReducer` /
 * `edgeReducer` callbacks read on every render. Each highlight source
 * is a separate Set so they compose without rebuilding the graph:
 *
 *   selection           → user-clicked focal node (1-hop highlight)
 *   citationIds         → AI chat citations (PR D5 will populate)
 *   toolHighlightIds    → tool-call results (e.g. blast-radius BFS)
 *   blastRadiusFrontier → animation-only set (CSS pulse) — separate
 *                         from `toolHighlightIds` so the impact "fan-out"
 *                         can ripple while the static highlight remains
 *                         visible underneath.
 *   hoverId             → hover tooltip target (transient)
 *
 * Filters live alongside because they're peer concerns of the same
 * canvas:
 *
 *   edgeKindFilters     → per-RepoGraphEdgeKind toggle (default: all on)
 *   depthFilter         → 1..5 hop depth from selection (default 5 = all)
 *
 * D5 will write to `citationIds` from the chat parser without ever
 * touching the canvas component — the reducer pattern is what makes
 * cross-page wiring trivial. Same for the future Cmd-K palette.
 */

import { create } from "zustand";

/**
 * Set of `RepoGraphEdgeKind` Debug-style names emitted by
 * `code_graph snapshot`. Mirrors the snake_case-via-Debug variant
 * name convention used on the wire (see `bridge::SnapshotEdge.kind`
 * in the Rust crate). Adding a new edge kind on the server requires
 * updating this list, but the reducer treats unknown kinds as
 * "always visible" so a missing entry never *hides* edges.
 */
export const EDGE_KINDS = [
  "ContainsDefinition",
  "DeclaredInFile",
  "FileReference",
  "SymbolReference",
  "Reads",
  "Writes",
  "SymbolRelationshipReference",
  "SymbolRelationshipImplementation",
  "SymbolRelationshipTypeDefinition",
  "SymbolRelationshipDefinition",
] as const;

export type EdgeKind = (typeof EDGE_KINDS)[number];

/** Minimum / maximum hop depth surfaced by the depth-filter slider. */
export const MIN_DEPTH = 1;
export const MAX_DEPTH = 5;
/** Sentinel that means "no depth filtering" — equal to {@link MAX_DEPTH}. */
export const DEFAULT_DEPTH = MAX_DEPTH;

export interface CodeGraphHighlightState {
  /** Currently focused node id (RepoNodeKey). */
  selectionId: string | null;
  /** AI chat citation set — populated by PR D5's parser. */
  citationIds: Set<string>;
  /** Tool-call result highlight (e.g. blast-radius BFS frontier). */
  toolHighlightIds: Set<string>;
  /** Animation-only set: nodes pulsing as part of an impact ripple. */
  blastRadiusFrontier: Set<string>;
  /** Hovered node id; cleared on `mouseleave`. */
  hoverId: string | null;
  /** Per-edge-kind visibility. Default: every kind on. */
  edgeKindFilters: Record<string, boolean>;
  /** Hop depth from selection to render; {@link DEFAULT_DEPTH} = no filtering. */
  depthFilter: number;
}

export interface CodeGraphHighlightActions {
  setSelection: (id: string | null) => void;
  setCitations: (ids: Iterable<string>) => void;
  clearCitations: () => void;
  setToolHighlight: (ids: Iterable<string>) => void;
  clearToolHighlight: () => void;
  setBlastRadiusFrontier: (ids: Iterable<string>) => void;
  clearBlastRadiusFrontier: () => void;
  setHover: (id: string | null) => void;
  toggleEdgeKind: (kind: string) => void;
  setEdgeKindEnabled: (kind: string, enabled: boolean) => void;
  setDepthFilter: (depth: number) => void;
  /** Drop every highlight + filter back to defaults — used on project switch. */
  reset: () => void;
}

function defaultEdgeKindFilters(): Record<string, boolean> {
  return Object.fromEntries(EDGE_KINDS.map((k) => [k, true]));
}

const INITIAL_STATE: CodeGraphHighlightState = {
  selectionId: null,
  citationIds: new Set(),
  toolHighlightIds: new Set(),
  blastRadiusFrontier: new Set(),
  hoverId: null,
  edgeKindFilters: defaultEdgeKindFilters(),
  depthFilter: DEFAULT_DEPTH,
};

export const useCodeGraphStore = create<
  CodeGraphHighlightState & CodeGraphHighlightActions
>((set) => ({
  ...INITIAL_STATE,

  setSelection: (id) => {
    // Selecting a new node implicitly resets depth filtering's effective
    // root, but we keep the depth value itself sticky so a user who
    // dialed it down doesn't lose their setting on every click.
    set({ selectionId: id });
  },

  setCitations: (ids) => {
    set({ citationIds: new Set(ids) });
  },

  clearCitations: () => {
    set({ citationIds: new Set() });
  },

  setToolHighlight: (ids) => {
    set({ toolHighlightIds: new Set(ids) });
  },

  clearToolHighlight: () => {
    set({ toolHighlightIds: new Set() });
  },

  setBlastRadiusFrontier: (ids) => {
    set({ blastRadiusFrontier: new Set(ids) });
  },

  clearBlastRadiusFrontier: () => {
    set({ blastRadiusFrontier: new Set() });
  },

  setHover: (id) => {
    set({ hoverId: id });
  },

  toggleEdgeKind: (kind) => {
    set((state) => ({
      edgeKindFilters: {
        ...state.edgeKindFilters,
        [kind]: !(state.edgeKindFilters[kind] ?? true),
      },
    }));
  },

  setEdgeKindEnabled: (kind, enabled) => {
    set((state) => ({
      edgeKindFilters: { ...state.edgeKindFilters, [kind]: enabled },
    }));
  },

  setDepthFilter: (depth) => {
    const clamped = Math.max(MIN_DEPTH, Math.min(MAX_DEPTH, Math.round(depth)));
    set({ depthFilter: clamped });
  },

  reset: () => {
    set({
      ...INITIAL_STATE,
      // Re-derive the filters so callers that mutated the map don't
      // share its reference with the next instance.
      citationIds: new Set(),
      toolHighlightIds: new Set(),
      blastRadiusFrontier: new Set(),
      edgeKindFilters: defaultEdgeKindFilters(),
    });
  },
}));

// ── Convenience selectors (stable identity, easy to memoize) ─────────────────

export const selectSelectionId = (
  s: CodeGraphHighlightState & CodeGraphHighlightActions,
) => s.selectionId;
export const selectHoverId = (
  s: CodeGraphHighlightState & CodeGraphHighlightActions,
) => s.hoverId;
export const selectCitationIds = (
  s: CodeGraphHighlightState & CodeGraphHighlightActions,
) => s.citationIds;
export const selectToolHighlightIds = (
  s: CodeGraphHighlightState & CodeGraphHighlightActions,
) => s.toolHighlightIds;
export const selectBlastRadiusFrontier = (
  s: CodeGraphHighlightState & CodeGraphHighlightActions,
) => s.blastRadiusFrontier;
export const selectEdgeKindFilters = (
  s: CodeGraphHighlightState & CodeGraphHighlightActions,
) => s.edgeKindFilters;
export const selectDepthFilter = (
  s: CodeGraphHighlightState & CodeGraphHighlightActions,
) => s.depthFilter;
