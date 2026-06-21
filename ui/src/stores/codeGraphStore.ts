/**
 * codeGraphStore — UI-side highlight + filter state for `/code-graph`.
 *
 * Owns the layered highlight slice that the Sigma `nodeReducer` /
 * `edgeReducer` callbacks read on every render. Each highlight source
 * is a separate Set so they compose without rebuilding the graph:
 *
 *   selection           → user-clicked focal node (1-hop highlight)
 *   citationIds         → AI chat citations
 *   toolHighlightIds    → tool-call results (non-DOI query overlays)
 *   blastRadiusFrontier → animation-only set; separate from
 *                         `toolHighlightIds` so path/query fan-out can
 *                         ripple while the static highlight remains
 *                         visible underneath.
 *   doiImpactIds        → server-sampled impact ids merged into the
 *                         DOI focus/context set for dependents traversal.
 *   hoverId             → hover tooltip target (transient)
 *
 * Filters live alongside because they're peer concerns of the same
 * canvas — per-edge-kind toggle, per-node-kind toggle, and DOI focus
 * controls. SymbolReference (the call graph) defaults OFF because at
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
 * Top-level snapshot node-kind filter. `kind` is the wire-level
 * `SnapshotNodeKind` (file/folder/symbol); `symbol_kind` discriminators
 * (function/method/class/...) live in {@link SYMBOL_KIND_FILTERS}.
 */
export const NODE_KINDS = ["folder", "file", "symbol"] as const;
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

/**
 * Intent lenses — named filter presets that swap the entire
 * node/symbol/edge filter set in one action. Each lens maps a
 * concrete analytical intent to the filter combination that serves it.
 *
 * - **Architecture** (default) — structural skeleton: folders, files,
 *   symbols visible, but no symbol sub-kinds and only the semantic
 *   spine edges.
 * - **Calls** — call-graph view: only functions/methods/constructors,
 *   only Defines and SymbolReference edges.
 * - **Types** — type hierarchy: only type-defining symbols (class,
 *   struct, interface, trait, enum, impl, type), only type-related
 *   edges.
 * - **Data flow** — reads/writes: only functions and methods, only
 *   Reads/Writes/Defines edges.
 */
export type LensId = "architecture" | "calls" | "types" | "dataflow";

export const LENS_PRESETS: Record<
  LensId,
  {
    nodeKindFilters: Record<string, boolean>;
    symbolKindFilters: Record<string, boolean>;
    edgeKindFilters: Record<string, boolean>;
  }
> = {
  architecture: {
    nodeKindFilters: {
      folder: true,
      file: true,
      symbol: true,
    },
    symbolKindFilters: Object.fromEntries(
      SYMBOL_KIND_FILTERS.map((k) => [k, false]),
    ),
    edgeKindFilters: {
      ContainsDefinition: false,
      DeclaredInFile: false,
      FileReference: false,
      SymbolReference: false,
      Reads: false,
      Writes: true,
      Extends: true,
      Implements: true,
      TypeDefines: true,
      Defines: true,
      EntryPointOf: true,
      MemberOf: false,
      StepInProcess: true,
    },
  },
  calls: {
    nodeKindFilters: {
      folder: false,
      file: false,
      symbol: true,
    },
    symbolKindFilters: Object.fromEntries(
      SYMBOL_KIND_FILTERS.map((k) => [
        k,
        k === "function" || k === "method" || k === "constructor",
      ]),
    ),
    edgeKindFilters: Object.fromEntries(
      EDGE_KINDS.map((k) => [k, k === "Defines" || k === "SymbolReference"]),
    ),
  },
  types: {
    nodeKindFilters: {
      folder: false,
      file: false,
      symbol: true,
    },
    symbolKindFilters: Object.fromEntries(
      SYMBOL_KIND_FILTERS.map((k) => [
        k,
        [
          "class",
          "struct",
          "interface",
          "trait",
          "enum",
          "impl",
          "type",
        ].includes(k),
      ]),
    ),
    edgeKindFilters: Object.fromEntries(
      EDGE_KINDS.map((k) => [
        k,
        ["Extends", "Implements", "TypeDefines", "Defines"].includes(k),
      ]),
    ),
  },
  dataflow: {
    nodeKindFilters: {
      folder: false,
      file: false,
      symbol: true,
    },
    symbolKindFilters: Object.fromEntries(
      SYMBOL_KIND_FILTERS.map((k) => [
        k,
        k === "function" || k === "method",
      ]),
    ),
    edgeKindFilters: Object.fromEntries(
      EDGE_KINDS.map((k) => [k, ["Reads", "Writes", "Defines"].includes(k)]),
    ),
  },
};

export type FocusDirection = "dependencies" | "dependents" | "both";
export const DEFAULT_FOCUS_DIRECTION: FocusDirection = "both";
export const MIN_DOI_REVEAL_COUNT = 10;
export const MAX_DOI_REVEAL_COUNT = 100;
export const DEFAULT_DOI_REVEAL_COUNT = 40;

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

