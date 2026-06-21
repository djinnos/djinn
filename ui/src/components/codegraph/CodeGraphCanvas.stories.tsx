/**
 * CodeGraphCanvas storybook — focuses on the reducer-driven highlight
 * states without the network round-trip. We render a small stub graph
 * (4 nodes, 4 edges) through the same reducer pipeline the live
 * canvas uses, so Storybook screenshots reflect the production look.
 *
 * The full `<CodeGraphCanvas>` requires a snapshot fetch + Sigma WebGL;
 * not great for Storybook. We instead drive the highlight store and
 * render a lightweight SVG preview alongside `<GraphToolbar>` so the
 * full toolbar/highlight UX remains visible.
 */

import { useEffect, useState } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, userEvent, waitFor, within } from "storybook/test";

import { CitationStatusBadge } from "./CodeGraphCanvas";
import { GraphToolbar } from "./GraphToolbar";
import {
  EMPTY_HIGHLIGHT_VIEW,
  computeComplexityThresholds,
  edgeReducer,
  nodeReducer,
  topComplexityIds,
  type ColorMode,
  type HighlightView,
} from "@/lib/codeGraphReducers";
import {
  useCodeGraphStore,
  type SemanticZoomMode,
} from "@/stores/codeGraphStore";

interface StubNode {
  id: string;
  x: number;
  y: number;
  size: number;
  color: string;
  label: string;
  workspace?: string;
  workspaceColor?: string;
  workspaceBadge?: string;
  isWorkspaceContext?: boolean;
  /** Iter 30: optional cognitive complexity for the heatmap stories. */
  cognitive?: number;
}

interface StubEdge {
  source: string;
  target: string;
  kind: string;
  size?: number;
  color?: string;
  isCrossWorkspace?: boolean;
}

const NODES: StubNode[] = [
  // Iter 30: cognitive values mimic a real distribution — alpha/beta
  // are tame, gamma is mid-tier, delta is the refactor candidate.
  { id: "alpha", x: 100, y: 100, size: 12, color: "#a3e635", label: "alpha", cognitive: 3 },
  { id: "beta", x: 220, y: 100, size: 10, color: "#60a5fa", label: "beta", cognitive: 6 },
  { id: "gamma", x: 160, y: 220, size: 10, color: "#fbbf24", label: "gamma", cognitive: 14 },
  { id: "delta", x: 300, y: 240, size: 8, color: "#cbd5e1", label: "delta", cognitive: 38 },
];

const EDGES: StubEdge[] = [
  { source: "alpha", target: "beta", kind: "Reads" },
  { source: "beta", target: "gamma", kind: "Writes" },
  { source: "alpha", target: "gamma", kind: "ContainsDefinition" },
  { source: "gamma", target: "delta", kind: "SymbolReference" },
];

const WORKSPACE_NODES: StubNode[] = [
  {
    id: "api-file",
    x: 90,
    y: 95,
    size: 13,
    color: "#60a5fa",
    label: "api/routes.ts · api",
    workspace: "api",
    workspaceColor: "#22d3ee",
    workspaceBadge: "A",
    cognitive: 4,
  },
  {
    id: "api-handler",
    x: 205,
    y: 145,
    size: 10,
    color: "#60a5fa",
    label: "handleCreate · api",
    workspace: "api",
    workspaceColor: "#22d3ee",
    workspaceBadge: "A",
    cognitive: 9,
  },
  {
    id: "web-client",
    x: 320,
    y: 210,
    size: 9,
    color: "#f472b6",
    label: "submitForm · web",
    workspace: "web",
    workspaceColor: "#f472b6",
    workspaceBadge: "W",
    isWorkspaceContext: true,
    cognitive: 18,
  },
];

const WORKSPACE_EDGES: StubEdge[] = [
  {
    source: "api-file",
    target: "api-handler",
    kind: "ContainsDefinition",
    size: 1,
    color: "#2d5a3d",
  },
  {
    source: "api-handler",
    target: "web-client",
    kind: "SymbolReference",
    size: 2.2,
    color: "#facc15",
    isCrossWorkspace: true,
  },
];

