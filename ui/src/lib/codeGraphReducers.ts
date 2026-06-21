/**
 * codeGraphReducers — pure functions that turn the highlight store
 * snapshot into Sigma `nodeReducer` / `edgeReducer` outputs.
 *
 * Sigma calls these on every render frame, so the rule is: NO side
 * effects, NO BFS traversal, NO new objects unless the visual state
 * actually changes. Heavy lifting (1-hop neighbor set, depth-N BFS)
 * lives upstream in `useGraphReducers` where it's memoized via
 * `useMemo`/`useEffect` and only recomputed when selection or graph
 * topology changes.
 *
 * Layer priority (high → low):
 *
 *   1. Selection (focal node + 1-hop neighbors highlighted, rest dim)
 *   2. AI citations          (citationIds — D5 will populate)
 *   3. Tool-call results     (toolHighlightIds — e.g. impact BFS)
 *   4. Blast-radius animation (CSS pulse via per-frame color modulation)
 *   5. Hover tooltip
 *
 * "When nothing is highlighted, render normally (don't dim everything)"
 * — that rule lives in `pickHighlightMode`: if every layer is empty,
 * we return `"none"` and the reducers pass node attributes through
 * untouched.
 */

import { shouldLabelAtZoom } from "./codeGraphLabels";

/**
 * Mirror of graphology's `Attributes` shape — graphology-types isn't
 * a direct dependency in this repo, so we widen-locally rather than
 * pull in another package just for an alias.
 */
export type Attributes = Record<string, unknown>;

/**
 * Pre-computed view of the highlight store the reducers consume on
 * every Sigma frame. Caller derives `selectionNeighbors` upstream so the
 * reducer itself is O(1) per node.
 */
export interface HighlightView {
  selectionId: string | null;
  /** 1-hop neighbors of `selectionId` (inclusive). Empty when no selection. */
  selectionNeighbors: ReadonlySet<string>;
  citationIds: ReadonlySet<string>;
  toolHighlightIds: ReadonlySet<string>;
  blastRadiusFrontier: ReadonlySet<string>;
  hoverId: string | null;
  edgeKindFilters: Readonly<Record<string, boolean>>;
  /** Top-level node-kind filter (file/folder/symbol). */
  nodeKindFilters: Readonly<Record<string, boolean>>;
  /** Per-symbol-kind filter (function/method/class/...). */
  symbolKindFilters: Readonly<Record<string, boolean>>;
  /** v10: when true, hide every node marked `isTest` (test files/symbols). */
  hideTests: boolean;
  /**
   * Legacy hide-gate slot kept inert while downstream DOI ranking replaces
   * hop-count depth filtering. `null` means "no store-level hide gate".
   */
  depthReachable: ReadonlySet<string> | null;
  /** Pulse phase ∈ [0, 1] driving the blast-radius color cycle. */
  pulsePhase: number;
  /**
   * Iter 30: active color mode. `"topology"` keeps the existing
   * dir-hash / community coloring; `"complexity"` swaps it for a
   * green→red heatmap keyed off the per-function cognitive percentile.
   */
  colorMode: ColorMode;
  /**
   * Iter 30: percentile breakpoints driving the heatmap. `null` when
   * either no function nodes carry a `cognitive` value, or
   * `colorMode === "topology"` and the heatmap isn't engaged. The
   * reducer treats `null` as "skip the heatmap layer entirely."
   */
  complexityThresholds: ComplexityThresholds | null;
  /**
   * Iter 30: top-N most-complex node ids that wear a persistent red
   * halo regardless of color mode. The reasoning is that even in
   * topology mode the user wants refactor candidates marked. Empty
   * set when complexity data is unavailable.
   */
  complexityHaloIds: ReadonlySet<string>;
  /**
   * Iter y3mf: per-snapshot PageRank percentile map (id → [0, 1]).
   * Empty when no nodes carry a `pagerank` attribute (older caches,
   * fixtures). Drives the zoom-adaptive label-density gate.
   */
  pagerankPercentile: ReadonlyMap<string, number>;
  /**
   * Iter y3mf: current Sigma camera ratio (1.0 ≈ fit-to-frame).
   * `Infinity` is the sentinel for "Sigma not yet bound" → first paint
   * shows every label. Plumbed in by `useGraphReducers` from
   * `SigmaInstanceHandle.getCameraRatio()`.
   */
  cameraRatio: number;
}

