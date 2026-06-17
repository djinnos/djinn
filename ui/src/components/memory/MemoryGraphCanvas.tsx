/**
 * MemoryGraphCanvas — memory knowledge-graph view: fetch → adapt → render →
 * interact.
 *
 * Mirrors the fetch/adapt/render pattern of `CodeGraphCanvas`:
 *   1. Fetch the `memory_graph` MCP payload for the selected project.
 *   2. Parse via `parseMemoryGraphResponse` and build a graphology graph via
 *      `buildMemoryGraph` (nodes colored by `note_type`, sized by
 *      `connection_count`, orphan override red, broken-target stub edges).
 *   3. Mount Sigma via the existing `useSigmaGraph` hook — no new render stack.
 *   4. Wire `useGraphReducers` for selection + hover layers, then overlay a
 *      memory-specific edge-reducer pass for broken (dashed) and co-access
 *      (cyan, weight-scaled) edges.
 *   5. Co-access toggle (OFF by default): when ON, fetch
 *      `memory_associations` per node and add `kind: "co_access"` edges.
 *
 * The dark radial-gradient background mirrors `CodeGraphCanvas`.
 */

import { useEffect, useMemo, useRef, useState } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  AlertCircleIcon,
  RefreshIcon,
  ConnectIcon,
} from "@hugeicons/core-free-icons";

import {
  buildMemoryGraph,
  parseMemoryGraphResponse,
} from "@/lib/memoryGraphAdapter";
import { useSigmaGraph } from "@/hooks/useSigmaGraph";
import { useGraphReducers } from "@/hooks/useGraphReducers";
import { useMemoryGraphStore } from "@/stores/memoryGraphStore";
import { useCodeGraphStore } from "@/stores/codeGraphStore";
import { callMcpTool } from "@/api/mcpClient";
import type { MemoryAssociationsOutputSchema } from "@/api/generated/mcp-tools.gen";
import type Graph from "graphology";
import { cn } from "@/lib/utils";

/** Memory association entry shape (nested under the output schema namespace). */
type MemoryAssociationEntry = MemoryAssociationsOutputSchema.MemoryAssociationEntry;

// ── Types ───────────────────────────────────────────────────────────────────

type FetchState =
  | { status: "loading" }
  | { status: "error"; error: string }
  | { status: "ready"; graph: Graph }
  | { status: "empty" };

/**
 * Co-access results keyed by the source node id. The `memory_associations`
 * endpoint returns neighbors of the queried note; we store entries keyed by
 * source so the graph-build step can draw `source → note_permalink` edges.
 */
type CoAccessMap = Map<string, MemoryAssociationEntry[]>;

interface MemoryGraphCanvasProps {
  /** Project slug in `owner/repo` form (same as `MemoryPage` uses for MCP calls). */
  projectSlug: string;
  /** Bumping this re-issues the graph fetch without unmounting. */
  reloadKey?: number;
  /**
   * Called when the user clicks a node. `MemoryPage` wires this to its
   * `handleSelectNote` so the existing detail panel opens — the canvas does
   * NOT duplicate the detail view.
   */
  onSelectNote?: (permalink: string) => void;
}

// ── Constants ───────────────────────────────────────────────────────────────

const CANVAS_BACKGROUND = `radial-gradient(circle at 50% 50%, rgba(124, 58, 237, 0.05) 0%, transparent 70%), linear-gradient(to bottom, #06060a, #0a0a10)`;

/** Edge styling for the memory-specific edge kinds. */
const BROKEN_EDGE_COLOR = "#f59e0b"; // amber-500
const BROKEN_EDGE_SIZE = 1.5;
const CO_ACCESS_EDGE_COLOR = "#22d3ee"; // cyan-400

/**
 * Weight-scaled edge size for co-access edges:
 * `clamp(0.5, 4, 0.5 + weight * 4)`.
 */
function coAccessEdgeSize(weight: number): number {
  return Math.max(0.5, Math.min(4, 0.5 + weight * 4));
}

/** Concurrency cap for the N+1 `memory_associations` fan-out. */
const CO_ACCESS_CONCURRENCY = 8;
/** Hard cap: skip co-access fetch above this node count (TODO for batched endpoint). */
const CO_ACCESS_NODE_CAP = 500;

// ── Component ───────────────────────────────────────────────────────────────

