/**
 * CodeGraphCanvas storybook — renders the *real* Sigma/WebGL canvas.
 *
 * Storybook runs in a real browser, so we mount the production
 * `<CodeGraphCanvas>` and seed it with a small hand-authored snapshot
 * through the aliased `@/api/mcpClient` mock (see `.storybook/main.ts`).
 * `fetchSnapshot` dispatches `callMcpTool("code_graph", { operation:
 * "snapshot", … })`, so a per-story `setMcpToolResponder` that answers the
 * snapshot op is all it takes to feed the fetch → parse → adapt → Sigma
 * pipeline the live page uses. This mirrors `MemoryGraphCanvas.stories.tsx`,
 * which drives its real canvas the same way.
 *
 * Every fixture node ships finite `x`/`y`, so `buildGraphFromSnapshot`
 * takes the precomputed-layout branch: no ForceAtlas2 worker, no
 * "Layout optimizing…" pill, and the graph paints immediately at
 * deterministic positions (stable screenshots).
 *
 * The default "architecture" lens hides every symbol node, so each story
 * seeds a permissive filter set (all node/symbol/edge kinds on) into the
 * real `useCodeGraphStore` before the canvas builds its graph — that's the
 * same store the live toolbar drives. Selection / citation / color-mode
 * states are seeded the same way.
 *
 * The `CitationBadgeMultiId` story is unchanged: it drives the real
 * `CitationStatusBadge` through the `setCitations` store seam.
 */

import { useEffect, useState } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, userEvent, waitFor, within } from "storybook/test";

import { CitationStatusBadge, CodeGraphCanvas } from "./CodeGraphCanvas";
import type { SnapshotPayload } from "@/lib/codeGraphAdapter";
import { setMcpToolResponder } from "@/storybook-mocks/mcpClient";
import {
  EDGE_KINDS,
  SYMBOL_KIND_FILTERS,
  useCodeGraphStore,
  type ColorMode,
} from "@/stores/codeGraphStore";

// ── Fixture: a small single-project codebase (5 files + 12 symbols) ──────────
//
// SCIP-ish ids, real symbol kinds, ContainsDefinition (containment →
// nesting, not drawn), SymbolReference / Reads / Writes edges, and per-
// function cognitive complexity so the heatmap story has a real
// distribution (handleLogin @ 31 is the refactor candidate). All symbols
// are structural kinds (function/method/class/interface) so the LOD "mid"
// tier keeps them visible at the default fit-to-view zoom.

interface FixtureNode {
  id: string;
  kind: SnapshotPayload["nodes"][number]["kind"];
  label: string;
  symbol_kind?: string;
  file_path?: string;
  pagerank: number;
  cognitive?: number;
  workspace?: string;
  x: number;
  y: number;
}

interface FixtureEdge {
  from: string;
  to: string;
  kind: string;
  confidence?: number;
}

