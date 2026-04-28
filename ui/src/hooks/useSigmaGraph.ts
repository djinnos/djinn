/**
 * useSigmaGraph — own the Sigma + ForceAtlas2 lifecycle for /code-graph.
 *
 * Phase split:
 *   1. Caller passes a graphology graph (built via `codeGraphAdapter`)
 *      and a container ref.
 *   2. We mount a Sigma instance once per (graph identity, container)
 *      pair, register the curved-edge program, paint a custom dark
 *      hover label/halo, and spin up a ForceAtlas2 layout supervisor
 *      running off the main thread.
 *   3. After FA2 stops, we run a short noverlap pass for visual cleanup
 *      and reset the camera so the user lands on a centered, settled
 *      layout.
 *   4. A camera-nudge effect fires on selection changes — Sigma 3
 *      caches edges aggressively across frames, and a 0.0001× zoom
 *      jiggle is the cheapest way to invalidate that cache.
 *
 * Layout settings scale with `nodeCount` so a 12k-node monorepo runs
 * with looser gravity / higher scaling-ratio than a 200-node sample.
 */

import { useEffect, useRef, useState } from "react";
import type Graph from "graphology";
import Sigma from "sigma";

import type { Attributes } from "@/lib/codeGraphReducers";
import EdgeCurveProgram from "@sigma/edge-curve";
import forceAtlas2 from "graphology-layout-forceatlas2";
import FA2LayoutSupervisor from "graphology-layout-forceatlas2/worker";
import noverlap from "graphology-layout-noverlap";
import { useCodeGraphStore } from "@/stores/codeGraphStore";

export interface SigmaReducerHooks {
  nodeReducer?: (id: string, attrs: Attributes) => Attributes;
  edgeReducer?: (id: string, attrs: Attributes) => Attributes;
}

export interface SigmaInstanceHandle {
  on: <E extends string>(
    event: E,
    handler: (payload: { node?: string; edge?: string }) => void,
  ) => () => void;
  refresh: () => void;
  getNodeAttributes: (id: string) => Attributes | null;
}

export interface UseSigmaGraphResult {
  ready: boolean;
  /**
   * True while the FA2 supervisor is iterating. Drives the
   * "Layout optimizing…" pill on the canvas.
   */
  layoutRunning: boolean;
  stopLayout: () => void;
  sigma: SigmaInstanceHandle | null;
}

/** Layout duration scales with node count — bigger graphs need longer to settle. */
function inferRunMs(nodeCount: number): number {
  if (nodeCount > 10_000) return 45_000;
  if (nodeCount > 5_000) return 35_000;
  if (nodeCount > 2_000) return 30_000;
  if (nodeCount > 1_000) return 25_000;
  if (nodeCount > 500) return 20_000;
  return 15_000;
}

/**
 * FA2 settings tuned for cluster spread on hub-heavy code graphs.
 * Higher scalingRatio + low slowDown = fast convergence to a wide
 * layout that doesn't compress folders into the center.
 */
function fa2Settings(nodeCount: number) {
  const isSmall = nodeCount < 500;
  const isMedium = nodeCount < 2_000;
  const isLarge = nodeCount < 10_000;
  return {
    gravity: isSmall ? 0.8 : isMedium ? 0.5 : isLarge ? 0.3 : 0.15,
    scalingRatio: isSmall ? 15 : isMedium ? 30 : isLarge ? 60 : 100,
    slowDown: isSmall ? 1 : isMedium ? 2 : isLarge ? 3 : 5,
    barnesHutOptimize: nodeCount > 200,
    barnesHutTheta: nodeCount > 2_000 ? 0.8 : 0.6,
    strongGravityMode: false,
    outboundAttractionDistribution: true,
    linLogMode: false,
    adjustSizes: true,
    edgeWeightInfluence: 1,
  };
}

const NOVERLAP_SETTINGS = {
  maxIterations: 20,
  settings: {
    ratio: 1.1,
    margin: 10,
    expansion: 1.05,
  },
};

// ── Color helpers for the custom hover paint ─────────────────────────────────

function parseHex(hex: string): { r: number; g: number; b: number } | null {
  const m = /^#?([a-f\d]{2})([a-f\d]{2})([a-f\d]{2})$/i.exec(hex);
  if (!m) return null;
  return {
    r: parseInt(m[1], 16),
    g: parseInt(m[2], 16),
    b: parseInt(m[3], 16),
  };
}

