/**
 * CodeGraphCanvas — main D2 view: fetch → adapt → render.
 *
 * Owns the round-trip from project id to a fully-laid-out Sigma
 * canvas. State machine has three terminal states (loading / error /
 * ready) plus an empty-graph fallback for projects that haven't been
 * warmed yet.
 *
 * Interactions, citation highlighting, Mermaid impact, Cmd-K — all
 * intentionally deferred to PR D3-D6 per the scope rules in the plan.
 */

import { useEffect, useMemo, useRef, useState } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import { ConnectIcon, AlertCircleIcon, RefreshIcon } from "@hugeicons/core-free-icons";

import { fetchSnapshot } from "@/api/codeGraph";
import {
  buildGraphFromSnapshot,
  parseSnapshotResponse,
  type SnapshotPayload,
} from "@/lib/codeGraphAdapter";
import { useSigmaGraph } from "@/hooks/useSigmaGraph";
import { useCodeGraphStore } from "@/stores/codeGraphStore";
import { cn } from "@/lib/utils";

type FetchState =
  | { status: "loading" }
  | { status: "error"; error: string }
  | { status: "ready"; snapshot: SnapshotPayload };

interface CodeGraphCanvasProps {
  projectId: string;
  /**
   * Maximum number of nodes to fetch. Default 2000 (Sigma WebGL ceiling
   * per the plan §"Risks & mitigations"). Useful to drop lower for
   * tests or raise for debugging.
   */
  nodeCap?: number;
  /** Bumping this re-issues the snapshot fetch without unmounting. */
  reloadKey?: number;
}

export function CodeGraphCanvas({
  projectId,
  nodeCap,
  reloadKey,
}: CodeGraphCanvasProps) {
  const [state, setState] = useState<FetchState>({ status: "loading" });
  const containerRef = useRef<HTMLDivElement | null>(null);

  // ── Fetch the snapshot ────────────────────────────────────────────────
  useEffect(() => {
    let cancelled = false;
    setState({ status: "loading" });

    (async () => {
      try {
        const raw = await fetchSnapshot(projectId, nodeCap);
        if (cancelled) return;
        const snapshot = parseSnapshotResponse(raw);
        if (!snapshot) {
          setState({
            status: "error",
            error:
              "Snapshot response was empty or malformed. The graph may not be warmed yet — try again in a minute.",
          });
          return;
        }
        setState({ status: "ready", snapshot });
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
  }, [projectId, nodeCap, reloadKey]);

  // ── Build the graphology graph (memoized so it doesn't churn) ─────────
  const graph = useMemo(() => {
    if (state.status !== "ready") return null;
    return buildGraphFromSnapshot(state.snapshot);
  }, [state]);

  // ── Sigma + ForceAtlas2 lifecycle ─────────────────────────────────────
  const { layoutRunning } = useSigmaGraph(containerRef, graph);

  return (
    <div className="absolute inset-0 bg-background">
      <div
        ref={containerRef}
        data-testid="code-graph-canvas"
        className="absolute inset-0"
      />
      <CanvasOverlay state={state} layoutRunning={layoutRunning} />
      {/*
       * D3's reducer will own the actual Sigma node-pulse animation.
       * Until D3 lands, this badge gives D5's chat round-trip an
       * observable signal — when a citation lands, the user sees the
       * pinned node id render in the corner so the navigation isn't
       * silent.
       */}
      <CitationStatusBadge />
    </div>
  );
}

function CitationStatusBadge() {
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
  layoutRunning: boolean;
}

function CanvasOverlay({ state, layoutRunning }: CanvasOverlayProps) {
  if (state.status === "loading") {
    return (
      <CenterCard>
        <SpinningIcon />
        <p className="mt-3 text-sm text-muted-foreground">
          Loading code graph snapshot…
        </p>
      </CenterCard>
    );
  }
  if (state.status === "error") {
    return (
      <CenterCard>
        <span className="mx-auto flex h-10 w-10 items-center justify-center rounded-full bg-destructive/15 text-destructive">
          <HugeiconsIcon icon={AlertCircleIcon} className="h-5 w-5" />
        </span>
        <p className="mt-3 text-sm font-medium text-foreground">
          Couldn&apos;t load the graph
        </p>
        <p className="mt-1 max-w-sm text-xs text-muted-foreground">
          {state.error}
        </p>
      </CenterCard>
    );
  }
  if (state.snapshot.nodes.length === 0) {
    return (
      <CenterCard>
        <span className="mx-auto flex h-10 w-10 items-center justify-center rounded-full bg-muted/30 text-muted-foreground/70">
          <HugeiconsIcon icon={ConnectIcon} className="h-5 w-5" />
        </span>
        <p className="mt-3 text-sm text-muted-foreground">
          No graph data yet — kick off an architect warm-up to populate it.
        </p>
      </CenterCard>
    );
  }

  const { snapshot } = state;
  return (
    <div className="pointer-events-none absolute left-3 top-3 flex flex-col gap-1.5">
      <Pill>
        {snapshot.nodes.length.toLocaleString()} nodes
        {snapshot.truncated && (
          <span className="ml-1 text-amber-300">
            (capped from {snapshot.total_nodes.toLocaleString()})
          </span>
        )}
      </Pill>
      <Pill>{snapshot.edges.length.toLocaleString()} edges</Pill>
      {layoutRunning && <Pill data-testid="layout-running">settling…</Pill>}
    </div>
  );
}

function CenterCard({ children }: { children: React.ReactNode }) {
  return (
    <div className="pointer-events-none absolute inset-0 flex items-center justify-center">
      <div className="max-w-sm rounded-lg border border-border/40 bg-background/80 px-5 py-4 text-center backdrop-blur">
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
        "inline-flex items-center gap-1 rounded-full border border-border/40 bg-background/85 px-2.5 py-0.5 text-[11px] font-medium text-muted-foreground backdrop-blur",
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
    <span className="mx-auto flex h-10 w-10 items-center justify-center rounded-full bg-muted/30 text-muted-foreground/70">
      <HugeiconsIcon
        icon={RefreshIcon}
        className="h-5 w-5 animate-spin [animation-duration:2s]"
      />
    </span>
  );
}