const PROJECT_NODES: FixtureNode[] = [
  { id: "folder:src", kind: "folder", label: "src", pagerank: 1.0, x: 10, y: 10 },

  // server.ts
  { id: "file:server.ts", kind: "file", label: "server.ts", file_path: "src/server.ts", pagerank: 0.4, x: 4, y: 16 },
  { id: "sym:createServer", kind: "symbol", label: "createServer", symbol_kind: "function", file_path: "src/server.ts", pagerank: 0.55, cognitive: 5, x: 2, y: 18 },

  // router.ts
  { id: "file:router.ts", kind: "file", label: "router.ts", file_path: "src/router.ts", pagerank: 0.5, x: 16, y: 16 },
  { id: "sym:Router", kind: "symbol", label: "Router", symbol_kind: "class", file_path: "src/router.ts", pagerank: 0.5, x: 18, y: 17 },
  { id: "sym:Router.register", kind: "symbol", label: "Router.register", symbol_kind: "method", file_path: "src/router.ts", pagerank: 0.35, cognitive: 8, x: 17, y: 19 },
  { id: "sym:Router.dispatch", kind: "symbol", label: "Router.dispatch", symbol_kind: "method", file_path: "src/router.ts", pagerank: 0.85, cognitive: 22, x: 19.5, y: 14 },

  // auth.ts
  { id: "file:auth.ts", kind: "file", label: "auth.ts", file_path: "src/auth.ts", pagerank: 0.55, x: 4, y: 4 },
  { id: "sym:authenticate", kind: "symbol", label: "authenticate", symbol_kind: "function", file_path: "src/auth.ts", pagerank: 0.7, cognitive: 14, x: 2, y: 6 },
  { id: "sym:hashToken", kind: "symbol", label: "hashToken", symbol_kind: "function", file_path: "src/auth.ts", pagerank: 0.4, cognitive: 6, x: 2, y: 2 },
  { id: "sym:AuthConfig", kind: "symbol", label: "AuthConfig", symbol_kind: "interface", file_path: "src/auth.ts", pagerank: 0.3, x: 5, y: 1 },

  // db.ts
  { id: "file:db.ts", kind: "file", label: "db.ts", file_path: "src/db.ts", pagerank: 0.5, x: 16, y: 4 },
  { id: "sym:Database", kind: "symbol", label: "Database", symbol_kind: "class", file_path: "src/db.ts", pagerank: 0.5, x: 18, y: 5 },
  { id: "sym:Database.query", kind: "symbol", label: "Database.query", symbol_kind: "method", file_path: "src/db.ts", pagerank: 0.75, cognitive: 11, x: 19.5, y: 2 },
  { id: "sym:Database.connect", kind: "symbol", label: "Database.connect", symbol_kind: "method", file_path: "src/db.ts", pagerank: 0.45, cognitive: 4, x: 17, y: 7 },

  // handlers.ts
  { id: "file:handlers.ts", kind: "file", label: "handlers.ts", file_path: "src/handlers.ts", pagerank: 0.6, x: 10, y: 18 },
  { id: "sym:handleLogin", kind: "symbol", label: "handleLogin", symbol_kind: "function", file_path: "src/handlers.ts", pagerank: 0.9, cognitive: 31, x: 8, y: 20 },
  { id: "sym:handleLogout", kind: "symbol", label: "handleLogout", symbol_kind: "function", file_path: "src/handlers.ts", pagerank: 0.4, cognitive: 7, x: 12, y: 20 },
];

const PROJECT_EDGES: FixtureEdge[] = [
  // Containment (folder → file, file → symbol). Converted to nesting
  // metadata by the adapter; never drawn as Sigma edges.
  { from: "folder:src", to: "file:server.ts", kind: "ContainsDefinition" },
  { from: "folder:src", to: "file:router.ts", kind: "ContainsDefinition" },
  { from: "folder:src", to: "file:auth.ts", kind: "ContainsDefinition" },
  { from: "folder:src", to: "file:db.ts", kind: "ContainsDefinition" },
  { from: "folder:src", to: "file:handlers.ts", kind: "ContainsDefinition" },
  { from: "file:server.ts", to: "sym:createServer", kind: "ContainsDefinition" },
  { from: "file:router.ts", to: "sym:Router", kind: "ContainsDefinition" },
  { from: "file:router.ts", to: "sym:Router.register", kind: "ContainsDefinition" },
  { from: "file:router.ts", to: "sym:Router.dispatch", kind: "ContainsDefinition" },
  { from: "file:auth.ts", to: "sym:authenticate", kind: "ContainsDefinition" },
  { from: "file:auth.ts", to: "sym:hashToken", kind: "ContainsDefinition" },
  { from: "file:auth.ts", to: "sym:AuthConfig", kind: "ContainsDefinition" },
  { from: "file:db.ts", to: "sym:Database", kind: "ContainsDefinition" },
  { from: "file:db.ts", to: "sym:Database.query", kind: "ContainsDefinition" },
  { from: "file:db.ts", to: "sym:Database.connect", kind: "ContainsDefinition" },
  { from: "file:handlers.ts", to: "sym:handleLogin", kind: "ContainsDefinition" },
  { from: "file:handlers.ts", to: "sym:handleLogout", kind: "ContainsDefinition" },

  // Drawn relationships — the call graph + data flow.
  { from: "sym:createServer", to: "sym:Router.register", kind: "SymbolReference", confidence: 0.95 },
  { from: "sym:createServer", to: "sym:Database.connect", kind: "SymbolReference", confidence: 0.9 },
  { from: "sym:Router.dispatch", to: "sym:handleLogin", kind: "SymbolReference", confidence: 0.95 },
  { from: "sym:Router.dispatch", to: "sym:handleLogout", kind: "SymbolReference", confidence: 0.9 },
  { from: "sym:handleLogin", to: "sym:authenticate", kind: "SymbolReference", confidence: 0.98 },
  { from: "sym:handleLogin", to: "sym:Database.query", kind: "Writes", confidence: 0.85 },
  { from: "sym:handleLogout", to: "sym:Database.query", kind: "SymbolReference", confidence: 0.8 },
  { from: "sym:authenticate", to: "sym:hashToken", kind: "SymbolReference", confidence: 0.95 },
  { from: "sym:authenticate", to: "sym:AuthConfig", kind: "Reads", confidence: 0.9 },
  { from: "sym:hashToken", to: "sym:AuthConfig", kind: "Reads", confidence: 0.85 },
  { from: "sym:Database.query", to: "sym:Database.connect", kind: "SymbolReference", confidence: 0.9 },
];