/** Bitset-style flag describing which highlight layer wins for a node. */
export type HighlightMode =
  | "none"
  | "focus" // the selected node itself
  | "neighbor" // 1-hop neighbor of selection
  | "citation"
  | "tool"
  | "blast"
  | "hover"
  | "dim";

/** Empty view — useful for the "render passthrough" path before mount. */
export const EMPTY_HIGHLIGHT_VIEW: HighlightView = {
  selectionId: null,
  selectionNeighbors: new Set<string>(),
  citationIds: new Set<string>(),
  toolHighlightIds: new Set<string>(),
  blastRadiusFrontier: new Set<string>(),
  hoverId: null,
  edgeKindFilters: {},
  nodeKindFilters: {},
  symbolKindFilters: {},
  hideTests: false,
  depthReachable: null,
  pulsePhase: 0,
  colorMode: "topology",
  complexityThresholds: null,
  complexityHaloIds: new Set<string>(),
  pagerankPercentile: new Map<string, number>(),
  cameraRatio: Infinity,
};

/**
 * `true` when no highlight layer is active — the canvas should render
 * normally instead of dimming everything to gray.
 */
export function isViewEmpty(view: HighlightView): boolean {
  return (
    view.selectionId === null &&
    view.citationIds.size === 0 &&
    view.toolHighlightIds.size === 0 &&
    view.blastRadiusFrontier.size === 0 &&
    view.hoverId === null
  );
}

/** Pick the dominant highlight layer for `nodeId` given the current view. */
export function pickHighlightMode(
  nodeId: string,
  view: HighlightView,
): HighlightMode {
  if (isViewEmpty(view)) return "none";

  // Layers in priority order — first hit wins. Hover *displays* a
  // tooltip but doesn't override stronger semantic layers like
  // "selected".
  if (view.selectionId === nodeId) return "focus";
  if (view.blastRadiusFrontier.has(nodeId)) return "blast";
  if (view.toolHighlightIds.has(nodeId)) return "tool";
  if (view.citationIds.has(nodeId)) return "citation";
  if (view.selectionId !== null && view.selectionNeighbors.has(nodeId))
    return "neighbor";
  if (view.hoverId === nodeId) return "hover";
  return "dim";
}

// ── Color palette ───────────────────────────────────────────────────────────

const COLOR_FOCUS = "#f97316"; // orange-500: the focal click target
const COLOR_NEIGHBOR = "#fde68a"; // amber-200: 1-hop neighborhood
const COLOR_CITATION = "#38bdf8"; // sky-400: AI citations
const COLOR_TOOL = "#a78bfa"; // violet-400: tool-call result
const COLOR_BLAST_LO = "#fbbf24"; // amber-400: blast pulse low
const COLOR_BLAST_HI = "#ef4444"; // red-500: blast pulse high
const COLOR_HOVER = "#facc15"; // yellow-400: hover preview
const COLOR_DIMMED = "rgba(100, 116, 139, 0.18)"; // slate-500 @ 18%

/**
 * Linear-interpolate between two `#rrggbb` hex colors. Used for the
 * blast-radius pulse so the animation cycles smoothly without us
 * needing a separate CSS keyframe. `t` clamped to [0, 1].
 */
