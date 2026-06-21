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
 *   3. Drive a `requestAnimationFrame` loop only while the blast-
 *      radius set is non-empty — otherwise we don't burn CPU.
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
  type Attributes,
  type ComplexityThresholds,
  type HighlightView,
  type MinimalGraph,
} from "@/lib/codeGraphReducers";
import { computePagerankPercentiles } from "@/lib/codeGraphLabels";
import { useCodeGraphStore } from "@/stores/codeGraphStore";
import type { SigmaInstanceHandle, SigmaReducerHooks } from "./useSigmaGraph";

/**
 * Wrap a graphology `Graph` so it satisfies the `MinimalGraph`
 * interface the BFS helpers expect — Sigma's graph carries directed
 * edges, but the highlight neighborhood walks both directions.
 */
function asMinimalGraph(graph: Graph): MinimalGraph {
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
  const hoverId = useCodeGraphStore((s) => s.hoverId);
  const edgeKindFilters = useCodeGraphStore((s) => s.edgeKindFilters);
  const nodeKindFilters = useCodeGraphStore((s) => s.nodeKindFilters);
  const symbolKindFilters = useCodeGraphStore((s) => s.symbolKindFilters);
  const hideTests = useCodeGraphStore((s) => s.hideTests);
  const colorMode = useCodeGraphStore((s) => s.colorMode);

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
        cognitive:
          typeof cog === "number" && Number.isFinite(cog) ? cog : null,
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

  // Depth filtering was removed in favor of the DOI focus model. The
  // reducer field remains inert until the downstream DOI ranking task
  // replaces it with focus-scored context/dimming data.
  const depthReachable: ReadonlySet<string> | null = null;

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
      depthReachable,
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
    };
    sigma?.refresh();
  }, [
    sigma,
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
    colorMode,
    complexityThresholds,
    complexityHaloIds,
    pagerankPercentile,
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
  useEffect(() => {
    if (!sigma) return;
    const off = sigma.on("afterRender", () => {
      try {
        const ratio = sigma.getCameraRatio();
        if (ratio !== viewRef.current.cameraRatio) {
          viewRef.current = { ...viewRef.current, cameraRatio: ratio };
          sigma.refresh();
        }
      } catch {
        // unmount race — no-op
      }
    });
    return off;
  }, [sigma]);

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
