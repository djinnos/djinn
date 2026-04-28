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

/** Edges that ship OFF — too dense to render at full repo scale. */
const NOISY_EDGE_KINDS: ReadonlySet<string> = new Set([
  "SymbolReference",
  "MemberOf",
]);

/**
 * Top-level snapshot node-kind filter. `kind` is the wire-level
 * `SnapshotNodeKind` (file/folder/symbol); `symbol_kind` discriminators
 * (function/method/class/...) live in {@link SYMBOL_KIND_FILTERS}.
 */
export const NODE_KINDS = ["folder", "file", "symbol"] as const;
export type NodeKind = (typeof NODE_KINDS)[number];

/** Symbol-level filter. Mirrors GitNexus's DEFAULT_VISIBLE_LABELS. */
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
  "variable",
  "const",
  "static",
  "property",
  "import",
] as const;
export type SymbolKindFilter = (typeof SYMBOL_KIND_FILTERS)[number];

/** Symbol kinds hidden by default — clutter without analytical value. */
const NOISY_SYMBOL_KINDS: ReadonlySet<string> = new Set([
  "variable",
  "const",
  "static",
  "property",
  "import",
]);

export const MIN_DEPTH = 1;
export const MAX_DEPTH = 5;
export const DEFAULT_DEPTH = MAX_DEPTH;

export interface CodeGraphHighlightState {
  selectionId: string | null;
  citationIds: Set<string>;
  toolHighlightIds: Set<string>;
  blastRadiusFrontier: Set<string>;
  hoverId: string | null;
  edgeKindFilters: Record<string, boolean>;
  nodeKindFilters: Record<string, boolean>;
  symbolKindFilters: Record<string, boolean>;
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
  toggleNodeKind: (kind: string) => void;
  toggleSymbolKind: (kind: string) => void;
  setDepthFilter: (depth: number) => void;
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
  depthFilter: DEFAULT_DEPTH,
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

  setDepthFilter: (depth) => {
    const clamped = Math.max(MIN_DEPTH, Math.min(MAX_DEPTH, Math.round(depth)));
    set({ depthFilter: clamped });
  },

  reset: () => {
    set({
      ...INITIAL_STATE,
      citationIds: new Set(),
      toolHighlightIds: new Set(),
      blastRadiusFrontier: new Set(),
      edgeKindFilters: defaultEdgeKindFilters(),
      nodeKindFilters: defaultNodeKindFilters(),
      symbolKindFilters: defaultSymbolKindFilters(),
    });
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
