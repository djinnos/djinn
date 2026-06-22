/**
 * useGraphReducers — derive a `HighlightView` from the Zustand
 * highlight store and feed it into Sigma's `nodeReducer` /
 * `edgeReducer` callbacks.
 *
 * This is where the memoization happens. The reducer functions
 * themselves stay pure: they read a snapshot of the view and emit
 * per-node attribute overrides. We:
 *
 *   1. Subscribe to every relevant store slice.
 *   2. Lazily compute `selectionNeighbors` (1-hop set) when
 *      `selectionId` changes.
 *   3. Drive a `requestAnimationFrame` loop only while a transient
 *      pulse set is non-empty — otherwise we don't burn CPU.
 *   4. Emit reducer fns whose closure reads `viewRef`, so Sigma sees
 *      a fresh view on every frame without forcing re-mounts.
 *
 * Sigma also needs a hint to repaint when the store mutates — we
 * call `sigma.refresh()` from a separate effect that watches the
 * non-animated slices. The pulse loop calls `refresh()` directly
 * inside its rAF callback.
 */

import { useEffect, useMemo, useRef } from "react";
import type Graph from "graphology";

import {
  EMPTY_HIGHLIGHT_VIEW,
  computeComplexityThresholds,
  edgeReducer as edgeReducerImpl,
  nodeReducer as nodeReducerImpl,
  oneHopNeighborhood,
  topComplexityIds,
  isTraversalContainmentEdge,
  type Attributes,
  type ComplexityThresholds,
  type EdgeKindAwareGraph,
  type HighlightView,
} from "@/lib/codeGraphReducers";
import { computePagerankPercentiles } from "@/lib/codeGraphLabels";
import {
  lodTierForZoom,
  VIEWPORT_CULLING_THRESHOLD,
  viewportBoundsEqual,
  type ViewportBounds,
} from "@/lib/codeGraphAdapter";
import {
  DEFAULT_DOI_REVEAL_COUNT,
  useCodeGraphStore,
  type FocusDirection,
} from "@/stores/codeGraphStore";
import type { SigmaInstanceHandle, SigmaReducerHooks } from "./useSigmaGraph";

const DOI_DISTANCE_LAMBDA = 0.18;

interface DoiFocusResult {
  focusIds: ReadonlySet<string>;
  contextIds: ReadonlySet<string>;
  scores: ReadonlyMap<string, number>;
}

const EMPTY_DOI_FOCUS: DoiFocusResult = {
  focusIds: new Set<string>(),
  contextIds: new Set<string>(),
  scores: new Map<string, number>(),
};

function normalizeEdgeKind(attrs: Attributes | undefined): string | null {
  const raw = attrs?.kind ?? attrs?.edgeKind;
  return typeof raw === "string" ? raw : null;
}

function edgeIdsForDirection(
  graph: Graph,
  nodeId: string,
  direction: FocusDirection,
): string[] {
  const g = graph as unknown as {
    outboundEdges?: (node: string) => string[];
    inboundEdges?: (node: string) => string[];
    outEdges?: (node: string) => string[];
    inEdges?: (node: string) => string[];
    edges?: (node?: string) => string[];
  };
  const out = new Set<string>();
  try {
    if (direction === "dependencies" || direction === "both") {
      const edges = (g.outboundEdges ?? g.outEdges)?.call(graph, nodeId) ?? [];
      for (const edge of edges) out.add(edge);
    }
    if (direction === "dependents" || direction === "both") {
      const edges = (g.inboundEdges ?? g.inEdges)?.call(graph, nodeId) ?? [];
      for (const edge of edges) out.add(edge);
    }
    if (out.size === 0 && direction === "both" && g.edges) {
      for (const edge of g.edges.call(graph, nodeId)) out.add(edge);
    }
  } catch {
    return [];
  }
  return Array.from(out);
}

