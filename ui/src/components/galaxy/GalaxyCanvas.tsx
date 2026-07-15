/**
 * GalaxyCanvas — the galaxy view: scene + HUD chrome.
 *
 * Owns selection/hover state, the adjacency index, per-mode base colors,
 * and the camera fly-to targets; renders the stats line ("showing N of M"
 * honesty included), the group fly-to selector, the hover tooltip, and the
 * color legend. The 3D internals live in `GalaxyScene`.
 *
 * Generic on purpose: feed it any `GalaxyData` (code snapshot today,
 * memory graph tomorrow) — see galaxyTypes.ts.
 */

import {
  Component,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
} from "@/components/ui/select";
import { groupColorCss } from "./galaxyColors";
import { boundsOf } from "./galaxyLayout";
import { GalaxyScene, type FlyTarget } from "./GalaxyScene";
import {
  DEFAULT_GALAXY_DISPLAY,
  type GalaxyColorMode,
  type GalaxyData,
  type GalaxyDisplay,
} from "./galaxyTypes";

export interface GalaxyCanvasProps {
  data: GalaxyData;
  colorMode?: GalaxyColorMode;
  showLabels?: boolean;
  display?: GalaxyDisplay;
  /** Optional headline shown in the HUD chip (e.g. project name). */
  title?: string;
  /** Interactive controls rendered in the top-left HUD, above the stats
   * line (project/workspace pickers on the live page). */
  headerPrimary?: ReactNode;
  /** Extra controls rendered in the top-right HUD row (page-level knobs). */
  headerExtra?: ReactNode;
  /**
   * glqk: optional advisory banner rendered as a top-center HUD layer (e.g. the
   * index-coverage-gap chip naming unindexed workspaces). Absent when coverage
   * is clean.
   */
  banner?: ReactNode;
  /**
   * External focus (Cmd-K search hits, AI citations): when non-empty the
   * camera flies to these nodes and they become the highlight set,
   * replacing any click selection. Cleared by passing null/empty.
   */
  focusIds?: string[] | null;
  className?: string;
  onSelect?: (nodeId: string | null) => void;
}

// Focus ids round-trip through a joined string (effect dep); ids can
// contain spaces, so the separator must be a char no id can contain.
const FOCUS_KEY_SEP = "\u0000";

const HEAT_LEGEND: Array<{ color: string; label: string }> = [
  { color: "#34d399", label: "tame" },
  { color: "#eab308", label: "warm" },
  { color: "#ef4444", label: "hot" },
];

/** `server:djinn-graph` → `djinn-graph` — the workspace prefix is noise in
 * a dropdown that already groups by crate. Full key stays in the tooltip. */
function displayGroupLabel(group: string): string {
  const colon = group.indexOf(":");
  return colon >= 0 ? group.slice(colon + 1) : group;
}

/**
 * WebGL canvases die non-recoverably when the browser exhausts its
 * context pool (Storybook HMR churn is a reliable trigger — surfaces as
 * `Cannot read properties of null (reading 'alpha')` from the effect
 * composer). Catch it and offer a remount instead of red-screening.
 */
class SceneErrorBoundary extends Component<
  { children: ReactNode },
  { failed: boolean; generation: number }
> {
  state = { failed: false, generation: 0 };

  static getDerivedStateFromError() {
    return { failed: true };
  }

  render() {
    if (this.state.failed) {
      return (
        <div className="flex h-full w-full items-center justify-center bg-[#06090f] font-mono text-sm text-slate-400">
          <div className="space-y-3 text-center">
            <p>WebGL context lost (too many canvases open, or GPU reset).</p>
            <button
              type="button"
              className="rounded-md border border-slate-700/60 bg-slate-900/80 px-3 py-1.5 text-slate-200 hover:text-white"
              onClick={() =>
                this.setState((s) => ({ failed: false, generation: s.generation + 1 }))
              }
            >
              Reload galaxy
            </button>
          </div>
        </div>
      );
    }
    return <div key={this.state.generation} className="h-full w-full">{this.props.children}</div>;
  }
}

