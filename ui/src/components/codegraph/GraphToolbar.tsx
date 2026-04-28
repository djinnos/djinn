/**
 * GraphToolbar — edge-kind, node-kind, and depth filters above the
 * Sigma canvas.
 *
 * Both edge / node kind toggles and the depth slider write straight to
 * the Zustand highlight store; the canvas reducer reads them on every
 * Sigma frame, so toggles take effect immediately without re-mounting
 * the graph.
 *
 * The toolbar shadows the new dark palette (border `#2d2d3d`, near-
 * black background) so it sits flush with the radial-gradient canvas
 * underneath without a visible seam.
 */

import { useCallback } from "react";

import {
  EDGE_KINDS,
  MAX_DEPTH,
  MIN_DEPTH,
  NODE_KINDS,
  SYMBOL_KIND_FILTERS,
  useCodeGraphStore,
} from "@/stores/codeGraphStore";
import { cn } from "@/lib/utils";

const EDGE_LABEL: Record<string, string> = {
  ContainsDefinition: "Contains",
  DeclaredInFile: "Declared",
  FileReference: "FileRef",
  SymbolReference: "Calls/Refs",
  Reads: "Reads",
  Writes: "Writes",
  Extends: "Extends",
  Implements: "Implements",
  TypeDefines: "TypeDef",
  Defines: "Defines",
  EntryPointOf: "EntryPoint",
  MemberOf: "Member",
  StepInProcess: "ProcessStep",
};

const NODE_KIND_LABEL: Record<string, string> = {
  folder: "Folders",
  file: "Files",
  symbol: "Symbols",
};

const SYMBOL_KIND_LABEL: Record<string, string> = {
  class: "Class",
  struct: "Struct",
  interface: "Interface",
  trait: "Trait",
  enum: "Enum",
  function: "Func",
  method: "Method",
  constructor: "Ctor",
  impl: "Impl",
  variable: "Var",
  const: "Const",
  static: "Static",
  property: "Prop",
  import: "Import",
};

interface GraphToolbarProps {
  className?: string;
}

export function GraphToolbar({ className }: GraphToolbarProps) {
  const edgeKindFilters = useCodeGraphStore((s) => s.edgeKindFilters);
  const toggleEdgeKind = useCodeGraphStore((s) => s.toggleEdgeKind);
  const nodeKindFilters = useCodeGraphStore((s) => s.nodeKindFilters);
  const toggleNodeKind = useCodeGraphStore((s) => s.toggleNodeKind);
  const symbolKindFilters = useCodeGraphStore((s) => s.symbolKindFilters);
  const toggleSymbolKind = useCodeGraphStore((s) => s.toggleSymbolKind);
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
        "flex shrink-0 flex-wrap items-center gap-x-4 gap-y-2 border-b border-[#2d2d3d] bg-[#0a0a10]/60 px-4 py-2 backdrop-blur",
        className,
      )}
    >
      <FilterGroup label="Nodes">
        {NODE_KINDS.map((kind) => {
          const active = nodeKindFilters[kind] ?? true;
          return (
            <Chip
              key={kind}
              active={active}
              onClick={() => toggleNodeKind(kind)}
              testId={`node-filter-${kind}`}
              title={kind}
            >
              {NODE_KIND_LABEL[kind] ?? kind}
            </Chip>
          );
        })}
      </FilterGroup>

      <FilterGroup label="Symbols">
        {SYMBOL_KIND_FILTERS.map((kind) => {
          const active = symbolKindFilters[kind] ?? true;
          return (
            <Chip
              key={kind}
              active={active}
              onClick={() => toggleSymbolKind(kind)}
              testId={`symbol-filter-${kind}`}
              title={kind}
            >
              {SYMBOL_KIND_LABEL[kind] ?? kind}
            </Chip>
          );
        })}
      </FilterGroup>

      <FilterGroup label="Edges">
        {EDGE_KINDS.map((kind) => {
          const active = edgeKindFilters[kind] ?? true;
          return (
            <Chip
              key={kind}
              active={active}
              onClick={() => toggleEdgeKind(kind)}
              testId={`edge-filter-${kind}`}
              title={kind}
            >
              {EDGE_LABEL[kind] ?? kind}
            </Chip>
          );
        })}
      </FilterGroup>

      <div className="ml-auto flex items-center gap-2">
        <label
          htmlFor="code-graph-depth"
          className="text-[10px] font-medium uppercase tracking-wide text-zinc-500"
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
          className="h-1 w-24 cursor-pointer accent-emerald-500 disabled:cursor-not-allowed disabled:opacity-50"
        />
        <span className="w-4 text-center text-[11px] tabular-nums text-zinc-200">
          {depthFilter}
        </span>
      </div>
    </div>
  );
}

interface FilterGroupProps {
  label: string;
  children: React.ReactNode;
}

function FilterGroup({ label, children }: FilterGroupProps) {
  return (
    <div className="flex items-center gap-1.5">
      <span className="shrink-0 text-[10px] font-medium uppercase tracking-wide text-zinc-500">
        {label}
      </span>
      <div className="flex flex-wrap items-center gap-1">{children}</div>
    </div>
  );
}

interface ChipProps {
  active: boolean;
  onClick: () => void;
  testId: string;
  title: string;
  children: React.ReactNode;
}

function Chip({ active, onClick, testId, title, children }: ChipProps) {
  return (
    <button
      type="button"
      role="checkbox"
      aria-checked={active}
      data-testid={testId}
      onClick={onClick}
      title={title}
      className={cn(
        "rounded-md border px-2 py-0.5 text-[11px] font-medium transition-colors",
        active
          ? "border-zinc-700 bg-zinc-800/70 text-zinc-100"
          : "border-zinc-800 bg-transparent text-zinc-500 hover:text-zinc-300",
      )}
    >
      {children}
    </button>
  );
}
