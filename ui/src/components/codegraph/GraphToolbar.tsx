/**
 * GraphToolbar — edge-kind checkboxes + depth-filter slider.
 *
 * Sits above the Sigma canvas. Both controls write straight to the
 * Zustand highlight store; the canvas reducer reads back on every
 * Sigma frame, so toggles take effect immediately without re-mounting
 * the graph.
 *
 * Edge-kind labels are intentionally compact (single-token) so the
 * row stays single-line on typical viewports. The full Debug-style
 * RepoGraphEdgeKind name is exposed via `title=` for power users.
 */

import { useCallback } from "react";

import {
  EDGE_KINDS,
  MAX_DEPTH,
  MIN_DEPTH,
  useCodeGraphStore,
} from "@/stores/codeGraphStore";
import { cn } from "@/lib/utils";

/** User-facing labels for the wire-format edge kinds. Keep short. */
const EDGE_LABEL: Record<string, string> = {
  ContainsDefinition: "Contains",
  DeclaredInFile: "Declared",
  FileReference: "FileRef",
  SymbolReference: "References",
  Reads: "Reads",
  Writes: "Writes",
  SymbolRelationshipReference: "Rel: Ref",
  SymbolRelationshipImplementation: "Implements",
  SymbolRelationshipTypeDefinition: "TypeDef",
  SymbolRelationshipDefinition: "Defines",
};

interface GraphToolbarProps {
  /** Optional className passthrough so the parent can position the bar. */
  className?: string;
}

export function GraphToolbar({ className }: GraphToolbarProps) {
  const edgeKindFilters = useCodeGraphStore((s) => s.edgeKindFilters);
  const toggleEdgeKind = useCodeGraphStore((s) => s.toggleEdgeKind);
  const depthFilter = useCodeGraphStore((s) => s.depthFilter);
  const setDepthFilter = useCodeGraphStore((s) => s.setDepthFilter);
  const selectionId = useCodeGraphStore((s) => s.selectionId);

  const handleDepthChange = useCallback(
    (e: React.ChangeEvent<HTMLInputElement>) => {
      setDepthFilter(parseInt(e.target.value, 10));
    },
    [setDepthFilter],
  );

  return (
    <div
      data-testid="graph-toolbar"
      className={cn(
        "flex shrink-0 flex-wrap items-center gap-3 border-b border-border/60 bg-background/40 px-4 py-2",
        className,
      )}
    >
      <span className="shrink-0 text-[10px] font-medium uppercase tracking-wide text-muted-foreground/70">
        Edges
      </span>
      <div className="flex flex-wrap items-center gap-1">
        {EDGE_KINDS.map((kind) => {
          const active = edgeKindFilters[kind] ?? true;
          return (
            <button
              key={kind}
              type="button"
              role="checkbox"
              aria-checked={active}
              data-testid={`edge-filter-${kind}`}
              onClick={() => toggleEdgeKind(kind)}
              title={kind}
              className={cn(
                "rounded-md border px-2 py-0.5 text-[11px] font-medium transition-colors",
                active
                  ? "border-border/70 bg-background text-foreground"
                  : "border-border/30 bg-muted/20 text-muted-foreground/60",
              )}
            >
              {EDGE_LABEL[kind] ?? kind}
            </button>
          );
        })}
      </div>
      <div className="ml-auto flex items-center gap-2">
        <label
          htmlFor="code-graph-depth"
          className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground/70"
          title={
            selectionId
              ? "Hop depth from the selected node"
              : "Select a node first to apply depth filtering"
          }
        >
          Depth
        </label>
        <input
          id="code-graph-depth"
          type="range"
          min={MIN_DEPTH}
          max={MAX_DEPTH}
          step={1}
          value={depthFilter}
          onChange={handleDepthChange}
          disabled={!selectionId}
          data-testid="depth-slider"
          className="h-1 w-24 cursor-pointer accent-primary disabled:cursor-not-allowed disabled:opacity-50"
        />
        <span className="w-4 text-center text-[11px] tabular-nums text-foreground">
          {depthFilter}
        </span>
      </div>
    </div>
  );
}