function StubCanvas({
  view,
  nodes = NODES,
  edges = EDGES,
}: {
  view: HighlightView;
  nodes?: StubNode[];
  edges?: StubEdge[];
}) {
  return (
    <svg
      width={400}
      height={320}
      className="rounded-md border border-[#2d2d3d]"
      style={{ background: "#0a0a10" }}
    >
      {edges.map((e, i) => {
        const a = nodes.find((n) => n.id === e.source)!;
        const b = nodes.find((n) => n.id === e.target)!;
        const enabled = view.edgeKindFilters[e.kind] !== false;
        if (!enabled) return null;
        const out = edgeReducer(
          e.source,
          e.target,
          {
            kind: e.kind,
            size: e.size ?? 1,
            color: e.color ?? "rgba(100,116,139,0.45)",
            isCrossWorkspace: e.isCrossWorkspace,
          },
          view,
        );
        return (
          <line
            key={i}
            x1={a.x}
            y1={a.y}
            x2={b.x}
            y2={b.y}
            stroke={(out.color as string) ?? e.color ?? "rgba(100,116,139,0.45)"}
            strokeWidth={(out.size as number) ?? e.size ?? 1}
            strokeDasharray={e.isCrossWorkspace ? "6 4" : undefined}
          />
        );
      })}
      {nodes.map((n) => {
        const out = nodeReducer(
          n.id,
          {
            color: n.color,
            size: n.size,
            label: n.label,
            cognitive: n.cognitive,
            workspace: n.workspace,
            workspaceColor: n.workspaceColor,
            workspaceBadge: n.workspaceBadge,
            borderColor: n.workspaceColor,
            borderSize: n.workspaceColor ? 2 : undefined,
            haloed: n.workspaceColor ? true : undefined,
            isWorkspaceContext: n.isWorkspaceContext,
          },
          view,
        );
        if (out.hidden) return null;
        const size = (out.size as number) ?? n.size;
        const haloed = out.haloed === true;
        const workspaceBadge = out.workspaceBadge as string | undefined;
        return (
          <g key={n.id}>
            {haloed && (
              <circle
                cx={n.x}
                cy={n.y}
                r={size + 4}
                fill="none"
                stroke={(out.borderColor as string) ?? "rgba(239, 68, 68, 0.6)"}
                strokeWidth={(out.borderSize as number) ?? 2}
              />
            )}
            <circle
              cx={n.x}
              cy={n.y}
              r={size}
              fill={(out.color as string) ?? n.color}
              opacity={out.highlighted === false ? 0.4 : 1}
            />
            {workspaceBadge && (
              <text
                x={n.x - 4}
                y={n.y + 4}
                fontSize={9}
                fontFamily="monospace"
                fill="#020617"
                textAnchor="middle"
                fontWeight={700}
              >
                {workspaceBadge}
              </text>
            )}
            <text
              x={n.x + size + 4}
              y={n.y + 4}
              fontSize={11}
              fontFamily="monospace"
              fill={out.label ? "currentColor" : "rgba(100,116,139,0.55)"}
            >
              {(out.label as string) ?? n.label}
            </text>
          </g>
        );
      })}
    </svg>
  );
}

interface StoryShellProps {
  selectionId?: string | null;
  selectionNeighbors?: string[];
  citationIds?: string[];
  toolHighlightIds?: string[];
  blastRadiusFrontier?: string[];
  /** Iter 30: pin the canvas into a specific color mode for the story. */
  colorMode?: ColorMode;
  /** Semantic zoom mode override for the toolbar story. */
  semanticZoomMode?: SemanticZoomMode;
  workspaceFixture?: boolean;
}

