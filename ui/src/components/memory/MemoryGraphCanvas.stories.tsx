/**
 * Memory/MemoryGraphCanvas — the radial time-disk knowledge graph.
 *
 * The canvas fetches `memory_graph` via `callMcpTool` and renders a canvas-2d
 * time disk (notes placed by age, calendar rings, playable chronological
 * reveal). Storybook runs in a real browser, so we render the real component
 * and seed graphs by installing a per-story responder on the aliased
 * `@/api/mcpClient` mock (see `.storybook/main.ts`) — no mock server, and no
 * `vi.mock`, which crashes under `storybook dev`.
 *
 * The wire payload's optional per-node `created_at` (epoch seconds or ISO
 * string) drives the time axis; the `Undated` story exercises the ordinal
 * fallback used when the backend doesn't provide it. The `LargeGraph` fixture
 * spreads ~30 notes across four months in bursts (incident clusters, mining
 * sessions) so the calendar rings and the replay build-up actually read.
 */

import type { Meta, StoryObj } from "@storybook/react-vite";

import { MemoryGraphCanvas } from "./MemoryGraphCanvas";
import { createSeededRandom } from "@/lib/memoryGraphAdapter";
import { validLifecycleResponse } from "@/lib/__fixtures__/memoryGraphLifecycle";
import { setMcpToolResponder } from "@/storybook-mocks/mcpClient";

// ── Seeded memory_graph payloads ─────────────────────────────────────────────

const ts = (iso: string) => Math.floor(Date.parse(iso) / 1000);

const node = (
  id: string,
  title: string,
  note_type: string,
  connection_count: number,
  created_at: string | null,
  extra: { is_orphan?: boolean; entity_type?: string } = {},
) => ({
  id,
  permalink: `memory/${note_type}/${id}`,
  title,
  note_type,
  folder: `memory/${note_type}`,
  connection_count,
  is_orphan: extra.is_orphan ?? false,
  broken_targets: [],
  ...(extra.entity_type ? { entity_type: extra.entity_type } : {}),
  ...(created_at ? { created_at: ts(created_at) } : {}),
});

const edge = (source_id: string, target_id: string) => ({
  source_id,
  target_id,
  raw_text: target_id,
});

/** 8 dated notes: typed + wikilink edges, an orphan — the quick smoke fixture. */
const smallPayload = {
  nodes: [
    node("oomkill", "Server OOMKill wiped tribunals", "pitfall", 4, "2026-07-12T08:00:00Z"),
    node("death-chain", "Tribunal death chain", "case", 3, "2026-07-12T15:00:00Z"),
    node("watchdog", "Refinement watchdog pending time", "pitfall", 2, "2026-07-12T18:00:00Z"),
    node("advisory-lock", "Coordinator advisory-lock leadership", "adr", 3, "2026-05-02T10:00:00Z"),
    node("cutover", "Cut-over over strangler", "pattern", 2, "2026-05-20T10:00:00Z"),
    node("openviking", "OpenViking context DB", "reference", 1, "2026-06-14T10:00:00Z"),
    node("merge-queue", "Merge queue", "entity", 3, "2026-04-08T10:00:00Z"),
    node("taskpod-cpu", "Task pods run 4 vCPU", "claim", 1, "2026-06-30T10:00:00Z", { is_orphan: true }),
  ],
  edges: [
    edge("oomkill", "death-chain"),
    edge("oomkill", "watchdog"),
    edge("death-chain", "advisory-lock"),
    edge("advisory-lock", "merge-queue"),
    edge("cutover", "merge-queue"),
    edge("merge-queue", "oomkill"),
  ],
  typed_edges: [
    { source_id: "death-chain", target_id: "oomkill", kind: "builds_on", weight: 0.8 },
    { source_id: "watchdog", target_id: "advisory-lock", kind: "contradicts", weight: 0.5 },
  ],
};

