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

import { useCallback, useMemo, useRef, useState } from "react";
import type { ThreeEvent } from "@react-three/fiber";

import { boundsOf } from "./galaxyLayout";
import { GALAXY_INTERACTION_LIMIT, GalaxyScene, type FlyTarget } from "./GalaxyScene";
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
  className?: string;
  onSelect?: (nodeId: string | null) => void;
}

const STELLAR_LEGEND: Array<{ color: string; label: string }> = [
  { color: "#ff6050", label: "leaf" },
  { color: "#ffe080", label: "linked" },
  { color: "#fff4e8", label: "busy" },
  { color: "#a8c0ff", label: "hub" },
  { color: "#80a0ff", label: "core" },
];

const HEAT_LEGEND: Array<{ color: string; label: string }> = [
  { color: "#34d399", label: "tame" },
  { color: "#eab308", label: "warm" },
  { color: "#ef4444", label: "hot" },
];

export function GalaxyCanvas({
  data,
  colorMode = "stellar",
  showLabels = false,
  display = DEFAULT_GALAXY_DISPLAY,
  title,
  className,
  onSelect,
}: GalaxyCanvasProps) {
  const [selectedId, setSelectedId] = useState<string | null>(null);
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
    if (!selectedId) return null;
    const set = new Set<string>([selectedId]);
    for (const n of adjacency.get(selectedId) ?? []) set.add(n);
    return set;
  }, [selectedId, adjacency]);

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
    (index: number | null, event?: ThreeEvent<PointerEvent>) => {
      if (index === null || !event) {
        setHovered(null);
        return;
      }
      const node = data.nodes[index];
      if (!node) return;
      const rect = containerRef.current?.getBoundingClientRect();
      setHovered({
        id: node.id,
        x: event.clientX - (rect?.left ?? 0),
        y: event.clientY - (rect?.top ?? 0),
      });
    },
    [data],
  );

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
  const legend = colorMode === "heat" ? HEAT_LEGEND : STELLAR_LEGEND;
  const interactive = data.nodes.length <= GALAXY_INTERACTION_LIMIT;

  return (
    <div
      ref={containerRef}
      className={`relative h-full w-full overflow-hidden bg-[#04060c] ${className ?? ""}`}
      data-testid="galaxy-canvas"
    >
      <GalaxyScene
        data={data}
        colorMode={colorMode}
        highlight={highlight}
        display={display}
        showLabels={showLabels}
        flyTarget={flyTarget}
        onNodePointer={handlePointer}
        onNodeClick={handleClick}
      />

      {/* ── HUD: stats (top-left) ── */}
      <div className="pointer-events-none absolute left-4 top-4 flex flex-col gap-1.5 font-mono text-[11px] text-slate-400">
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
        {!interactive && (
          <span className="text-slate-500">
            hover/select disabled above{" "}
            {GALAXY_INTERACTION_LIMIT.toLocaleString()} nodes
          </span>
        )}
      </div>

      {/* ── HUD: controls (top-right) ── */}
      <div className="absolute right-4 top-4 flex items-center gap-2">
        {groups.length > 1 && (
          <select
            aria-label="Fly to group"
            defaultValue=""
            onChange={(e) => {
              handleGroupJump(e.target.value);
              e.target.value = "";
            }}
            className="rounded-md border border-slate-700/60 bg-slate-900/80 px-2 py-1 font-mono text-[11px] text-slate-300 backdrop-blur-sm focus:outline-none"
          >
            <option value="" disabled>
              Fly to…
            </option>
            {groups.map(([group, count]) => (
              <option key={group} value={group}>
                {group} ({count})
              </option>
            ))}
          </select>
        )}
        {selectedId && (
          <button
            type="button"
            onClick={() => handleClick(null)}
            className="rounded-md border border-slate-700/60 bg-slate-900/80 px-2 py-1 font-mono text-[11px] text-slate-300 backdrop-blur-sm hover:text-slate-100"
          >
            Clear selection
          </button>
        )}
      </div>

      {/* ── HUD: legend (bottom-right) ── */}
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
