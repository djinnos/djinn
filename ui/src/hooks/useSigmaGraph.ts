/**
 * useSigmaGraph — own the Sigma + ForceAtlas2 lifecycle for /code-graph.
 *
 * Phase split (PR D2):
 *   1. Caller passes a graphology graph (built via `codeGraphAdapter`)
 *      and a container ref.
 *   2. We mount a Sigma instance once per (graph identity, container)
 *      pair, register the curved-edge program from `@sigma/edge-curve`,
 *      and spin up a ForceAtlas2 layout supervisor running off the
 *      main thread.
 *   3. The supervisor auto-stops after `RUN_MS` (3.5s on the default
 *      ~2k-node fixture, scaled mildly with node count) so the canvas
 *      is interactive at acceptance and we don't burn CPU after the
 *      nodes have settled.
 *
 * Layout settings follow the plan: gravity / scalingRatio /
 * Barnes-Hut θ scale with `nodeCount` via `inferSettings`. The
 * per-type mass that `codeGraphAdapter` writes into each node's
 * attributes feeds FA2 implicitly through `inferSettings` defaults
 * (FA2 reads `mass` when present; otherwise falls back to in-degree).
 *
 * Interactions, hovers, selection, citation highlighting — all D3+.
 */

import { useEffect, useRef, useState } from "react";
import type Graph from "graphology";
import Sigma from "sigma";

import type { Attributes } from "@/lib/codeGraphReducers";
import EdgeCurveProgram from "@sigma/edge-curve";
import forceAtlas2 from "graphology-layout-forceatlas2";
import FA2LayoutSupervisor from "graphology-layout-forceatlas2/worker";

/**
 * Per-render hooks the caller can supply to recolor / hide nodes &
 * edges without re-mounting Sigma. Sigma calls these on every frame
 * — keep them pure and *cheap* (lookups in pre-computed sets).
 *
 * Used by D3 to layer selection / citation / tool-call highlights over
 * the base render. The hook itself stays agnostic about the highlight
 * source; only the canvas wires in the Zustand store.
 */
export interface SigmaReducerHooks {
  nodeReducer?: (id: string, attrs: Attributes) => Attributes;
  edgeReducer?: (id: string, attrs: Attributes) => Attributes;
}

/** Imperative handle returned by the hook for click / hover wiring. */
export interface SigmaInstanceHandle {
  on: <E extends string>(
    event: E,
    handler: (payload: { node?: string; edge?: string }) => void,
  ) => () => void;
  /** Force a fresh paint — used after the store mutates. */
  refresh: () => void;
  /** Look up node attributes (for tooltip / detail panels). */
  getNodeAttributes: (id: string) => Attributes | null;
}

export interface UseSigmaGraphResult {
  /** True once the Sigma instance is mounted and the layout has started. */
  ready: boolean;
  /**
   * True while the FA2 supervisor is iterating. Useful for showing a
   * "settling…" indicator. Goes false either when the auto-stop
   * timer fires or the caller invokes `stopLayout()`.
   */
  layoutRunning: boolean;
  /** Force the layout supervisor to halt. Idempotent. */
  stopLayout: () => void;
  /** Imperative handle for events; null until Sigma is mounted. */
  sigma: SigmaInstanceHandle | null;
}

/**
 * Auto-stop window for the FA2 supervisor. The plan's acceptance
 * criterion is "~2k nodes render in <3s", which means we want the
 * layout settled (or close to it) within that budget. Empirically
 * 3.5s on a desktop browser produces a stable layout for ~2k-node
 * graphs; we add a 1ms-per-node nudge so very dense graphs get a
 * little extra time without blocking forever.
 */
function inferRunMs(nodeCount: number): number {
  return 3_500 + Math.min(nodeCount, 5_000) * 1.5;
}