/** ~30 dated notes across four months, in bursts — the full replay fixture. */
const largePayload = {
  nodes: [
    // ── Late March: founding decisions ────────────────────────────────────
    node("postgres-only", "Dolt retired — Postgres only", "adr", 4, "2026-03-20T14:00:00Z"),
    node("tilt-canonical", "Tilt is the canonical local workflow", "pattern", 2, "2026-03-22T10:00:00Z"),
    node("advisory-lock", "Coordinator advisory-lock leadership", "adr", 5, "2026-03-26T16:00:00Z"),
    node("openviking", "OpenViking context DB", "reference", 2, "2026-03-28T11:00:00Z"),

    // ── Mid April: build/cache warm work ──────────────────────────────────
    node("seeder-hardlink", "Seeder hardlinks cargo lock across pods", "pitfall", 3, "2026-04-10T09:00:00Z"),
    node("warm-graph-langs", "Warm graph three-language fixes", "case", 2, "2026-04-12T15:00:00Z"),
    node("scip-versions", "Seven SCIP indexers, pinned versions", "reference", 2, "2026-04-14T13:00:00Z"),
    node("merge-queue", "Merge queue", "entity", 6, "2026-04-16T10:00:00Z"),
    node("ci-all-features", "CI gates on --all-features", "pattern", 2, "2026-04-18T17:00:00Z"),

    // ── Early May: throughput collapse incident ───────────────────────────
    node("stall-kills", "No-op heartbeats caused mass stall kills", "case", 4, "2026-05-05T08:00:00Z"),
    node("cost-basis", "Cost basis is string-derived", "pitfall", 2, "2026-05-06T12:00:00Z"),
    node("per-user-breaker", "Breaker is per (scope, model)", "adr", 3, "2026-05-07T15:00:00Z"),
    node("taskpod-cpu", "Task pods run 4 vCPU", "claim", 1, "2026-05-08T10:00:00Z"),
    node("codex-empty-turn", "Empty turn means Codex throttling", "pitfall", 2, "2026-05-09T18:00:00Z"),

    // ── Early June: refinement bugs + repo mining ─────────────────────────
    node("round-collision", "Refinement rounds collide across runs", "pitfall", 3, "2026-06-01T09:00:00Z"),
    node("advocate-starve", "Restarted refinement starves the advocate", "pitfall", 3, "2026-06-03T14:00:00Z"),
    node("memory-attribution", "Write-dedup LLM is unattributed", "research", 1, "2026-06-05T11:00:00Z"),
    node("reference-mining", "Reference repo steal-list", "reference", 4, "2026-06-07T16:00:00Z"),
    node("moa-planner", "Ensemble models for planner/review roles", "research", 2, "2026-06-08T10:00:00Z", {
      entity_type: "proposal",
    }),

    // ── Mid June: proposal pipeline hardening ─────────────────────────────
    node("ac-merge-wipe", "AC merge wipes string-form criteria", "pitfall", 3, "2026-06-18T09:00:00Z"),
    node("block-attr", "Blocks must render attr content", "pitfall", 2, "2026-06-19T12:00:00Z"),
    node("markdown-bypass", "Markdown bodies skip block validation", "pitfall", 2, "2026-06-20T15:00:00Z"),
    node("tribunal", "Tribunal", "entity", 5, "2026-06-22T10:00:00Z"),
    node("stop-build", "proposal_stop_build is the inverse of graduate", "adr", 2, "2026-06-24T14:00:00Z", {
      entity_type: "proposal",
    }),

    // ── Mid July: the OOMKill week ────────────────────────────────────────
    node("oomkill", "Server OOMKill wiped in-flight tribunals", "case", 6, "2026-07-12T08:00:00Z"),
    node("death-chain", "Tribunal death chain", "case", 4, "2026-07-12T11:00:00Z"),
    node("watchdog-pending", "Watchdog counts pod Pending time", "pitfall", 3, "2026-07-12T14:00:00Z"),
    node("utf8-panic", "UTF-8 byte-slice panic kills the actor", "pitfall", 3, "2026-07-12T17:00:00Z"),
    node("graph-double-load", "570MiB graph double-load at 2Gi", "case", 3, "2026-07-12T20:00:00Z"),
    node("restart-resume", "Restart-resume verified live", "claim", 2, "2026-07-13T09:00:00Z"),
    node("durable-delivery", "Durable completion delivery ledger", "research", 2, "2026-07-13T12:00:00Z"),
    node("scratch-note", "Unlinked scratch note", "claim", 0, "2026-07-13T13:00:00Z", { is_orphan: true }),
  ],
  edges: [
    edge("postgres-only", "advisory-lock"),
    edge("tilt-canonical", "advisory-lock"),
    edge("seeder-hardlink", "warm-graph-langs"),
    edge("warm-graph-langs", "scip-versions"),
    edge("seeder-hardlink", "merge-queue"),
    edge("ci-all-features", "merge-queue"),
    edge("stall-kills", "per-user-breaker"),
    edge("cost-basis", "per-user-breaker"),
    edge("codex-empty-turn", "per-user-breaker"),
    edge("stall-kills", "merge-queue"),
    edge("round-collision", "advocate-starve"),
    edge("round-collision", "tribunal"),
    edge("reference-mining", "moa-planner"),
    edge("reference-mining", "openviking"),
    edge("ac-merge-wipe", "block-attr"),
    edge("block-attr", "markdown-bypass"),
    edge("ac-merge-wipe", "tribunal"),
    edge("stop-build", "tribunal"),
    edge("oomkill", "death-chain"),
    edge("oomkill", "graph-double-load"),
    edge("death-chain", "utf8-panic"),
    edge("death-chain", "watchdog-pending"),
    edge("oomkill", "advisory-lock"),
    edge("restart-resume", "oomkill"),
    edge("durable-delivery", "reference-mining"),
    edge("oomkill", "tribunal"),
  ],
  typed_edges: [
    { source_id: "death-chain", target_id: "oomkill", kind: "builds_on", weight: 0.8 },
    { source_id: "restart-resume", target_id: "oomkill", kind: "supersedes", weight: 0.7 },
    { source_id: "watchdog-pending", target_id: "advisory-lock", kind: "contradicts", weight: 0.5 },
    { source_id: "durable-delivery", target_id: "moa-planner", kind: "builds_on", weight: 0.6 },
  ],
};

