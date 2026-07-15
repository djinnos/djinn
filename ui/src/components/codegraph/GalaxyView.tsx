/**
 * GalaxyView — THE code-graph view (proposal lmkv cutover).
 *
 * Fetches the whole-graph `code_graph snapshot` for the selected project
 * (the server enforces its own ceiling), adapts it to GalaxyData, runs
 * the force layout in a Web Worker so the main thread never freezes, and
 * renders `GalaxyCanvas` with:
 *
 *   - top-left: project chip + workspace picker (mirrors the old page
 *     affordances; project switching lives in the shared chrome),
 *   - top-right: color mode (crate / complexity) + hide-tests toggle,
 *   - Cmd-K integration: search hits / AI citations from the shared
 *     codeGraphStore fly the camera to the matching stars.
 *
 * Workspace and test filters apply AFTER layout, so toggling them never
 * reshuffles positions — stars just appear/disappear in place.
 */

import { useEffect, useMemo, useState } from "react";

import { fetchSnapshot, fetchWorkspaces, type CodeGraphWorkspace } from "@/api/codeGraph";
import { GalaxyCanvas } from "@/components/galaxy/GalaxyCanvas";
import type {
  GalaxyColorMode,
  GalaxyData,
} from "@/components/galaxy/galaxyTypes";
import { layoutInWorker } from "@/components/galaxy/galaxyLayoutClient";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  galaxyLayoutSeed,
  snapshotToGalaxy,
} from "@/lib/codeGraphGalaxyAdapter";
import { parseSnapshotResponse } from "@/lib/codeGraphAdapter";
import { useCodeGraphStore } from "@/stores/codeGraphStore";
import { useSelectedProject } from "@/stores/useProjectStore";
import { cn } from "@/lib/utils";

/** Ask for everything; the server enforces its own ceiling. */
const GALAXY_NODE_BUDGET = 1_000_000;

type GalaxyState =
  | { phase: "loading"; message: string }
  | { phase: "error"; message: string }
  | { phase: "ready"; data: GalaxyData };

const COLOR_MODES: Array<{ value: GalaxyColorMode; label: string }> = [
  { value: "group", label: "Crates" },
  { value: "heat", label: "Complexity" },
];

export function GalaxyView({ projectId }: { projectId: string }) {
  const project = useSelectedProject();
  const [state, setState] = useState<GalaxyState>({
    phase: "loading",
    message: "Fetching graph…",
  });
  const [colorMode, setColorMode] = useState<GalaxyColorMode>("group");
  const [hideTests, setHideTests] = useState(false);
  const [workspaces, setWorkspaces] = useState<CodeGraphWorkspace[]>([]);
  const [workspaceSlug, setWorkspaceSlug] = useState<string | null>(null);

  // Cmd-K search hits / AI citations fly the camera to their stars.
  const citationIds = useCodeGraphStore((s) => s.citationIds);
  const toolHighlightIds = useCodeGraphStore((s) => s.toolHighlightIds);
  const focusIds = useMemo(
    () => [...citationIds, ...toolHighlightIds],
    [citationIds, toolHighlightIds],
  );

  useEffect(() => {
    let cancelled = false;
    // eslint-disable-next-line react-hooks/set-state-in-effect -- async snapshot-fetch state machine: reset/loading transition around the network read, same convention as the old page's workspace fetch.
    setState({ phase: "loading", message: "Fetching graph…" });
    setWorkspaceSlug(null);

    void fetchWorkspaces(projectId)
      .then((list) => {
        if (!cancelled) setWorkspaces(list);
      })
      .catch(() => {
        if (!cancelled) setWorkspaces([]);
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
        setState({
          phase: "loading",
          message: `Computing layout for ${snapshot.nodes.length.toLocaleString()} nodes…`,
        });
        const data = snapshotToGalaxy(snapshot, { layout: false });
        const positioned = await layoutInWorker(
          data.nodes,
          data.edges,
          galaxyLayoutSeed(snapshot.project_id),
        );
        if (cancelled) return;
        setState({ phase: "ready", data: { ...data, nodes: positioned } });
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
      colorMode={colorMode}
      focusIds={focusIds}
      headerPrimary={
        <>
          <span className="rounded-md border border-slate-700/60 bg-slate-900/80 px-2 py-1 text-[11px] font-semibold tracking-wide text-slate-200 backdrop-blur-sm">
            {project?.name ?? projectId}
          </span>
          {workspaces.length > 1 && (
            <Select
              value={workspaceSlug ?? "__all__"}
              onValueChange={(value) => {
                if (typeof value === "string") {
                  setWorkspaceSlug(value === "__all__" ? null : value);
                }
              }}
            >
              <SelectTrigger
                size="sm"
                aria-label="Workspace"
                className="border-slate-700/60 bg-slate-900/80 font-mono text-[11px] text-slate-300 backdrop-blur-sm"
              >
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="__all__">All workspaces</SelectItem>
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
              "rounded-md border border-slate-700/60 bg-slate-900/80 px-2 py-1 font-mono text-[11px] backdrop-blur-sm transition-colors",
              hideTests ? "text-sky-300" : "text-slate-300 hover:text-slate-100",
            )}
          >
            {hideTests ? "Tests hidden" : "Hide tests"}
          </button>
          <Select
            value={colorMode}
            onValueChange={(value) => {
              if (typeof value === "string") {
                setColorMode(value as GalaxyColorMode);
              }
            }}
          >
            <SelectTrigger
              size="sm"
              aria-label="Color mode"
              className="border-slate-700/60 bg-slate-900/80 font-mono text-[11px] text-slate-300 backdrop-blur-sm"
            >
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {COLOR_MODES.map(({ value, label }) => (
                <SelectItem key={value} value={value}>
                  {label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </>
      }
    />
  );
}
