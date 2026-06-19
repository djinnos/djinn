/**
 * codeGraphStore — UI-side highlight + filter state for `/code-graph`.
 *
 * Owns the layered highlight slice that the Sigma `nodeReducer` /
 * `edgeReducer` callbacks read on every render. Each highlight source
 * is a separate Set so they compose without rebuilding the graph:
 *
 *   selection           → user-clicked focal node (1-hop highlight)
 *   citationIds         → AI chat citations
 *   toolHighlightIds    → tool-call results (e.g. blast-radius BFS)
 *   blastRadiusFrontier → animation-only set; separate from
 *                         `toolHighlightIds` so impact "fan-out" can
 *                         ripple while the static highlight remains
 *                         visible underneath.
 *   hoverId             → hover tooltip target (transient)
 *
 * Filters live alongside because they're peer concerns of the same
 * canvas — per-edge-kind toggle, per-node-kind toggle, and depth from
 * selection. SymbolReference (the call graph) defaults OFF because at
 * 60-80% of edges it dominates everything else.
 */

import { create } from "zustand";

/**
 * `RepoGraphEdgeKind` Debug-style names emitted by `code_graph snapshot`.
 * Adding a new kind on the server requires updating this list, but the
 * reducer treats unknown kinds as visible so a missing entry never
 * silently hides edges.
 */
export const EDGE_KINDS = [
  "ContainsDefinition",
  "DeclaredInFile",
  "FileReference",
  "SymbolReference",
  "Reads",
  "Writes",
  "Extends",
  "Implements",
  "TypeDefines",
  "Defines",
  "EntryPointOf",
  "MemberOf",
  "StepInProcess",
] as const;

export type EdgeKind = (typeof EDGE_KINDS)[number];

/**
 * Edges that ship OFF. The default view shows only the *semantic spine*
 * (Implements, Extends, Defines, TypeDefines, EntryPointOf, Writes,
 * StepInProcess) — everything else is firehose noise on a 12k-node repo
 * (FileReference alone is ~33% of edges, Contains/Declared are
 * inverse duplicates of each other at ~23% each, Reads is per-query
 * useful but globally meaningless). User toggles on demand.
 */
const NOISY_EDGE_KINDS: ReadonlySet<string> = new Set([
  "ContainsDefinition",
  "DeclaredInFile",
  "FileReference",
  "SymbolReference",
  "Reads",
  "MemberOf",
]);

/**
 * Top-level snapshot node-kind filter. `kind` is the wire-level
 * `SnapshotNodeKind` (file/folder/symbol); `symbol_kind` discriminators
 * (function/method/class/...) live in {@link SYMBOL_KIND_FILTERS}.
 */
export const NODE_KINDS = ["folder", "file", "symbol", "community"] as const;
export type NodeKind = (typeof NODE_KINDS)[number];

/**
 * Symbol-level filter. `other` catches every symbol the SCIP indexer
 * left without a recognized kind — in practice that's the
 * `symbol:local N` bucket (function-locals like `ctx`, `args`, `logger`)
 * which dominate in volume but carry no architectural signal.
 */
export const SYMBOL_KIND_FILTERS = [
  "class",
  "struct",
  "interface",
  "trait",
  "enum",
  "function",
  "method",
  "constructor",
  "impl",
  "type",
  "field",
  "variable",
  "const",
  "static",
  "property",
  "import",
  "other",
] as const;
export type SymbolKindFilter = (typeof SYMBOL_KIND_FILTERS)[number];

/** Symbol kinds hidden by default — clutter without analytical value. */
const NOISY_SYMBOL_KINDS: ReadonlySet<string> = new Set([
  "variable",
  "const",
  "static",
  "property",
  "import",
  "field",
  "other",
]);

export const MIN_DEPTH = 1;
export const MAX_DEPTH = 5;
export const DEFAULT_DEPTH = MAX_DEPTH;

/**
 * Iter 30: color-mode discriminator for the canvas.
 *   - `"topology"` (default) — preserves the dir-hash / community
 *     coloring shipped in earlier iterations. Heatmap is opt-in.
 *   - `"complexity"` — swaps the color channel for a green→red gradient
 *     keyed off cognitive complexity percentile. Sizing stays from
 *     PageRank so a "big and red" node is the strongest refactor
 *     candidate. Disabled when no function nodes carry a `cognitive`
 *     value (walker hasn't run / no supported languages).
 */
export type ColorMode = "topology" | "complexity";
export const DEFAULT_COLOR_MODE: ColorMode = "topology";

export type LayoutMode = "force" | "sequential" | "radial";
export const DEFAULT_LAYOUT_MODE: LayoutMode = "force";

const LAYOUT_MODE_STORAGE_KEY = "codegraph.layoutMode";
const LAYOUT_MODES: ReadonlySet<string> = new Set([
  "force",
  "sequential",
  "radial",
]);