export interface CodeGraphHighlightState {
  selectionId: string | null;
  citationIds: Set<string>;
  toolHighlightIds: Set<string>;
  blastRadiusFrontier: Set<string>;
  /** Server-sampled impact ids folded into the unified DOI focus model. */
  doiImpactIds: Set<string>;
  /**
   * Internal canvas fallback state for collapsed community snapshots.
   * This is not a user-facing node-kind filter; it only lets the canvas
   * splice symbol members into legacy `kind: "community"` payloads.
   */
  expandedCommunityIds: Set<string>;
  hoverId: string | null;
  edgeKindFilters: Record<string, boolean>;
  nodeKindFilters: Record<string, boolean>;
  symbolKindFilters: Record<string, boolean>;
  /**
   * v10: when true, hide test files and the symbols defined in them
   * (canonical `is_test` classification). Default `false` (whole graph).
   */
  hideTests: boolean;
  /** Explicit DOI focus anchor. Kept separate from detail-panel selection. */
  focusAnchorId: string | null;
  /** Direction used by downstream dependency/dependent DOI traversal. */
  focusDirection: FocusDirection;
  /** Bounded top-N DOI context count to reveal around the focus anchor. */
  doiRevealCount: number;
  /** Iter 30: see {@link ColorMode}. Default `"topology"`. */
  colorMode: ColorMode;
  /**
   * Iter 30: when true, the current snapshot has at least one function
   * node with a populated `cognitive` value. Drives the toolbar's
   * heatmap-toggle disabled state — `false` means the walker hasn't
   * run for any of the project's languages yet, so the gradient would
   * be degenerate.
   */
  complexityAvailable: boolean;
  /**
   * `true` when the canvas has a ready snapshot with at least one node.
   * Drives the toolbar's disabled state when loading or empty.
   */
  graphReady: boolean;
  selectedWorkspaceSlug: string | null;
  /**
   * Active intent lens. `null` means the user has manually toggled
   * individual filters (Advanced mode). Applying a lens replaces all
   * three filter maps from the preset.
   */
  activeLens: LensId | null;
}

export interface CodeGraphHighlightActions {
  setSelection: (id: string | null) => void;
  setCitations: (ids: Iterable<string>) => void;
  clearCitations: () => void;
  setToolHighlight: (ids: Iterable<string>) => void;
  clearToolHighlight: () => void;
  setBlastRadiusFrontier: (ids: Iterable<string>) => void;
  clearBlastRadiusFrontier: () => void;
  /** Merge server impact samples into the DOI focus/context render path. */
  setDoiImpact: (ids: Iterable<string>) => void;
  clearDoiImpact: () => void;
  expandCommunity: (communityId: string) => void;
  collapseCommunity: (communityId: string) => void;
  clearExpandedCommunities: () => void;
  setHover: (id: string | null) => void;
  toggleEdgeKind: (kind: string) => void;
  setEdgeKindEnabled: (kind: string, enabled: boolean) => void;
  toggleNodeKind: (kind: string) => void;
  toggleSymbolKind: (kind: string) => void;
  /** v10: toggle hiding of test files/symbols. */
  setHideTests: (hide: boolean) => void;
  /** Set the explicit DOI focus anchor. Selection remains detail-panel state. */
  setFocusAnchor: (id: string | null) => void;
  /** Clear the DOI focus anchor without changing selection. */
  clearFocusAnchor: () => void;
  /** Direction used by downstream DOI traversal. */
  setFocusDirection: (direction: FocusDirection) => void;
  /** Bounded top-N DOI context count. */
  setDoiRevealCount: (count: number) => void;
  /** Iter 30: switch heatmap mode. */
  setColorMode: (mode: ColorMode) => void;
  /** Iter 30: canvas reports whether complexity data is present in the snapshot. */
  setComplexityAvailable: (available: boolean) => void;
  /** Canvas reports whether the graph is ready (loaded with at least one node). */
  setGraphReady: (ready: boolean) => void;
  setSelectedWorkspaceSlug: (slug: string | null) => void;
  /** Apply an intent lens, replacing all three filter maps from the preset. */
  applyLens: (lensId: LensId) => void;
  reset: () => void;
}

const INITIAL_STATE: CodeGraphHighlightState = {
  selectionId: null,
  citationIds: new Set(),
  toolHighlightIds: new Set(),
  blastRadiusFrontier: new Set(),
  doiImpactIds: new Set(),
  expandedCommunityIds: new Set(),
  hoverId: null,
  edgeKindFilters: { ...LENS_PRESETS.architecture.edgeKindFilters },
  nodeKindFilters: { ...LENS_PRESETS.architecture.nodeKindFilters },
  symbolKindFilters: { ...LENS_PRESETS.architecture.symbolKindFilters },
  hideTests: false,
  focusAnchorId: null,
  focusDirection: DEFAULT_FOCUS_DIRECTION,
  doiRevealCount: DEFAULT_DOI_REVEAL_COUNT,
  colorMode: DEFAULT_COLOR_MODE,
  complexityAvailable: false,
  graphReady: false,
  selectedWorkspaceSlug: null,
  activeLens: "architecture",
};

