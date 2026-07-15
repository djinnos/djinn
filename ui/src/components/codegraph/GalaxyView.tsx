/**
 * GalaxyView — THE code-graph view (proposal lmkv cutover).
 *
 * Fetches the whole-graph `code_graph snapshot` for the selected project
 * (the server enforces its own ceiling), adapts it to GalaxyData, runs
 * the force layout in a Web Worker so the main thread never freezes, and
 * renders `GalaxyCanvas` with:
 *
 *   - top-left: project selector + workspace picker,
 *   - top-right: hide-tests toggle + hotspot-panel toggle,
 *   - right: the HotspotPanel leaderboard (complexity × co-change);
 *     clicking a row flies the camera to the file + its co-change
 *     partners and lights their pink coupling web,
 *   - Cmd-K integration: search hits / AI citations from the shared
 *     codeGraphStore fly the camera to the matching stars.
 *
 * Workspace and test filters apply AFTER layout, so toggling them never
 * reshuffles positions — stars just appear/disappear in place.
 */

import { useEffect, useMemo, useState } from "react";

import {
  fetchCoverage,
  fetchSnapshot,
  fetchWorkspaces,
  type CodeGraphCoverage,
  type CodeGraphWorkspace,
} from "@/api/codeGraph";
import { CoverageGapBanner } from "@/components/codegraph/CoverageGapBanner";
import { HotspotPanel } from "@/components/codegraph/HotspotPanel";
import { GalaxyCanvas } from "@/components/galaxy/GalaxyCanvas";
import type { GalaxyData } from "@/components/galaxy/galaxyTypes";
import { layoutInWorker } from "@/components/galaxy/galaxyLayoutClient";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
} from "@/components/ui/select";
import {
  galaxyLayoutSeed,
  snapshotHotspots,
  snapshotToGalaxy,
  type GalaxyHotspot,
} from "@/lib/codeGraphGalaxyAdapter";
import { parseSnapshotResponse } from "@/lib/codeGraphAdapter";
import { useCodeGraphStore } from "@/stores/codeGraphStore";
import {
  useProjects,
  useProjectStore,
  useSelectedProject,
} from "@/stores/useProjectStore";
import { cn } from "@/lib/utils";

/** Ask for everything; the server enforces its own ceiling. */
const GALAXY_NODE_BUDGET = 1_000_000;

type GalaxyState =
  | { phase: "loading"; message: string }
  | { phase: "error"; message: string }
  | { phase: "ready"; data: GalaxyData; hotspots: GalaxyHotspot[] };

/** One chip voice for every HUD control (chips, selects, toggles). The
 * dark-variant background mirrors what SelectTrigger's base classes
 * resolve to, so buttons and selects read as the same chip. */
const HUD_CHIP =
  "border-slate-700/60 bg-slate-900/80 font-mono text-[11px] text-slate-300 backdrop-blur-sm dark:bg-input/30 dark:hover:bg-input/50";

