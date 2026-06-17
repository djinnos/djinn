/**
 * MemoryGraphCanvas — memory knowledge-graph view: fetch → build → cluster →
 * render → interact.
 *
 * Mirrors the fetch/adapt/render pattern of `CodeGraphCanvas`, but for the
 * memory wikilink graph:
 *   1. Fetch the `memory_graph` MCP payload for the selected project.
 *   2. Build a graphology graph and run Louvain community detection, writing
 *      `communityId` / `communityLabel` onto clustered nodes.
 *   3. Mount Sigma via `useSigmaGraph`, passing a `postLayout` callback that
 *      runs the per-community attraction pass AFTER ForceAtlas2 settles and
 *      BEFORE the final noverlap / camera reset. Singleton/unclustered notes
 *      are never attraction-eligible — `applyPerCommunityAttraction` skips
 *      nodes without a non-empty `communityId`.
 *
 * The nodeReducer colors clustered notes by community id (reusing the code
 * graph's community palette) and leaves unclustered notes at the default
 * slate color so the two populations are visually distinct.
 */

import { useEffect, useMemo, useRef, useState } from "react";

import { fetchMemoryGraph, buildClusteredMemoryGraph } from "@/lib/memoryGraphAdapter";
import { applyPerCommunityAttraction } from "@/lib/perCommunityAttraction";
import { colorForCommunity } from "@/lib/codeGraphAdapter";
import { useSigmaGraph, type SigmaReducerHooks } from "@/hooks/useSigmaGraph";

type ClusteredMemoryGraph = ReturnType<typeof buildClusteredMemoryGraph>;

type FetchState =
  | { status: "loading" }
  | { status: "error"; error: string }
  | { status: "ready"; graph: ClusteredMemoryGraph["graph"]; communities: ClusteredMemoryGraph["communities"] }
  | { status: "empty" };

interface MemoryGraphCanvasProps {
  /** Project slug in `owner/repo` form (same as `MemoryPage` uses for MCP calls). */
  projectSlug: string;
}

const CANVAS_BACKGROUND = `radial-gradient(circle at 50% 50%, rgba(124, 58, 237, 0.05) 0%, transparent 70%), linear-gradient(to bottom, #06060a, #0a0a10)`;

export function MemoryGraphCanvas({ projectSlug }: MemoryGraphCanvasProps) {
  const [state, setState] = useState<FetchState>({ status: "loading" });
  const containerRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    let cancelled = false;

    (async () => {
      try {
        const payload = await fetchMemoryGraph(projectSlug);
        if (cancelled) return;
        if (!payload) {
          setState({ status: "empty" });
          return;
        }
        const { graph, communities } = buildClusteredMemoryGraph(payload);
        if (cancelled) return;
        if (graph.order === 0) {
          setState({ status: "empty" });
          return;
        }
        setState({ status: "ready", graph, communities });
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
  }, [projectSlug]);

  const graph = useMemo(
    () => (state.status === "ready" ? state.graph : null),
    [state],
  );
  const communities = useMemo(
    () => (state.status === "ready" ? state.communities : null),
    [state],
  );

  // Color clustered nodes by community id; unclustered notes keep their
  // default slate color. Memoized so the reducer identity is stable across
  // renders unless the graph changes.
  const reducers = useMemo<SigmaReducerHooks | undefined>(() => {
    if (!graph) return undefined;
    return {
      nodeReducer: (_id, attrs) => {
        const communityId = attrs.communityId as string | undefined;
        if (!communityId) return attrs;
        return { ...attrs, color: colorForCommunity(communityId) };
      },
    };
  }, [graph]);

  // Per-community attraction runs after FA2 settles and before noverlap /
  // camera reset. The `communities` map is the same one `clusterMemoryCommunities`
  // produced when building the graph, so it carries exactly the attraction-
  // eligible nodes (singletons / unclustered notes are absent). Graphs without
  // clustered communities (single note, no edges) produce an empty map and the
  // attraction pass is a documented no-op — `applyPerCommunityAttraction`
  // early-returns when no community has ≥ minCommunitySize members.
  const postLayout = useMemo(() => {
    if (!graph || !communities) return undefined;
    const metadata = communities;
    return (g: ClusteredMemoryGraph["graph"]) => {
      applyPerCommunityAttraction(g, metadata, {
        clusterRadius: 400,
        strength: 0.1,
      });
    };
  }, [graph, communities]);

  const { layoutRunning } = useSigmaGraph(
    containerRef,
    graph,
    reducers,
    postLayout ? { postLayout } : undefined,
  );

  return (
    <div className="relative h-full min-h-0 w-full" style={{ background: CANVAS_BACKGROUND }}>
      <div
        ref={containerRef}
        data-testid="memory-graph-canvas"
        className="absolute inset-0"
        style={{ cursor: "grab" }}
      />
      <MemoryGraphOverlay state={state} layoutRunning={layoutRunning} />
    </div>
  );
}

interface MemoryGraphOverlayProps {
  state: FetchState;
  layoutRunning: boolean;
}

function MemoryGraphOverlay({ state, layoutRunning }: MemoryGraphOverlayProps) {
  if (state.status === "loading") {
    return (
      <div className="pointer-events-none absolute inset-0 flex items-center justify-center">
        <div className="max-w-sm rounded-lg border border-[#2d2d3d] bg-[#0a0a10]/85 px-5 py-4 text-center backdrop-blur">
          <div className="mx-auto h-5 w-5 animate-spin rounded-full border-2 border-zinc-500 border-t-transparent" />
          <p className="mt-3 text-sm text-zinc-400">Loading memory graph…</p>
        </div>
      </div>
    );
  }
  if (state.status === "error") {
    return (
      <div className="pointer-events-none absolute inset-0 flex items-center justify-center">
        <div className="max-w-sm rounded-lg border border-[#2d2d3d] bg-[#0a0a10]/85 px-5 py-4 text-center backdrop-blur">
          <p className="text-sm font-medium text-zinc-200">Couldn&apos;t load the graph</p>
          <p className="mt-1 text-xs text-zinc-400">{state.error}</p>
        </div>
      </div>
    );
  }
  if (state.status === "empty") {
    return (
      <div className="pointer-events-none absolute inset-0 flex items-center justify-center">
        <div className="max-w-sm rounded-lg border border-[#2d2d3d] bg-[#0a0a10]/85 px-5 py-4 text-center backdrop-blur">
          <p className="text-sm text-zinc-400">
            No graph data yet — notes need wikilinks between them to form a graph.
          </p>
        </div>
      </div>
    );
  }
  return (
    <>
      <div className="pointer-events-none absolute left-3 top-3 flex flex-col gap-1.5">
        <div className="inline-flex items-center gap-1 rounded-full border border-[#2d2d3d] bg-black/40 px-2.5 py-0.5 text-[11px] font-medium text-zinc-300 backdrop-blur">
          {state.graph.order} notes
        </div>
      </div>
      {layoutRunning ? (
        <div className="pointer-events-none absolute bottom-4 left-1/2 flex -translate-x-1/2 items-center gap-2 rounded-full border border-emerald-500/30 bg-emerald-500/20 px-3 py-1.5 backdrop-blur">
          <span className="relative flex h-2 w-2">
            <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-emerald-400 opacity-75" />
            <span className="relative inline-flex h-2 w-2 rounded-full bg-emerald-400" />
          </span>
          <span className="text-xs font-medium text-emerald-300">Layout optimizing…</span>
        </div>
      ) : null}
    </>
  );
}
