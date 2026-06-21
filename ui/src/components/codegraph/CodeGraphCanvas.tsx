/* eslint-disable react-refresh/only-export-components -- LARGE_GRAPH_THRESHOLD and shouldFallbackToCommunity are exported for focused unit tests. */
/**
 * CodeGraphCanvas — main view: fetch → adapt → render → interact.
 *
 * Owns the round-trip from project id to a fully-laid-out Sigma canvas.
 * State machine has three terminal states (loading / error / ready)
 * plus an empty-graph fallback for projects that haven't been warmed
 * yet.
 *
 * Highlight layers (selection / hover / citation / blast-radius)
 * compose via the Zustand `codeGraphStore` and the `useGraphReducers`
 * hook — Sigma's `nodeReducer` / `edgeReducer` callbacks read the
 * latest view on every frame, so toggles are flicker-free without
 * re-mounting.
 *
 * The dark radial-gradient background and bottom-center "Layout
 * optimizing…" pill mirror the GitNexus aesthetic the page is
 * matching.
 */

import { useEffect, useMemo, useRef, useState } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  ConnectIcon,
  AlertCircleIcon,
  RefreshIcon,
} from "@hugeicons/core-free-icons";

import { fetchSnapshot, type SnapshotLevel } from "@/api/codeGraph";
import {
  buildGraphFromSnapshot,
  filterSnapshotForWorkspace,
  isDoubleClick,
  parseSnapshotResponse,
  type SnapshotPayload,
} from "@/lib/codeGraphAdapter";
import { useSigmaGraph } from "@/hooks/useSigmaGraph";
import { useGraphReducers } from "@/hooks/useGraphReducers";
import { selectCitationIds, useCodeGraphStore } from "@/stores/codeGraphStore";
import { RendererCapabilityDialog } from "./RendererCapabilityDialog";
import { cn } from "@/lib/utils";

type FetchState =
  | { status: "loading" }
  | { status: "error"; error: string }
  | {
      status: "ready";
      snapshot: SnapshotPayload;
      /** Semantic-zoom level that produced this snapshot. */
      level: SnapshotLevel;
    };

/**
 * Auto-semantic-zoom threshold. When `semanticZoomMode === "auto"`, the
 * canvas starts with a symbol-level fetch; if the snapshot reports
 * `truncated === true`, `total_nodes >= LARGE_GRAPH_THRESHOLD`, or
 * `total_nodes > nodeCap`, it refetches the same project at
 * `level="community"` so the user sees a handful of legible blobs
 * instead of a truncated 10k-node graph. The value sits just below the
 * server's default cap (10,000) so the collapse happens precisely when
 * the user would otherwise hit the cap.
 */
export const LARGE_GRAPH_THRESHOLD = 8_000;

/**
 * Decide whether an initial symbol-level snapshot warrants a community
 * fallback. Kept as a pure exported helper so focused tests can pin the
 * boundary without rendering the canvas.
 */
export function shouldFallbackToCommunity(
  snapshot: SnapshotPayload,
  nodeCap: number,
  threshold: number = LARGE_GRAPH_THRESHOLD,
): boolean {
  return (
    snapshot.truncated ||
    snapshot.total_nodes >= threshold ||
    snapshot.total_nodes > nodeCap
  );
}

interface CodeGraphCanvasProps {
  projectId: string;
  /**
   * Maximum number of nodes to fetch. Default 10,000 — the server's
   * own clamp ceiling so we render the full repo on every project that
   * fits under it. Reference: GitNexus comfortably renders 6.5k nodes
   * with Sigma + WebGL; Sigma 3 starts to slow at ~5k *with all edges
   * shown*, but the toolbar defaults already strip
   * Contains/Declared/FileRef/Reads/Calls/MemberOf so the live edge count
   * is one-third of `total_edges`. Drop this for tests, raise it once
   * the server clamp ceiling moves.
   */
  nodeCap?: number;
  /** Bumping this re-issues the snapshot fetch without unmounting. */
  reloadKey?: number;
}

const DEFAULT_NODE_CAP = 10_000;

/** DOI focus-anchor camera animation duration. */
const FOCUS_ANCHOR_DURATION_MS = 150;