// ── Fixture: two workspaces (api + web) with a cross-workspace edge ───────────

const WORKSPACE_NODES: FixtureNode[] = [
  // api workspace
  { id: "ws:file:api/routes.ts", kind: "file", label: "routes.ts", file_path: "api/src/routes.ts", workspace: "api", pagerank: 0.6, x: 4, y: 10 },
  { id: "ws:sym:registerRoutes", kind: "symbol", label: "registerRoutes", symbol_kind: "function", file_path: "api/src/routes.ts", workspace: "api", pagerank: 0.5, cognitive: 6, x: 2, y: 12 },
  { id: "ws:sym:createHandler", kind: "symbol", label: "createHandler", symbol_kind: "function", file_path: "api/src/routes.ts", workspace: "api", pagerank: 0.8, cognitive: 12, x: 6, y: 8 },
  { id: "ws:file:api/db.ts", kind: "file", label: "db.ts", file_path: "api/src/db.ts", workspace: "api", pagerank: 0.5, x: 3, y: 5 },
  { id: "ws:sym:apiQuery", kind: "symbol", label: "apiQuery", symbol_kind: "function", file_path: "api/src/db.ts", workspace: "api", pagerank: 0.6, cognitive: 8, x: 1, y: 4 },

  // web workspace
  { id: "ws:file:web/App.tsx", kind: "file", label: "App.tsx", file_path: "web/src/App.tsx", workspace: "web", pagerank: 0.55, x: 16, y: 12 },
  { id: "ws:sym:submitForm", kind: "symbol", label: "submitForm", symbol_kind: "function", file_path: "web/src/App.tsx", workspace: "web", pagerank: 0.6, cognitive: 15, x: 18, y: 14 },
  { id: "ws:file:web/api.ts", kind: "file", label: "api.ts", file_path: "web/src/api.ts", workspace: "web", pagerank: 0.5, x: 15, y: 8 },
  { id: "ws:sym:useApiClient", kind: "symbol", label: "useApiClient", symbol_kind: "function", file_path: "web/src/api.ts", workspace: "web", pagerank: 0.7, cognitive: 9, x: 13, y: 9 },
];

const WORKSPACE_EDGES: FixtureEdge[] = [
  // Containment
  { from: "ws:file:api/routes.ts", to: "ws:sym:registerRoutes", kind: "ContainsDefinition" },
  { from: "ws:file:api/routes.ts", to: "ws:sym:createHandler", kind: "ContainsDefinition" },
  { from: "ws:file:api/db.ts", to: "ws:sym:apiQuery", kind: "ContainsDefinition" },
  { from: "ws:file:web/App.tsx", to: "ws:sym:submitForm", kind: "ContainsDefinition" },
  { from: "ws:file:web/api.ts", to: "ws:sym:useApiClient", kind: "ContainsDefinition" },

  // Intra-workspace calls
  { from: "ws:sym:registerRoutes", to: "ws:sym:createHandler", kind: "SymbolReference", confidence: 0.95 },
  { from: "ws:sym:createHandler", to: "ws:sym:apiQuery", kind: "SymbolReference", confidence: 0.9 },
  { from: "ws:sym:submitForm", to: "ws:sym:useApiClient", kind: "SymbolReference", confidence: 0.95 },

  // Cross-workspace: web → api (rendered yellow + dashed by the adapter).
  { from: "ws:sym:useApiClient", to: "ws:sym:createHandler", kind: "SymbolReference", confidence: 0.9 },
];

/**
 * Coordinates are authored on a ~0..20 grid for readability, then scaled
 * into a ~[0,1] box. Sigma normalizes for the initial fit regardless of
 * scale, but `focusNodes` (the citation auto-focus) derives its camera
 * ratio from *raw* graph coordinates — a ~20-unit span would zoom the
 * camera 20× too far out and blank the canvas. Keeping raw extent ~1 makes
 * that ratio land sensibly.
 */
const LAYOUT_SCALE = 1 / 20;