function lerpHex(from: string, to: string, t: number): string {
  const clamped = Math.max(0, Math.min(1, t));
  const a = parseHex(from);
  const b = parseHex(to);
  const r = Math.round(a[0] + (b[0] - a[0]) * clamped);
  const g = Math.round(a[1] + (b[1] - a[1]) * clamped);
  const bl = Math.round(a[2] + (b[2] - a[2]) * clamped);
  return `#${[r, g, bl].map((v) => v.toString(16).padStart(2, "0")).join("")}`;
}

function parseHex(hex: string): [number, number, number] {
  const m = /^#?([0-9a-f]{6})$/i.exec(hex);
  if (!m) return [255, 255, 255];
  const n = parseInt(m[1], 16);
  return [(n >> 16) & 0xff, (n >> 8) & 0xff, n & 0xff];
}

/**
 * Per-node visual override the Sigma `nodeReducer` returns. Keeping
 * this typed (rather than `Record<string, unknown>`) makes it
 * obvious which Sigma-known fields we touch on the highlight path.
 */
export interface NodeReducerOverride extends Attributes {
  color?: string;
  size?: number;
  label?: string;
  zIndex?: number;
  /** Sigma 3 hides nodes with `hidden: true`. */
  hidden?: boolean;
  /** Custom flag the canvas uses to wire hover-tooltip logic. */
  highlighted?: boolean;
}

/** Defensive numeric read — `Attributes` is `Record<string, unknown>`. */
function attrSize(attrs: Attributes, fallback: number): number {
  const v = attrs.size;
  return typeof v === "number" && Number.isFinite(v) ? v : fallback;
}

/**
 * Build the per-node override Sigma should merge with the base
 * attributes for `nodeId`. Returns the original attribute object
 * untouched when no highlight applies — Sigma compares object
 * identity so passing through saves unnecessary repaints.
 */