function nextNodeForDirection(
  graph: Graph,
  edgeId: string,
  nodeId: string,
  direction: FocusDirection,
): string | null {
  try {
    const source = graph.source(edgeId);
    const target = graph.target(edgeId);
    if (direction === "dependencies") return source === nodeId ? target : null;
    if (direction === "dependents") return target === nodeId ? source : null;
    if (source === nodeId) return target;
    if (target === nodeId) return source;
  } catch {
    return null;
  }
  return null;
}

export function computeDoiFocus(
  graph: Graph | null,
  focusAnchorId: string | null,
  focusDirection: FocusDirection,
  pagerankPercentile: ReadonlyMap<string, number>,
  revealCount: number,
): DoiFocusResult {
  if (!graph || !focusAnchorId || !graph.hasNode(focusAnchorId)) {
    return EMPTY_DOI_FOCUS;
  }

  const distances = new Map<string, number>([[focusAnchorId, 0]]);
  const queue: string[] = [focusAnchorId];
  for (let cursor = 0; cursor < queue.length; cursor += 1) {
    const nodeId = queue[cursor];
    const distance = distances.get(nodeId) ?? 0;
    for (const edgeId of edgeIdsForDirection(graph, nodeId, focusDirection)) {
      let attrs: Attributes | undefined;
      try {
        attrs = graph.getEdgeAttributes(edgeId) as Attributes;
      } catch {
        attrs = undefined;
      }
      if (isTraversalContainmentEdge(normalizeEdgeKind(attrs))) continue;
      const next = nextNodeForDirection(graph, edgeId, nodeId, focusDirection);
      if (next === null || distances.has(next) || !graph.hasNode(next))
        continue;
      distances.set(next, distance + 1);
      queue.push(next);
    }
  }

  const scored = Array.from(distances.entries()).map(([id, distance]) => ({
    id,
    distance,
    score: (pagerankPercentile.get(id) ?? 0) - DOI_DISTANCE_LAMBDA * distance,
  }));
  scored.sort(
    (a, b) =>
      b.score - a.score || a.distance - b.distance || a.id.localeCompare(b.id),
  );

  const limit = Number.isFinite(revealCount)
    ? Math.max(1, Math.round(revealCount))
    : DEFAULT_DOI_REVEAL_COUNT;
  const focusIds = new Set<string>([focusAnchorId]);
  for (const entry of scored) {
    if (focusIds.size >= limit) break;
    focusIds.add(entry.id);
  }

  const scores = new Map<string, number>();
  const contextIds = new Set<string>();
  for (const entry of scored) {
    scores.set(entry.id, entry.score);
    if (!focusIds.has(entry.id)) contextIds.add(entry.id);
  }

  return { focusIds, contextIds, scores };
}

/**
 * Wrap a graphology `Graph` so it satisfies the `EdgeKindAwareGraph`
 * interface the neighborhood helpers expect — Sigma's graph carries directed
 * edges, but the highlight neighborhood walks both directions. The
 * `edgeKind` reader lets `oneHopNeighborhood` skip containment edges
 * so the selection frontier does not expand through structural nesting.
 */
function asMinimalGraph(graph: Graph): EdgeKindAwareGraph {
  return {
    hasNode: (id) => graph.hasNode(id),
    neighbors: (id) => {
      // graphology's `.neighbors()` returns the union of in + out
      // neighbors on a directed graph, which is exactly what we want
      // for "show me everything one hop from here."
      try {
        return graph.neighbors(id);
      } catch {
        return [];
      }
    },
    edgeKind: (source, target) => {
      try {
        // graphology's `.edge()` returns the first edge between two
        // nodes in either direction on a directed graph. We read the
        // `kind` attribute from it.
        const edge = graph.edge(source, target);
        if (!edge) return null;
        const kind = graph.getEdgeAttribute(edge, "kind");
        return typeof kind === "string" ? kind : null;
      } catch {
        return null;
      }
    },
  };
}