function buildSnapshot(
  nodes: FixtureNode[],
  edges: FixtureEdge[],
): SnapshotPayload {
  return {
    project_id: "project-djinn",
    git_head: "abc1234",
    generated_at: "2026-07-13T00:00:00.000Z",
    truncated: false,
    total_nodes: nodes.length,
    total_edges: edges.length,
    node_cap: 10_000,
    // `FixtureNode` is a subset of `SnapshotNode` (every extra field on
    // `SnapshotNode` is optional), so this assigns directly (with coords
    // scaled into the ~[0,1] box).
    nodes: nodes.map((n) => ({
      ...n,
      x: n.x * LAYOUT_SCALE,
      y: n.y * LAYOUT_SCALE,
    })),
    edges: edges.map((e) => ({
      from: e.from,
      to: e.to,
      kind: e.kind,
      confidence: e.confidence ?? 1,
    })),
  };
}

const PROJECT_SNAPSHOT = buildSnapshot(PROJECT_NODES, PROJECT_EDGES);
const WORKSPACE_SNAPSHOT = buildSnapshot(WORKSPACE_NODES, WORKSPACE_EDGES);

/**
 * Build a responder for the aliased `@/api/mcpClient` mock. Answers the
 * `code_graph` snapshot op with `{ snapshot }` (the exact shape
 * `parseSnapshotResponse` expects); everything else resolves to `{}`.
 */
function snapshotResponder(snapshot: SnapshotPayload) {
  return (name: string, args: Record<string, unknown> | undefined) => {
    if (name === "code_graph" && args?.operation === "snapshot") {
      return { snapshot };
    }
    return {};
  };
}

// ── Permissive filter set ────────────────────────────────────────────────────
// The default "architecture" lens hides every symbol; these open all node /
// symbol / edge kinds so files + symbols + the call graph all render.
// Containment edges stay excluded by the adapter regardless.

const ALL_NODE_KINDS = { folder: true, file: true, symbol: true };
const ALL_SYMBOL_KINDS = Object.fromEntries(
  SYMBOL_KIND_FILTERS.map((k) => [k, true]),
);
const ALL_EDGE_KINDS = Object.fromEntries(EDGE_KINDS.map((k) => [k, true]));

/**
 * Stable empty-array default so the seed effect below doesn't re-run (and
 * `reset()` on cleanup) on every render — a fresh `[]` literal default would
 * change identity each render and thrash the store (including the heatmap's
 * complexity-available gate).
 */
const NO_IDS: string[] = [];

interface HarnessProps {
  selectionId?: string | null;
  citationIds?: string[];
  toolHighlightIds?: string[];
  colorMode?: ColorMode;
  /** Pin the canvas to a single workspace (drives cross-workspace context). */
  workspaceSlug?: string | null;
}

/**
 * Mounts the real `<CodeGraphCanvas>` and seeds the highlight store so the
 * populated graph is visible. The canvas resets the store on mount (a child
 * effect); this parent effect runs *after* that reset, so the seed wins.
 */
function CanvasHarness({
  selectionId = null,
  citationIds = NO_IDS,
  toolHighlightIds = NO_IDS,
  colorMode = "topology",
  workspaceSlug = null,
}: HarnessProps) {
  // Complexity availability is reported up by the canvas once the snapshot is
  // parsed and the heatmap thresholds are computed.
  const complexityAvailable = useCodeGraphStore((s) => s.complexityAvailable);

  useEffect(() => {
    useCodeGraphStore.setState({
      nodeKindFilters: ALL_NODE_KINDS,
      symbolKindFilters: ALL_SYMBOL_KINDS,
      edgeKindFilters: ALL_EDGE_KINDS,
      activeLens: null,
      selectionId,
      citationIds: new Set(citationIds),
      toolHighlightIds: new Set(toolHighlightIds),
      selectedWorkspaceSlug: workspaceSlug,
    });
    return () => {
      useCodeGraphStore.getState().reset();
    };
  }, [selectionId, citationIds, toolHighlightIds, workspaceSlug]);

  // Engage the complexity heatmap only *after* the canvas reports complexity
  // available. Setting `colorMode` earlier races the canvas's own
  // `setComplexityAvailable(false)` effect-cleanup (fired as the thresholds
  // transition null → non-null), whose guard would snap `colorMode` back to
  // topology. Keying off `complexityAvailable` means the snap has already
  // happened, so the mode sticks — the same sequencing the live toolbar sees
  // (the heatmap toggle only enables once the graph is ready).
  useEffect(() => {
    if (colorMode === "complexity" && complexityAvailable) {
      useCodeGraphStore.getState().setColorMode("complexity");
    }
  }, [colorMode, complexityAvailable]);

  return (
    <div className="relative h-screen w-full">
      <CodeGraphCanvas projectId="project-djinn" />
    </div>
  );
}