export function MemoryGraphCanvas({
  projectSlug,
  reloadKey,
  onSelectNote,
}: MemoryGraphCanvasProps) {
  const [state, setState] = useState<FetchState>({ status: "loading" });
  const [coAccessMap, setCoAccessMap] = useState<CoAccessMap | null>(null);
  const [coAccessLoading, setCoAccessLoading] = useState(false);
  const containerRef = useRef<HTMLDivElement | null>(null);

  const coAccessEnabled = useMemoryGraphStore((s) => s.coAccessEnabled);
  const setCoAccessEnabled = useMemoryGraphStore((s) => s.setCoAccessEnabled);

  // ── Fetch memory_graph on project / reload change ──────────────────────────
  useEffect(() => {
    let cancelled = false;
    setState({ status: "loading" });
    setCoAccessMap(null);

    (async () => {
      try {
        const raw = await callMcpTool("memory_graph", {
          project: projectSlug,
        });
        if (cancelled) return;
        const payload = parseMemoryGraphResponse(raw);
        if (!payload || payload.nodes.length === 0) {
          setState({ status: "empty" });
          return;
        }
        const graph = buildMemoryGraph(payload);
        if (cancelled) return;
        if (graph.order === 0) {
          setState({ status: "empty" });
          return;
        }
        setState({ status: "ready", graph });
      } catch (err) {
        if (cancelled) return;
        setState({
          status: "error",
          error: err instanceof Error ? err.message : String(err),
        });
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [projectSlug, reloadKey]);

  // ── Co-access layer: fetch per-node associations when toggle is ON ─────────
  const baseGraph = state.status === "ready" ? state.graph : null;

  useEffect(() => {
    if (!coAccessEnabled || !baseGraph) {
      setCoAccessMap(null);
      return;
    }

    // TODO(2chl): batched associations endpoint for >500-node projects
    if (baseGraph.order > CO_ACCESS_NODE_CAP) {
      console.warn(
        `[MemoryGraphCanvas] Co-access layer skipped: ${baseGraph.order} nodes exceeds cap of ${CO_ACCESS_NODE_CAP}.`,
      );
      setCoAccessMap(null);
      return;
    }

    let cancelled = false;
    setCoAccessLoading(true);

    const nodeIds = baseGraph.nodes();
    const resultMap: CoAccessMap = new Map();

    // Throttle N+1 calls with a small concurrency cap.
    const run = async () => {
      let cursor = 0;
      const worker = async () => {
        while (cursor < nodeIds.length) {
          const nodeId = nodeIds[cursor++];
          const attrs = baseGraph.getNodeAttributes(nodeId);
          const identifier =
            (attrs.permalink as string | undefined) ?? nodeId;
          try {
            const result = await callMcpTool("memory_associations", {
              project: projectSlug,
              identifier,
              min_weight: 0.0,
              limit: 20,
            });
            if (cancelled) return;
            if (Array.isArray(result.associations) && result.associations.length > 0) {
              resultMap.set(nodeId, result.associations);
            }
          } catch {
            // Per-node failure is non-fatal — keep going.
          }
        }
      };
      const workers = Array.from(
        { length: Math.min(CO_ACCESS_CONCURRENCY, nodeIds.length) },
        () => worker(),
      );
      await Promise.all(workers);
      if (!cancelled) {
        setCoAccessMap(resultMap);
      }
    };
    run().finally(() => {
      if (!cancelled) setCoAccessLoading(false);
    });

    return () => {
      cancelled = true;
    };
  }, [coAccessEnabled, baseGraph, projectSlug]);

  // ── Build the renderable graph (base + optional co-access edges) ───────────
  const graph = useMemo(() => {
    if (!baseGraph) return null;
    if (!coAccessMap || coAccessMap.size === 0) return baseGraph;

    // Clone the base graph and add co-access edges on top. Cloning keeps
    // `baseGraph` pristine so toggling off cleanly drops the layer.
    const clone = baseGraph.copy();
    // Build a permalink → node-id index for resolving association targets.
    const permalinkToId = new Map<string, string>();
    clone.forEachNode((id, attrs) => {
      const p = attrs.permalink as string | undefined;
      if (p) permalinkToId.set(p, id);
    });
    for (const [sourceId, entries] of coAccessMap) {
      for (const entry of entries) {
        const targetId = permalinkToId.get(entry.note_permalink);
        if (!targetId || targetId === sourceId) continue;
        if (!clone.hasEdge(sourceId, targetId)) {
          clone.addEdge(sourceId, targetId, {
            color: CO_ACCESS_EDGE_COLOR,
            size: coAccessEdgeSize(entry.weight),
            kind: "co_access",
            weight: entry.weight,
          });
        }
      }
    }
    return clone;
  }, [baseGraph, coAccessMap]);

  // ── Reducer wiring (selection + hover via useGraphReducers) ────────────────
  // The reducers hook needs the live Sigma handle to call refresh() when store
  // slices change. We init null and lift the handle from the second pass of
  // useSigmaGraph (same two-pass pattern as CodeGraphCanvas).
  const [sigmaHandle, setSigmaHandle] = useState<
    ReturnType<typeof useSigmaGraph>["sigma"]
  >(null);
  const { reducers: baseReducers } = useGraphReducers(graph, sigmaHandle);

  // Overlay memory-specific edge styling on top of the code-graph reducers.
  const reducers = useMemo(() => {
    if (!graph) return baseReducers;
    return {
      nodeReducer: baseReducers.nodeReducer,
      edgeReducer: (id: string, attrs: Record<string, unknown>): Record<string, unknown> => {
        // Memory edges carry a `kind` attribute: "wikilink" (default),
        // "broken" (stub from broken_targets), or "co_access" (added by
        // the canvas toggle). The adapter's `MemoryGraphEdgeKind` only
        // covers the first two; co_access is a canvas-only extension.
        const kind = attrs.kind as string | undefined;
        if (kind === "broken") {
          return {
            ...attrs,
            dashed: true,
            color: BROKEN_EDGE_COLOR,
            size: BROKEN_EDGE_SIZE,
            label: attrs.raw_text ?? "missing",
          };
        }
        if (kind === "co_access") {
          const weight =
            typeof attrs.weight === "number" ? attrs.weight : 0;
          return {
            ...attrs,
            color: CO_ACCESS_EDGE_COLOR,
            size: coAccessEdgeSize(weight),
          };
        }
        // Delegate to the base code-graph edge reducer for selection/dim logic.
        return baseReducers.edgeReducer
          ? baseReducers.edgeReducer(id, attrs)
          : attrs;
      },
    };
  }, [baseReducers, graph]);

  const { layoutRunning, sigma } = useSigmaGraph(
    containerRef,
    graph,
    reducers,
  );

  useEffect(() => {
    setSigmaHandle(sigma);
  }, [sigma]);

  // ── Click / hover event wiring ─────────────────────────────────────────────
  // `useGraphReducers` reads selection/hover from `codeGraphStore`, so we drive
  // the selection/hover layers through that store. `memoryGraphStore` owns only
  // the co-access toggle (canvas-local state that doesn't belong to the shared
  // highlight pipeline).
  const setSelection = useCodeGraphStore((s) => s.setSelection);
  const setHover = useCodeGraphStore((s) => s.setHover);
  const resetHighlights = useCodeGraphStore((s) => s.reset);

  useEffect(() => {
    if (!sigma) return;
    const offClick = sigma.on("clickNode", ({ node }) => {
      if (!node) return;
      setSelection(node);
      if (onSelectNote && graph) {
        const attrs = sigma.getNodeAttributes(node);
        const permalink = attrs?.permalink as string | undefined;
        if (permalink) onSelectNote(permalink);
      }
    });
    const offStage = sigma.on("clickStage", () => {
      setSelection(null);
    });
    const offEnter = sigma.on("enterNode", ({ node }) => {
      if (node) setHover(node);
      const c = containerRef.current;
      if (c) c.style.cursor = "pointer";
    });
    const offLeave = sigma.on("leaveNode", () => {
      setHover(null);
      const c = containerRef.current;
      if (c) c.style.cursor = "grab";
    });
    return () => {
      offClick();
      offStage();
      offEnter();
      offLeave();
    };
  }, [sigma, setSelection, setHover, onSelectNote, graph]);

  // Reset selection/hover when project changes.
  useEffect(() => {
    resetHighlights();
    return () => resetHighlights();
  }, [projectSlug, resetHighlights]);

  return (
    <div
      className="relative h-full min-h-0 w-full"
      style={{ background: CANVAS_BACKGROUND }}
    >
      <div
        ref={containerRef}
        data-testid="memory-graph-canvas"
        className="absolute inset-0"
        style={{ cursor: "grab" }}
      />
      <CoAccessToggle
        enabled={coAccessEnabled}
        loading={coAccessLoading}
        onChange={setCoAccessEnabled}
        disabled={state.status !== "ready"}
      />
      <MemoryGraphOverlay state={state} layoutRunning={layoutRunning} />
    </div>
  );
}

// ── Co-access toggle ─────────────────────────────────────────────────────────

function CoAccessToggle({
  enabled,
  loading,
  onChange,
  disabled,
}: {
  enabled: boolean;
  loading: boolean;
  onChange: (enabled: boolean) => void;
  disabled: boolean;
}) {
  return (
    <div className="absolute right-3 top-3 z-10">
      <label
        className={cn(
          "flex items-center gap-2 rounded-lg border border-[#2d2d3d] bg-black/50 px-3 py-1.5 text-[11px] text-zinc-300 backdrop-blur",
          disabled && "cursor-not-allowed opacity-50",
        )}
      >
        <input
          type="checkbox"
          checked={enabled}
          disabled={disabled}
          onChange={(e) => onChange(e.target.checked)}
          className="h-3 w-3 accent-cyan-400"
          aria-label="Toggle co-access edges"
        />
        {loading ? (
          <span className="flex items-center gap-1.5">
            <span className="h-2 w-2 animate-spin rounded-full border border-cyan-400 border-t-transparent" />
            Loading co-access…
          </span>
        ) : (
          <span>Co-access edges</span>
        )}
      </label>
    </div>
  );
}

// ── Overlay (loading / error / empty / stats) ────────────────────────────────

interface MemoryGraphOverlayProps {
  state: FetchState;
  layoutRunning: boolean;
}

function MemoryGraphOverlay({ state, layoutRunning }: MemoryGraphOverlayProps) {
  if (state.status === "loading") {
    return (
      <CenterCard>
        <SpinningIcon />
        <p className="mt-3 text-sm text-zinc-400">Loading memory graph…</p>
      </CenterCard>
    );
  }
  if (state.status === "error") {
    return (
      <CenterCard>
        <span className="mx-auto flex h-10 w-10 items-center justify-center rounded-full bg-red-500/15 text-red-400">
          <HugeiconsIcon icon={AlertCircleIcon} className="h-5 w-5" />
        </span>
        <p className="mt-3 text-sm font-medium text-zinc-200">
          Couldn&apos;t load the graph
        </p>
        <p className="mt-1 max-w-sm text-xs text-zinc-400">{state.error}</p>
      </CenterCard>
    );
  }
  if (state.status === "empty") {
    return (
      <CenterCard>
        <span className="mx-auto flex h-10 w-10 items-center justify-center rounded-full bg-zinc-800/60 text-zinc-400">
          <HugeiconsIcon icon={ConnectIcon} className="h-5 w-5" />
        </span>
        <p className="mt-3 text-sm text-zinc-400">
          No notes yet — this project has no memory notes to graph.
        </p>
      </CenterCard>
    );
  }

  return (
    <>
      <div className="pointer-events-none absolute left-3 top-3 flex flex-col gap-1.5">
        <Pill>{state.graph.order.toLocaleString()} notes</Pill>
        <Pill>{state.graph.size.toLocaleString()} edges</Pill>
      </div>
      {layoutRunning ? <LayoutOptimizingPill /> : null}
    </>
  );
}

function CenterCard({ children }: { children: React.ReactNode }) {
  return (
    <div className="pointer-events-none absolute inset-0 flex items-center justify-center">
      <div className="max-w-sm rounded-lg border border-[#2d2d3d] bg-[#0a0a10]/85 px-5 py-4 text-center backdrop-blur">
        {children}
      </div>
    </div>
  );
}

function Pill({
  children,
  className,
  ...rest
}: React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      className={cn(
        "inline-flex items-center gap-1 rounded-full border border-[#2d2d3d] bg-black/40 px-2.5 py-0.5 text-[11px] font-medium text-zinc-300 backdrop-blur",
        className,
      )}
      {...rest}
    >
      {children}
    </div>
  );
}

function SpinningIcon() {
  return (
    <span className="mx-auto flex h-10 w-10 items-center justify-center rounded-full bg-zinc-800/60 text-zinc-400">
      <HugeiconsIcon
        icon={RefreshIcon}
        className="h-5 w-5 animate-spin [animation-duration:2s]"
      />
    </span>
  );
}

function LayoutOptimizingPill() {
  return (
    <div className="pointer-events-none absolute bottom-4 left-1/2 flex -translate-x-1/2 items-center gap-2 rounded-full border border-emerald-500/30 bg-emerald-500/20 px-3 py-1.5 backdrop-blur">
      <span className="relative flex h-2 w-2">
        <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-emerald-400 opacity-75" />
        <span className="relative inline-flex h-2 w-2 rounded-full bg-emerald-400" />
      </span>
      <span className="text-xs font-medium text-emerald-300">
        Layout optimizing…
      </span>
    </div>
  );
}