function isLayoutMode(value: string | null): value is LayoutMode {
  return value !== null && LAYOUT_MODES.has(value);
}

function readPersistedLayoutMode(): LayoutMode {
  if (typeof window === "undefined") return DEFAULT_LAYOUT_MODE;

  try {
    const persisted = window.sessionStorage.getItem(LAYOUT_MODE_STORAGE_KEY);
    return isLayoutMode(persisted) ? persisted : DEFAULT_LAYOUT_MODE;
  } catch {
    return DEFAULT_LAYOUT_MODE;
  }
}

function persistLayoutMode(mode: LayoutMode): void {
  if (typeof window === "undefined") return;

  try {
    window.sessionStorage.setItem(LAYOUT_MODE_STORAGE_KEY, mode);
  } catch {
    // Best-effort UI preference persistence; state updates should still work.
  }
}

/**
 * Semantic zoom mode for community-collapse behaviour.
 *   - `"auto"` (default) — the canvas decides `symbol` vs `community`
 *     based on the snapshot node-count threshold.
 *   - `"symbol"` — force a symbol-level snapshot.
 *   - `"community"` — force a community-level (collapsed) snapshot.
 *
 * This task only owns the state + toolbar control; the canvas fetch
 * wiring lives in a sibling task.
 */
export type SemanticZoomMode = "auto" | "symbol" | "community";
export const DEFAULT_SEMANTIC_ZOOM_MODE: SemanticZoomMode = "auto";

export interface CodeGraphHighlightState {
  selectionId: string | null;
  citationIds: Set<string>;
  toolHighlightIds: Set<string>;
  blastRadiusFrontier: Set<string>;
  hoverId: string | null;
  edgeKindFilters: Record<string, boolean>;
  nodeKindFilters: Record<string, boolean>;
  symbolKindFilters: Record<string, boolean>;
  /**
   * v10: when true, hide test files and the symbols defined in them
   * (canonical `is_test` classification). Default `false` (whole graph).
   */
  hideTests: boolean;
  depthFilter: number;
  /** Iter 30: see {@link ColorMode}. Default `"topology"`. */
  colorMode: ColorMode;
  /** Layout algorithm preference for the code graph canvas. */
  layoutMode: LayoutMode;
  /**
   * Iter 30: `true` when the current snapshot has at least one function
   * node with a populated `cognitive` value. Drives the toolbar's
   * heatmap-toggle disabled state — `false` means the walker hasn't
   * run for any of the project's languages yet, so the gradient would
   * be degenerate.
   */
  complexityAvailable: boolean;
  /**
   * Semantic zoom mode — `auto` lets the canvas choose symbol vs
   * community based on the snapshot node threshold; `symbol`/`community`
   * force the level. Default `"auto"`. See {@link SemanticZoomMode}.
   */
  semanticZoomMode: SemanticZoomMode;
  /**
   * Stable community ids that the user has expanded via double-click.
   * Keyed by `community_id` (the server's sha256-of-members hash) so
   * expansion state survives snapshot re-renders for the same project
   * and is safely reset when the project changes (via `reset`).
   */
  expandedCommunityIds: Set<string>;
  selectedWorkspaceSlug: string | null;
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
  toggleNodeKind: (kind: string) => void;
  toggleSymbolKind: (kind: string) => void;
  /** v10: toggle hiding of test files/symbols. */
  setHideTests: (hide: boolean) => void;
  setDepthFilter: (depth: number) => void;
  /** Iter 30: switch heatmap mode. */
  setColorMode: (mode: ColorMode) => void;
  /** Switch the graph layout algorithm and persist the UI preference. */
  setLayoutMode: (mode: LayoutMode) => void;
  /** Iter 30: canvas reports whether complexity data is present in the snapshot. */
  setComplexityAvailable: (available: boolean) => void;
  /** Set the semantic zoom mode override (auto/symbol/community). */
  setSemanticZoomMode: (mode: SemanticZoomMode) => void;
  /**
   * Mark a community as expanded (double-click). Idempotent: the stable
   * `community_id` is stored so expansion survives snapshot re-renders.
   */
  expandCommunity: (communityId: string) => void;
  /**
   * Mark a community as collapsed (double-click on an expanded community).
   * Idempotent.
   */
  collapseCommunity: (communityId: string) => void;
  /** Clear all expanded communities (called on project change). */
  clearExpandedCommunities: () => void;
  setSelectedWorkspaceSlug: (slug: string | null) => void;
  reset: () => void;
}

function defaultEdgeKindFilters(): Record<string, boolean> {
  return Object.fromEntries(
    EDGE_KINDS.map((k) => [k, !NOISY_EDGE_KINDS.has(k)]),
  );
}

function defaultNodeKindFilters(): Record<string, boolean> {
  return Object.fromEntries(NODE_KINDS.map((k) => [k, true]));
}

function defaultSymbolKindFilters(): Record<string, boolean> {
  return Object.fromEntries(
    SYMBOL_KIND_FILTERS.map((k) => [k, !NOISY_SYMBOL_KINDS.has(k)]),
  );
}

