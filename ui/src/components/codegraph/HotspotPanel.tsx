/**
 * HotspotPanel — the galaxy's right-side complexity/co-change leaderboard.
 *
 * Replaces the old heat color mode (a 3D point cloud can't rank): a table
 * of the worst files by hotspot score — worst-function cognitive
 * complexity × commit co-change coupling (proposal qoxm) — with Sonar-band
 * badges (green < 15 ≤ yellow < 25 ≤ red). Clicking a row flies the galaxy
 * camera to the file and its co-change partners and lights their pink
 * coupling web; clicking again clears.
 */

import { useMemo, useState } from "react";

import {
  COMPLEXITY_SEVERE,
  COMPLEXITY_WARN,
  type GalaxyHotspot,
} from "@/lib/codeGraphGalaxyAdapter";
import { cn } from "@/lib/utils";

/** Leaderboard, not a browser: past ~30 rows nothing is a "top" anything.
 * The footer stays honest about what was cut. */
const MAX_ROWS = 30;

type SortKey = "hotspot" | "complexity" | "coupling";

const SORTS: Array<{ key: SortKey; label: string }> = [
  { key: "hotspot", label: "Hotspot" },
  { key: "complexity", label: "Complexity" },
  { key: "coupling", label: "Coupling" },
];

function badgeClass(complexity: number): string {
  if (complexity >= COMPLEXITY_SEVERE)
    return "border-red-500/40 bg-red-500/15 text-red-400";
  if (complexity >= COMPLEXITY_WARN)
    return "border-yellow-500/40 bg-yellow-500/15 text-yellow-400";
  return "border-emerald-500/40 bg-emerald-500/15 text-emerald-400";
}

function basename(path: string): string {
  const segments = path.split("/");
  return segments[segments.length - 1] || path;
}

/** Epoch day → compact age ("today", "12d", "3mo", "2y"). */
function agoLabel(epochDay: number): string {
  const days = Math.max(0, Math.floor(Date.now() / 86_400_000) - epochDay);
  if (days === 0) return "today";
  if (days < 30) return `${days}d`;
  if (days < 365) return `${Math.round(days / 30)}mo`;
  return `${Math.round(days / 365)}y`;
}

export interface HotspotPanelProps {
  /** Pre-ranked rows (snapshotHotspots order = hotspot score). */
  hotspots: GalaxyHotspot[];
  /** fileId of the active row, if any. */
  selectedId: string | null;
  /** Row click — the row itself, or null when toggling the active row off. */
  onSelect: (hotspot: GalaxyHotspot | null) => void;
}

export function HotspotPanel({
  hotspots,
  selectedId,
  onSelect,
}: HotspotPanelProps) {
  const [sortKey, setSortKey] = useState<SortKey>("hotspot");

  const rows = useMemo(() => {
    const sorted = [...hotspots];
    if (sortKey === "complexity") {
      sorted.sort((a, b) => b.complexity - a.complexity || b.score - a.score);
    } else if (sortKey === "coupling") {
      sorted.sort(
        (a, b) =>
          b.coupling - a.coupling ||
          b.partnerIds.length - a.partnerIds.length ||
          b.score - a.score,
      );
    }
    return sorted.slice(0, MAX_ROWS);
  }, [hotspots, sortKey]);

  return (
    <div
      data-testid="hotspot-panel"
      className="flex max-h-full w-80 flex-col overflow-hidden rounded-lg border border-slate-700/60 bg-slate-950/85 font-mono text-[11px] text-slate-300 shadow-xl backdrop-blur-md"
    >
      <div className="flex items-center justify-between gap-2 border-b border-slate-800/70 px-3 py-2">
        <span className="font-semibold tracking-wide text-slate-200">
          Hotspots
        </span>
        <div className="flex items-center gap-1">
          {SORTS.map(({ key, label }) => (
            <button
              key={key}
              type="button"
              aria-pressed={sortKey === key}
              onClick={() => setSortKey(key)}
              className={cn(
                "rounded px-1.5 py-0.5 text-[10px] transition-colors",
                sortKey === key
                  ? "bg-slate-600/40 text-slate-100"
                  : "text-slate-500 hover:text-slate-300",
              )}
            >
              {label}
            </button>
          ))}
        </div>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto">
        {rows.length === 0 ? (
          <p className="px-3 py-4 text-slate-500">
            No scored functions in view — is the graph warmed?
          </p>
        ) : (
          rows.map((h, rank) => {
            const selected = h.fileId === selectedId;
            return (
              <button
                key={h.fileId}
                type="button"
                aria-pressed={selected}
                onClick={() => onSelect(selected ? null : h)}
                className={cn(
                  "block w-full border-b border-slate-800/50 px-3 py-2 text-left transition-colors",
                  selected ? "bg-slate-500/15" : "hover:bg-slate-800/40",
                )}
              >
                <div className="flex items-center gap-2">
                  <span className="w-5 shrink-0 text-right text-slate-600">
                    {rank + 1}
                  </span>
                  <span
                    className="min-w-0 flex-1 truncate text-slate-200"
                    title={h.path}
                  >
                    {basename(h.path)}
                  </span>
                  <span
                    className={cn(
                      "shrink-0 rounded border px-1.5 py-px text-[10px] font-semibold tabular-nums",
                      badgeClass(h.complexity),
                    )}
                    title={`Worst cognitive complexity (${h.functionCount} scored ${h.functionCount === 1 ? "function" : "functions"})`}
                  >
                    {h.complexity}
                  </span>
                </div>
                <div className="flex items-center gap-2 pl-7 text-[10px] text-slate-500">
                  <span className="min-w-0 truncate" title={h.worstSymbol}>
                    {h.worstSymbol}
                  </span>
                  {h.partnerIds.length > 0 && (
                    <span
                      className="ml-auto flex shrink-0 items-center gap-1 text-pink-400/90"
                      title={`Co-changes with ${h.partnerIds.length} ${h.partnerIds.length === 1 ? "file" : "files"} (max coupling ${h.coupling.toFixed(2)})`}
                    >
                      <span className="inline-block h-1.5 w-1.5 rounded-full bg-pink-500/80" />
                      ×{h.partnerIds.length}
                      {h.lastCoChangeDay !== undefined && (
                        <span className="text-slate-500">
                          · {agoLabel(h.lastCoChangeDay)}
                        </span>
                      )}
                    </span>
                  )}
                </div>
              </button>
            );
          })
        )}
      </div>

      {hotspots.length > rows.length && (
        <div className="border-t border-slate-800/70 px-3 py-1.5 text-[10px] text-slate-500">
          top {rows.length} of {hotspots.length.toLocaleString()} scored files
        </div>
      )}
    </div>
  );
}