export function nodeReducer(
  nodeId: string,
  attrs: Attributes,
  view: HighlightView,
): Attributes {
  // v10: "hide tests" toggle. Drops every node the graph builder marked
  // `isTest` (test files + symbols defined in them). Sits first so the
  // gate fires regardless of any other highlight/filter state.
  if (view.hideTests && attrs.isTest === true) {
    return { ...attrs, hidden: true };
  }

  // Node-kind filter (file/folder/symbol). Treat missing entries as
  // visible so an under-populated filter map never silently hides a
  // whole class of nodes.
  if (typeof attrs.kind === "string") {
    const enabled = view.nodeKindFilters[attrs.kind];
    if (enabled === false) return { ...attrs, hidden: true };
  }

  // Symbol-kind filter (function/method/class/...). Only applies when
  // the node carries a `symbolKind` attribute (i.e. is a symbol, not
  // a structural node) and the kind is in the filter map.
  if (typeof attrs.symbolKind === "string") {
    const enabled = view.symbolKindFilters[attrs.symbolKind];
    if (enabled === false) return { ...attrs, hidden: true };
  }

  // Depth filter hides nodes outside the configured BFS frontier.
  // This sits *outside* `pickHighlightMode` so the depth gate fires
  // even when no other highlight is active.
  if (view.depthReachable !== null && !view.depthReachable.has(nodeId)) {
    return { ...attrs, hidden: true };
  }

  // Iter 30: heatmap base layer. In `"complexity"` mode we replace the
  // topology color with a green→red gradient keyed off the cognitive
  // percentile; the persistent halo fires in *both* modes (always-on
  // refactor-candidate marker). The selection / citation / blast
  // overrides further down still win the color channel — heatmap is a
  // *base* coat, not a top coat.
  let baseAttrs: Attributes = attrs;
  const haloed = view.complexityHaloIds.has(nodeId);
  if (view.colorMode === "complexity" && view.complexityThresholds !== null) {
    baseAttrs = applyComplexityHeatmap(
      attrs,
      view.complexityThresholds,
      view.complexityHaloIds,
      nodeId,
    );
  } else if (haloed) {
    // Topology mode: keep the dir-hash color but still ring the
    // top-N. The halo reads as "this matters" without screaming.
    baseAttrs = {
      ...attrs,
      borderColor: HEATMAP_COLOR_TOP,
      borderSize: 2,
      haloed: true,
    };
  }

  if (attrs.isWorkspaceContext === true) {
    baseAttrs = {
      ...baseAttrs,
      color: "rgba(148, 163, 184, 0.42)",
      size: attrSize(attrs, 6) * 0.78,
      label: undefined,
      zIndex: -1,
      highlighted: false,
      workspaceContextDimmed: true,
      // Keep the workspace ring/badge visible so remote context still has an
      // identity, but make it thinner than selected-workspace nodes.
      borderSize: Math.max(1, (typeof attrs.borderSize === "number" ? attrs.borderSize : 1) * 0.7),
    };
  }

  // Iter y3mf: zoom-adaptive PageRank-percentile label gate. The
  // decision helper is pure; `cameraRatio` is plumbed in from
  // `useGraphReducers` via the HighlightView. Workspace-context nodes
  // are already de-emphasized to `label: undefined` above, so this
  // gate is a no-op for them. Highlight-mode branches
  // (`focus` / `neighbor` / `citation` / ...) set their own labels on
  // top of `baseAttrs`, so a selected/focused node always keeps its
  // label regardless of zoom. The `dim` branch already strips
  // `label`, so a dimmed node at low zoom is doubly label-stripped
  // (still correct, no double-strip needed).
  if (baseAttrs.label !== undefined) {
    const percentile = view.pagerankPercentile.get(nodeId);
    if (!shouldLabelAtZoom(percentile, view.cameraRatio)) {
      baseAttrs = { ...baseAttrs, label: undefined };
    }
  }

  const mode = pickHighlightMode(nodeId, view);
  if (mode === "none") return baseAttrs;

  switch (mode) {
    case "focus":
      return {
        ...baseAttrs,
        color: COLOR_FOCUS,
        size: attrSize(attrs, 6) * 1.6,
        // Preserve the original label even when the gate above
        // stripped it — a selected/focused node should always carry
        // its label regardless of zoom, otherwise the focal click
        // target has no readable identifier at far-zoom.
        label: attrs.label,
        zIndex: 100,
        highlighted: true,
      };
    case "neighbor":
      return {
        ...baseAttrs,
        color: COLOR_NEIGHBOR,
        size: attrSize(attrs, 6) * 1.15,
        label: attrs.label,
        zIndex: 60,
        highlighted: true,
      };
    case "citation":
      return {
        ...baseAttrs,
        color: COLOR_CITATION,
        size: attrSize(attrs, 6) * 1.2,
        label: attrs.label,
        zIndex: 80,
        highlighted: true,
      };
    case "tool":
      return {
        ...baseAttrs,
        color: COLOR_TOOL,
        size: attrSize(attrs, 6) * 1.15,
        label: attrs.label,
        zIndex: 70,
        highlighted: true,
      };
    case "blast": {
      // Triangular wave on `pulsePhase ∈ [0, 1]` produces a
      // smoothly-cycling lo↔hi animation as long as the canvas drives
      // re-renders. The supervisor in the hook tweens phase via
      // `requestAnimationFrame`.
      const t =
        view.pulsePhase < 0.5 ? view.pulsePhase * 2 : (1 - view.pulsePhase) * 2;
      return {
        ...baseAttrs,
        color: lerpHex(COLOR_BLAST_LO, COLOR_BLAST_HI, t),
        size: attrSize(attrs, 6) * (1.1 + 0.25 * t),
        label: attrs.label,
        zIndex: 90,
        highlighted: true,
      };
    }
    case "hover":
      return {
        ...baseAttrs,
        color: COLOR_HOVER,
        size: attrSize(attrs, 6) * 1.15,
        label: attrs.label,
        zIndex: 50,
        highlighted: true,
      };
    case "dim":
    default:
      return {
        ...baseAttrs,
        color: COLOR_DIMMED,
        label: undefined, // de-emphasize: hide labels on dimmed nodes
        zIndex: 0,
        highlighted: false,
      };
  }
}