/**
 * Prod-scale synthetic payload (~6.5k notes over five months, recency-heavy,
 * ~55% orphans — the shape that broke the first cut of this canvas). Seeded so
 * every render is identical.
 */
function densePayload(count = 6500) {
  const rng = createSeededRandom(11);
  const types = ["pitfall", "case", "adr", "pattern", "reference", "research", "entity", "claim"];
  const start = ts("2026-02-15T00:00:00Z");
  const end = ts("2026-07-13T00:00:00Z");
  const nodes = [];
  const edges = [];
  for (let i = 0; i < count; i += 1) {
    // Recency-heavy spread (prod memory grows over time), with day-level noise.
    const t = start + Math.floor(Math.pow(rng(), 0.6) * (end - start));
    const note_type = types[Math.floor(rng() * types.length)];
    const is_orphan = rng() < 0.55;
    const connection_count = is_orphan ? 0 : Math.floor(rng() * rng() * 12);
    nodes.push({
      id: `n${i}`,
      permalink: `memory/${note_type}/n${i}`,
      title: `Synthetic note ${i}`,
      note_type,
      folder: `memory/${note_type}`,
      connection_count,
      is_orphan,
      broken_targets: [],
      created_at: t,
    });
    if (i > 0 && !is_orphan && rng() < 0.6) {
      edges.push({ source_id: `n${i}`, target_id: `n${Math.floor(rng() * i)}`, raw_text: `Synthetic note` });
    }
  }
  return { nodes, edges, typed_edges: [] };
}

/**
 * The lifecycle fixture is deliberately small enough that the visual states
 * below remain easy to inspect. Add stable creation times so its disk has the
 * same calendar geometry every time Storybook opens it.
 */
const lifecyclePayload = {
  ...validLifecycleResponse,
  nodes: validLifecycleResponse.nodes.map((node, index) => ({
    ...node,
    created_at: ts(`2026-07-${String(12 + index).padStart(2, "0")}T12:00:00Z`),
  })),
};

const lifecycleActiveOnlyPayload = {
  ...lifecyclePayload,
  nodes: lifecyclePayload.nodes.filter((node) => node.status === "active"),
  edges: [],
  typed_edges: [],
};

const lifecycleCapPayload = {
  ...lifecyclePayload,
  lifecycle_summary: { inactive_total: 503, inactive_returned: 500, inactive_omitted: 3 },
};

const mixedLifecycleSupersedesPayload = {
  ...lifecyclePayload,
  typed_edges: [{ source_id: "archived-note", target_id: "active-note", kind: "supersedes", weight: 1 }],
};

const ghostConnectedContradictsPayload = {
  ...lifecyclePayload,
  typed_edges: [{ source_id: "deprecated-note", target_id: "active-note", kind: "contradicts", weight: 1 }],
};

// Fixed transition data keeps this visual state repeatable and explicitly
// documents the seven-day recent-transition case.
const recentTransitionFadePayload = {
  ...lifecyclePayload,
  nodes: lifecyclePayload.nodes.map((node) =>
    node.id === "archived-note" ? { ...node, lifecycle_changed_at: "2026-07-20T12:00:00Z" } : node,
  ),
};