export function useSigmaGraph(
  containerRef: React.RefObject<HTMLDivElement | null>,
  graph: Graph | null,
  reducers?: SigmaReducerHooks,
): UseSigmaGraphResult {
  const sigmaRef = useRef<Sigma | null>(null);
  const supervisorRef = useRef<FA2LayoutSupervisor | null>(null);
  const stopTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

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

    let sigma: Sigma | null = null;
    try {
      sigma = new Sigma(graph, container, {
        renderEdgeLabels: false,
        renderLabels: true,
        defaultEdgeType: "curved",
        edgeProgramClasses: {
          curved: EdgeCurveProgram,
        },
        labelFont: "JetBrains Mono, ui-monospace, monospace",
        labelSize: 11,
        labelWeight: "500",
        labelColor: { color: "#e4e4ed" },
        labelDensity: 0.07,
        labelGridCellSize: 70,
        labelRenderedSizeThreshold: 8,
        defaultNodeColor: "#6b7280",
        defaultEdgeColor: "#2d2d3d",
        minCameraRatio: 0.002,
        maxCameraRatio: 50,
        hideEdgesOnMove: true,
        hideLabelsOnMove: true,
        zIndex: true,
        // Custom hover renderer — dark pill + glow ring matching the
        // node color, so the interaction reads on the near-black canvas
        // instead of bleaching out via Sigma's default white halo.
        defaultDrawNodeHover: (context, data, settings) => {
          const label = data.label as string | undefined;
          const nodeSize = (data.size as number | undefined) ?? 8;
          const nodeColor = (data.color as string | undefined) ?? "#6366f1";

          // Glow ring around the node first — sits underneath the label.
          context.beginPath();
          context.arc(data.x, data.y, nodeSize + 4, 0, Math.PI * 2);
          context.strokeStyle = nodeColor;
          context.lineWidth = 2;
          const rgb = parseHex(nodeColor);
          if (rgb) {
            context.strokeStyle = `rgba(${rgb.r}, ${rgb.g}, ${rgb.b}, 0.55)`;
          }
          context.stroke();

          if (!label) return;

          const size = settings.labelSize || 11;
          const font =
            settings.labelFont || "JetBrains Mono, ui-monospace, monospace";
          const weight = settings.labelWeight || "500";
          context.font = `${weight} ${size}px ${font}`;
          const textWidth = context.measureText(label).width;
          const x = data.x;
          const y = data.y - nodeSize - 12;
          const paddingX = 8;
          const paddingY = 5;
          const height = size + paddingY * 2;
          const width = textWidth + paddingX * 2;
          const radius = 4;

          context.fillStyle = "#12121c";
          context.beginPath();
          context.roundRect(
            x - width / 2,
            y - height / 2,
            width,
            height,
            radius,
          );
          context.fill();

          context.strokeStyle = nodeColor;
          context.lineWidth = 1.5;
          context.stroke();

          context.fillStyle = "#f5f5f7";
          context.textAlign = "center";
          context.textBaseline = "middle";
          context.fillText(label, x, y);
        },
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
      // environment without WebGL stub), bail without throwing — the
      // canvas will be empty rather than crash the page.
      console.warn("[useSigmaGraph] Sigma init failed:", err);
      return;
    }
    sigmaRef.current = sigma;
    setReady(true);

    const sigmaInstance = sigma;
    setHandle({
      on: (event, fn) => {
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
    const inferred = forceAtlas2.inferSettings(graph);
    const tuned = fa2Settings(graph.order);
    const supervisor = new FA2LayoutSupervisor(graph, {
      settings: { ...inferred, ...tuned },
    });
    supervisorRef.current = supervisor;
    supervisor.start();
    setLayoutRunning(true);

    const runMs = inferRunMs(graph.order);
    const timer = setTimeout(() => {
      try {
        supervisor.stop();
      } catch {
        // graceful — supervisor may already be torn down by unmount
      }
      setLayoutRunning(false);
      // Light noverlap pass for the final cleanup.
      try {
        noverlap.assign(graph, NOVERLAP_SETTINGS);
      } catch {
        // jsdom / older graph engines may not support noverlap; the
        // visual gain is incremental, so swallow.
      }
      // Refit camera once the layout has cooled.
      try {
        sigma?.refresh();
        sigma?.getCamera().animatedReset({ duration: 400 });
      } catch {
        // Sigma may already be killed by unmount; ignore.
      }
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

  // Camera-nudge on selection change — Sigma 3 caches edge geometry
  // across frames, so an imperceptible zoom jiggle is the cheapest way
  // to make sure the dim/highlight reducer paints the new state.
  const selectionId = useCodeGraphStore((s) => s.selectionId);
  useEffect(() => {
    const sigma = sigmaRef.current;
    if (!sigma) return;
    try {
      const camera = sigma.getCamera();
      const r = camera.ratio;
      camera.animate({ ratio: r * 1.0001 }, { duration: 50 });
    } catch {
      // unmount race — no-op
    }
  }, [selectionId]);

  const stopLayout = () => {
    if (stopTimerRef.current) {
      clearTimeout(stopTimerRef.current);
      stopTimerRef.current = null;
    }
    if (supervisorRef.current) {
      try {
        supervisorRef.current.stop();
      } catch {
        // already stopped
      }
    }
    if (graph) {
      try {
        noverlap.assign(graph, NOVERLAP_SETTINGS);
      } catch {
        // ignore
      }
    }
    sigmaRef.current?.refresh();
    setLayoutRunning(false);
  };

  return { ready, layoutRunning, stopLayout, sigma: handle };
}