// ── Complexity heatmap (iter 30) ────────────────────────────────────────────

/**
 * Color palette for the complexity-heatmap overlay. Green → yellow →
 * orange → red ramp keyed off the project-internal cognitive-complexity
 * percentile. Tailwind-500/600 hexes so they slot into the existing
 * design system without introducing new tokens.
 *
 *   ≤ p33 → green   (#10b981 emerald-500)
 *   ≤ p67 → yellow  (#eab308 yellow-500)
 *   ≤ p90 → orange  (#f97316 orange-500)
 *   >  p90 → red    (#ef4444 red-500)
 *   null  → gray    (#6b7280 gray-500) — non-function or unsupported
 *                   language; muted so the eye doesn't latch onto it.
 */
export const HEATMAP_COLOR_LOW = "#10b981";
export const HEATMAP_COLOR_MID = "#eab308";
export const HEATMAP_COLOR_HIGH = "#f97316";
export const HEATMAP_COLOR_TOP = "#ef4444";
export const HEATMAP_COLOR_NULL = "#6b7280";

/**
 * Pre-computed cognitive-complexity percentile breakpoints for the
 * current snapshot. `null` means "no function nodes had a populated
 * `cognitive` field"; callers should disable the heatmap toggle in
 * that case rather than show a degenerate single-color overlay.
 */
export interface ComplexityThresholds {
  /** 33rd percentile of cognitive complexity. */
  p33: number;
  /** 67th percentile. */
  p67: number;
  /** 90th percentile. */
  p90: number;
  /** Number of function nodes that contributed to the percentiles. */
  sampleSize: number;
}

/**
 * Compute the heatmap's three percentile breakpoints from a list of
 * raw cognitive-complexity values. Linear-interpolation percentile
 * (the same convention `numpy.percentile` defaults to). Returns `null`
 * when the sample is empty so the UI can fall back to "toggle disabled."
 */
export function computeComplexityThresholds(
  cognitiveValues: ReadonlyArray<number>,
): ComplexityThresholds | null {
  if (cognitiveValues.length === 0) return null;
  const sorted = [...cognitiveValues]
    .filter((v) => Number.isFinite(v))
    .sort((a, b) => a - b);
  if (sorted.length === 0) return null;
  return {
    p33: percentile(sorted, 0.33),
    p67: percentile(sorted, 0.67),
    p90: percentile(sorted, 0.9),
    sampleSize: sorted.length,
  };
}

/**
 * Linear-interpolation percentile on a pre-sorted ascending array.
 * `q ∈ [0, 1]`. Mirrors the default behavior of `numpy.percentile` /
 * Excel `PERCENTILE.INC` so docs and screenshots line up with what an
 * engineer would compute by hand.
 */
function percentile(sortedAsc: ReadonlyArray<number>, q: number): number {
  if (sortedAsc.length === 0) return 0;
  if (sortedAsc.length === 1) return sortedAsc[0];
  const clamped = Math.max(0, Math.min(1, q));
  const pos = clamped * (sortedAsc.length - 1);
  const lo = Math.floor(pos);
  const hi = Math.ceil(pos);
  if (lo === hi) return sortedAsc[lo];
  const frac = pos - lo;
  return sortedAsc[lo] + (sortedAsc[hi] - sortedAsc[lo]) * frac;
}

/**
 * Pick the heatmap color for a single cognitive value given the
 * snapshot-wide thresholds. `null` / `undefined` cognitive falls into
 * the muted-gray "non-function" bucket so the eye doesn't latch onto
 * file/folder/external nodes.
 */