export function GalaxyView({ projectId }: { projectId: string }) {
  const project = useSelectedProject();
  const projects = useProjects();
  const setSelectedProjectId = useProjectStore((s) => s.setSelectedProjectId);
  const [state, setState] = useState<GalaxyState>({
    phase: "loading",
    message: "Fetching graph…",
  });
  const [hideTests, setHideTests] = useState(false);
  const [workspaces, setWorkspaces] = useState<CodeGraphWorkspace[]>([]);
  const [workspaceSlug, setWorkspaceSlug] = useState<string | null>(null);
  const [coverage, setCoverage] = useState<CodeGraphCoverage | null>(null);
  const [showHotspots, setShowHotspots] = useState(false);
  const [hotspotSelection, setHotspotSelection] =
    useState<GalaxyHotspot | null>(null);

  // Cmd-K search hits / AI citations fly the camera to their stars. An
  // active hotspot row takes over the focus set (file + co-change
  // partners); a fresh Cmd-K/citation signal takes it back.
  const citationIds = useCodeGraphStore((s) => s.citationIds);
  const toolHighlightIds = useCodeGraphStore((s) => s.toolHighlightIds);
  const focusIds = useMemo(() => {
    if (hotspotSelection) {
      return [hotspotSelection.fileId, ...hotspotSelection.partnerIds];
    }
    return [...citationIds, ...toolHighlightIds];
  }, [hotspotSelection, citationIds, toolHighlightIds]);
  useEffect(() => {
    if (citationIds.size > 0 || toolHighlightIds.size > 0) {
      // eslint-disable-next-line react-hooks/set-state-in-effect -- external focus signal preempts the panel selection.
      setHotspotSelection(null);
    }
  }, [citationIds, toolHighlightIds]);

  useEffect(() => {
    let cancelled = false;
    // eslint-disable-next-line react-hooks/set-state-in-effect -- async snapshot-fetch state machine: reset/loading transition around the network read, same convention as the old page's workspace fetch.
    setState({ phase: "loading", message: "Fetching graph…" });
    setWorkspaceSlug(null);
    setHotspotSelection(null);

    void fetchWorkspaces(projectId)
      .then((list) => {
        if (!cancelled) setWorkspaces(list);
      })
      .catch(() => {
        if (!cancelled) setWorkspaces([]);
      });

    // glqk: fetch index coverage in parallel — the HUD banner names any
    // unindexed workspaces so the galaxy never lies by omission. Best-effort:
    // a failure just hides the banner.
    setCoverage(null);
    void fetchCoverage(projectId)
      .then((cov) => {
        if (!cancelled) setCoverage(cov);
      })
      .catch(() => {
        if (!cancelled) setCoverage(null);
      });

    void (async () => {
      try {
        const response = await fetchSnapshot(projectId, GALAXY_NODE_BUDGET);
        if (cancelled) return;
        const snapshot = parseSnapshotResponse(response);
        if (!snapshot) {
          setState({
            phase: "error",
            message: "Snapshot unavailable — is the graph warmed?",
          });
          return;
        }
        const data = snapshotToGalaxy(snapshot, { layout: false });
        const hotspots = snapshotHotspots(snapshot);
        // lmkv fast path: the server already shipped a warm-time galaxy layout
        // for every node — render immediately, no worker, no seconds-long
        // main-thread-free-but-still-slow force pass.
        if (data.serverPositioned) {
          if (cancelled) return;
          setState({ phase: "ready", data, hotspots });
          return;
        }
        setState({
          phase: "loading",
          message: `Computing layout for ${snapshot.nodes.length.toLocaleString()} nodes…`,
        });
        const positioned = await layoutInWorker(
          data.nodes,
          data.edges,
          galaxyLayoutSeed(snapshot.project_id),
        );
        if (cancelled) return;
        setState({
          phase: "ready",
          data: { ...data, nodes: positioned },
          hotspots,
        });
      } catch (error) {
        if (cancelled) return;
        setState({
          phase: "error",
          message:
            error instanceof Error ? error.message : "Failed to load the galaxy.",
        });
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [projectId]);

  // Post-layout filters: positions stay put, stars appear/disappear.
  const visibleData = useMemo<GalaxyData | null>(() => {
    if (state.phase !== "ready") return null;
    const full = state.data;
    if (!hideTests && !workspaceSlug) return full;
    const nodes = full.nodes.filter(
      (n) =>
        (!hideTests || !n.isTest) &&
        (!workspaceSlug || n.workspace === undefined || n.workspace === workspaceSlug),
    );
    const kept = new Set(nodes.map((n) => n.id));
    const edges = full.edges.filter(
      (e) => kept.has(e.source) && kept.has(e.target),
    );
    return { ...full, nodes, edges };
  }, [state, hideTests, workspaceSlug]);

  // Hotspot rows obey the same post-layout filters as the stars.
  const visibleHotspots = useMemo<GalaxyHotspot[]>(() => {
    if (state.phase !== "ready") return [];
    return state.hotspots.filter(
      (h) =>
        (!hideTests || !h.isTest) &&
        (!workspaceSlug || h.workspace === undefined || h.workspace === workspaceSlug),
    );
  }, [state, hideTests, workspaceSlug]);

  if (state.phase !== "ready" || !visibleData) {
    return (
      <div className="flex h-full w-full items-center justify-center bg-[#06090f] font-mono text-sm text-slate-400">
        <div className="space-y-2 text-center">
          {state.phase === "loading" && (
            <span
              className="mx-auto block h-5 w-5 animate-spin rounded-full border-2 border-slate-600 border-t-slate-200"
              role="status"
              aria-label="Loading galaxy"
            />
          )}
          <p>{state.phase === "error" ? state.message : state.phase === "loading" ? state.message : null}</p>
        </div>
      </div>
    );
  }

  return (
    <GalaxyCanvas
      data={visibleData}
      focusIds={focusIds}
      focusPrimaryId={hotspotSelection?.fileId ?? null}
      banner={<CoverageGapBanner coverage={coverage} />}
      sidePanel={
        showHotspots ? (
          <HotspotPanel
            hotspots={visibleHotspots}
            selectedId={hotspotSelection?.fileId ?? null}
            onSelect={setHotspotSelection}
          />
        ) : undefined
      }
      headerPrimary={
        <>
          <Select
            value={projectId}
            onValueChange={(value) => {
              if (typeof value === "string" && value) {
                setSelectedProjectId(value);
              }
            }}
          >
            <SelectTrigger size="sm" aria-label="Project" className={HUD_CHIP}>
              <span className="font-semibold tracking-wide text-slate-200">
                {project?.name ?? projectId}
              </span>
            </SelectTrigger>
            <SelectContent className="max-h-80 min-w-56">
              {projects.map((p) => (
                <SelectItem key={p.id} value={p.id}>
                  {p.name}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          {workspaces.length > 1 && (
            <Select
              value={workspaceSlug ?? "__all__"}
              onValueChange={(value) => {
                if (typeof value === "string") {
                  setWorkspaceSlug(value === "__all__" ? null : value);
                }
              }}
            >
              <SelectTrigger size="sm" aria-label="Workspace" className={HUD_CHIP}>
                <span>
                  {workspaceSlug
                    ? (workspaces.find((w) => w.slug === workspaceSlug)?.display ??
                      workspaceSlug)
                    : "All"}
                </span>
              </SelectTrigger>
              <SelectContent className="max-h-80 min-w-56">
                <SelectItem value="__all__">All</SelectItem>
                {workspaces.map((workspace) => (
                  <SelectItem key={workspace.slug} value={workspace.slug}>
                    {workspace.display ?? workspace.slug}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          )}
        </>
      }
      headerExtra={
        <>
          <button
            type="button"
            aria-pressed={hideTests}
            onClick={() => setHideTests((v) => !v)}
            className={cn(
              "h-7 rounded-lg border px-2.5 transition-colors",
              HUD_CHIP,
              hideTests ? "text-sky-300" : "hover:text-slate-100",
            )}
          >
            {hideTests ? "Tests hidden" : "Hide tests"}
          </button>
          <button
            type="button"
            aria-pressed={showHotspots}
            onClick={() => {
              if (showHotspots) setHotspotSelection(null);
              setShowHotspots(!showHotspots);
            }}
            className={cn(
              "h-7 rounded-lg border px-2.5 transition-colors",
              HUD_CHIP,
              showHotspots
                ? "bg-slate-600/40 text-slate-100 dark:bg-slate-600/40"
                : "hover:text-slate-100",
            )}
          >
            Hotspots
          </button>
        </>
      }
    />
  );
}
