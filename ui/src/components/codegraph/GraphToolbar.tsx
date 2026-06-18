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
  type ColorMode,
  type LayoutMode,
  type SemanticZoomMode,
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
  community: "Communities",
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
  type: "Type",
  field: "Field",
  variable: "Var",
  const: "Const",
  static: "Static",
  property: "Prop",
  import: "Import",
  other: "Other",
};

interface GraphToolbarProps {
  className?: string;
  disabled?: boolean;
}

export function GraphToolbar({
  className,
  disabled = false,
}: GraphToolbarProps) {
  const edgeKindFilters = useCodeGraphStore((s) => s.edgeKindFilters);
  const toggleEdgeKind = useCodeGraphStore((s) => s.toggleEdgeKind);
  const nodeKindFilters = useCodeGraphStore((s) => s.nodeKindFilters);
  const toggleNodeKind = useCodeGraphStore((s) => s.toggleNodeKind);
  const symbolKindFilters = useCodeGraphStore((s) => s.symbolKindFilters);
  const toggleSymbolKind = useCodeGraphStore((s) => s.toggleSymbolKind);
  const hideTests = useCodeGraphStore((s) => s.hideTests);
  const setHideTests = useCodeGraphStore((s) => s.setHideTests);
  const depthFilter = useCodeGraphStore((s) => s.depthFilter);
  const setDepthFilter = useCodeGraphStore((s) => s.setDepthFilter);
  const selectionId = useCodeGraphStore((s) => s.selectionId);
  const colorMode = useCodeGraphStore((s) => s.colorMode);
  const setColorMode = useCodeGraphStore((s) => s.setColorMode);
  const layoutMode = useCodeGraphStore((s) => s.layoutMode);
  const setLayoutMode = useCodeGraphStore((s) => s.setLayoutMode);
  const complexityAvailable = useCodeGraphStore((s) => s.complexityAvailable);
  const semanticZoomMode = useCodeGraphStore((s) => s.semanticZoomMode);
  const setSemanticZoomMode = useCodeGraphStore(
    (s) => s.setSemanticZoomMode,
  );

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

      <FilterGroup label="Tests">
        <Chip
          active={hideTests}
          onClick={() => setHideTests(!hideTests)}
          testId="tests-hide-toggle"
          title={
            hideTests
              ? "Showing production only — click to include test files & symbols"
              : "Showing the whole graph — click to hide test files & symbols"
          }
        >
          Hide tests
        </Chip>
      </FilterGroup>

      <div className="ml-auto flex items-center gap-3">
        <SemanticZoomToggle
          mode={semanticZoomMode}
          onChange={setSemanticZoomMode}
        />
        <LayoutModeToggle
          mode={layoutMode}
          onChange={setLayoutMode}
          disabled={disabled}
        />
        <ColorModeToggle
          mode={colorMode}
          onChange={setColorMode}
          disabled={!complexityAvailable}
        />
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

interface LayoutModeToggleProps {
  mode: LayoutMode;
  onChange: (mode: LayoutMode) => void;
  disabled: boolean;
}

function LayoutModeToggle({
  mode,
  onChange,
  disabled,
}: LayoutModeToggleProps) {
  return (
    <div className="flex items-center gap-1.5" data-testid="layout-mode-toggle">
      <span className="text-[10px] font-medium uppercase tracking-wide text-zinc-500">
        Layout
      </span>
      <div
        role="radiogroup"
        aria-label="Layout mode"
        className={cn(
          "flex items-center rounded-md border border-zinc-800 bg-[#0a0a10]/40 p-0.5",
          disabled && "opacity-50",
        )}
      >
        <LayoutModeButton
          active={mode === "force"}
          disabled={disabled}
          onClick={() => onChange("force")}
          testId="layout-mode-force"
          label="Force"
          tooltip="ForceAtlas2 simulation — physics-based settling, slowest on large repos"
        />
        <LayoutModeButton
          active={mode === "sequential"}
          disabled={disabled}
          onClick={() => onChange("sequential")}
          testId="layout-mode-sequential"
          label="Sequential"
          tooltip="Layered (folder → file → symbol) — deterministic, no physics"
        />
        <LayoutModeButton
          active={mode === "radial"}
          disabled={disabled}
          onClick={() => onChange("radial")}
          testId="layout-mode-radial"
          label="Radial"
          tooltip="Concentric shells from the highest-PageRank focal — deterministic, no physics"
        />
      </div>
    </div>
  );
}

interface LayoutModeButtonProps {
  active: boolean;
  disabled: boolean;
  onClick: () => void;
  testId: string;
  label: string;
  tooltip: string;
}

function LayoutModeButton({
  active,
  disabled,
  onClick,
  testId,
  label,
  tooltip,
}: LayoutModeButtonProps) {
  return (
    <button
      type="button"
      role="radio"
      aria-checked={active}
      aria-disabled={disabled}
      disabled={disabled}
      data-testid={testId}
      onClick={onClick}
      title={tooltip}
      className={cn(
        "rounded px-2 py-0.5 text-[11px] font-medium transition-colors",
        active
          ? "bg-zinc-800/80 text-zinc-100"
          : "text-zinc-400 hover:text-zinc-200",
        disabled && "cursor-not-allowed hover:text-zinc-400",
      )}
    >
      {label}
    </button>
  );
}

interface ColorModeToggleProps {
  mode: ColorMode;
  onChange: (mode: ColorMode) => void;
  /**
   * `true` when the current snapshot has zero function nodes carrying
   * a `cognitive` value — the heatmap would be degenerate, so we
   * disable the toggle and surface a tooltip explaining why.
   */
  disabled: boolean;
}

/**
 * Iter 30: segmented control swapping between topology coloring (the
 * default dir-hash / community palette) and the cognitive-complexity
 * heatmap. Sized to fit the existing toolbar's vertical rhythm so it
 * sits next to the depth slider without breaking layout.
 */
function ColorModeToggle({ mode, onChange, disabled }: ColorModeToggleProps) {
  return (
    <div className="flex items-center gap-1.5" data-testid="color-mode-toggle">
      <span className="text-[10px] font-medium uppercase tracking-wide text-zinc-500">
        Color
      </span>
      <div
        role="radiogroup"
        aria-label="Color mode"
        className={cn(
          "flex items-center rounded-md border border-zinc-800 bg-[#0a0a10]/40 p-0.5",
          disabled && "opacity-50",
        )}
      >
        <ColorModeButton
          active={mode === "topology"}
          disabled={false}
          onClick={() => onChange("topology")}
          testId="color-mode-topology"
          label="Topology"
          tooltip="Color nodes by parent directory / community"
        />
        <ColorModeButton
          active={mode === "complexity"}
          disabled={disabled}
          onClick={() => onChange("complexity")}
          testId="color-mode-complexity"
          label="Complexity"
          tooltip={
            disabled
              ? "No complexity data — graph not yet warmed for languages in the walker"
              : "Color nodes by cognitive-complexity percentile"
          }
        />
      </div>
    </div>
  );
}

interface ColorModeButtonProps {
  active: boolean;
  disabled: boolean;
  onClick: () => void;
  testId: string;
  label: string;
  tooltip: string;
}

function ColorModeButton({
  active,
  disabled,
  onClick,
  testId,
  label,
  tooltip,
}: ColorModeButtonProps) {
  return (
    <button
      type="button"
      role="radio"
      aria-checked={active}
      aria-disabled={disabled}
      disabled={disabled}
      data-testid={testId}
      onClick={onClick}
      title={tooltip}
      className={cn(
        "rounded px-2 py-0.5 text-[11px] font-medium transition-colors",
        active
          ? "bg-zinc-800/80 text-zinc-100"
          : "text-zinc-400 hover:text-zinc-200",
        disabled && "cursor-not-allowed hover:text-zinc-400",
      )}
    >
      {label}
    </button>
  );
}

interface SemanticZoomToggleProps {
  mode: SemanticZoomMode;
  onChange: (mode: SemanticZoomMode) => void;
}

/**
 * Segmented control for semantic zoom: Auto (let the canvas pick the
 * level based on node count), Symbol (force individual symbol nodes),
 * or Community (force collapsed community blobs). This only sets state;
 * the canvas reads it when deciding the fetch level.
 */
function SemanticZoomToggle({ mode, onChange }: SemanticZoomToggleProps) {
  return (
    <div className="flex items-center gap-1.5" data-testid="semantic-zoom-toggle">
      <span className="text-[10px] font-medium uppercase tracking-wide text-zinc-500">
        Zoom
      </span>
      <div
        role="radiogroup"
        aria-label="Semantic zoom"
        className="flex items-center rounded-md border border-zinc-800 bg-[#0a0a10]/40 p-0.5"
      >
        <ZoomModeButton
          active={mode === "auto"}
          onClick={() => onChange("auto")}
          testId="semantic-zoom-auto"
          label="Auto"
          tooltip="Let the canvas choose symbol vs community based on graph size"
        />
        <ZoomModeButton
          active={mode === "symbol"}
          onClick={() => onChange("symbol")}
          testId="semantic-zoom-symbol"
          label="Symbol"
          tooltip="Force individual symbol-level nodes"
        />
        <ZoomModeButton
          active={mode === "community"}
          onClick={() => onChange("community")}
          testId="semantic-zoom-community"
          label="Community"
          tooltip="Force collapsed community-level blobs"
        />
      </div>
    </div>
  );
}

interface ZoomModeButtonProps {
  active: boolean;
  onClick: () => void;
  testId: string;
  label: string;
  tooltip: string;
}

function ZoomModeButton({
  active,
  onClick,
  testId,
  label,
  tooltip,
}: ZoomModeButtonProps) {
  return (
    <button
      type="button"
      role="radio"
      aria-checked={active}
      data-testid={testId}
      onClick={onClick}
      title={tooltip}
      className={cn(
        "rounded px-2 py-0.5 text-[11px] font-medium transition-colors",
        active
          ? "bg-zinc-800/80 text-zinc-100"
          : "text-zinc-400 hover:text-zinc-200",
      )}
    >
      {label}
    </button>
  );
}