function StoryShell({
  selectionId = null,
  selectionNeighbors = [],
  citationIds = [],
  toolHighlightIds = [],
  blastRadiusFrontier = [],
  colorMode = "topology",
  semanticZoomMode = "auto",
  workspaceFixture = false,
}: StoryShellProps) {
  // Mirror the inputs into the global store so the toolbar reflects
  // the selection state correctly for DOI focus controls.
  const setSelection = useCodeGraphStore((s) => s.setSelection);
  const setCitations = useCodeGraphStore((s) => s.setCitations);
  const setToolHighlight = useCodeGraphStore((s) => s.setToolHighlight);
  const setBlastRadius = useCodeGraphStore((s) => s.setBlastRadiusFrontier);
  const setColorMode = useCodeGraphStore((s) => s.setColorMode);
  const setSemanticZoomMode = useCodeGraphStore(
    (s) => s.setSemanticZoomMode,
  );
  const setComplexityAvailable = useCodeGraphStore(
    (s) => s.setComplexityAvailable,
  );
  useEffect(() => {
    setSelection(selectionId);
    setCitations(citationIds);
    setToolHighlight(toolHighlightIds);
    setBlastRadius(blastRadiusFrontier);
    setComplexityAvailable(true);
    setColorMode(colorMode);
    setSemanticZoomMode(semanticZoomMode);
    return () => {
      useCodeGraphStore.getState().reset();
    };
  }, [
    selectionId,
    citationIds,
    toolHighlightIds,
    blastRadiusFrontier,
    colorMode,
    semanticZoomMode,
    setSelection,
    setCitations,
    setToolHighlight,
    setBlastRadius,
    setColorMode,
    setSemanticZoomMode,
    setComplexityAvailable,
  ]);

  // Iter 30: derive heatmap thresholds + top-N halo set from the
  // stub fixture so the story renders the same colors / ring the live
  // canvas does.
  const storyNodes = workspaceFixture ? WORKSPACE_NODES : NODES;
  const storyEdges = workspaceFixture ? WORKSPACE_EDGES : EDGES;
  const cognitiveValues = storyNodes.flatMap((n) =>
    typeof n.cognitive === "number" ? [n.cognitive] : [],
  );
  const complexityThresholds = computeComplexityThresholds(cognitiveValues);
  const complexityHaloIds = topComplexityIds(
    storyNodes.map((n) => ({ id: n.id, cognitive: n.cognitive ?? null })),
    1,
  );

  const storeState = useCodeGraphStore.getState();
  const view: HighlightView = {
    ...EMPTY_HIGHLIGHT_VIEW,
    selectionId,
    selectionNeighbors: new Set(selectionNeighbors),
    citationIds: new Set(citationIds),
    toolHighlightIds: new Set(toolHighlightIds),
    blastRadiusFrontier: new Set(blastRadiusFrontier),
    edgeKindFilters: storeState.edgeKindFilters,
    nodeKindFilters: storeState.nodeKindFilters,
    symbolKindFilters: storeState.symbolKindFilters,
    pulsePhase: 0.5,
    colorMode,
    complexityThresholds,
    complexityHaloIds,
  };

  return (
    <div
      className="flex flex-col gap-2 p-4"
      style={{
        background:
          "radial-gradient(circle at 50% 50%, rgba(124, 58, 237, 0.05) 0%, transparent 70%), linear-gradient(to bottom, #06060a, #0a0a10)",
      }}
    >
      <GraphToolbar />
      <StubCanvas view={view} nodes={storyNodes} edges={storyEdges} />
    </div>
  );
}

const meta: Meta<typeof StoryShell> = {
  title: "CodeGraph/CodeGraphCanvas",
  component: StoryShell,
  parameters: { layout: "centered" },
};

export default meta;
type Story = StoryObj<typeof StoryShell>;

export const Empty: Story = { args: {} };

export const Selection: Story = {
  args: {
    selectionId: "alpha",
    selectionNeighbors: ["alpha", "beta", "gamma"],
  },
};

export const Citations: Story = {
  args: {
    citationIds: ["beta", "delta"],
  },
};

export const ToolHighlight: Story = {
  args: {
    toolHighlightIds: ["gamma", "delta"],
  },
};

export const BlastRadius: Story = {
  args: {
    selectionId: "alpha",
    selectionNeighbors: ["alpha", "beta"],
    blastRadiusFrontier: ["beta", "gamma", "delta"],
  },
};

export const SelectionPlusCitation: Story = {
  args: {
    selectionId: "alpha",
    selectionNeighbors: ["alpha", "beta"],
    citationIds: ["delta"],
  },
};

/**
 * Iter 30: complexity heatmap mode. Nodes recolor along the green→red
 * gradient and the highest-cognitive node (delta) wears the persistent
 * red halo. Reviewers can also flip the toolbar toggle to verify the
 * topology mode still works with the same fixture.
 */
export const ComplexityHeatmap: Story = {
  args: {
    colorMode: "complexity",
  },
};