function setGhostPreference(enabled: boolean) {
  window.localStorage.setItem("djinn:memory-graph:lifecycle-ghosts:djinnos/djinn", enabled ? "1" : "0");
}

function useReducedMotion() {
  const original = window.matchMedia;
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    value: (query: string) => ({
      matches: query === "(prefers-reduced-motion: reduce)",
      media: query,
      onchange: null,
      addEventListener: () => undefined,
      removeEventListener: () => undefined,
      addListener: () => undefined,
      removeListener: () => undefined,
      dispatchEvent: () => false,
    }),
  });
  return () => Object.defineProperty(window, "matchMedia", { configurable: true, value: original });
}

/** Same notes with `created_at` stripped — drives the ordinal-fallback path. */
const undatedPayload = {
  ...smallPayload,
  nodes: smallPayload.nodes.map(({ created_at: _created_at, ...rest }) => rest),
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

/** Small seeded graph (8 dated notes): typed + wikilink edges, an orphan. */
export const SmallGraph: Story = {
  beforeEach: () => {
    setMcpToolResponder(graphResponder(smallPayload));
  },
};

/** ~30 dated notes across four months, in bursts — the full replay. */
export const LargeGraph: Story = {
  beforeEach: () => {
    setMcpToolResponder(graphResponder(largePayload));
  },
};

/** ~6.5k dated notes (prod scale) — layout speed, density scaling, orphan dimming. */
export const DenseGraph: Story = {
  beforeEach: () => {
    setMcpToolResponder(graphResponder(densePayload()));
  },
};

/** No timestamps → stable ordinal spread, unlabeled rings. */
export const Undated: Story = {
  beforeEach: () => {
    setMcpToolResponder(graphResponder(undatedPayload));
  },
};

/** Lifecycle preference off: responder returns an active-only replacement graph. */
export const LifecycleGhostsOff: Story = {
  beforeEach: () => {
    setGhostPreference(false);
    setMcpToolResponder(graphResponder(lifecycleActiveOnlyPayload));
  },
};

/** Default lifecycle preference on: archived and deprecated notes render as ghosts. */
export const LifecycleGhostsOn: Story = {
  beforeEach: () => {
    setGhostPreference(true);
    setMcpToolResponder(graphResponder(lifecyclePayload));
  },
};

/** Bounded lifecycle response exposes the inactive-node cap badge. */
export const LifecycleGhostCapBadge: Story = {
  beforeEach: () => {
    setGhostPreference(true);
    setMcpToolResponder(graphResponder(lifecycleCapPayload));
  },
};

/** Ghost titles and lifecycle status are available only through hover/focus interaction. */
export const GhostHoverAndFocusLabels: Story = {
  beforeEach: () => {
    setGhostPreference(true);
    setMcpToolResponder(graphResponder(lifecyclePayload));
  },
};

/** A mixed active/archived supersedes edge is drawn active-to-ghost. */
export const MixedLifecycleSupersedes: Story = {
  beforeEach: () => {
    setGhostPreference(true);
    setMcpToolResponder(graphResponder(mixedLifecycleSupersedesPayload));
  },
};

/** A ghost-connected contradicts edge remains dashed and non-directional. */
export const GhostConnectedContradicts: Story = {
  beforeEach: () => {
    setGhostPreference(true);
    setMcpToolResponder(graphResponder(ghostConnectedContradictsPayload));
  },
};

/** A recently archived note performs its one-shot 60% → 22% transition fade. */
export const RecentLifecycleTransitionFade: Story = {
  beforeEach: () => {
    setGhostPreference(true);
    setMcpToolResponder(graphResponder(recentTransitionFadePayload));
  },
};

/** Reduced motion renders the recent lifecycle ghost immediately at steady opacity. */
export const ReducedMotionLifecycleGhost: Story = {
  beforeEach: () => {
    setGhostPreference(true);
    setMcpToolResponder(graphResponder(recentTransitionFadePayload));
    return useReducedMotion();
  },
};

/** No notes → the "No notes yet" empty overlay. */
export const Empty: Story = {
  beforeEach: () => {
    setMcpToolResponder(graphResponder({ edges: [], nodes: [] }));
  },
};

/** The fetch rejects → the "Couldn't load the graph" error overlay. */
export const ErrorState: Story = {
  beforeEach: () => {
    setMcpToolResponder(graphResponder(new Error("memory graph unavailable")));
  },
};