export interface UseGraphReducersResult {
  /** Pass straight to `useSigmaGraph(...)`'s reducers parameter. */
  reducers: SigmaReducerHooks;
  /**
   * Iter 30: percentile breakpoints for the complexity heatmap.
   * `null` when no function nodes carry a cognitive value — callers
   * should disable the heatmap toggle in that case.
   */
  complexityThresholds: ComplexityThresholds | null;
  /** Top-N most-complex node ids (used by the halo + legend). */
  complexityHaloIds: ReadonlySet<string>;
}

/**
 * Build the reducer pair the canvas hands to `useSigmaGraph`.
 *
 * `sigma` is optional — when provided, the hook calls `refresh()` on
 * the Sigma instance whenever the highlight slices change so the
 * canvas repaints with the new view without us touching the graph
 * itself.
 */
export function useGraphReducers(
  graph: Graph | null,
  sigma: SigmaInstanceHandle | null,
): UseGraphReducersResult {
  // ── Subscribe to the store slices we care about ────────────────────────
  const selectionId = useCodeGraphStore((s) => s.selectionId);
  const citationIds = useCodeGraphStore((s) => s.citationIds);
  const toolHighlightIds = useCodeGraphStore((s) => s.toolHighlightIds);
  const blastRadiusFrontier = useCodeGraphStore((s) => s.blastRadiusFrontier);
  const doiImpactIds = useCodeGraphStore((s) => s.doiImpactIds);
  const hoverId = useCodeGraphStore((s) => s.hoverId);
  const edgeKindFilters = useCodeGraphStore((s) => s.edgeKindFilters);
  const nodeKindFilters = useCodeGraphStore((s) => s.nodeKindFilters);
  const symbolKindFilters = useCodeGraphStore((s) => s.symbolKindFilters);
  const hideTests = useCodeGraphStore((s) => s.hideTests);
  const colorMode = useCodeGraphStore((s) => s.colorMode);
  const focusAnchorId = useCodeGraphStore((s) => s.focusAnchorId);
  const focusDirection = useCodeGraphStore((s) => s.focusDirection);
  const doiRevealCount = useCodeGraphStore((s) => s.doiRevealCount);
  const expandedRegions = useCodeGraphStore((s) => s.expandedRegions);
  const crateFilter = useCodeGraphStore((s) => s.crateFilter);

  // ── Lazy 1-hop neighbor set (memoized) ─────────────────────────────────
  const selectionNeighbors = useMemo<ReadonlySet<string>>(() => {
    if (!graph || !selectionId) return new Set();
    return oneHopNeighborhood(asMinimalGraph(graph), selectionId);
  }, [graph, selectionId]);

  // ── Iter 30: complexity heatmap thresholds + halo set ──────────────────
  // Computed once per graph identity. Only function-like nodes that
  // actually carry a `cognitive` attribute contribute — files, types,
  // externals, and unsupported-language nodes are filtered out so the
  // percentile distribution isn't dragged down by zeros.
  const complexityThresholds = useMemo<ComplexityThresholds | null>(() => {
    if (!graph) return null;
    const values: number[] = [];
    for (const id of graph.nodes()) {
      const cog = graph.getNodeAttribute(id, "cognitive");
      if (typeof cog === "number" && Number.isFinite(cog)) values.push(cog);
    }
    return computeComplexityThresholds(values);
  }, [graph]);

  /**
   * Top-5 most-complex node ids — these wear a persistent red halo
   * regardless of color mode. The reasoning is that even in topology
   * mode the user wants refactor candidates visually marked.
   */
  const TOP_COMPLEXITY_HALO_N = 5;
  const complexityHaloIds = useMemo<ReadonlySet<string>>(() => {
    if (!graph) return new Set();
    const pairs: Array<{ id: string; cognitive: number | null }> = [];
    for (const id of graph.nodes()) {
      const cog = graph.getNodeAttribute(id, "cognitive");
      pairs.push({
        id,
        cognitive: typeof cog === "number" && Number.isFinite(cog) ? cog : null,
      });
    }
    return topComplexityIds(pairs, TOP_COMPLEXITY_HALO_N);
  }, [graph]);

  // ── Iter y3mf: PageRank percentile map for zoom-adaptive labels ─────────
  // Computed once per graph identity. When the snapshot has no
  // `pagerank` data the map is empty and `shouldLabelAtZoom` falls
  // through to its `true` sentinel — so existing LOD behavior is
  // preserved for older caches / fixtures.
  const pagerankPercentile = useMemo<ReadonlyMap<string, number>>(() => {
    if (!graph) return new Map();
    return computePagerankPercentiles(graph);
  }, [graph]);

  // Directional DOI focus model: compute graph distance from the anchor using
  // dependency direction semantics, combine it with PageRank percentile
  // importance, and hand reducers precomputed O(1) membership sets.
  const topologyDoiFocus = useMemo<DoiFocusResult>(() => {
    return computeDoiFocus(
      graph,
      focusAnchorId,
      focusDirection,
      pagerankPercentile,
      doiRevealCount,
    );
  }, [
    graph,
    focusAnchorId,
    focusDirection,
    pagerankPercentile,
    doiRevealCount,
  ]);

  const doiFocus = useMemo<DoiFocusResult>(() => {
    if (doiImpactIds.size === 0) return topologyDoiFocus;
    const focusIds = new Set(topologyDoiFocus.focusIds);
    const contextIds = new Set(topologyDoiFocus.contextIds);
    for (const id of doiImpactIds) {
      if (!id || !graph?.hasNode(id)) continue;
      focusIds.add(id);
      contextIds.delete(id);
    }
    return {
      focusIds,
      contextIds,
      scores: topologyDoiFocus.scores,
    };
  }, [doiImpactIds, graph, topologyDoiFocus]);

  // ── Build the live HighlightView (mutable ref, read on each frame) ────
  // Sigma reads `viewRef.current` from inside its rAF render loop —
  // separate from React's commit phase — so we sync the ref inside
  // `useEffect` and then poke Sigma to repaint.
  const viewRef = useRef<HighlightView>(EMPTY_HIGHLIGHT_VIEW);

  useEffect(() => {
    viewRef.current = {
      selectionId,
      selectionNeighbors,
      citationIds,
      toolHighlightIds,
      blastRadiusFrontier,
      hoverId,
      edgeKindFilters,
      nodeKindFilters,
      symbolKindFilters,
      hideTests,
      doiFocusIds: doiFocus.focusIds,
      doiContextIds: doiFocus.contextIds,
      doiScores: doiFocus.scores,
      // Preserve the latest animated phase so we don't snap to 0
      // every time a non-pulse slice changes.
      pulsePhase: viewRef.current.pulsePhase,
      colorMode,
      complexityThresholds,
      complexityHaloIds,
      pagerankPercentile,
      // Preserve the live camera value so re-syncs (e.g. on
      // selection change) don't clobber the post-render value the
      // `afterRender` effect just pushed in. The two are kept
      // orthogonal: the store-mirror effect owns graph-derived
      // fields, the camera effect owns the lens.
      cameraRatio: viewRef.current.cameraRatio,
      // Continuous semantic LOD tier — preserved across store
      // re-syncs (the `afterRender` effect updates it).
      lodTier: viewRef.current.lodTier,
      // Viewport bounds — preserved across store re-syncs.
      viewportBounds: viewRef.current.viewportBounds,
      // Click-to-expand regions from the store.
      expandedRegions,
      // Crate isolation from the legend.
      crateFilter,
    };
    sigma?.refresh();
  }, [
    sigma,
    crateFilter,
    selectionId,
    selectionNeighbors,
    citationIds,
    toolHighlightIds,
    blastRadiusFrontier,
    doiImpactIds,
    hoverId,
    edgeKindFilters,
    nodeKindFilters,
    symbolKindFilters,
    hideTests,
    colorMode,
    focusAnchorId,
    focusDirection,
    doiRevealCount,
    doiFocus,
    complexityThresholds,
    complexityHaloIds,
    pagerankPercentile,
    expandedRegions,
  ]);

  // ── Pulse phase (animated only when blast frontier is non-empty) ──────
  // Writes straight into `viewRef` so the rAF tick is independent of
  // React's commit cycle — Sigma sees the new phase on its next paint.
  useEffect(() => {
    if (blastRadiusFrontier.size === 0) {
      viewRef.current = { ...viewRef.current, pulsePhase: 0 };
      sigma?.refresh();
      return;
    }
    let raf = 0;
    const start = performance.now();
    // 1.2s loop matches the spec's CSS-driven 1.2s pulse.
    const PERIOD_MS = 1200;
    const tick = (now: number) => {
      const dt = (now - start) % PERIOD_MS;
      viewRef.current = { ...viewRef.current, pulsePhase: dt / PERIOD_MS };
      sigma?.refresh();
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [blastRadiusFrontier, sigma]);

  // ── Iter y3mf: mirror Sigma's camera ratio into the view ──────────────
  // Sigma emits `afterRender` on every frame (the same hook the
  // `pulsePhase` rAF loop ultimately drives). We read the camera
  // ratio post-mutation and only call `refresh()` when it actually
  // changes — Sigma already knows to repaint via its own internal
  // camera state, the `refresh()` here is for the
  // `nodeReducer` re-running the percentile gate.
  //
  // Also computes LOD tier and viewport bounds from the camera state
  // for continuous semantic zoom and viewport culling.
  const nodeCountRef = useRef(0);
  useEffect(() => {
    nodeCountRef.current = graph?.order ?? 0;
  }, [graph]);

  useEffect(() => {
    if (!sigma) return;
    const off = sigma.on("afterRender", () => {
      try {
        const ratio = sigma.getCameraRatio();
        const lodTier = lodTierForZoom(ratio);
        // Viewport culling only activates for large graphs.
        const viewportBounds: ViewportBounds | null =
          nodeCountRef.current >= VIEWPORT_CULLING_THRESHOLD
            ? sigma.getViewportBounds()
            : null;
        // `getViewportBounds()` returns a fresh object every call, so a
        // reference (`!==`) comparison here is ALWAYS true — which would
        // fire `refresh()` on every `afterRender`, and since `refresh()`
        // schedules another render, that's a perpetual 60fps render loop
        // (re-running every reducer each frame, even at idle). This only
        // bites graphs past VIEWPORT_CULLING_THRESHOLD, where bounds are
        // non-null. Compare by value so we only refresh when the viewport
        // actually moved.
        if (
          ratio !== viewRef.current.cameraRatio ||
          lodTier !== viewRef.current.lodTier ||
          !viewportBoundsEqual(viewportBounds, viewRef.current.viewportBounds)
        ) {
          viewRef.current = {
            ...viewRef.current,
            cameraRatio: ratio,
            lodTier,
            viewportBounds,
          };
          sigma.refresh();
        }
      } catch {
        // unmount race — no-op
      }
    });
    return off;
  }, [sigma, graph]);

  // ── Stable reducer pair — closures read `viewRef` so the latest
  //    slice always wins without us re-creating the fns on every render.
  const reducers = useMemo<SigmaReducerHooks>(
    () => ({
      nodeReducer: (id: string, attrs: Attributes) =>
        nodeReducerImpl(id, attrs, viewRef.current),
      edgeReducer: (id: string, attrs: Attributes) => {
        // Sigma's `edgeReducer` signature only hands us the edge id
        // and attrs — the source/target endpoints aren't passed
        // through. We pull them off the underlying graph; this is
        // O(1) on graphology.
        if (!graph) return attrs;
        let source = "";
        let target = "";
        try {
          source = graph.source(id);
          target = graph.target(id);
        } catch {
          return attrs;
        }
        return edgeReducerImpl(source, target, attrs, viewRef.current);
      },
    }),
    [graph],
  );

  return { reducers, complexityThresholds, complexityHaloIds };
}
