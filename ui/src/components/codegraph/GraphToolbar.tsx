/**
 * GraphToolbar — edge-kind, node-kind, DOI focus, and lens controls above the
 * Sigma canvas.
 *
 * Store-backed controls write straight to the Zustand highlight store; the
 * canvas reducer reads relevant slices on every Sigma frame, so toggles take
 * effect immediately without re-mounting the graph.
 *
 * The toolbar shadows the new dark palette (border `#2d2d3d`, near-
 * black background) so it sits flush with the radial-gradient canvas
 * underneath without a visible seam.
 */
import {
  EDGE_KINDS,
  MAX_DOI_REVEAL_COUNT,
  MIN_DOI_REVEAL_COUNT,
  NODE_KINDS,
  SYMBOL_KIND_FILTERS,
  useCodeGraphStore,
  type ColorMode,
  type FocusDirection,
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
}

export function GraphToolbar({
  className,
}: GraphToolbarProps) {
  const edgeKindFilters = useCodeGraphStore((s) => s.edgeKindFilters);
  const toggleEdgeKind = useCodeGraphStore((s) => s.toggleEdgeKind);
  const nodeKindFilters = useCodeGraphStore((s) => s.nodeKindFilters);
  const toggleNodeKind = useCodeGraphStore((s) => s.toggleNodeKind);
  const symbolKindFilters = useCodeGraphStore((s) => s.symbolKindFilters);
  const toggleSymbolKind = useCodeGraphStore((s) => s.toggleSymbolKind);
  const hideTests = useCodeGraphStore((s) => s.hideTests);
  const setHideTests = useCodeGraphStore((s) => s.setHideTests);
  const focusAnchorId = useCodeGraphStore((s) => s.focusAnchorId);
  const setFocusAnchor = useCodeGraphStore((s) => s.setFocusAnchor);
  const focusDirection = useCodeGraphStore((s) => s.focusDirection);
  const setFocusDirection = useCodeGraphStore((s) => s.setFocusDirection);
  const doiRevealCount = useCodeGraphStore((s) => s.doiRevealCount);
  const setDoiRevealCount = useCodeGraphStore((s) => s.setDoiRevealCount);
  const selectionId = useCodeGraphStore((s) => s.selectionId);
  const colorMode = useCodeGraphStore((s) => s.colorMode);
  const setColorMode = useCodeGraphStore((s) => s.setColorMode);
  const complexityAvailable = useCodeGraphStore((s) => s.complexityAvailable);
  const semanticZoomMode = useCodeGraphStore((s) => s.semanticZoomMode);
  const setSemanticZoomMode = useCodeGraphStore(
    (s) => s.setSemanticZoomMode,
  );
  const graphReady = useCodeGraphStore((s) => s.graphReady);

  const disabled = !graphReady;

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
        <ColorModeToggle
          mode={colorMode}
          onChange={setColorMode}
          disabled={!complexityAvailable}
        />
        <FocusDirectionToggle
          direction={focusDirection}
          onChange={setFocusDirection}
          disabled={disabled}
        />
        <DoiRevealControl
          count={doiRevealCount}
          onChange={setDoiRevealCount}
          disabled={disabled}
        />
        <button
          type="button"
          data-testid="focus-anchor-toggle"
          disabled={disabled || !selectionId}
          onClick={() =>
            setFocusAnchor(focusAnchorId === selectionId ? null : selectionId)
          }
          title={
            selectionId
              ? focusAnchorId === selectionId
                ? "Clear the DOI focus anchor"
                : "Use the selected node as the DOI focus anchor"
              : "Select a node before anchoring DOI focus"
          }
          className={cn(
            "rounded-md border px-2 py-0.5 text-[11px] font-medium transition-colors",
            focusAnchorId
              ? "border-emerald-700/70 bg-emerald-950/50 text-emerald-200"
              : "border-zinc-800 bg-transparent text-zinc-400 hover:text-zinc-200",
            (disabled || !selectionId) &&
              "cursor-not-allowed opacity-50 hover:text-zinc-400",
          )}
        >
          {focusAnchorId ? "Focus set" : "Anchor focus"}
        </button>
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

interface FocusDirectionToggleProps {
  direction: FocusDirection;
  onChange: (direction: FocusDirection) => void;
  disabled: boolean;
}

function FocusDirectionToggle({
  direction,
  onChange,
  disabled,
}: FocusDirectionToggleProps) {
  return (
    <div className="flex items-center gap-1.5" data-testid="focus-direction-toggle">
      <span className="text-[10px] font-medium uppercase tracking-wide text-zinc-500">
        Focus
      </span>
      <div
        role="radiogroup"
        aria-label="DOI focus direction"
        className={cn(
          "flex items-center rounded-md border border-zinc-800 bg-[#0a0a10]/40 p-0.5",
          disabled && "opacity-50",
        )}
      >
        <FocusDirectionButton
          active={direction === "dependencies"}
          disabled={disabled}
          onClick={() => onChange("dependencies")}
          testId="focus-direction-dependencies"
          label="Deps"
          tooltip="Prioritize dependencies: what the focus anchor uses"
        />
        <FocusDirectionButton
          active={direction === "dependents"}
          disabled={disabled}
          onClick={() => onChange("dependents")}
          testId="focus-direction-dependents"
          label="Impact"
          tooltip="Prioritize dependents: what uses the focus anchor"
        />
        <FocusDirectionButton
          active={direction === "both"}
          disabled={disabled}
          onClick={() => onChange("both")}
          testId="focus-direction-both"
          label="Both"
          tooltip="Prioritize dependencies and dependents around the focus anchor"
        />
      </div>
    </div>
  );
}

interface FocusDirectionButtonProps {
  active: boolean;
  disabled: boolean;
  onClick: () => void;
  testId: string;
  label: string;
  tooltip: string;
}

function FocusDirectionButton({
  active,
  disabled,
  onClick,
  testId,
  label,
  tooltip,
}: FocusDirectionButtonProps) {
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

interface DoiRevealControlProps {
  count: number;
  onChange: (count: number) => void;
  disabled: boolean;
}

function DoiRevealControl({
  count,
  onChange,
  disabled,
}: DoiRevealControlProps) {
  return (
    <label
      className="flex items-center gap-1.5"
      data-testid="doi-reveal-control"
    >
      <span
        className="text-[10px] font-medium uppercase tracking-wide text-zinc-500"
        title="Maximum number of high-DOI context nodes to reveal"
      >
        DOI
      </span>
      <input
        type="number"
        min={MIN_DOI_REVEAL_COUNT}
        max={MAX_DOI_REVEAL_COUNT}
        step={5}
        value={count}
        disabled={disabled}
        onChange={(e) => onChange(e.currentTarget.valueAsNumber)}
        aria-label="DOI reveal count"
        className="h-6 w-14 rounded-md border border-zinc-800 bg-[#0a0a10]/60 px-1.5 text-right text-[11px] tabular-nums text-zinc-200 disabled:cursor-not-allowed disabled:opacity-50"
      />
    </label>
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
 * sits alongside the DOI focus controls without breaking layout.
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
