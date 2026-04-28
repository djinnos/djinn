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
import EdgeCurveProgram from "@sigma/edge-curve";
import forceAtlas2 from "graphology-layout-forceatlas2";
import FA2LayoutSupervisor from "graphology-layout-forceatlas2/worker";

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
): UseSigmaGraphResult {
  const sigmaRef = useRef<Sigma | null>(null);
  const supervisorRef = useRef<FA2LayoutSupervisor | null>(null);
  const stopTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const [ready, setReady] = useState(false);
  const [layoutRunning, setLayoutRunning] = useState(false);

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

  return { ready, layoutRunning, stopLayout };
}