export function colorForComplexity(
  cognitive: number | null | undefined,
  thresholds: ComplexityThresholds,
): string {
  if (cognitive === null || cognitive === undefined) return HEATMAP_COLOR_NULL;
  if (cognitive <= thresholds.p33) return HEATMAP_COLOR_LOW;
  if (cognitive <= thresholds.p67) return HEATMAP_COLOR_MID;
  if (cognitive <= thresholds.p90) return HEATMAP_COLOR_HIGH;
  return HEATMAP_COLOR_TOP;
}

/**
 * Color-mode discriminator owned by `CodeGraphPage`.
 *   - `"topology"` (default) — preserves the dir-hash / community
 *     coloring that ships in the adapter; this iteration doesn't
 *     change it.
 *   - `"complexity"` — swaps the color channel for a green→red gradient
 *     keyed off the function's cognitive-complexity percentile. Size
 *     stays driven by PageRank so a "big and red" node is the strongest
 *     refactor candidate.
 */
export type ColorMode = "topology" | "complexity";

/**
 * Defensive readers — graphology attribute bags are typed `unknown` at
 * the boundary, so we narrow them once and reuse.
 */
function attrCognitive(attrs: Attributes): number | null {
  const v = attrs.cognitive;
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

/**
 * Apply the heatmap color override on top of a base attribute bag.
 * Pulled out into its own helper so the wrapping reducer in
 * `useGraphReducers` can compose it with the existing highlight logic
 * — color flips first, then `nodeReducer` decides whether the
 * selection / dim / blast layers should override that color further.
 */
export function applyComplexityHeatmap(
  attrs: Attributes,
  thresholds: ComplexityThresholds,
  haloIds: ReadonlySet<string>,
  nodeId: string,
): Attributes {
  const cognitive = attrCognitive(attrs);
  const color = colorForComplexity(cognitive, thresholds);
  if (haloIds.has(nodeId)) {
    return {
      ...attrs,
      color,
      // `borderColor` / `borderSize` aren't first-class in vanilla Sigma,
      // but the canvas re-uses them in the custom hover renderer
      // (`useSigmaGraph`) and they're picked up by the noverlap pass.
      // Keeping the keys explicit so we can wire a halo program later
      // without touching this reducer.
      borderColor: HEATMAP_COLOR_TOP,
      borderSize: 2,
      haloed: true,
    };
  }
  return { ...attrs, color };
}

/**
 * Build the top-N most-complex node ids from the snapshot. Iter 30
 * pins this halo regardless of color mode — even in topology mode you
 * want refactor candidates visually marked.
 */
export function topComplexityIds(
  cognitiveByNode: ReadonlyArray<{ id: string; cognitive: number | null }>,
  topN: number,
): Set<string> {
  const out = new Set<string>();
  const ranked = cognitiveByNode
    .filter((n): n is { id: string; cognitive: number } => n.cognitive !== null)
    .sort((a, b) => b.cognitive - a.cognitive)
    .slice(0, Math.max(0, topN));
  for (const r of ranked) out.add(r.id);
  return out;
}

// ── Edge reducer ────────────────────────────────────────────────────────────

const EDGE_COLOR_DIMMED = "rgba(100, 116, 139, 0.08)"; // slate-500 @ 8%
const EDGE_COLOR_HIGHLIGHTED = "rgba(251, 146, 60, 0.85)"; // orange-400 @ 85%
const EDGE_COLOR_CROSS_WORKSPACE = "rgba(250, 204, 21, 0.92)"; // yellow-400 @ 92%
const EDGE_COLOR_CROSS_WORKSPACE_DIMMED = "rgba(250, 204, 21, 0.55)";

export interface EdgeReducerOverride extends Attributes {
  color?: string;
  size?: number;
  hidden?: boolean;
  zIndex?: number;
}

/**
 * Edge reducer:
 *   - Hide edges whose `kind` is filtered off in the store.
 *   - Hide edges crossing the depth-filter frontier.
 *   - Highlight edges incident on the selection's 1-hop set.
 *   - Otherwise dim them so node clusters remain readable.
 *
 * Edge kind defaults to "visible" when missing from the filter map;
 * the store's reducer always re-merges the kind list on toggle but
 * snapshot edges may carry kinds the UI hasn't enumerated yet.
 */
export function edgeReducer(
  source: string,
  target: string,
  attrs: Attributes,
  view: HighlightView,
): Attributes {
  // Edge-kind toggle. Default true for unknown kinds.
  if (typeof attrs.kind === "string") {
    const enabled = view.edgeKindFilters[attrs.kind];
    if (enabled === false) return { ...attrs, hidden: true };
  }

  // Depth filter: an edge is visible only if both endpoints are visible.
  if (view.depthReachable !== null) {
    if (
      !view.depthReachable.has(source) ||
      !view.depthReachable.has(target)
    ) {
      return { ...attrs, hidden: true };
    }
  }

  if (isViewEmpty(view)) return attrs;

  // Edge sits inside the selection's 1-hop frontier?
  const isCrossWorkspace =
    attrs.isCrossWorkspace === true || attrs.crossWorkspace === true;
  const isSelectionEdge =
    view.selectionId !== null &&
    (source === view.selectionId ||
      target === view.selectionId ||
      (view.selectionNeighbors.has(source) &&
        view.selectionNeighbors.has(target)));

  if (isSelectionEdge) {
    return {
      ...attrs,
      color: isCrossWorkspace ? EDGE_COLOR_CROSS_WORKSPACE : EDGE_COLOR_HIGHLIGHTED,
      size: attrSize(attrs, 1) * (isCrossWorkspace ? 1.55 : 1.3),
      zIndex: isCrossWorkspace ? 30 : 5,
    };
  }

  if (isCrossWorkspace) {
    return {
      ...attrs,
      color: EDGE_COLOR_CROSS_WORKSPACE_DIMMED,
      size: attrSize(attrs, 1) * 1.2,
      zIndex: 18,
    };
  }

  return {
    ...attrs,
    color: EDGE_COLOR_DIMMED,
    zIndex: 0,
  };
}

// ── BFS helpers ─────────────────────────────────────────────────────────────

/**
 * Compute the 1-hop neighborhood of `nodeId` (inclusive of `nodeId`
 * itself). Walks both incoming and outgoing edges so the visual
 * "neighborhood" highlight is undirected — that matches user mental
 * model better than a strict outbound walk.
 *
 * The graph type is intentionally loose (`{ neighbors: (id) =>
 * Iterable<string> }`) so the reducer can be unit-tested without
 * pulling graphology into the test path.
 */
export interface MinimalGraph {
  hasNode(id: string): boolean;
  /** Returns *both* in-edge and out-edge endpoints (undirected view). */
  neighbors(id: string): string[];
}

export function oneHopNeighborhood(
  graph: MinimalGraph,
  nodeId: string,
): Set<string> {
  const out = new Set<string>();
  if (!graph.hasNode(nodeId)) return out;
  out.add(nodeId);
  for (const n of graph.neighbors(nodeId)) out.add(n);
  return out;
}

/**
 * BFS up to `maxDepth` hops from `seed`. Returns `null` (= "no depth
 * filtering") when `maxDepth` is at or above {@link Number.MAX_SAFE_INTEGER}
 * so callers can use it as a sentinel.
 */
export function bfsReachable(
  graph: MinimalGraph,
  seed: string,
  maxDepth: number,
): Set<string> {
  const out = new Set<string>();
  if (!graph.hasNode(seed)) return out;
  out.add(seed);
  if (maxDepth <= 0) return out;

  let frontier: string[] = [seed];
  for (let d = 0; d < maxDepth; d += 1) {
    const next: string[] = [];
    for (const id of frontier) {
      for (const n of graph.neighbors(id)) {
        if (out.has(n)) continue;
        out.add(n);
        next.push(n);
      }
    }
    if (next.length === 0) break;
    frontier = next;
  }
  return out;
}