const meta = {
  title: "CodeGraph/CodeGraphCanvas",
  component: CanvasHarness,
  parameters: { layout: "fullscreen" },
} satisfies Meta<typeof CanvasHarness>;

export default meta;
type Story = StoryObj<typeof meta>;

/**
 * Populated topology view: files + symbols + the call graph, colored by
 * crate/folder. The top-5 most-complex symbols wear the persistent red
 * refactor-candidate halo even in topology mode.
 */
export const Default: Story = {
  beforeEach: () => {
    setMcpToolResponder(snapshotResponder(PROJECT_SNAPSHOT));
  },
};

/**
 * `handleLogin` selected: it renders orange, its 1-hop neighborhood
 * (Router.dispatch, authenticate, Database.query) amber, and the rest of
 * the graph dims. Neighbors are derived from the real graph topology by
 * `useGraphReducers` — the story only seeds `selectionId`.
 */
export const Selection: Story = {
  args: { selectionId: "sym:handleLogin" },
  beforeEach: () => {
    setMcpToolResponder(snapshotResponder(PROJECT_SNAPSHOT));
  },
};

/**
 * Three AI citations pinned. Cited symbols render sky-blue, the
 * `CitationStatusBadge` shows "3 citations pinned", and the canvas
 * auto-focuses the camera on the citation set (`useAutoFocusOnCitations`).
 */
export const Citations: Story = {
  args: {
    citationIds: ["sym:authenticate", "sym:hashToken", "sym:handleLogin"],
  },
  beforeEach: () => {
    setMcpToolResponder(snapshotResponder(PROJECT_SNAPSHOT));
  },
};

/**
 * Complexity heatmap: symbols recolor along the green→red cognitive-
 * complexity gradient (handleLogin @ 31 is red, Router.dispatch @ 22
 * orange, the tame functions green) and the ComplexityLegend renders
 * bottom-right. The canvas reports complexity availability up through the
 * store; the story just pins `colorMode: "complexity"`.
 */
export const ComplexityHeatmap: Story = {
  args: { colorMode: "complexity" },
  beforeEach: () => {
    setMcpToolResponder(snapshotResponder(PROJECT_SNAPSHOT));
  },
};

/**
 * Two workspaces (api + web) with the `api` workspace pinned. The api
 * cluster renders fully; the `web` endpoint (`useApiClient`) is pulled in
 * as dimmed remote context, connected by the yellow dashed cross-workspace
 * dependency. Driven by `filterSnapshotForWorkspace` via the real
 * `selectedWorkspaceSlug` store slice.
 */
export const WorkspacesAndCrossWorkspaceEdge: Story = {
  args: { workspaceSlug: "api" },
  beforeEach: () => {
    setMcpToolResponder(snapshotResponder(WORKSPACE_SNAPSHOT));
  },
};

/**
 * Empty snapshot → the "No graph data yet" overlay path. The canvas chrome
 * renders even when WebGL can't paint, so this covers the empty state.
 */
export const Empty: Story = {
  beforeEach: () => {
    setMcpToolResponder(
      snapshotResponder(buildSnapshot([], [])),
    );
  },
};

// ── Multi-id citation badge story (g293) ──────────────────────────────────
//
// Unchanged from the prior file: drives the *real* `CitationStatusBadge`
// through the `setCitations` store seam (the same action the chat-agent
// harvest uses) and asserts the multi-id "3 citations pinned" text against
// the production component.

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

type BadgeStory = StoryObj<typeof CitationBadgeHarness>;

/**
 * Pre-populates `citationIds` with a 3-id set via `setCitations` and
 * asserts the `CitationStatusBadge` renders the multi-id
 * "3 citations pinned" text.
 */
export const CitationBadgeMultiId: BadgeStory = {
  parameters: { layout: "centered" },
  render: () => <CitationBadgeHarness />,
  play: async ({ canvasElement }) => {
    useCodeGraphStore.getState().clearCitations();
    const canvas = within(canvasElement);

    expect(canvas.queryByTestId("citation-status")).toBeNull();

    await userEvent.click(canvas.getByTestId("populate-citations"));

    await waitFor(() => {
      expect(canvas.getByTestId("citation-status")).toHaveTextContent(
        "3 citations pinned",
      );
    });
  },
};