export function useSigmaGraph(
  containerRef: React.RefObject<HTMLDivElement | null>,
  graph: Graph | null,
  reducers?: SigmaReducerHooks,
): UseSigmaGraphResult {
  const sigmaRef = useRef<Sigma | null>(null);
  const supervisorRef = useRef<FA2LayoutSupervisor | null>(null);
  const stopTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Stash the reducer hooks in a ref so the Sigma instance always reads
  // the latest pair without us having to re-mount on every render. This
  // is the trick that makes D3 layered highlights flicker-free.
  const reducersRef = useRef<SigmaReducerHooks | undefined>(reducers);
  useEffect(() => {
    reducersRef.current = reducers;
  }, [reducers]);

  const [ready, setReady] = useState(false);
  const [layoutRunning, setLayoutRunning] = useState(false);
  const [handle, setHandle] = useState<SigmaInstanceHandle | null>(null);

  useEffect(() => {
    const container = containerRef.current;
    if (!container || !graph) return;

    // ── Sigma instance ────────────────────────────────────────────
    // Curved edges keep parallel relationships (e.g. "calls" + "reads"
    // between the same pair) visually distinct. Sigma 3 picks WebGL
    // by default; we leave that path on so we hit the ~5k-node
    // performance ceiling rather than the SVG cliff at ~500.
    let sigma: Sigma | null = null;
    try {
      sigma = new Sigma(graph, container, {
        renderEdgeLabels: false,
        defaultEdgeType: "curved",
        edgeProgramClasses: {
          curved: EdgeCurveProgram,
        },
        labelDensity: 0.07,
        labelGridCellSize: 60,
        labelRenderedSizeThreshold: 6,
        // Z-order so high-pagerank (large) nodes render last and
        // sit above the smaller ones — much easier to click hubs.
        zIndex: true,
        // PR D3: layered highlight reducers. The wrapper indirection
        // through `reducersRef` lets the canvas swap reducer fns on
        // every render without re-mounting Sigma — Sigma reads the
        // current ref each frame.
        nodeReducer: (id, attrs) => {
          const fn = reducersRef.current?.nodeReducer;
          return fn ? fn(id, attrs) : attrs;
        },
        edgeReducer: (id, attrs) => {
          const fn = reducersRef.current?.edgeReducer;
          return fn ? fn(id, attrs) : attrs;
        },
      });
    } catch (err) {
      // Defensive: if WebGL initialization fails (e.g. headless test
      // environment without WebGL stub), bail without throwing —
      // the canvas will be empty rather than crash the page.
      console.warn("[useSigmaGraph] Sigma init failed:", err);
      return;
    }
    sigmaRef.current = sigma;
    setReady(true);

    // Imperative handle — exposed so the canvas can wire click /
    // hover handlers without poking Sigma directly. Each method
    // tolerates a stubbed Sigma in tests by no-op'ing when the
    // underlying API is missing.
    const sigmaInstance = sigma;
    setHandle({
      on: (event, fn) => {
        // Sigma's typing for event names is a string union; we widen
        // here so the handle accepts any Sigma-supported event name
        // (`clickNode`, `enterNode`, `leaveNode`, ...).
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        const onFn = (sigmaInstance as any).on?.bind(sigmaInstance);
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        const offFn = (sigmaInstance as any).removeListener?.bind(
          sigmaInstance,
        );
        onFn?.(event, fn);
        return () => offFn?.(event, fn);
      },
      refresh: () => {
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        (sigmaInstance as any).refresh?.();
      },
      getNodeAttributes: (id) => {
        try {
          if (!graph.hasNode(id)) return null;
          return graph.getNodeAttributes(id);
        } catch {
          return null;
        }
      },
    });

    // ── ForceAtlas2 supervisor (off main thread) ────────────────
    // `inferSettings` scales gravity / scalingRatio / Barnes-Hut θ
    // with the node count, exactly as the plan asks for. We layer
    // a small `slowDown` boost so the cooling curve looks less
    // jittery at the end.
    const settings = forceAtlas2.inferSettings(graph);
    const supervisor = new FA2LayoutSupervisor(graph, {
      settings: {
        ...settings,
        slowDown: Math.max(settings.slowDown ?? 1, 1.5),
        // BarnesHut at θ=0.5 is the sweet spot for ~2k nodes; the
        // inferred default is sometimes 1.2 which hurts cluster
        // separation on hub-heavy code graphs.
        barnesHutTheta: Math.min(settings.barnesHutTheta ?? 0.5, 0.5),
      },
    });
    supervisorRef.current = supervisor;
    supervisor.start();
    setLayoutRunning(true);

    // Auto-stop so we don't burn CPU after the layout has settled.
    const runMs = inferRunMs(graph.order);
    const timer = setTimeout(() => {
      supervisor.stop();
      setLayoutRunning(false);
      // Small refit once the layout has cooled — Sigma's auto-fit on
      // mount captured pre-FA2 random positions, so call `refresh`
      // and let the reducer re-pick the camera bounds.
      sigma?.getCamera().animatedReset({ duration: 400 });
    }, runMs);
    stopTimerRef.current = timer;

    return () => {
      if (stopTimerRef.current) {
        clearTimeout(stopTimerRef.current);
        stopTimerRef.current = null;
      }
      if (supervisorRef.current) {
        supervisorRef.current.kill();
        supervisorRef.current = null;
      }
      if (sigmaRef.current) {
        sigmaRef.current.kill();
        sigmaRef.current = null;
      }
      setReady(false);
      setLayoutRunning(false);
      setHandle(null);
    };
  }, [containerRef, graph]);

  const stopLayout = () => {
    if (stopTimerRef.current) {
      clearTimeout(stopTimerRef.current);
      stopTimerRef.current = null;
    }
    if (supervisorRef.current) {
      supervisorRef.current.stop();
    }
    setLayoutRunning(false);
  };

  return { ready, layoutRunning, stopLayout, sigma: handle };
}