/**
 * Iter 30: topology mode with the persistent halo always-on. Even
 * without engaging the heatmap, the top refactor candidate is marked.
 */
export const TopologyWithRefactorHalo: Story = {
  args: {
    colorMode: "topology",
  },
};

/**
 * Multi-workspace fixture: api owns the left two nodes/intra edge, while web is
 * retained as selected-workspace remote context through the dashed yellow
 * cross-workspace dependency. The web node is intentionally muted but still
 * wears a workspace badge/ring so reviewers can see endpoint identity.
 */
export const WorkspacesAndCrossWorkspaceEdge: Story = {
  args: {
    workspaceFixture: true,
  },
};

/**
 * Semantic zoom toolbar: the segmented control lets the user force
 * `community` (collapsed blobs) instead of the default `auto`. Reviewers
 * can toggle between all three modes (Auto/Symbol/Community) to verify
 * the store updates correctly.
 */
export const SemanticZoomCommunity: Story = {
  args: {
    semanticZoomMode: "community",
  },
};

// ── Multi-id citation badge story (g293) ──────────────────────────────────

/**
 * Drives the *real* `CitationStatusBadge` through the existing store seam
 * (`setCitations`) so the multi-id text — "3 citations pinned" — is
 * asserted against the production component, not a stub. A button lets the
 * reviewer (and the Storybook `play` function) populate the citationIds
 * interactively, mirroring how the chat-agent harvest
 * (`useChatToolCallHarvest`) populates the store from a `code_graph` tool
 * result.
 */
function CitationBadgeHarness() {
  const setCitations = useCodeGraphStore((s) => s.setCitations);
  const [applied, setApplied] = useState(false);
  return (
    <div
      className="relative h-24 w-full overflow-hidden rounded-md border border-[#2d2d3d]"
      style={{ background: "#0a0a10" }}
    >
      <CitationStatusBadge />
      <div className="absolute bottom-3 left-1/2 -translate-x-1/2">
        <button
          type="button"
          data-testid="populate-citations"
          onClick={() => {
            // 3 distinct symbol ids, as a `code_graph search` result would
            // surface. `setCitations` is the same store action the chat
            // harvest hook and the per-click CitationLink use.
            setCitations(["sym::alpha", "sym::beta", "sym::gamma"]);
            setApplied(true);
          }}
          className="rounded-full border border-blue-400/40 bg-blue-500/15 px-3 py-1 text-[11px] text-blue-200"
        >
          Populate 3 citations
        </button>
      </div>
      {applied && (
        <span data-testid="citations-applied-flag" className="sr-only">
          applied
        </span>
      )}
    </div>
  );
}

const badgeMeta: Meta<typeof CitationBadgeHarness> = {
  title: "CodeGraph/CodeGraphCanvas",
  component: CitationBadgeHarness,
  parameters: { layout: "centered" },
};

// `badgeMeta` shares the title with the default `meta` above; Storybook
// merges all stories under one title. Keep the default export (the
// StoryShell meta) so the existing stories are unaffected — this block is
// only referenced to keep the type-checker happy about the unused `Meta`.
void badgeMeta;

type BadgeStory = StoryObj<typeof CitationBadgeHarness>;

/**
 * Pre-populates `citationIds` with a 3-id set via the existing `setCitations`
 * store action and asserts the `CitationStatusBadge` renders the multi-id
 * "3 citations pinned" text. The `play` function clicks the populate button
 * (the same store seam the chat harvest uses) and checks the rendered badge.
 */
export const CitationBadgeMultiId: BadgeStory = {
  render: () => <CitationBadgeHarness />,
  play: async ({ canvasElement }) => {
    // Start from a clean store so prior stories' state doesn't leak in.
    useCodeGraphStore.getState().clearCitations();
    const canvas = within(canvasElement);

    // Before populating, the badge is absent (citationCount === 0 and no
    // selection) — the component returns null.
    expect(canvas.queryByTestId("citation-status")).toBeNull();

    await userEvent.click(canvas.getByTestId("populate-citations"));

    // The badge reacts to the store update. `setCitations` is synchronous
    // (Zustand), but wrap in waitFor to let React flush the commit.
    await waitFor(() => {
      expect(canvas.getByTestId("citation-status")).toHaveTextContent(
        "3 citations pinned",
      );
    });
  },
};