const CANVAS_BACKGROUND = `radial-gradient(circle at 50% 50%, rgba(124, 58, 237, 0.05) 0%, transparent 70%), linear-gradient(to bottom, #06060a, #0a0a10)`;

export function CodeGraphCanvas({
  projectId,
  nodeCap = DEFAULT_NODE_CAP,
  reloadKey,
}: CodeGraphCanvasProps) {
  const [state, setState] = useState<FetchState>({ status: "loading" });
  const containerRef = useRef<HTMLDivElement | null>(null);
  const selectedWorkspaceSlug = useCodeGraphStore(
    (s) => s.selectedWorkspaceSlug,
  );
  const semanticZoomMode = useCodeGraphStore((s) => s.semanticZoomMode);
  const setGraphReady = useCodeGraphStore((s) => s.setGraphReady);

  useEffect(() => {
    let cancelled = false;
    setState({ status: "loading" });
    setGraphReady(false);

    // Pick the *initial* fetch level from the toolbar mode.
    //   - "community" → always start at community level
    //   - "symbol"    → always start at symbol level
    //   - "auto"      → start at symbol level, then conditionally
    //                   refetch at community level when the snapshot
    //                   is truncated or over the large-graph threshold.
    const initialLevel: SnapshotLevel =
      semanticZoomMode === "community" ? "community" : "symbol";

    (async () => {
      try {
        const raw = await fetchSnapshot(projectId, nodeCap, initialLevel);
        if (cancelled) return;
        let snapshot = parseSnapshotResponse(raw);
        if (!snapshot) {
          setState({
            status: "error",
            error:
              "Snapshot response was empty or malformed. The graph may not be warmed yet — try again in a minute.",
          });
          setGraphReady(false);
          return;
        }
        let level = initialLevel;

        // Auto mode: if the symbol snapshot was truncated / capped,
        // refetch at community level so the user sees legible blobs
        // rather than an incomplete 10k-node graph. Only symbol-starts
        // can fall back; forced modes never change level.
        if (
          semanticZoomMode === "auto" &&
          level === "symbol" &&
          shouldFallbackToCommunity(snapshot, nodeCap)
        ) {
          const communityRaw = await fetchSnapshot(
            projectId,
            nodeCap,
            "community",
          );
          if (cancelled) return;
          const communitySnapshot = parseSnapshotResponse(communityRaw);
          if (communitySnapshot) {
            snapshot = communitySnapshot;
            level = "community";
          }
        }

        if (cancelled) return;
        setState({ status: "ready", snapshot, level });
        setGraphReady(snapshot.nodes.length > 0);
      } catch (err) {
        if (cancelled) return;
        setState({
          status: "error",
          error: err instanceof Error ? err.message : String(err),
        });
        setGraphReady(false);
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [projectId, nodeCap, reloadKey, semanticZoomMode, setGraphReady]);

  const effectiveLevel: SnapshotLevel =
    state.status === "ready" ? state.level : "symbol";

  const visibleSnapshot = useMemo(() => {
    if (state.status !== "ready") return null;
    return filterSnapshotForWorkspace(
      state.snapshot,
      selectedWorkspaceSlug,
    );
  }, [state, selectedWorkspaceSlug]);

  const graph = useMemo(() => {
    if (!visibleSnapshot) return null;
    return buildGraphFromSnapshot(visibleSnapshot);
  }, [visibleSnapshot]);

  // The reducers hook needs the live Sigma handle to call refresh()
  // when store slices change. We init `null` and lift the handle from
  // the second pass of `useSigmaGraph`.
  const [sigmaHandle, setSigmaHandle] =
    useState<ReturnType<typeof useSigmaGraph>["sigma"]>(null);
  const { reducers, complexityThresholds } = useGraphReducers(
    graph,
    sigmaHandle,
  );

  // Iter 30: report complexity availability up to the toolbar via the
  // store. Drives the heatmap-toggle's disabled/tooltip state. Set
  // false on unmount so the toolbar resets when the user switches
  // projects mid-flight.
  const setComplexityAvailable = useCodeGraphStore(
    (s) => s.setComplexityAvailable,
  );
  useEffect(() => {
    setComplexityAvailable(complexityThresholds !== null);
    return () => setComplexityAvailable(false);
  }, [complexityThresholds, setComplexityAvailable]);

  const { ready, layoutRunning, sigma } = useSigmaGraph(
    containerRef,
    graph,
    reducers,
  );
  useAutoFocusOnCitations({ ready, layoutRunning, sigma });

  useEffect(() => {
    setSigmaHandle(sigma);
  }, [sigma]);

  const setSelection = useCodeGraphStore((s) => s.setSelection);
  const setHover = useCodeGraphStore((s) => s.setHover);
  const setFocusAnchor = useCodeGraphStore((s) => s.setFocusAnchor);
  const clearFocusAnchor = useCodeGraphStore((s) => s.clearFocusAnchor);

  useEffect(() => {
    if (!sigma) return;
    // Communities are background hulls, not clickable nodes (see
    // [[n2a1-roadmap]] wave 1). The click handler now only handles
    // single-click selection + DOI focus anchoring. The double-click
    // guard is retained so future progressive-expansion behavior
    // (sibling task b5vu) can reuse the same timestamp/last-node
    // detector, but community expand/collapse is no longer wired here.
    let lastClick: { nodeId: string; at: number } | null = null;

    const offClick = sigma.on("clickNode", ({ node }) => {
      if (!node) return;
      const now = Date.now();
      const isDouble = isDoubleClick(lastClick, node, now);
      lastClick = { nodeId: node, at: now };

      if (isDouble) {
        // Reset the guard so a triple-click doesn't look like two
        // separate double-clicks. No community expand/collapse action
        // — communities are non-clickable hulls now.
        lastClick = null;
        return;
      }

      // Normal single-click: set selection (detail panel) and DOI
      // focus anchor (directional traversal), then recenter camera.
      setSelection(node);
      setFocusAnchor(node);
      sigma.focusNode(node, FOCUS_ANCHOR_DURATION_MS);
    });
    const offStage = sigma.on("clickStage", () => {
      setSelection(null);
      clearFocusAnchor();
      lastClick = null;
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
  }, [
    sigma,
    setSelection,
    setHover,
    setFocusAnchor,
    clearFocusAnchor,
  ]);

  const resetHighlights = useCodeGraphStore((s) => s.reset);
  useEffect(() => {
    resetHighlights();
    return () => resetHighlights();
  }, [projectId, resetHighlights]);

  return (
    <div className="absolute inset-0" style={{ background: CANVAS_BACKGROUND }}>
      <RendererCapabilityDialog />
      <div
        ref={containerRef}
        data-testid="code-graph-canvas"
        className="absolute inset-0"
        style={{ cursor: "grab" }}
      />
      <CanvasOverlay
        state={state}
        visibleSnapshot={visibleSnapshot}
        level={effectiveLevel}
      />
      {layoutRunning && visibleSnapshot?.nodes.length ? (
        <LayoutOptimizingPill />
      ) : null}
      <CitationStatusBadge />
      <ComplexityLegend thresholds={complexityThresholds} />
    </div>
  );
}

function useAutoFocusOnCitations({
  ready,
  layoutRunning,
  sigma,
}: Pick<
  ReturnType<typeof useSigmaGraph>,
  "ready" | "layoutRunning" | "sigma"
>) {
  const citationIds = useCodeGraphStore(selectCitationIds);

  useEffect(() => {
    if (!ready || layoutRunning || !sigma) return;
    sigma.focusNodes(citationIds);
  }, [citationIds, layoutRunning, ready, sigma]);
}

/**
 * Iter 30: bottom-right gradient legend for the complexity heatmap.
 * Visible only in `colorMode === "complexity"` and when thresholds
 * are populated; the four-color ramp + percentile labels mirror the
 * `colorForComplexity` bucketing in `codeGraphReducers.ts`.
 */
function ComplexityLegend({
  thresholds,
}: {
  thresholds: {
    p33: number;
    p67: number;
    p90: number;
    sampleSize: number;
  } | null;
}) {
  const colorMode = useCodeGraphStore((s) => s.colorMode);
  if (colorMode !== "complexity" || !thresholds) return null;
  const fmt = (n: number) => (Number.isInteger(n) ? `${n}` : n.toFixed(1));
  return (
    <div
      data-testid="complexity-legend"
      className="pointer-events-none absolute bottom-3 right-3 flex flex-col gap-1 rounded-lg border border-[#2d2d3d] bg-black/60 px-3 py-2 text-[10px] text-zinc-200 shadow backdrop-blur"
    >
      <div className="text-[9px] font-medium uppercase tracking-wide text-zinc-400">
        Cognitive complexity
      </div>
      <LegendRow color="#10b981" label={`≤ p33 (${fmt(thresholds.p33)})`} />
      <LegendRow color="#eab308" label={`≤ p67 (${fmt(thresholds.p67)})`} />
      <LegendRow color="#f97316" label={`≤ p90 (${fmt(thresholds.p90)})`} />
      <LegendRow color="#ef4444" label={`> p90`} />
      <LegendRow color="#6b7280" label="non-function" />
      <div className="mt-1 text-[9px] text-zinc-500">
        n={thresholds.sampleSize.toLocaleString()} fns
      </div>
    </div>
  );
}

function LegendRow({ color, label }: { color: string; label: string }) {
  return (
    <div className="flex items-center gap-1.5">
      <span
        aria-hidden
        className="inline-block h-2.5 w-2.5 rounded-sm"
        style={{ backgroundColor: color }}
      />
      <span className="tabular-nums">{label}</span>
    </div>
  );
}

function LayoutOptimizingPill() {
  return (
    <div
      data-testid="layout-running"
      className="pointer-events-none absolute bottom-4 left-1/2 flex -translate-x-1/2 items-center gap-2 rounded-full border border-emerald-500/30 bg-emerald-500/20 px-3 py-1.5 backdrop-blur"
    >
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

export function CitationStatusBadge() {
  const selectionId = useCodeGraphStore((s) => s.selectionId);
  const citationCount = useCodeGraphStore((s) => s.citationIds.size);
  const clear = useCodeGraphStore((s) => s.clearCitations);
  if (!selectionId && citationCount === 0) return null;
  return (
    <div className="pointer-events-auto absolute right-3 top-3 flex items-center gap-1.5 rounded-full border border-blue-400/40 bg-blue-500/15 px-3 py-1 text-[11px] text-blue-200 shadow-sm backdrop-blur">
      <span className="h-1.5 w-1.5 animate-pulse rounded-full bg-blue-300" />
      <span data-testid="citation-status">
        {citationCount > 1
          ? `${citationCount} citations pinned`
          : `Pinned: ${selectionId ?? ""}`}
      </span>
      <button
        type="button"
        onClick={clear}
        className="ml-1 text-blue-300/80 hover:text-blue-100"
        aria-label="Clear citations"
      >
        ×
      </button>
    </div>
  );
}

interface CanvasOverlayProps {
  state: FetchState;
  visibleSnapshot: SnapshotPayload | null;
  /** Effective semantic-zoom level that produced the rendered snapshot. */
  level: SnapshotLevel;
}

function CanvasOverlay({ state, visibleSnapshot, level }: CanvasOverlayProps) {
  if (state.status === "loading") {
    return (
      <CenterCard>
        <SpinningIcon />
        <p className="mt-3 text-sm text-zinc-400">
          Loading code graph snapshot…
        </p>
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
  const snapshot = visibleSnapshot ?? state.snapshot;
  if (snapshot.nodes.length === 0) {
    return (
      <CenterCard>
        <span className="mx-auto flex h-10 w-10 items-center justify-center rounded-full bg-zinc-800/60 text-zinc-400">
          <HugeiconsIcon icon={ConnectIcon} className="h-5 w-5" />
        </span>
        <p className="mt-3 text-sm text-zinc-400">
          No graph data yet — kick off an architect warm-up to populate it.
        </p>
      </CenterCard>
    );
  }

  return (
    <div className="pointer-events-none absolute left-3 top-3 flex flex-col gap-1.5">
      <Pill data-testid="semantic-zoom-level">
        {level === "community" ? "Community view" : "Symbol view"}
      </Pill>
      <Pill>
        {snapshot.nodes.length.toLocaleString()} nodes
        {snapshot.truncated && (
          <span className="ml-1 text-amber-300">
            (capped from {snapshot.total_nodes.toLocaleString()})
          </span>
        )}
      </Pill>
      <Pill>{snapshot.edges.length.toLocaleString()} edges</Pill>
    </div>
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