const INITIAL_STATE: CodeGraphHighlightState = {
  selectionId: null,
  citationIds: new Set(),
  toolHighlightIds: new Set(),
  blastRadiusFrontier: new Set(),
  hoverId: null,
  edgeKindFilters: defaultEdgeKindFilters(),
  nodeKindFilters: defaultNodeKindFilters(),
  symbolKindFilters: defaultSymbolKindFilters(),
  hideTests: false,
  depthFilter: DEFAULT_DEPTH,
  colorMode: DEFAULT_COLOR_MODE,
  layoutMode: DEFAULT_LAYOUT_MODE,
  complexityAvailable: false,
  semanticZoomMode: DEFAULT_SEMANTIC_ZOOM_MODE,
  expandedCommunityIds: new Set<string>(),
  selectedWorkspaceSlug: null,
};

export const useCodeGraphStore = create<
  CodeGraphHighlightState & CodeGraphHighlightActions
>((set) => ({
  ...INITIAL_STATE,
  layoutMode: readPersistedLayoutMode(),

  setSelection: (id) => {
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

  toggleNodeKind: (kind) => {
    set((state) => ({
      nodeKindFilters: {
        ...state.nodeKindFilters,
        [kind]: !(state.nodeKindFilters[kind] ?? true),
      },
    }));
  },

  toggleSymbolKind: (kind) => {
    set((state) => ({
      symbolKindFilters: {
        ...state.symbolKindFilters,
        [kind]: !(state.symbolKindFilters[kind] ?? true),
      },
    }));
  },

  setHideTests: (hide) => {
    set({ hideTests: hide });
  },

  setDepthFilter: (depth) => {
    const clamped = Math.max(MIN_DEPTH, Math.min(MAX_DEPTH, Math.round(depth)));
    set({ depthFilter: clamped });
  },

  setColorMode: (mode) => {
    set({ colorMode: mode });
  },

  setLayoutMode: (mode) => {
    persistLayoutMode(mode);
    set({ layoutMode: mode });
  },

  setComplexityAvailable: (available) => {
    set((state) => {
      // Auto-snap the mode back to topology if complexity becomes
      // unavailable while the heatmap is engaged — guards against the
      // canvas being stuck on a degenerate gradient when the snapshot
      // refetches after a project switch.
      if (!available && state.colorMode === "complexity") {
        return { complexityAvailable: false, colorMode: "topology" };
      }
      return { complexityAvailable: available };
    });
  },

  setSelectedWorkspaceSlug: (slug) => {
    set({ selectedWorkspaceSlug: slug });
  },

  setSemanticZoomMode: (mode) => {
    set({ semanticZoomMode: mode });
  },

  expandCommunity: (communityId) => {
    if (!communityId) return;
    set((state) => {
      if (state.expandedCommunityIds.has(communityId)) return state;
      const next = new Set(state.expandedCommunityIds);
      next.add(communityId);
      return { expandedCommunityIds: next };
    });
  },

  collapseCommunity: (communityId) => {
    if (!communityId) return;
    set((state) => {
      if (!state.expandedCommunityIds.has(communityId)) return state;
      const next = new Set(state.expandedCommunityIds);
      next.delete(communityId);
      return { expandedCommunityIds: next };
    });
  },

  clearExpandedCommunities: () => {
    set((state) => {
      if (state.expandedCommunityIds.size === 0) return state;
      return { expandedCommunityIds: new Set() };
    });
  },

  reset: () => {
    set((state) => ({
      ...INITIAL_STATE,
      selectedWorkspaceSlug: state.selectedWorkspaceSlug,
      citationIds: new Set(),
      toolHighlightIds: new Set(),
      blastRadiusFrontier: new Set(),
      edgeKindFilters: defaultEdgeKindFilters(),
      nodeKindFilters: defaultNodeKindFilters(),
      symbolKindFilters: defaultSymbolKindFilters(),
      colorMode: DEFAULT_COLOR_MODE,
      layoutMode: state.layoutMode,
      complexityAvailable: false,
      semanticZoomMode: DEFAULT_SEMANTIC_ZOOM_MODE,
      expandedCommunityIds: new Set(),
    }));
  },
}));

// ── Convenience selectors ────────────────────────────────────────────────────

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
export const selectNodeKindFilters = (
  s: CodeGraphHighlightState & CodeGraphHighlightActions,
) => s.nodeKindFilters;
export const selectSymbolKindFilters = (
  s: CodeGraphHighlightState & CodeGraphHighlightActions,
) => s.symbolKindFilters;
export const selectDepthFilter = (
  s: CodeGraphHighlightState & CodeGraphHighlightActions,
) => s.depthFilter;
export const selectLayoutMode = (
  s: CodeGraphHighlightState & CodeGraphHighlightActions,
) => s.layoutMode;