export const useCodeGraphStore = create<
  CodeGraphHighlightState & CodeGraphHighlightActions
>((set) => ({
  ...INITIAL_STATE,

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

  setDoiImpact: (ids) => {
    set({ doiImpactIds: new Set(ids) });
  },

  clearDoiImpact: () => {
    set({ doiImpactIds: new Set() });
  },

  expandCommunity: (communityId) => {
    set((state) => ({
      expandedCommunityIds: new Set(state.expandedCommunityIds).add(
        communityId,
      ),
    }));
  },

  collapseCommunity: (communityId) => {
    set((state) => {
      const next = new Set(state.expandedCommunityIds);
      next.delete(communityId);
      return { expandedCommunityIds: next };
    });
  },

  clearExpandedCommunities: () => {
    set({ expandedCommunityIds: new Set() });
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
      activeLens: null,
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
      activeLens: null,
    }));
  },

  toggleSymbolKind: (kind) => {
    set((state) => ({
      symbolKindFilters: {
        ...state.symbolKindFilters,
        [kind]: !(state.symbolKindFilters[kind] ?? true),
      },
      activeLens: null,
    }));
  },

  setHideTests: (hide) => {
    set({ hideTests: hide });
  },

  setFocusAnchor: (id) => {
    set({ focusAnchorId: id });
  },

  clearFocusAnchor: () => {
    set({ focusAnchorId: null });
  },

  setFocusDirection: (direction) => {
    set({ focusDirection: direction });
  },

  setDoiRevealCount: (count) => {
    const rounded = Number.isFinite(count)
      ? Math.round(count)
      : DEFAULT_DOI_REVEAL_COUNT;
    const clamped = Math.max(
      MIN_DOI_REVEAL_COUNT,
      Math.min(MAX_DOI_REVEAL_COUNT, rounded),
    );
    set({ doiRevealCount: clamped });
  },

  setColorMode: (mode) => {
    set({ colorMode: mode });
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

  setGraphReady: (ready) => {
    set({ graphReady: ready });
  },

  setSelectedWorkspaceSlug: (slug) => {
    set({ selectedWorkspaceSlug: slug });
  },

  applyLens: (lensId) => {
    const preset = LENS_PRESETS[lensId];
    set({
      nodeKindFilters: { ...preset.nodeKindFilters },
      symbolKindFilters: { ...preset.symbolKindFilters },
      edgeKindFilters: { ...preset.edgeKindFilters },
      activeLens: lensId,
    });
  },

  reset: () => {
    set((state) => ({
      ...INITIAL_STATE,
      selectedWorkspaceSlug: state.selectedWorkspaceSlug,
      citationIds: new Set(),
      toolHighlightIds: new Set(),
      blastRadiusFrontier: new Set(),
      doiImpactIds: new Set(),
      expandedCommunityIds: new Set(),
      edgeKindFilters: { ...LENS_PRESETS.architecture.edgeKindFilters },
      nodeKindFilters: { ...LENS_PRESETS.architecture.nodeKindFilters },
      symbolKindFilters: { ...LENS_PRESETS.architecture.symbolKindFilters },
      activeLens: "architecture",
      colorMode: DEFAULT_COLOR_MODE,
      complexityAvailable: false,
      graphReady: false,
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
export const selectDoiImpactIds = (
  s: CodeGraphHighlightState & CodeGraphHighlightActions,
) => s.doiImpactIds;
export const selectEdgeKindFilters = (
  s: CodeGraphHighlightState & CodeGraphHighlightActions,
) => s.edgeKindFilters;
export const selectNodeKindFilters = (
  s: CodeGraphHighlightState & CodeGraphHighlightActions,
) => s.nodeKindFilters;
export const selectSymbolKindFilters = (
  s: CodeGraphHighlightState & CodeGraphHighlightActions,
) => s.symbolKindFilters;
export const selectFocusAnchorId = (
  s: CodeGraphHighlightState & CodeGraphHighlightActions,
) => s.focusAnchorId;
export const selectActiveLens = (
  s: CodeGraphHighlightState & CodeGraphHighlightActions,
) => s.activeLens;
export const selectFocusDirection = (
  s: CodeGraphHighlightState & CodeGraphHighlightActions,
) => s.focusDirection;
export const selectDoiRevealCount = (
  s: CodeGraphHighlightState & CodeGraphHighlightActions,
) => s.doiRevealCount;
