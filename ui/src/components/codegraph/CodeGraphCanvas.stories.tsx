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

import { useEffect } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";

import { GraphToolbar } from "./GraphToolbar";
import {
  EMPTY_HIGHLIGHT_VIEW,
  nodeReducer,
  type HighlightView,
} from "@/lib/codeGraphReducers";
import { useCodeGraphStore } from "@/stores/codeGraphStore";

interface StubNode {
  id: string;
  x: number;
  y: number;
  size: number;
  color: string;
  label: string;
}

interface StubEdge {
  source: string;
  target: string;
  kind: string;
}

const NODES: StubNode[] = [
  { id: "alpha", x: 100, y: 100, size: 12, color: "#a3e635", label: "alpha" },
  { id: "beta", x: 220, y: 100, size: 10, color: "#60a5fa", label: "beta" },
  { id: "gamma", x: 160, y: 220, size: 10, color: "#fbbf24", label: "gamma" },
  { id: "delta", x: 300, y: 240, size: 8, color: "#cbd5e1", label: "delta" },
];

const EDGES: StubEdge[] = [
  { source: "alpha", target: "beta", kind: "Reads" },
  { source: "beta", target: "gamma", kind: "Writes" },
  { source: "alpha", target: "gamma", kind: "ContainsDefinition" },
  { source: "gamma", target: "delta", kind: "SymbolReference" },
];

function StubCanvas({ view }: { view: HighlightView }) {
  return (
    <svg
      width={400}
      height={320}
      className="rounded-md border border-[#2d2d3d]"
      style={{ background: "#0a0a10" }}
    >
      {EDGES.map((e, i) => {
        const a = NODES.find((n) => n.id === e.source)!;
        const b = NODES.find((n) => n.id === e.target)!;
        const enabled = view.edgeKindFilters[e.kind] !== false;
        if (!enabled) return null;
        const isSel =
          view.selectionId &&
          (view.selectionId === e.source || view.selectionId === e.target);
        return (
          <line
            key={i}
            x1={a.x}
            y1={a.y}
            x2={b.x}
            y2={b.y}
            stroke={isSel ? "rgba(251,146,60,0.85)" : "rgba(100,116,139,0.45)"}
            strokeWidth={isSel ? 2 : 1}
          />
        );
      })}
      {NODES.map((n) => {
        const out = nodeReducer(
          n.id,
          { color: n.color, size: n.size, label: n.label },
          view,
        );
        if (out.hidden) return null;
        return (
          <g key={n.id}>
            <circle
              cx={n.x}
              cy={n.y}
              r={(out.size as number) ?? n.size}
              fill={(out.color as string) ?? n.color}
              opacity={out.highlighted === false ? 0.4 : 1}
            />
            <text
              x={n.x + ((out.size as number) ?? n.size) + 4}
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
}

function StoryShell({
  selectionId = null,
  selectionNeighbors = [],
  citationIds = [],
  toolHighlightIds = [],
  blastRadiusFrontier = [],
}: StoryShellProps) {
  // Mirror the inputs into the global store so the toolbar reflects
  // the selection state correctly (depth-slider enable etc.).
  const setSelection = useCodeGraphStore((s) => s.setSelection);
  const setCitations = useCodeGraphStore((s) => s.setCitations);
  const setToolHighlight = useCodeGraphStore((s) => s.setToolHighlight);
  const setBlastRadius = useCodeGraphStore((s) => s.setBlastRadiusFrontier);
  useEffect(() => {
    setSelection(selectionId);
    setCitations(citationIds);
    setToolHighlight(toolHighlightIds);
    setBlastRadius(blastRadiusFrontier);
    return () => {
      useCodeGraphStore.getState().reset();
    };
  }, [
    selectionId,
    citationIds,
    toolHighlightIds,
    blastRadiusFrontier,
    setSelection,
    setCitations,
    setToolHighlight,
    setBlastRadius,
  ]);

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
      <StubCanvas view={view} />
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
