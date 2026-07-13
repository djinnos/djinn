/**
 * Memory/MemoryGraphCanvas — the knowledge-graph view.
 *
 * The canvas fetches `memory_graph` via `callMcpTool` and renders it through
 * Sigma/graphology (WebGL) with a ForceAtlas2 layout. Storybook runs in a
 * real browser, so we render the real component and seed a *small* graph by
 * installing a per-story responder on the aliased `@/api/mcpClient` mock (see
 * `.storybook/main.ts`) — no mock server, and no `vi.mock`, which crashes under
 * `storybook dev`. Keeping the graph tiny (8 notes) keeps the layout cheap.
 * `memory_associations` resolves to empty so the co-access toggle is a no-op
 * rather than a fan-out.
 *
 * The `Empty` and `ErrorState` stories drive the overlay paths (which render
 * regardless of WebGL), so the loading/empty/error chrome is covered even if a
 * given environment can't paint the canvas.
 */

import type { Meta, StoryObj } from "@storybook/react-vite";

import { MemoryGraphCanvas } from "./MemoryGraphCanvas";
import { setMcpToolResponder } from "@/storybook-mocks/mcpClient";

// ── Seeded memory_graph payload ──────────────────────────────────────────────

const node = (
  id: string,
  title: string,
  note_type: string,
  connection_count: number,
  extra: { is_orphan?: boolean; broken_targets?: string[] } = {},
) => ({
  id,
  permalink: `memory/${note_type}/${id}`,
  title,
  note_type,
  folder: `memory/${note_type}`,
  connection_count,
  is_orphan: extra.is_orphan ?? false,
  broken_targets: extra.broken_targets ?? [],
});

const graphPayload = {
  nodes: [
    node("oomkill", "Server OOMKill wiped tribunals", "pitfall", 4),
    node("death-chain", "Tribunal death chain", "case", 3),
    node("watchdog", "Refinement watchdog pending time", "pitfall", 2, {
      broken_targets: ["Session timeout budget"],
    }),
    node("advisory-lock", "Coordinator advisory-lock leadership", "adr", 3),
    node("cutover", "Cut-over over strangler", "pattern", 2),
    node("openviking", "OpenViking context DB", "reference", 1),
    node("merge-queue", "Merge queue", "entity", 3),
    node("taskpod-cpu", "Task pods run 4 vCPU", "claim", 1, { is_orphan: true }),
  ],
  edges: [
    { source_id: "oomkill", target_id: "death-chain", raw_text: "Tribunal death chain" },
    { source_id: "oomkill", target_id: "watchdog", raw_text: "Refinement watchdog" },
    { source_id: "death-chain", target_id: "advisory-lock", raw_text: "advisory-lock leadership" },
    { source_id: "advisory-lock", target_id: "merge-queue", raw_text: "Merge queue" },
    { source_id: "cutover", target_id: "merge-queue", raw_text: "Merge queue" },
    { source_id: "merge-queue", target_id: "oomkill", raw_text: "OOMKill" },
  ],
  typed_edges: [
    { source_id: "death-chain", target_id: "oomkill", kind: "builds_on", weight: 0.8 },
    { source_id: "watchdog", target_id: "advisory-lock", kind: "contradicts", weight: 0.5 },
  ],
};

// ── MCP responder. Each story installs its graph payload via `beforeEach`. ───

/**
 * Build a responder for the aliased `@/api/mcpClient` mock. A thrown/rejected
 * error is the only path to the "Couldn't load" overlay — a payload with an
 * `error` field parses to null and renders "empty".
 */
function graphResponder(graphResponse: unknown) {
  return (tool: string) => {
    if (tool === "memory_graph") {
      if (graphResponse instanceof Error) throw graphResponse;
      return graphResponse;
    }
    if (tool === "memory_associations") return { associations: [] };
    return {};
  };
}

function CanvasHarness() {
  return (
    <div className="h-screen w-full">
      <MemoryGraphCanvas projectSlug="djinnos/djinn" />
    </div>
  );
}

const meta = {
  title: "Memory/MemoryGraphCanvas",
  component: CanvasHarness,
  parameters: { layout: "fullscreen" },
} satisfies Meta<typeof CanvasHarness>;

export default meta;
type Story = StoryObj<typeof meta>;

/** Small seeded graph (8 notes): typed + wikilink + broken-target edges. */
export const SmallGraph: Story = {
  beforeEach: () => {
    setMcpToolResponder(graphResponder(graphPayload));
  },
};

/** No notes → the "No notes yet" empty overlay. */
export const Empty: Story = {
  beforeEach: () => {
    setMcpToolResponder(graphResponder({ nodes: [], edges: [] }));
  },
};

/** The fetch rejects → the "Couldn't load the graph" error overlay. */
export const ErrorState: Story = {
  beforeEach: () => {
    setMcpToolResponder(graphResponder(new Error("memory graph unavailable")));
  },
};