export function GalaxyCanvas({
  data,
  colorMode = "group",
  showLabels = false,
  display = DEFAULT_GALAXY_DISPLAY,
  title,
  headerPrimary,
  headerExtra,
  banner,
  focusIds,
  className,
  onSelect,
}: GalaxyCanvasProps) {
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [focusHighlight, setFocusHighlight] = useState<Set<string> | null>(null);
  const [flyToValue, setFlyToValue] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [contextLost, setContextLost] = useState(false);
  const [sceneGeneration, setSceneGeneration] = useState(0);
  const [hovered, setHovered] = useState<{ id: string; x: number; y: number } | null>(null);
  const flyKey = useRef(0);
  const containerRef = useRef<HTMLDivElement>(null);

  const adjacency = useMemo(() => {
    const map = new Map<string, Set<string>>();
    for (const edge of data.edges) {
      let s = map.get(edge.source);
      if (!s) map.set(edge.source, (s = new Set()));
      s.add(edge.target);
      let t = map.get(edge.target);
      if (!t) map.set(edge.target, (t = new Set()));
      t.add(edge.source);
    }
    return map;
  }, [data]);

  const groups = useMemo(() => {
    const counts = new Map<string, number>();
    for (const node of data.nodes) {
      if (!node.group) continue;
      counts.set(node.group, (counts.get(node.group) ?? 0) + 1);
    }
    return [...counts.entries()].sort((a, b) => b[1] - a[1]);
  }, [data]);

  const highlight = useMemo(() => {
    if (focusHighlight && focusHighlight.size > 0) return focusHighlight;
    if (!selectedId) return null;
    const set = new Set<string>([selectedId]);
    for (const n of adjacency.get(selectedId) ?? []) set.add(n);
    return set;
  }, [focusHighlight, selectedId, adjacency]);

  const overviewTarget = useMemo<FlyTarget>(() => {
    const { center, radius } = boundsOf(data.nodes);
    return { center, distance: radius * 2.3, key: 0 };
  }, [data]);
  const [flyTarget, setFlyTarget] = useState<FlyTarget>(overviewTarget);

  const flyToNodes = useCallback(
    (ids: Set<string> | null) => {
      if (!ids || ids.size === 0) {
        flyKey.current += 1;
        setFlyTarget({ ...overviewTarget, key: flyKey.current });
        return;
      }
      const members = data.nodes.filter((n) => ids.has(n.id));
      const { center, radius } = boundsOf(members);
      flyKey.current += 1;
      setFlyTarget({
        center,
        distance: Math.max(radius * 3, 130),
        key: flyKey.current,
      });
    },
    [data, overviewTarget],
  );

  const handleClick = useCallback(
    (index: number | null) => {
      const id = index === null ? null : (data.nodes[index]?.id ?? null);
      setFocusHighlight(null);
      setSelectedId(id);
      onSelect?.(id);
      if (id) {
        const set = new Set<string>([id]);
        for (const n of adjacency.get(id) ?? []) set.add(n);
        flyToNodes(set);
      } else {
        flyToNodes(null);
      }
    },
    [data, adjacency, flyToNodes, onSelect],
  );

  const handlePointer = useCallback(
    (index: number | null, clientX?: number, clientY?: number) => {
      if (index === null || clientX === undefined || clientY === undefined) {
        setHovered(null);
        return;
      }
      const node = data.nodes[index];
      if (!node) return;
      const rect = containerRef.current?.getBoundingClientRect();
      setHovered({
        id: node.id,
        x: clientX - (rect?.left ?? 0),
        y: clientY - (rect?.top ?? 0),
      });
    },
    [data],
  );

  // External focus (search hits / citations): fly there and spotlight.
  const focusKey = focusIds && focusIds.length > 0 ? focusIds.join(FOCUS_KEY_SEP) : "";
  useEffect(() => {
    let next: Set<string> | null = null;
    if (focusKey) {
      const known = new Set(data.nodes.map((n) => n.id));
      const valid = focusKey.split(FOCUS_KEY_SEP).filter((id) => known.has(id));
      if (valid.length > 0) next = new Set(valid);
    }
    // eslint-disable-next-line react-hooks/set-state-in-effect -- syncing highlight + camera to an external focus signal (Cmd-K hits / citations) when the prop changes.
    setFocusHighlight(next);
    if (!next) return;
     
    setSelectedId(null);
    flyToNodes(next);
  }, [focusKey, data, flyToNodes]);

  // HUD search: live-highlight matching stars while typing (debounced),
  // Enter flies the camera to them, Escape/empty clears.
  const searchMatches = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (q.length < 2) return null;
    const ids = new Set<string>();
    for (const node of data.nodes) {
      if (
        node.label.toLowerCase().includes(q) ||
        node.id.toLowerCase().includes(q)
      ) {
        ids.add(node.id);
      }
    }
    return ids;
  }, [data, query]);

  const searchOwnsHighlight = useRef(false);
  useEffect(() => {
    // Only touch the highlight while a search is active (or was, and needs
    // clearing) — otherwise this would stomp Cmd-K/citation focus.
    if (searchMatches === null && !searchOwnsHighlight.current) return;
    const timer = setTimeout(() => {
      searchOwnsHighlight.current = searchMatches !== null;
      setFocusHighlight(
        searchMatches && searchMatches.size > 0 ? searchMatches : null,
      );
    }, 200);
    return () => clearTimeout(timer);
  }, [searchMatches]);

  const handleGroupJump = useCallback(
    (group: string) => {
      if (!group) return;
      const ids = new Set(
        data.nodes.filter((n) => n.group === group).map((n) => n.id),
      );
      setSelectedId(null);
      flyToNodes(ids);
    },
    [data, flyToNodes],
  );

  const hoveredNode = hovered
    ? data.nodes.find((n) => n.id === hovered.id)
    : null;
  const selectedNode = selectedId
    ? data.nodes.find((n) => n.id === selectedId)
    : null;
  const truncated =
    data.totalNodes !== undefined && data.totalNodes > data.nodes.length;
  // Group mode carries its color key inside the Fly-to dropdown (a dot per
  // crate), so the corner legend would be redundant there.
  const legend = colorMode === "heat" ? HEAT_LEGEND : null;

  return (
    <div
      ref={containerRef}
      className={`relative h-full w-full overflow-hidden bg-[#04060c] ${className ?? ""}`}
      data-testid="galaxy-canvas"
    >
      <SceneErrorBoundary>
        <GalaxyScene
          key={sceneGeneration}
          data={data}
          colorMode={colorMode}
          highlight={highlight}
          display={display}
          showLabels={showLabels}
          flyTarget={flyTarget}
          onNodePointer={handlePointer}
          onNodeClick={handleClick}
          onContextLost={() => setContextLost(true)}
          onContextRestored={() => {
            // The browser handed the context back — remount for a clean
            // GPU-resource rebuild instead of waiting for a manual reload.
            setContextLost(false);
            setSceneGeneration((g) => g + 1);
          }}
        />
      </SceneErrorBoundary>

      {contextLost && (
        <div className="absolute inset-0 z-20 flex items-center justify-center bg-[#06090f]/95 font-mono text-sm text-slate-400">
          <div className="space-y-3 text-center">
            <p>The GPU dropped the galaxy's WebGL context — recovering…</p>
            <button
              type="button"
              className="rounded-md border border-slate-700/60 bg-slate-900/80 px-3 py-1.5 text-slate-200 hover:text-white"
              onClick={() => {
                setContextLost(false);
                setSceneGeneration((g) => g + 1);
              }}
            >
              Reload galaxy
            </button>
          </div>
        </div>
      )}

      {/* ── HUD: pickers + stats (top-left) ── */}
      <div className="pointer-events-none absolute left-4 top-4 flex flex-col gap-1.5 font-mono text-[11px] text-slate-400">
        {headerPrimary && (
          <div className="pointer-events-auto flex items-center gap-2">
            {headerPrimary}
          </div>
        )}
        {title && (
          <span className="text-[12px] font-semibold tracking-wide text-slate-200">
            {title}
          </span>
        )}
        <span>
          {data.nodes.length.toLocaleString()} nodes /{" "}
          {data.edges.length.toLocaleString()} edges
          {truncated && (
            <span className="text-amber-400/90">
              {" "}
              · showing {data.nodes.length.toLocaleString()} of{" "}
              {data.totalNodes!.toLocaleString()}
            </span>
          )}
        </span>
        {selectedNode && (
          <span className="text-sky-300">
            {selectedNode.label} · {highlight ? highlight.size - 1 : 0} neighbors
          </span>
        )}
        <div className="pointer-events-auto flex items-center gap-2">
          <input
            type="search"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && searchMatches && searchMatches.size > 0) {
                flyToNodes(searchMatches);
              } else if (e.key === "Escape") {
                setQuery("");
              }
            }}
            placeholder="Search stars…"
            aria-label="Search nodes"
            className="h-7 w-56 rounded-lg border border-slate-700/60 bg-slate-900/80 px-2.5 font-mono text-[11px] text-slate-200 outline-none backdrop-blur-sm transition-colors placeholder:text-slate-500 focus:border-slate-500 dark:bg-input/30"
          />
          {searchMatches && (
            <span className={searchMatches.size > 0 ? "text-sky-300" : "text-slate-500"}>
              {searchMatches.size.toLocaleString()}{" "}
              {searchMatches.size === 1 ? "match" : "matches"}
            </span>
          )}
        </div>
      </div>

      {/* ── HUD: advisory banner (top-center) ── */}
      {banner && (
        <div className="pointer-events-none absolute left-1/2 top-4 flex -translate-x-1/2 justify-center">
          <div className="pointer-events-auto">{banner}</div>
        </div>
      )}

      {/* ── HUD: controls (top-right) ── */}
      <div className="absolute right-4 top-4 flex items-center gap-2">
        {headerExtra}
        {groups.length > 1 && (
          <Select
            value={flyToValue}
            onValueChange={(value) => {
              if (typeof value === "string" && value) {
                handleGroupJump(value);
                // Action-select: snap back to the placeholder so the same
                // group can be flown to again later.
                requestAnimationFrame(() => setFlyToValue(null));
              }
            }}
          >
            <SelectTrigger
              size="sm"
              aria-label="Fly to group"
              className="border-slate-700/60 bg-slate-900/80 font-mono text-[11px] text-slate-300 backdrop-blur-sm"
            >
              <span>Fly to…</span>
            </SelectTrigger>
            <SelectContent className="max-h-80 min-w-56">
              {groups.map(([group, count]) => (
                <SelectItem key={group} value={group}>
                  <span className="flex items-center gap-2 font-mono text-xs">
                    <span
                      className="inline-block h-2 w-2 shrink-0 rounded-full"
                      style={{ background: groupColorCss(group) }}
                    />
                    <span className="truncate" title={group}>
                      {displayGroupLabel(group)}
                    </span>
                    <span className="ml-auto pl-2 text-muted-foreground">
                      {count}
                    </span>
                  </span>
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        )}
        {selectedId && (
          <button
            type="button"
            onClick={() => handleClick(null)}
            className="h-7 rounded-lg border border-slate-700/60 bg-slate-900/80 px-2.5 font-mono text-[11px] text-slate-300 backdrop-blur-sm transition-colors hover:text-slate-100 dark:bg-input/30 dark:hover:bg-input/50"
          >
            Clear selection
          </button>
        )}
      </div>

      {/* ── HUD: legend (bottom-right; group mode keys colors in Fly-to) ── */}
      {legend && (
        <div className="pointer-events-none absolute bottom-4 right-4 flex items-center gap-3 rounded-md border border-slate-800/60 bg-slate-950/60 px-3 py-1.5 font-mono text-[10px] text-slate-400 backdrop-blur-sm">
          {legend.map(({ color, label }) => (
            <span key={label} className="flex items-center gap-1.5">
              <span
                className="inline-block h-2 w-2 rounded-full"
                style={{ background: color, boxShadow: `0 0 6px ${color}` }}
              />
              {label}
            </span>
          ))}
        </div>
      )}

      {/* ── Hover tooltip ── */}
      {hoveredNode && hovered && (
        <div
          className="pointer-events-none absolute z-10 rounded-md border border-slate-700/70 bg-slate-950/90 px-2.5 py-1.5 font-mono text-[11px] text-slate-200 shadow-xl backdrop-blur-sm"
          style={{ left: hovered.x + 14, top: hovered.y + 10 }}
        >
          <div className="font-semibold text-slate-100">{hoveredNode.label}</div>
          <div className="text-slate-400">
            {hoveredNode.group && <span>{hoveredNode.group} · </span>}
            {hoveredNode.degree} connections
            {hoveredNode.heatEligible && hoveredNode.heat !== undefined && (
              <span> · heat {(hoveredNode.heat * 100).toFixed(0)}%</span>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
